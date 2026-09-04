fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=APP_GIT_HASH");
    println!("cargo:rerun-if-env-changed=APP_BUILD_TIME");
    println!("cargo:rerun-if-env-changed=APP_DEV_BUILD");
    println!("cargo:rerun-if-env-changed=TARGET");

    // This is really, really simple: we look for some specific environment variables and write the values to a file
    // that will get imported by lib.rs. Ta-da.
    //
    // Only build metadata lives here. Which application is being built isn't knowable from this crate -- one compiled
    // copy is shared by every binary in the workspace -- so names and versions are declared by the binaries themselves
    // and registered at startup. See `declare_app_details!`.
    let app_git_hash = get_env_var_or_default("APP_GIT_HASH", "unknown");
    let app_build_time = get_env_var_or_default("APP_BUILD_TIME", "0000-00-00 00:00:00");
    let app_dev_build = get_env_var_bool_or_default("APP_DEV_BUILD", true);
    let target_arch = get_env_var_or_default("TARGET", "unknown-arch");

    // Release builds shouldn't silently ship the placeholder values above, so treat any that survive as a build
    // failure. CI sets APP_DEV_BUILD at the workflow level, so on a tag pipeline it's also set for jobs that only test
    // or lint; those run through the Makefile, which exports this metadata for every recipe, so they satisfy the check
    // the same way a real build does.
    if !app_dev_build {
        let mut placeholders = Vec::new();

        if app_git_hash == "unknown" || app_git_hash == "not-in-git" {
            placeholders.push("APP_GIT_HASH");
        }
        if app_build_time.starts_with("0000-00-00") {
            placeholders.push("APP_BUILD_TIME");
        }

        if !placeholders.is_empty() {
            panic!(
                "APP_DEV_BUILD is 'false', marking this a release build, but the following build metadata environment \
                 variables are unset or still hold their placeholder defaults: {}. Set them in whichever build entry \
                 point is being used, or set APP_DEV_BUILD=true if this isn't actually a release build.",
                placeholders.join(", ")
            );
        }
    }

    let details_file = std::env::var("OUT_DIR").unwrap() + "/details.rs";
    std::fs::write(
        details_file,
        format!(
            r#"
    pub const DETECTED_GIT_HASH: &str = "{}";
    pub const DETECTED_APP_BUILD_TIME: &str = "{}";
    pub const DETECTED_APP_DEV_BUILD: bool = {};
    pub const DETECTED_TARGET_ARCH: &str = "{}";
            "#,
            app_git_hash, app_build_time, app_dev_build, target_arch,
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
/// variable is missing/empty, or if it's not a valid boolean.
fn get_env_var_bool_or_default(var_name: &str, default: bool) -> bool {
    let value = get_env_var_or_default(var_name, "").to_ascii_lowercase();
    match value.as_str() {
        "true" => true,
        "false" => false,
        _ => default,
    }
}
