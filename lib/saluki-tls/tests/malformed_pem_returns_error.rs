//! Integration test: a file with a PEM-shaped block whose body is not valid base64 surfaces as a load error
//! when no other usable certificates are present.

mod common;

use tempfile::TempDir;

#[test]
fn malformed_pem_returns_error() {
    let temp_dir = TempDir::new().expect("should create temp dir");
    common::write_malformed_pem(&temp_dir.path().join("bad.pem"));
    common::use_cert_dir(temp_dir.path());

    let err = saluki_tls::load_platform_root_certificates_inner().expect_err("expected malformed PEM to error");

    let message = err.to_string();
    assert!(
        message.contains("Failed to load certificates"),
        "expected error to mention failure to load certificates, got: {}",
        message
    );
}
