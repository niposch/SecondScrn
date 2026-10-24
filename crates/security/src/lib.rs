//! Security primitives for SecondScrn.
//!
//! * [`identity`]: long-term device identity (ECDSA P-256 key + self-signed
//!   certificate). Peers are identified by the SHA-256 fingerprint of their
//!   certificate, never by IP address or name.
//! * [`tls`]: rustls configurations for the control channel. TLS 1.3 only,
//!   mutual authentication, certificate pinning instead of a PKI.
//! * [`pairing`]: first-time authorization. The tablet shows a 6-digit PIN,
//!   the PC user types it in, and both sides run SPAKE2 bound to the TLS
//!   channel, which defeats man-in-the-middle attacks on the first connection.
//! * [`trust`]: the persisted list of paired peers.
//! * [`SecretProtector`]: platform hook for encrypting private keys at rest
//!   (DPAPI on Windows, Android Keystore on Android).
//!
//! No custom cryptography: rustls/ring, the `spake2` crate and HMAC-SHA256.

pub mod identity;
pub mod pairing;
pub mod tls;
pub mod trust;

pub use identity::{Fingerprint, Identity};

#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    #[error("certificate generation failed: {0}")]
    CertGen(#[from] rcgen::Error),
    #[error("tls configuration error: {0}")]
    Tls(#[from] rustls::Error),
    #[error("stored identity is corrupt: {0}")]
    CorruptIdentity(String),
    #[error("secret protection failed: {0}")]
    Protector(String),
    #[error("pairing failed: {0}")]
    Pairing(&'static str),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// Encrypts secrets at rest using a platform facility bound to the current
/// user/device. Implementations must be authenticated (tamper-evident).
pub trait SecretProtector: Send + Sync {
    fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecurityError>;
    fn unprotect(&self, ciphertext: &[u8]) -> Result<Vec<u8>, SecurityError>;
}

/// No-op protector for tests and for platforms where the OS already isolates
/// application storage. Never use it for production identities on Windows.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnprotectedForTests;

impl SecretProtector for UnprotectedForTests {
    fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecurityError> {
        Ok(plaintext.to_vec())
    }
    fn unprotect(&self, ciphertext: &[u8]) -> Result<Vec<u8>, SecurityError> {
        Ok(ciphertext.to_vec())
    }
}

/// Writes `bytes` to `path` atomically (write to temp file + rename), so a
/// crash never leaves a truncated identity or trust store behind.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}
