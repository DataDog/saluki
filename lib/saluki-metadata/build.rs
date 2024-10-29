fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=APP_NAME");
    println!("cargo:rerun-if-env-changed=APP_SHORT_NAME");
    println!("cargo:rerun-if-env-changed=APP_GIT_HASH");
    println!("cargo:rerun-if-env-changed=APP_VERSION");
    println!("cargo:rerun-if-env-changed=APP_BUILD_TIME");
    println!("cargo:rerun-if-env-changed=TARGET");

    // This is really, really simple: we look for some specific environment variables, split one of them apart into
    // numbers if we find it, and then write the values to a file that will get imported by lib.rs. Ta-da.
    let app_name = std::env::var("APP_NAME").unwrap_or("unknown".to_string());
    let app_short_name = std::env::var("APP_SHORT_NAME").unwrap_or("unknown".to_string());
    let app_git_hash = std::env::var("APP_GIT_HASH").unwrap_or("unknown".to_string());
    let app_version = std::env::var("APP_VERSION").unwrap_or("0.0.0".to_string());
    let app_build_time = std::env::var("APP_BUILD_TIME").unwrap_or("0000-00-00 00:00:00".to_string());
    let target_arch = std::env::var("TARGET").unwrap_or("unknown-arch".to_string());

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
    pub const DETECTED_APP_NAME: &str = "{}";
	pub const DETECTED_APP_SHORT_NAME: &str = "{}";
  pub const DETECTED_GIT_HASH: &str = "{}";
	pub const DETECTED_APP_VERSION: &str = "{}";
	pub const DETECTED_APP_VERSION_MAJOR: u32 = {};
	pub const DETECTED_APP_VERSION_MINOR: u32 = {};
	pub const DETECTED_APP_VERSION_PATCH: u32 = {};
  pub const DETECTED_APP_BUILD_TIME: &str = "{}";
  pub const DETECTED_TARGET_ARCH: &str = "{}";
			"#,
            app_name, app_short_name, app_git_hash, app_version, major, minor, patch, app_build_time, target_arch,
        ),
    )
    .expect("failed to write details file");
}
