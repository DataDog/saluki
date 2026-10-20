//! Regression tests with captured public certificate chains and unchanged trusted roots.

use std::{slice, sync::Arc, time::Duration};

use rustls::{
    client::{danger::ServerCertVerifier, WebPkiServerVerifier},
    pki_types::{pem::PemObject as _, CertificateDer, ServerName, UnixTime},
    version::{TLS12, TLS13},
    CertificateError, ClientConfig, ClientConnection, Error, RootCertStore, ServerConfig, ServerConnection,
    SupportedProtocolVersion,
};

use super::{
    certificate_distrust::DistrustServerCertVerifier, default_crypto_provider, initialize_default_crypto_provider,
    test_util::SelfSignedCert, ClientTLSConfigBuilder,
};

// Freeze validation at capture time so certificate expiry cannot change the regression.
const VALIDATION_TIME: UnixTime = UnixTime::since_unix_epoch(Duration::from_secs(1_792_507_200));
const ROOTS: &[u8] = include_bytes!("../tests/data/distrusted/roots.pem");
const AFFECTED_CHAINS: &[(&str, &[u8])] = &[
    (
        "dv-g3b-emudhra.we-have-a-problem.com",
        include_bytes!("../tests/data/distrusted/dv-g3b-emudhra.pem"),
    ),
    (
        "dv-g3c-emudhra.we-have-a-problem.com",
        include_bytes!("../tests/data/distrusted/dv-g3c-emudhra.pem"),
    ),
    (
        "ov-g3b-emudhra.we-have-a-problem.com",
        include_bytes!("../tests/data/distrusted/ov-g3b-emudhra.pem"),
    ),
    (
        "ov-g3c-emudhra.we-have-a-problem.com",
        include_bytes!("../tests/data/distrusted/ov-g3c-emudhra.pem"),
    ),
    (
        "ev-g3b-emudhra.we-have-a-problem.com",
        include_bytes!("../tests/data/distrusted/ev-g3b-emudhra.pem"),
    ),
    (
        "ev-g3c-emudhra.we-have-a-problem.com",
        include_bytes!("../tests/data/distrusted/ev-g3c-emudhra.pem"),
    ),
    (
        "dv-g3b-aaa-emudhra.we-have-a-problem.com",
        include_bytes!("../tests/data/distrusted/dv-g3b-aaa-emudhra.pem"),
    ),
    (
        "dv-g3c-aaa-emudhra.we-have-a-problem.com",
        include_bytes!("../tests/data/distrusted/dv-g3c-aaa-emudhra.pem"),
    ),
    (
        "ov-g3b-aaa-emudhra.we-have-a-problem.com",
        include_bytes!("../tests/data/distrusted/ov-g3b-aaa-emudhra.pem"),
    ),
    (
        "ov-g3c-aaa-emudhra.we-have-a-problem.com",
        include_bytes!("../tests/data/distrusted/ov-g3c-aaa-emudhra.pem"),
    ),
    (
        "ev-g3b-aaa-emudhra.we-have-a-problem.com",
        include_bytes!("../tests/data/distrusted/ev-g3b-aaa-emudhra.pem"),
    ),
    (
        "ev-g3c-aaa-emudhra.we-have-a-problem.com",
        include_bytes!("../tests/data/distrusted/ev-g3c-aaa-emudhra.pem"),
    ),
];

fn roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    for root in CertificateDer::pem_slice_iter(ROOTS) {
        roots
            .add(root.expect("valid root PEM"))
            .expect("valid root certificate");
    }
    roots
}

fn chain(pem: &[u8]) -> Vec<CertificateDer<'_>> {
    CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .expect("valid certificate chain")
}

fn unprotected(roots: RootCertStore) -> Arc<WebPkiServerVerifier> {
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::new(default_crypto_provider()))
        .build()
        .expect("valid verifier")
}

fn protected(roots: RootCertStore) -> DistrustServerCertVerifier {
    DistrustServerCertVerifier::new(Arc::new(roots), Arc::new(default_crypto_provider())).expect("valid verifier")
}

fn verify(verifier: &dyn ServerCertVerifier, host: &str, chain: &[CertificateDer<'_>]) -> Result<(), Error> {
    let (leaf, intermediates) = chain.split_first().expect("nonempty chain");
    verifier
        .verify_server_cert(
            leaf,
            intermediates,
            &ServerName::try_from(host).expect("valid DNS name"),
            &[],
            VALIDATION_TIME,
        )
        .map(|_| ())
}

fn assert_distrusted(result: Result<(), Error>) {
    assert_eq!(
        result,
        Err(Error::InvalidCertificate(
            CertificateError::ApplicationVerificationFailure
        ))
    );
}

#[test]
fn rejects_all_twelve_affected_chains_accepted_by_unmodified_webpki() {
    let baseline = unprotected(roots());
    let verifier = protected(roots());
    for &(host, pem) in AFFECTED_CHAINS {
        let chain = chain(pem);
        verify(baseline.as_ref(), host, &chain).expect(host);
        assert_distrusted(verify(&verifier, host, &chain));
    }
}

#[test]
fn accepts_unaffected_chain_with_the_same_roots() {
    let chain = chain(include_bytes!("../tests/data/distrusted/unaffected.pem"));
    verify(&protected(roots()), "testovg1.emsign.com", &chain).expect("unaffected chain is valid");
}

#[test]
fn rejects_blocked_key_when_presented_as_the_leaf() {
    let chain = chain(AFFECTED_CHAINS[0].1);
    assert_distrusted(verify(&protected(roots()), AFFECTED_CHAINS[0].0, &chain[1..]));
}

#[test]
fn rejects_unused_blocked_intermediate_even_when_a_valid_path_exists() {
    let mut control = chain(include_bytes!("../tests/data/distrusted/unaffected.pem"));
    let affected = chain(AFFECTED_CHAINS[0].1);
    control.push(affected[1].clone());
    verify(unprotected(roots()).as_ref(), "testovg1.emsign.com", &control).expect("blocked extra is not on the path");
    assert_distrusted(verify(&protected(roots()), "testovg1.emsign.com", &control));
}

#[test]
fn rejects_blocked_trust_anchor_omitted_by_the_server() {
    let affected = chain(AFFECTED_CHAINS[0].1);
    let mut roots = roots();
    roots
        .add(affected[1].clone())
        .expect("intermediate can be trusted explicitly");
    verify(
        unprotected(roots.clone()).as_ref(),
        AFFECTED_CHAINS[0].0,
        &affected[..1],
    )
    .expect("unmodified verifier trusts the explicitly configured intermediate");
    let error = verify(&protected(roots), AFFECTED_CHAINS[0].0, &affected[..1]).expect_err("blocked anchor removed");
    assert_eq!(error, Error::InvalidCertificate(CertificateError::UnknownIssuer));
}

#[test]
fn rejects_configuration_when_all_anchors_are_blocked() {
    let affected = chain(AFFECTED_CHAINS[0].1);
    let mut roots = RootCertStore::empty();
    roots.add(affected[1].clone()).expect("valid intermediate");
    let error = DistrustServerCertVerifier::new(Arc::new(roots), Arc::new(default_crypto_provider()))
        .expect_err("no usable trust anchors");
    assert!(error.to_string().contains("no root trust anchors"));
}

#[test]
fn malformed_certificates_never_fall_back_to_success() {
    let verifier = protected(roots());
    let malformed = CertificateDer::from(&b"not a DER certificate"[..]);
    assert!(verify(&verifier, "testovg1.emsign.com", slice::from_ref(&malformed)).is_err());
    let mut control = chain(include_bytes!("../tests/data/distrusted/unaffected.pem"));
    control.push(malformed);
    assert!(verify(&verifier, "testovg1.emsign.com", &control).is_err());
}

#[test]
fn preserves_hostname_expiry_and_certificate_signature_validation() {
    let verifier = protected(roots());
    let control = chain(include_bytes!("../tests/data/distrusted/unaffected.pem"));
    let name = ServerName::try_from("testovg1.emsign.com").unwrap();
    assert!(verify(&verifier, "wrong.example", &control).is_err());
    assert!(verifier
        .verify_server_cert(
            &control[0],
            &control[1..],
            &name,
            &[],
            UnixTime::since_unix_epoch(Duration::from_secs(4_000_000_000)),
        )
        .is_err());

    let mut damaged_leaf = control[0].to_vec();
    *damaged_leaf.last_mut().unwrap() ^= 1;
    let error = verifier
        .verify_server_cert(&damaged_leaf.into(), &control[1..], &name, &[], VALIDATION_TIME)
        .expect_err("certificate signature must still be checked");
    assert!(matches!(
        error,
        Error::InvalidCertificate(CertificateError::BadSignature)
    ));
    assert_eq!(
        verifier.supported_verify_schemes(),
        unprotected(roots()).supported_verify_schemes()
    );
}

fn handshake(
    client_config: ClientConfig, cert: &SelfSignedCert, blocked_extra: bool, version: &'static SupportedProtocolVersion,
) -> Result<(), Error> {
    let mut certificates = cert.cert_chain();
    if blocked_extra {
        certificates.push(chain(AFFECTED_CHAINS[0].1)[1].clone());
    }
    let server_config = ServerConfig::builder_with_protocol_versions(&[version])
        .with_no_client_auth()
        .with_single_cert(certificates, cert.private_key())
        .expect("valid server configuration");
    let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();
    let mut client =
        ClientConnection::new(Arc::new(client_config), ServerName::try_from("localhost").unwrap()).unwrap();
    for _ in 0..10 {
        let mut to_server = Vec::new();
        client.write_tls(&mut to_server).unwrap();
        server.read_tls(&mut to_server.as_slice()).unwrap();
        server.process_new_packets()?;
        let mut to_client = Vec::new();
        server.write_tls(&mut to_client).unwrap();
        client.read_tls(&mut to_client.as_slice()).unwrap();
        client.process_new_packets()?;
        if !client.is_handshaking() && !server.is_handshaking() {
            return Ok(());
        }
    }
    panic!("handshake did not finish");
}

#[test]
fn builder_enforces_distrust_in_tls12_and_tls13_handshakes() {
    initialize_default_crypto_provider().unwrap();
    let cert = SelfSignedCert::localhost();
    let mut roots = RootCertStore::empty();
    roots.add(cert.cert_chain().remove(0)).unwrap();
    for version in [&TLS12, &TLS13] {
        let config = ClientTLSConfigBuilder::new()
            .with_root_cert_store(roots.clone())
            .build()
            .unwrap();
        handshake(config.clone(), &cert, false, version).expect("normal handshake remains valid");
        assert_distrusted(handshake(config, &cert, true, version));
        let insecure = ClientTLSConfigBuilder::new()
            .danger_accept_invalid_certs()
            .build()
            .unwrap();
        handshake(insecure, &cert, true, version).expect("explicit local bypass remains separate");
    }
}
