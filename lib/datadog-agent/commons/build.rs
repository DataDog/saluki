fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DD_AGENT_VERSION");

    let raw = std::env::var("DD_AGENT_VERSION").unwrap_or_default();

    let is_dev = ["devel", "dev", "nightly", "master"]
        .iter()
        .any(|marker| raw.contains(marker));

    let version_part = raw.split(['-', '+']).next().unwrap_or("");
    let mut components = version_part.split('.');
    let major: u64 = components.next().and_then(|c| c.parse().ok()).unwrap_or(0);
    let minor: u64 = components.next().and_then(|c| c.parse().ok()).unwrap_or(0);
    let patch: u64 = components.next().and_then(|c| c.parse().ok()).unwrap_or(0);

    let details_file = std::env::var("OUT_DIR").unwrap() + "/details.rs";
    std::fs::write(
        details_file,
        format!(
            r#"
/// Raw `DD_AGENT_VERSION` string baked in at build time, or empty when unset.
pub const DETECTED_AGENT_VERSION: &str = "{}";
/// Major version component of the bundled Agent.
pub const DETECTED_AGENT_VERSION_MAJOR: u64 = {};
/// Minor version component of the bundled Agent.
pub const DETECTED_AGENT_VERSION_MINOR: u64 = {};
/// Patch version component of the bundled Agent.
pub const DETECTED_AGENT_VERSION_PATCH: u64 = {};
/// `true` when the bundled Agent is a development or pre-release build.
pub const DETECTED_AGENT_IS_DEV: bool = {};
            "#,
            raw, major, minor, patch, is_dev,
        ),
    )
    .expect("failed to write details file");
}
