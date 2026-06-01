//! Shared helpers for `saluki-tls` integration tests.
//!
//! Not every test binary uses every helper, so `dead_code` is suppressed for the module as a whole.

#![allow(dead_code)]

use std::{fs, path::Path};

use rcgen::{generate_simple_self_signed, CertifiedKey};

/// A PEM block whose body is not valid base64. `rustls-native-certs` surfaces this as a PEM parsing error.
const MALFORMED_PEM: &str = "-----BEGIN CERTIFICATE-----\nNOT VALID BASE64!!!\n-----END CERTIFICATE-----\n";

/// Generates a self-signed certificate using `rcgen` and writes it as a PEM file at `path`.
pub fn write_self_signed_cert(path: &Path) {
    let CertifiedKey { cert, .. } =
        generate_simple_self_signed(["example.com".to_owned()]).expect("rcgen should generate a self-signed cert");
    fs::write(path, cert.pem()).expect("should write generated certificate");
}

/// Writes a syntactically PEM-shaped block with an invalid base64 body to `path`.
pub fn write_malformed_pem(path: &Path) {
    fs::write(path, MALFORMED_PEM).expect("should write malformed PEM file");
}

/// Configures the env vars consulted by `rustls-native-certs` so that certificates are loaded from `dir` only.
///
/// Clears `SSL_CERT_FILE` so the host environment doesn't leak into the test.
pub fn use_cert_dir(dir: &Path) {
    std::env::set_var("SSL_CERT_DIR", dir);
    std::env::remove_var("SSL_CERT_FILE");
}

/// Configures the env vars consulted by `rustls-native-certs` so that certificates are loaded from `file` only.
///
/// Clears `SSL_CERT_DIR` so the host environment doesn't leak into the test.
pub fn use_cert_file(file: &Path) {
    std::env::set_var("SSL_CERT_FILE", file);
    std::env::remove_var("SSL_CERT_DIR");
}
