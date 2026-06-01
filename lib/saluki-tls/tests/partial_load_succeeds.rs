//! Integration test: when at least one valid certificate is present alongside malformed ones, the loader succeeds and
//! returns a store containing the valid certificates.

mod common;

use tempfile::TempDir;

#[test]
fn partial_load_succeeds_with_valid_certificates() {
    let temp_dir = TempDir::new().expect("should create temp dir");
    common::write_self_signed_cert(&temp_dir.path().join("good.pem"));
    common::write_malformed_pem(&temp_dir.path().join("bad.pem"));
    common::use_cert_dir(temp_dir.path());

    let store = saluki_tls::load_platform_root_certificates_inner()
        .expect("expected valid certificate to load even alongside malformed sibling");

    assert_eq!(store.len(), 1);
}
