//! Integration test: the process-wide default root certificate store may only be initialized once; a second attempt
//! to initialize it returns the documented "already initialized" error rather than silently overwriting it.

mod common;

use saluki_tls::load_platform_root_certificates;
use tempfile::TempDir;

#[test]
fn load_platform_root_certificates_rejects_reinitialization() {
    let temp_dir = TempDir::new().expect("should create temp dir");
    let cert_path = temp_dir.path().join("ca.pem");
    common::write_self_signed_cert(&cert_path);
    common::use_cert_file(&cert_path);

    // The first initialization populates the process-wide default store.
    load_platform_root_certificates().expect("first initialization should succeed");

    // A second initialization must be rejected because the default store has already been set.
    let error = load_platform_root_certificates().expect_err("second initialization should be rejected");
    assert!(
        error.to_string().contains("already initialized"),
        "unexpected error: {error}"
    );
}
