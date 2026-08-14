//! Verifies that the binary actually reports its own identity.
//!
//! `saluki-metadata` reports an unknown application until `main` registers ADP's details, and that fallback is
//! deliberately silent so library unit tests can run without a `main` to register anything. That makes a missing
//! registration invisible to every other test in the workspace, so exercise the real binary instead.

use std::process::Command;

fn run_version_command(extra_args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-data-plane"))
        .arg("version")
        .args(extra_args)
        .output()
        .expect("agent-data-plane version command should launch");

    assert!(
        output.status.success(),
        "version command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout).expect("version output should be valid UTF-8")
}

#[test]
fn version_command_reports_the_applications_identity() {
    // Parsing as JSON also covers the version command continuing to run before logging is initialized: were it to run
    // afterwards, log output would land on stdout and this would no longer be valid JSON.
    let details: serde_json::Value =
        serde_json::from_str(&run_version_command(&["--json"])).expect("version output should be valid JSON");

    assert_eq!(details["full_name"], "Agent Data Plane");
    assert_eq!(details["short_name"], "data-plane");
    assert_eq!(details["identifier"], "adp");

    // Sourced from ADP's manifest, which is this test's manifest too.
    assert_eq!(details["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn version_command_reports_the_version_in_human_readable_form() {
    let stdout = run_version_command(&[]);
    let expected_prefix = format!("v{}-", env!("CARGO_PKG_VERSION"));

    assert!(
        stdout.starts_with(&expected_prefix),
        "expected version output to start with {expected_prefix:?}, got {stdout:?}"
    );
}
