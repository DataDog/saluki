fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=APP_FULL_NAME");
    println!("cargo:rerun-if-env-changed=APP_SHORT_NAME");
    println!("cargo:rerun-if-env-changed=APP_IDENTIFIER");
    println!("cargo:rerun-if-env-changed=APP_GIT_HASH");
    println!("cargo:rerun-if-env-changed=APP_VERSION");
    println!("cargo:rerun-if-env-changed=APP_BUILD_TIME");
    println!("cargo:rerun-if-env-changed=APP_DEV_BUILD");
    println!("cargo:rerun-if-env-changed=TARGET");

    // This is really, really simple: we look for some specific environment variables, split one of them apart into
    // numbers if we find it, and then write the values to a file that will get imported by lib.rs. Ta-da.
    let app_full_name = get_env_var_or_default("APP_FULL_NAME", "unknown");
    let app_short_name = get_env_var_or_default("APP_SHORT_NAME", "unknown");
    let app_identifier = get_env_var_or_default("APP_IDENTIFIER", "unknown");
    let app_git_hash = get_env_var_or_default("APP_GIT_HASH", "unknown");
    let app_version = get_env_var_or_default("APP_VERSION", "0.0.0");
    let app_build_time = get_env_var_or_default("APP_BUILD_TIME", "0000-00-00 00:00:00");
    let app_dev_build = get_env_var_bool_or_default("APP_DEV_BUILD", true);
    let target_arch = get_env_var_or_default("TARGET", "unknown-arch");

    // Split the version string on periods to try and extract the major, minor, and patch numbers.
    //
    // For the patch component, we also split on hyphens to remove any pre-release or build metadata, and only return
    // the first portion, assuming it's a number.
    let version_parts: Vec<&str> = app_version.split('.').collect();
    let major = version_parts.first().unwrap_or(&"0").parse::<u32>().unwrap_or(0);
    let minor = version_parts.first().unwrap_or(&"0").parse::<u32>().unwrap_or(0);
    let patch = version_parts
        .first()
        .map(|s| s.split('-').next().unwrap_or(s))
        .unwrap_or("0")
        .parse::<u32>()
        .unwrap_or(0);

    let details_file = std::env::var("OUT_DIR").unwrap() + "/details.rs";
    std::fs::write(
        details_file,
        format!(
            r#"
    pub const DETECTED_APP_FULL_NAME: &str = "{}";
    pub const DETECTED_APP_SHORT_NAME: &str = "{}";
    pub const DETECTED_APP_IDENTIFIER: &str = "{}";
    pub const DETECTED_GIT_HASH: &str = "{}";
    pub const DETECTED_APP_VERSION: &str = "{}";
    pub const DETECTED_APP_VERSION_MAJOR: u32 = {};
    pub const DETECTED_APP_VERSION_MINOR: u32 = {};
    pub const DETECTED_APP_VERSION_PATCH: u32 = {};
    pub const DETECTED_APP_BUILD_TIME: &str = "{}";
    pub const DETECTED_APP_DEV_BUILD: bool = {};
    pub const DETECTED_TARGET_ARCH: &str = "{}";
            "#,
            app_full_name,
            app_short_name,
            app_identifier,
            app_git_hash,
            app_version,
            major,
            minor,
            patch,
            app_build_time,
            app_dev_build,
            target_arch,
        ),
    )
    .expect("failed to write details file");
}

/// Returns the value the given environment variable, or the default value if the environment variable is missing/empty.
fn get_env_var_or_default(var_name: &str, default: &str) -> String {
    std::env::var(var_name)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or(default.to_string())
}

/// Returns the value the given environment variable after parsing as a boolean, or the default value if the environment
/// variable is missing/empty, or if it is not a valid boolean.
fn get_env_var_bool_or_default(var_name: &str, default: bool) -> bool {
    let value = get_env_var_or_default(var_name, "").to_ascii_lowercase();
    match value.as_str() {
        "true" => true,
        "false" => false,
        _ => default,
    }
}
