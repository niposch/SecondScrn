//! Protects the host's private key at rest with DPAPI (current user scope).

use secondscrn_security::{SecretProtector, SecurityError};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::w;

/// Additional entropy binds the blob to this application.
const ENTROPY: &[u8] = b"SecondScrn host identity v1";

#[derive(Debug, Default, Clone, Copy)]
pub struct Dpapi;

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
}

unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    unsafe {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
        v
    }
}

impl SecretProtector for Dpapi {
    fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecurityError> {
        let mut out = CRYPT_INTEGER_BLOB::default();
        let entropy = blob(ENTROPY);
        unsafe {
            CryptProtectData(
                &blob(plaintext),
                w!("SecondScrn identity"),
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| SecurityError::Protector(e.to_string()))?;
            Ok(take(out))
        }
    }

    fn unprotect(&self, ciphertext: &[u8]) -> Result<Vec<u8>, SecurityError> {
        let mut out = CRYPT_INTEGER_BLOB::default();
        let entropy = blob(ENTROPY);
        unsafe {
            CryptUnprotectData(
                &blob(ciphertext),
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| SecurityError::Protector(e.to_string()))?;
            Ok(take(out))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tamper_detection() {
        let secret = b"private key material";
        let enc = Dpapi.protect(secret).unwrap();
        assert_ne!(&enc[..], &secret[..]);
        assert_eq!(Dpapi.unprotect(&enc).unwrap(), secret);
        let mut bad = enc.clone();
        let n = bad.len() - 5;
        bad[n] ^= 0xff;
        assert!(Dpapi.unprotect(&bad).is_err());
    }
}
