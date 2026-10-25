# Security

Goal: a tablet on an untrusted network can only show the PC's screen and send
input after the PC user explicitly authorised that specific tablet, and
nobody on the network can read or alter the stream or the input.

## Identities

Each device generates an ECDSA P-256 key and a self-signed certificate on
first start (`security::identity`). The SHA-256 of the certificate is the
device's identity ("fingerprint", shown abbreviated as `3F2A-91C0`).

* Windows: the key is encrypted with DPAPI (current user, application
  entropy) in `%LOCALAPPDATA%\SecondScrnData\identity.bin`.
* Android: encrypted with an AES-256-GCM key in the Android Keystore
  (non-exportable) in the app's private storage.

## Control channel

TLS 1.3 only (rustls, ring), mutual authentication, ALPN `secondscrn/1`.
There is no PKI; certificate verifiers check the handshake signature (proof of
possession) and leave authorisation to the application:

* The host accepts any client certificate in the handshake but only serves
  fingerprints in its trust store. Unknown clients may only start pairing.
* A paired tablet pins the host fingerprint in the handshake; a different
  certificate aborts the connection with a clear "identity changed" error.

## Pairing

Explicit first-time authorisation with a 6-digit code, resistant to
man-in-the-middle attacks on the first connection:

1. The tablet connects (TLS, host not yet pinned) and announces `Pair`.
2. The host shows a pairing request (device name, model, address, tablet
   fingerprint) and asks the PC user for the code.
3. The tablet shows a random code and sends a SPAKE2 message (side B) whose
   identities are the two certificate fingerprints.
4. When the user types the code, the host runs SPAKE2 (side A) and sends its
   message plus `HMAC(K, "host confirm" ‖ TLS-exporter)`.
5. The tablet verifies, replies with `HMAC(K, "client confirm" ‖ exporter)`;
   the host verifies and only then stores the tablet as trusted. The tablet
   pins the host fingerprint.

The exporter value differs on each leg of a TLS man-in-the-middle, so a
relayed pairing fails; a wrong code fails; each guess costs a user-visible
pairing attempt. Pairing requests are limited (one at a time, 5 per 5 min)
and time out after 2 minutes. Typing the code is the host user's consent.

## Media

WebRTC DTLS-SRTP (str0m, RustCrypto backend). The SDP containing the DTLS
fingerprints is exchanged over the mutually authenticated TLS channel, so
media encryption keys are bound to the authenticated devices. WebRTC's own
encryption is not relied on for device authentication.

## Input

Input events arrive only on the authenticated control channel of an active
session, are sequence-numbered (stale/replayed events dropped) and injected
on a dedicated thread with `SendInput`. Windows UIPI prevents injection into
elevated windows.

## Revocation and changed identities

Removing a tablet on the PC deletes it from the trust store and ends its
session; it must pair again. If a host identity changes (reinstall, or an
impersonation attempt) the tablet refuses to connect and offers to forget
the PC and pair again.

## Exposure and privileges

* Listens on one TCP port (47800, configurable) and ephemeral UDP ports for
  active sessions only. mDNS announcement can be disabled.
* The app runs without elevation. Only the optional driver setup requests
  administrator rights, when the user starts it. The driver's own control pipe
  is local-only and never reachable from the network.
* Diagnostic exports redact fingerprints, base64 blobs and codes.

## Not covered (yet)

* Brute-force resistance of a 6-digit code relies on the per-attempt user
  interaction and rate limits, not on code length.
* No protection against a malicious local user on the PC (they can already
  control the desktop).
