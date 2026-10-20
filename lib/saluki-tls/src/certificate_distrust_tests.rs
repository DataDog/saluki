//! Regression tests with captured public certificate chains and unchanged trusted roots.

use std::{sync::Arc, time::Duration};

use rustls::{
    client::{danger::ServerCertVerifier, WebPkiServerVerifier},
    pki_types::{pem::PemObject as _, CertificateDer, ServerName, UnixTime},
    CertificateError, Error, RootCertStore,
};

use super::default_crypto_provider;

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

fn verify_chain(host: &str, pem: &[u8]) -> Result<(), Error> {
    let mut roots = RootCertStore::empty();
    for root in CertificateDer::pem_slice_iter(ROOTS) {
        roots
            .add(root.expect("valid root PEM"))
            .expect("valid root certificate");
    }
    let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::new(default_crypto_provider()))
        .build()
        .expect("valid verifier");
    let chain = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .expect("valid certificate chain");
    let (leaf, intermediates) = chain.split_first().expect("nonempty chain");
    let name = ServerName::try_from(host).expect("valid DNS name");
    verifier
        .verify_server_cert(leaf, intermediates, &name, &[], VALIDATION_TIME)
        .map(|_| ())
}

#[test]
fn rejects_compromised_intermediates_with_roots_still_trusted() {
    for &(host, chain) in AFFECTED_CHAINS {
        let error = verify_chain(host, chain).expect_err(host);
        let Error::InvalidCertificate(CertificateError::Other(cause)) = error else {
            panic!("{host}: expected explicit distrust, got {error:?}");
        };
        // Rustls wraps WebPKI's new error in Other; require distrust, not an unrelated TLS failure.
        assert_eq!(cause.0.to_string(), "CertDistrusted", "{host}");
    }
}

#[test]
fn accepts_unaffected_chain_with_the_same_roots() {
    verify_chain(
        "testovg1.emsign.com",
        include_bytes!("../tests/data/distrusted/unaffected.pem"),
    )
    .expect("unaffected certificate chain should remain trusted");
}
