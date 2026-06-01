//! Integration test: pointing `SSL_CERT_FILE` at a path that does not exist is tolerated: the loader returns an empty
//! store rather than surfacing the underlying NotFound IO error.

mod common;

use tempfile::TempDir;

#[test]
fn missing_file_returns_empty_store() {
    let temp_dir = TempDir::new().expect("should create temp dir");
    let missing = temp_dir.path().join("does-not-exist.pem");
    common::use_cert_file(&missing);

    let store = saluki_tls::load_platform_root_certificates_inner().expect("expected missing file to be tolerated");

    assert!(store.is_empty());
}
