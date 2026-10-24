//! Pinned mutual-TLS configuration for the control channel.
//!
//! There is no certificate authority. Both peers present self-signed
//! certificates; the handshake proves possession of the private key, and the
//! application then authorises the peer by its certificate fingerprint:
//!
//! * The host accepts any client certificate during the handshake and checks
//!   the fingerprint against the trust store before doing anything else
//!   (unpaired clients may only start PIN pairing).
//! * The client pins the host fingerprint at handshake time once paired, so a
//!   changed host identity fails the handshake.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::{Fingerprint, Identity, SecurityError};

/// Dummy SNI. Certificates are pinned, so names are irrelevant.
pub const SERVER_NAME: &str = "secondscrn.invalid";
/// TLS exporter label used to bind pairing to the TLS session.
pub const PAIRING_EXPORTER_LABEL: &[u8] = b"EXPORTER-secondscrn-pairing-v1";

pub fn crypto_provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn key_of(identity: &Identity) -> PrivateKeyDer<'static> {
    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der().to_vec()))
}

fn cert_of(identity: &Identity) -> Vec<CertificateDer<'static>> {
    vec![CertificateDer::from(identity.cert_der().to_vec())]
}

/// Server (host) configuration: requires a client certificate, accepts any.
pub fn server_config(identity: &Identity, alpn: &[u8]) -> Result<rustls::ServerConfig, SecurityError> {
    let provider = crypto_provider();
    let verifier = Arc::new(AnyClientCert { algs: provider.signature_verification_algorithms });
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert_of(identity), key_of(identity))?;
    cfg.alpn_protocols = vec![alpn.to_vec()];
    Ok(cfg)
}

/// Client (tablet) configuration. With `pinned_host = Some(fp)` the handshake
/// fails unless the host presents exactly that certificate.
pub fn client_config(
    identity: &Identity,
    pinned_host: Option<Fingerprint>,
    alpn: &[u8],
) -> Result<rustls::ClientConfig, SecurityError> {
    let provider = crypto_provider();
    let verifier = Arc::new(PinnedServerCert { pinned: pinned_host, algs: provider.signature_verification_algorithms });
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(cert_of(identity), key_of(identity))?;
    cfg.alpn_protocols = vec![alpn.to_vec()];
    Ok(cfg)
}

pub fn server_name() -> ServerName<'static> {
    ServerName::try_from(SERVER_NAME).expect("valid dns name")
}

/// Fingerprint of the peer's end-entity certificate on an established
/// connection (either side).
pub fn peer_fingerprint(certs: Option<&[CertificateDer<'_>]>) -> Option<Fingerprint> {
    certs.and_then(|c| c.first()).map(|c| Fingerprint::of_cert(c.as_ref()))
}

#[derive(Debug)]
struct AnyClientCert {
    algs: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for AnyClientCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        // Authorisation happens at the application layer by fingerprint.
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // Proof of possession of the certificate's private key.
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }
}

#[derive(Debug)]
struct PinnedServerCert {
    pinned: Option<Fingerprint>,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedServerCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.pinned {
            None => Ok(ServerCertVerified::assertion()),
            Some(expected) if Fingerprint::of_cert(end_entity.as_ref()) == expected => {
                Ok(ServerCertVerified::assertion())
            }
            Some(_) => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const ALPN: &[u8] = b"test/1";

    async fn handshake(
        host: &Identity,
        client: &Identity,
        pin: Option<Fingerprint>,
    ) -> Result<(Fingerprint, Fingerprint, [u8; 32], [u8; 32]), String> {
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config(host, ALPN).unwrap()));
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config(client, pin, ALPN).unwrap()));
        let (a, b) = tokio::io::duplex(64 * 1024);

        let server = tokio::spawn(async move {
            let mut s = acceptor.accept(a).await.map_err(|e| e.to_string())?;
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
            let (_, conn) = s.get_ref();
            let fp = peer_fingerprint(conn.peer_certificates()).unwrap();
            let mut ekm = [0u8; 32];
            conn.export_keying_material(&mut ekm, PAIRING_EXPORTER_LABEL, None).unwrap();
            Ok::<_, String>((fp, ekm))
        });

        let mut c = connector.connect(server_name(), b).await.map_err(|e| e.to_string())?;
        c.write_all(b"ping").await.map_err(|e| e.to_string())?;
        let (_, conn) = c.get_ref();
        let host_fp = peer_fingerprint(conn.peer_certificates()).unwrap();
        let mut ekm = [0u8; 32];
        conn.export_keying_material(&mut ekm, PAIRING_EXPORTER_LABEL, None).unwrap();
        let (client_fp, server_ekm) = server.await.unwrap()?;
        Ok((host_fp, client_fp, ekm, server_ekm))
    }

    #[tokio::test]
    async fn mutual_tls_exposes_both_fingerprints_and_matching_exporters() {
        let host = Identity::generate("host").unwrap();
        let client = Identity::generate("client").unwrap();
        let (host_fp, client_fp, ekm_c, ekm_s) = handshake(&host, &client, None).await.unwrap();
        assert_eq!(host_fp, host.fingerprint());
        assert_eq!(client_fp, client.fingerprint());
        assert_eq!(ekm_c, ekm_s);
    }

    #[tokio::test]
    async fn pinned_host_must_match() {
        let host = Identity::generate("host").unwrap();
        let client = Identity::generate("client").unwrap();
        assert!(handshake(&host, &client, Some(host.fingerprint())).await.is_ok());

        let impostor = Identity::generate("impostor").unwrap();
        assert!(handshake(&impostor, &client, Some(host.fingerprint())).await.is_err());
    }
}
