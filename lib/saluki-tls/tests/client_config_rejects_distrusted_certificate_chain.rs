//! Integration test: a client TLS configuration rejects a server certificate chain that contains a distrusted
//! intermediate, even when the root of that chain is trusted.
//!
//! `rustls-webpki` distrusts the public keys of eight compromised intermediate certificate authorities
//! (RUSTSEC-2026-0382). This test catches a downgrade of `rustls-webpki` or a client verifier that skips this check.
//!
//! `data/distrusted/dv-g3b-emudhra.pem` holds the public chain that `dv-g3b-emudhra.we-have-a-problem.com` served on
//! October 20, 2026. The chain has the leaf, the distrusted `EM DV TLS CA - G3B` intermediate, and the
//! `emSign Root TLS CA - G3` intermediate. `data/distrusted/roots.pem` holds `emSign ECC Root CA - G3`, which issues
//! the chain.

use std::{sync::Arc, time::Duration};

use rustls::{
    crypto::CryptoProvider,
    pki_types::{pem::PemObject as _, CertificateDer, ServerName, UnixTime},
    sign::{CertifiedKey, SingleCertAndKey},
    time_provider::TimeProvider,
    version::{TLS12, TLS13},
    CertificateError, ClientConfig, ClientConnection, Error, RootCertStore, ServerConfig, ServerConnection,
    SupportedProtocolVersion,
};
use saluki_tls::{initialize_default_crypto_provider, test_util::SelfSignedCert, ClientTLSConfigBuilder};

const SERVER_NAME: &str = "dv-g3b-emudhra.we-have-a-problem.com";
const AFFECTED_CHAIN: &[u8] = include_bytes!("data/distrusted/dv-g3b-emudhra.pem");
const ROOTS: &[u8] = include_bytes!("data/distrusted/roots.pem");

/// Fixes the validation time at the capture time, so that the expiry of the leaf can't change the result.
#[derive(Debug)]
struct CaptureTime;

impl TimeProvider for CaptureTime {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(Duration::from_secs(1_792_507_200)))
    }
}

fn certificates(pem: &[u8]) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .expect("valid certificate PEM")
}

/// Builds a server configuration that presents the affected chain.
///
/// We don't have the private key of the leaf, so the server signs with an unrelated key. The client verifies the
/// certificate chain before it verifies the handshake signature, so the chain decides the result.
fn server_config(provider: &CryptoProvider, version: &'static SupportedProtocolVersion) -> ServerConfig {
    let signing_key = provider
        .key_provider
        .load_private_key(SelfSignedCert::localhost().private_key())
        .expect("valid signing key");
    let certified_key = CertifiedKey::new(certificates(AFFECTED_CHAIN), signing_key);
    ServerConfig::builder_with_protocol_versions(&[version])
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(SingleCertAndKey::from(certified_key)))
}

fn handshake(client_config: ClientConfig, server_config: ServerConfig) -> Result<(), Error> {
    let mut client = ClientConnection::new(
        Arc::new(client_config),
        ServerName::try_from(SERVER_NAME).expect("valid DNS name"),
    )
    .expect("valid client connection");
    let mut server = ServerConnection::new(Arc::new(server_config)).expect("valid server connection");
    for _ in 0..10 {
        let mut to_server = Vec::new();
        client.write_tls(&mut to_server).expect("client writes TLS");
        server.read_tls(&mut to_server.as_slice()).expect("server reads TLS");
        server.process_new_packets()?;
        let mut to_client = Vec::new();
        server.write_tls(&mut to_client).expect("server writes TLS");
        client.read_tls(&mut to_client.as_slice()).expect("client reads TLS");
        client.process_new_packets()?;
        if !client.is_handshaking() && !server.is_handshaking() {
            return Ok(());
        }
    }
    panic!("handshake did not finish");
}

#[test]
fn client_config_rejects_distrusted_certificate_chain() {
    initialize_default_crypto_provider().expect("crypto provider installs");
    let provider = CryptoProvider::get_default().expect("crypto provider is installed");

    let mut roots = RootCertStore::empty();
    let (added, ignored) = roots.add_parsable_certificates(certificates(ROOTS));
    assert_eq!((added, ignored), (1, 0));

    for version in [&TLS12, &TLS13] {
        // The client accepts TLS 1.2 and TLS 1.3, so the server decides the version.
        let mut client_config = ClientTLSConfigBuilder::new()
            .with_root_cert_store(roots.clone())
            .build()
            .expect("client TLS config builds");
        client_config.time_provider = Arc::new(CaptureTime);

        let error = handshake(client_config, server_config(provider, version))
            .expect_err("distrusted chain must fail the handshake");
        match error {
            Error::InvalidCertificate(CertificateError::Other(other)) => {
                assert_eq!(other.to_string(), "CertDistrusted", "{version:?}");
            }
            other => panic!("expected a distrusted certificate error for {version:?}, got {other:?}"),
        }
    }
}
