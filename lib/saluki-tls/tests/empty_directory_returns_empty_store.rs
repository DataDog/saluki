//! Integration test: a directory with no files yields an empty store without returning an error.

mod common;

use tempfile::TempDir;

#[test]
fn empty_directory_returns_empty_store() {
    let temp_dir = TempDir::new().expect("should create temp dir");
    common::use_cert_dir(temp_dir.path());

    let store = saluki_tls::load_platform_root_certificates_inner().expect("expected empty directory to succeed");

    assert!(store.is_empty());
}
