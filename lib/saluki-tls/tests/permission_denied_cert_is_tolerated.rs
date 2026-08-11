//! Integration test: an unreadable certificate file in the cert directory is tolerated. The loader skips it (logging at
//! debug level) and returns an empty store rather than failing bootstrap, mirroring Go's TLS stack (and thus the
//! Datadog Agent), which skips cert files it can't read.
#![cfg(unix)]

mod common;

use std::{fs, os::unix::fs::PermissionsExt};

use tempfile::TempDir;

#[test]
fn permission_denied_cert_is_tolerated() {
    let temp_dir = TempDir::new().expect("should create temp dir");
    let cert_path = temp_dir.path().join("unreadable.pem");
    common::write_self_signed_cert(&cert_path);
    fs::set_permissions(&cert_path, fs::Permissions::from_mode(0o000)).expect("should chmod cert file");

    // Running as root bypasses filesystem permission checks, so the file would still be readable and the scenario under
    // test can't be reproduced. Skip in that case rather than assert something untrue.
    if fs::read(&cert_path).is_ok() {
        eprintln!("skipping: process can read a mode-0000 file (likely running as root)");
        return;
    }

    common::use_cert_dir(temp_dir.path());

    let store =
        saluki_tls::load_platform_root_certificates_inner().expect("expected unreadable certificate to be tolerated");

    assert!(store.is_empty());
}
