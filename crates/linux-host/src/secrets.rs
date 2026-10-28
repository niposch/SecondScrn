//! Protection of the device identity at rest.
//!
//! The identity file is encrypted (AES-256-GCM) with a key kept in a separate
//! file readable only by the user (mode 0600). Like the user's other files it
//! is protected by the OS account; copying the identity file alone (e.g. into
//! a backup or bug report) does not leak the key.

use std::io::Write;
use std::path::PathBuf;

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use secondscrn_security::{SecretProtector, SecurityError};

pub struct FileKeyProtector {
    key_path: PathBuf,
}

impl FileKeyProtector {
    pub fn new(data_dir: &std::path::Path) -> Self {
        FileKeyProtector { key_path: data_dir.join("identity.key") }
    }

    fn key(&self) -> Result<LessSafeKey, SecurityError> {
        let bytes = match std::fs::read(&self.key_path) {
            Ok(b) if b.len() == 32 => b,
            Ok(_) => return Err(SecurityError::Protector("identity key file is damaged".into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.create()?,
            Err(e) => return Err(SecurityError::Protector(format!("cannot read the identity key: {e}"))),
        };
        let key = UnboundKey::new(&AES_256_GCM, &bytes).map_err(|_| SecurityError::Protector("bad key".into()))?;
        Ok(LessSafeKey::new(key))
    }

    fn create(&self) -> Result<Vec<u8>, SecurityError> {
        let mut key = vec![0u8; 32];
        SystemRandom::new().fill(&mut key).map_err(|_| SecurityError::Protector("no randomness".into()))?;
        if let Some(dir) = self.key_path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| SecurityError::Protector(e.to_string()))?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&self.key_path).map_err(|e| SecurityError::Protector(e.to_string()))?;
        file.write_all(&key).map_err(|e| SecurityError::Protector(e.to_string()))?;
        Ok(key)
    }
}

impl SecretProtector for FileKeyProtector {
    fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecurityError> {
        let key = self.key()?;
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).map_err(|_| SecurityError::Protector("no randomness".into()))?;
        let mut data = plaintext.to_vec();
        key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut data)
            .map_err(|_| SecurityError::Protector("encryption failed".into()))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&data);
        Ok(out)
    }

    fn unprotect(&self, ciphertext: &[u8]) -> Result<Vec<u8>, SecurityError> {
        if ciphertext.len() < NONCE_LEN {
            return Err(SecurityError::Protector("identity file is damaged".into()));
        }
        let key = self.key()?;
        let (nonce, sealed) = ciphertext.split_at(NONCE_LEN);
        let nonce =
            Nonce::try_assume_unique_for_key(nonce).map_err(|_| SecurityError::Protector("bad nonce".into()))?;
        let mut data = sealed.to_vec();
        let plain = key
            .open_in_place(nonce, Aad::empty(), &mut data)
            .map_err(|_| SecurityError::Protector("the identity could not be decrypted".into()))?;
        Ok(plain.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tamper_detection() {
        let dir = std::env::temp_dir().join(format!("secondscrn-key-test-{}", std::process::id()));
        let p = FileKeyProtector::new(&dir);
        let sealed = p.protect(b"secret identity").unwrap();
        assert_eq!(p.unprotect(&sealed).unwrap(), b"secret identity");
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(p.unprotect(&bad).is_err());
        // A second instance uses the same key file.
        assert_eq!(FileKeyProtector::new(&dir).unprotect(&sealed).unwrap(), b"secret identity");
        let _ = std::fs::remove_dir_all(dir);
    }
}
