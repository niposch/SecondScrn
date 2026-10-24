use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{SecretProtector, SecurityError, write_atomic};

/// SHA-256 of a peer's DER certificate.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of_cert(der: &[u8]) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, der);
        let mut out = [0u8; 32];
        out.copy_from_slice(digest.as_ref());
        Fingerprint(out)
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        let bytes = hex::decode(s).ok()?;
        Some(Fingerprint(bytes.try_into().ok()?))
    }

    /// Short, human-comparable form ("3F2A-91C0") for UI and logs.
    /// Not a security boundary on its own; pairing uses SPAKE2.
    pub fn short(&self) -> String {
        let h = hex::encode_upper(&self.0[..4]);
        format!("{}-{}", &h[..4], &h[4..])
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", self.short())
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.short())
    }
}

impl Serialize for Fingerprint {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Fingerprint::from_hex(&s).ok_or_else(|| serde::de::Error::custom("invalid fingerprint"))
    }
}

/// A device's long-term identity: ECDSA P-256 key and self-signed certificate.
pub struct Identity {
    cert_der: Vec<u8>,
    key_der: Zeroizing<Vec<u8>>,
    fingerprint: Fingerprint,
}

#[derive(Serialize, Deserialize)]
struct StoredIdentity {
    version: u32,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

impl Identity {
    /// Generates a fresh identity. `common_name` is informational only.
    pub fn generate(common_name: &str) -> Result<Self, SecurityError> {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let mut params = rcgen::CertificateParams::new(vec!["secondscrn.invalid".to_string()])?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, common_name.to_string());
        let cert = params.self_signed(&key)?;
        let cert_der = cert.der().to_vec();
        Ok(Identity {
            fingerprint: Fingerprint::of_cert(&cert_der),
            cert_der,
            key_der: Zeroizing::new(key.serialize_der()),
        })
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    pub(crate) fn key_der(&self) -> &[u8] {
        &self.key_der
    }

    /// Serialises the identity and encrypts it with `protector`.
    pub fn to_protected_bytes(&self, protector: &dyn SecretProtector) -> Result<Vec<u8>, SecurityError> {
        let stored = StoredIdentity {
            version: 1,
            cert_der: self.cert_der.clone(),
            key_der: self.key_der.to_vec(),
        };
        let plain = Zeroizing::new(serde_json::to_vec(&stored).expect("serialisable"));
        protector.protect(&plain)
    }

    pub fn from_protected_bytes(bytes: &[u8], protector: &dyn SecretProtector) -> Result<Self, SecurityError> {
        let plain = Zeroizing::new(protector.unprotect(bytes)?);
        let stored: StoredIdentity =
            serde_json::from_slice(&plain).map_err(|e| SecurityError::CorruptIdentity(e.to_string()))?;
        if stored.version != 1 {
            return Err(SecurityError::CorruptIdentity(format!("unsupported version {}", stored.version)));
        }
        // Validate that the key is usable before accepting the identity.
        rcgen::KeyPair::try_from(stored.key_der.as_slice())
            .map_err(|e| SecurityError::CorruptIdentity(e.to_string()))?;
        Ok(Identity {
            fingerprint: Fingerprint::of_cert(&stored.cert_der),
            cert_der: stored.cert_der,
            key_der: Zeroizing::new(stored.key_der),
        })
    }

    /// Loads the identity at `path`, generating and saving one if absent.
    /// A corrupt file is an error rather than silently replaced: replacing the
    /// identity would invalidate every pairing, so the caller decides.
    pub fn load_or_create(
        path: &Path,
        common_name: &str,
        protector: &dyn SecretProtector,
    ) -> Result<Self, SecurityError> {
        match std::fs::read(path) {
            Ok(bytes) => Self::from_protected_bytes(&bytes, protector),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let id = Self::generate(common_name)?;
                write_atomic(path, &id.to_protected_bytes(protector)?)?;
                tracing::info!(fingerprint = %id.fingerprint(), "generated new device identity");
                Ok(id)
            }
            Err(e) => Err(e.into()),
        }
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity").field("fingerprint", &self.fingerprint).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UnprotectedForTests;

    #[test]
    fn identities_are_unique_and_roundtrip() {
        let a = Identity::generate("a").unwrap();
        let b = Identity::generate("b").unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());

        let bytes = a.to_protected_bytes(&UnprotectedForTests).unwrap();
        let a2 = Identity::from_protected_bytes(&bytes, &UnprotectedForTests).unwrap();
        assert_eq!(a.fingerprint(), a2.fingerprint());
        assert_eq!(a.key_der(), a2.key_der());
    }

    #[test]
    fn load_or_create_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.bin");
        let a = Identity::load_or_create(&path, "x", &UnprotectedForTests).unwrap();
        let b = Identity::load_or_create(&path, "x", &UnprotectedForTests).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn corrupt_identity_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.bin");
        std::fs::write(&path, b"garbage").unwrap();
        assert!(matches!(
            Identity::load_or_create(&path, "x", &UnprotectedForTests),
            Err(SecurityError::CorruptIdentity(_))
        ));
    }

    #[test]
    fn fingerprint_hex_roundtrip_and_short_form() {
        let fp = Fingerprint([0xAB; 32]);
        assert_eq!(Fingerprint::from_hex(&fp.to_hex()), Some(fp));
        assert_eq!(fp.short(), "ABAB-ABAB");
        assert_eq!(Fingerprint::from_hex("abcd"), None);
    }
}
