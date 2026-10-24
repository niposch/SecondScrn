//! PIN pairing with SPAKE2, bound to the TLS session.
//!
//! The tablet (client, SPAKE2 side B) displays a random 6-digit PIN. The PC
//! user types it into the host (side A). Both sides derive a shared key K that
//! only matches if they used the same PIN, then exchange key-confirmation MACs
//! over the TLS exporter value. Because the exporter differs on each leg of a
//! man-in-the-middle, an attacker cannot relay a successful pairing, and every
//! guess costs a full, user-visible pairing attempt.
//!
//! The SPAKE2 identities are the two certificate fingerprints, so the derived
//! key also commits to exactly these two device identities.

use rand::Rng;
use ring::hmac;
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};
use zeroize::Zeroizing;

use crate::{Fingerprint, SecurityError};

pub const PIN_DIGITS: usize = 6;

const HOST_CONFIRM: &[u8] = b"secondscrn pairing host confirm v1";
const CLIENT_CONFIRM: &[u8] = b"secondscrn pairing client confirm v1";

/// Generates a uniformly random 6-digit PIN.
pub fn generate_pin() -> String {
    let n: u32 = rand::rngs::OsRng.gen_range(0..1_000_000);
    format!("{n:06}")
}

/// Normalises user input ("123 456", "123-456") to digits.
pub fn normalize_pin(input: &str) -> Option<String> {
    let digits: String = input.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    (digits.len() == PIN_DIGITS && digits.chars().all(|c| c.is_ascii_digit())).then_some(digits)
}

fn spake_ids(host: Fingerprint, client: Fingerprint) -> (SpakeId, SpakeId) {
    (SpakeId::new(&host.0), SpakeId::new(&client.0))
}

fn confirm_mac(key: &[u8], label: &[u8], exporter: &[u8]) -> Vec<u8> {
    let k = hmac::Key::new(hmac::HMAC_SHA256, key);
    let mut ctx = hmac::Context::with_key(&k);
    ctx.update(label);
    ctx.update(exporter);
    ctx.sign().as_ref().to_vec()
}

fn verify_mac(key: &[u8], label: &[u8], exporter: &[u8], mac: &[u8]) -> bool {
    let k = hmac::Key::new(hmac::HMAC_SHA256, key);
    let mut msg = Vec::with_capacity(label.len() + exporter.len());
    msg.extend_from_slice(label);
    msg.extend_from_slice(exporter);
    hmac::verify(&k, &msg, mac).is_ok()
}

/// Client (tablet) side.
pub struct ClientPairing {
    spake: Spake2<Ed25519Group>,
}

impl ClientPairing {
    /// Starts pairing with the PIN shown on screen. Returns the SPAKE2 message
    /// to send in `PairStart`.
    pub fn start(pin: &str, host: Fingerprint, client: Fingerprint) -> (Self, Vec<u8>) {
        let (a, b) = spake_ids(host, client);
        let (spake, msg) = Spake2::<Ed25519Group>::start_b(&Password::new(pin.as_bytes()), &a, &b);
        (ClientPairing { spake }, msg)
    }

    /// Processes the host's `PairChallenge`. On success returns the MAC to
    /// send in `PairConfirm`. Failure means the PIN was wrong or the channel
    /// is being intercepted.
    pub fn finish(self, host_msg: &[u8], host_mac: &[u8], exporter: &[u8]) -> Result<Vec<u8>, SecurityError> {
        let key = Zeroizing::new(
            self.spake
                .finish(host_msg)
                .map_err(|_| SecurityError::Pairing("malformed SPAKE2 message"))?,
        );
        if !verify_mac(&key, HOST_CONFIRM, exporter, host_mac) {
            return Err(SecurityError::Pairing("PIN mismatch"));
        }
        Ok(confirm_mac(&key, CLIENT_CONFIRM, exporter))
    }
}

/// Host (PC) side, created once the user has typed the PIN.
pub struct HostPairing {
    key: Zeroizing<Vec<u8>>,
    exporter: Vec<u8>,
}

impl HostPairing {
    /// Returns the pairing state plus the SPAKE2 message and MAC for
    /// `PairChallenge`.
    pub fn respond(
        pin: &str,
        host: Fingerprint,
        client: Fingerprint,
        client_msg: &[u8],
        exporter: &[u8],
    ) -> Result<(Self, Vec<u8>, Vec<u8>), SecurityError> {
        let (a, b) = spake_ids(host, client);
        let (spake, msg) = Spake2::<Ed25519Group>::start_a(&Password::new(pin.as_bytes()), &a, &b);
        let key = Zeroizing::new(
            spake
                .finish(client_msg)
                .map_err(|_| SecurityError::Pairing("malformed SPAKE2 message"))?,
        );
        let mac = confirm_mac(&key, HOST_CONFIRM, exporter);
        Ok((HostPairing { key, exporter: exporter.to_vec() }, msg, mac))
    }

    /// Checks the client's `PairConfirm`. Only after this succeeds may the
    /// client be added to the trust store.
    pub fn verify(&self, client_mac: &[u8]) -> Result<(), SecurityError> {
        if verify_mac(&self.key, CLIENT_CONFIRM, &self.exporter, client_mac) {
            Ok(())
        } else {
            Err(SecurityError::Pairing("PIN mismatch"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: Fingerprint = Fingerprint([1; 32]);
    const CLIENT: Fingerprint = Fingerprint([2; 32]);
    const EXPORTER: [u8; 32] = [7; 32];

    fn run(client_pin: &str, host_pin: &str, client_exp: &[u8], host_exp: &[u8]) -> Result<(), SecurityError> {
        let (client, client_msg) = ClientPairing::start(client_pin, HOST, CLIENT);
        let (host, host_msg, host_mac) = HostPairing::respond(host_pin, HOST, CLIENT, &client_msg, host_exp)?;
        let client_mac = client.finish(&host_msg, &host_mac, client_exp)?;
        host.verify(&client_mac)
    }

    #[test]
    fn matching_pin_pairs() {
        assert!(run("123456", "123456", &EXPORTER, &EXPORTER).is_ok());
    }

    #[test]
    fn wrong_pin_fails() {
        assert!(run("123456", "123457", &EXPORTER, &EXPORTER).is_err());
    }

    #[test]
    fn different_tls_sessions_fail() {
        // Simulates a MITM terminating TLS separately towards each side.
        assert!(run("123456", "123456", &[7; 32], &[8; 32]).is_err());
    }

    #[test]
    fn identities_are_bound() {
        let (client, client_msg) = ClientPairing::start("123456", HOST, CLIENT);
        let (_, host_msg, host_mac) =
            HostPairing::respond("123456", HOST, Fingerprint([3; 32]), &client_msg, &EXPORTER).unwrap();
        assert!(client.finish(&host_msg, &host_mac, &EXPORTER).is_err());
    }

    #[test]
    fn forged_client_confirmation_is_rejected() {
        let (_, client_msg) = ClientPairing::start("123456", HOST, CLIENT);
        let (host, _, host_mac) = HostPairing::respond("123456", HOST, CLIENT, &client_msg, &EXPORTER).unwrap();
        // Replaying the host's own MAC must not be accepted as the client's.
        assert!(host.verify(&host_mac).is_err());
        assert!(host.verify(&[0; 32]).is_err());
    }

    #[test]
    fn pins_are_six_digits_and_normalised() {
        for _ in 0..100 {
            let pin = generate_pin();
            assert_eq!(pin.len(), 6);
            assert!(pin.chars().all(|c| c.is_ascii_digit()));
        }
        assert_eq!(normalize_pin(" 123-456 ").as_deref(), Some("123456"));
        assert_eq!(normalize_pin("12345"), None);
        assert_eq!(normalize_pin("12345a"), None);
    }
}
