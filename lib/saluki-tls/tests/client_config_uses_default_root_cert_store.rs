//! Integration test: once the process-wide default root certificate store has been initialized, a client TLS
//! configuration built without an explicit store falls back to that default instead of failing.

mod common;

use saluki_tls::{initialize_default_crypto_provider, load_platform_root_certificates, ClientTLSConfigBuilder};
use tempfile::TempDir;

#[test]
fn client_config_uses_default_root_cert_store() {
    let temp_dir = TempDir::new().expect("should create temp dir");
    let cert_path = temp_dir.path().join("ca.pem");
    common::write_self_signed_cert(&cert_path);
    common::use_cert_file(&cert_path);

    // The client builder's default-store fallback needs the process-wide default installed.
    let _ = initialize_default_crypto_provider();

    // Populate the process-wide default root certificate store from the environment we just configured.
    load_platform_root_certificates().expect("platform root certificates should load into the default store");

    // Building a client configuration without an explicit root cert store now succeeds because it falls back to the
    // populated default. (Without the default being initialized, this same build fails; see the unit test
    // `danger_accept_invalid_certs_builds_without_root_cert_store`.)
    ClientTLSConfigBuilder::new()
        .build()
        .expect("client TLS config should build using the default root cert store");
}
