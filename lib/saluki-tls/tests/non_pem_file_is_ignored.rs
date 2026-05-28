//! Integration test: a non-PEM text file in the cert directory is silently skipped (no certs, no errors).

mod common;

use std::fs;

use tempfile::TempDir;

#[test]
fn non_pem_file_is_ignored() {
    let temp_dir = TempDir::new().expect("should create temp dir");
    fs::write(temp_dir.path().join("notes.txt"), "this is not a certificate").expect("should write file");
    common::use_cert_dir(temp_dir.path());

    let store =
        saluki_tls::load_platform_root_certificates_inner().expect("expected non-PEM file to be silently ignored");

    assert!(store.is_empty());
}
