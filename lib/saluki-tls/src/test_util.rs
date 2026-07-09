//! Test utilities for generating self-signed TLS certificates.
//!
//! These helpers are exposed behind the non-default `test-util` Cargo feature so that other crates can reuse them from
//! their own test suites (via a `dev-dependency` on `saluki-tls` with the `test-util` feature enabled) instead of
//! hand-rolling `rcgen`-based certificate generation. They are not compiled into production builds.

use std::{fs, path::Path};

use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// A self-signed certificate and its private key, generated with `rcgen`, for use in tests.
///
/// The generated material is suitable both for building an in-memory `rustls` server configuration (via
/// [`cert_chain`][Self::cert_chain]/[`private_key`][Self::private_key]) and for writing a PEM-encoded certificate to
/// disk (via [`write_cert_pem`][Self::write_cert_pem]).
pub struct SelfSignedCert {
    cert_der: CertificateDer<'static>,
    key_der: Vec<u8>,
    cert_pem: String,
}

impl SelfSignedCert {
    /// Generates a self-signed certificate valid for the given subject alternative names.
    ///
    /// # Panics
    ///
    /// Panics if certificate generation fails, which should not happen for the default generation parameters used here.
    pub fn new<S, I>(subject_alt_names: I) -> Self
    where
        S: Into<String>,
        I: IntoIterator<Item = S>,
    {
        let subject_alt_names = subject_alt_names.into_iter().map(Into::into).collect::<Vec<_>>();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(subject_alt_names).expect("rcgen should generate a self-signed certificate");

        Self {
            cert_der: cert.der().clone(),
            key_der: signing_key.serialize_der(),
            cert_pem: cert.pem(),
        }
    }

    /// Generates a self-signed certificate valid for `localhost`.
    pub fn localhost() -> Self {
        Self::new(["localhost"])
    }

    /// Returns the certificate chain (a single self-signed certificate) in the form expected by `rustls`.
    pub fn cert_chain(&self) -> Vec<CertificateDer<'static>> {
        vec![self.cert_der.clone()]
    }

    /// Returns the private key (PKCS#8) in the form expected by `rustls`.
    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()))
    }

    /// Returns the certificate encoded as PEM.
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// Writes the PEM-encoded certificate to `path`.
    ///
    /// # Panics
    ///
    /// Panics if the file cannot be written.
    pub fn write_cert_pem(&self, path: &Path) {
        fs::write(path, &self.cert_pem).expect("should write self-signed certificate to path");
    }
}

/// Generates a self-signed certificate valid for `localhost` and writes it (PEM-encoded) to `path`.
///
/// This is a convenience wrapper around [`SelfSignedCert::localhost`] and [`SelfSignedCert::write_cert_pem`] for the
/// common case of a test that only needs a valid certificate file on disk.
///
/// # Panics
///
/// Panics if certificate generation or writing the file fails.
pub fn write_self_signed_cert(path: &Path) {
    SelfSignedCert::localhost().write_cert_pem(path);
}
