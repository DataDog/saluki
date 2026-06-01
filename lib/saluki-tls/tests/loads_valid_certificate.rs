//! Integration test: a directory containing a valid PEM certificate produces a populated store.

mod common;

use tempfile::TempDir;

#[test]
fn loads_valid_certificate() {
    let temp_dir = TempDir::new().expect("should create temp dir");
    common::write_self_signed_cert(&temp_dir.path().join("ca.pem"));
    common::use_cert_dir(temp_dir.path());

    let store = saluki_tls::load_platform_root_certificates_inner().expect("expected valid certificate to load");

    assert_eq!(store.len(), 1);
}
