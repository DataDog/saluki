use std::process::Command;

fn main() {
    // Always rerun if the build script itself changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=VERSION");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");
    println!("cargo:rerun-if-env-changed=TARGET");

    // Try and grab the version from `VERSION`, which will be set if we're doing a build in CI.
    //
    // Otherwise, default to the package version as defined in Cargo.toml and the short Git hash, if available.
    let pkg_version_cargo = env!("CARGO_PKG_VERSION").to_string();
    let git_output = Command::new("git").args(["rev-parse", "--short", "HEAD"]).output().ok();
    let pkg_version = match git_output {
        Some(output) => {
            let git_hash = String::from_utf8_lossy(&output.stdout);
            format!("{}-{}", pkg_version_cargo, git_hash.trim())
        }
        None => {
            println!("cargo:rustc-env=GIT_HASH=unknown");
            return;
        }
    };

    let build_version = std::env::var("VERSION").unwrap_or_else(|_| pkg_version.clone());
    let current_date = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    println!("cargo:rustc-env=ADP_VERSION={build_version}");
    println!(
        "cargo:rustc-env=ADP_BUILD_DESC={} {current_date}",
        std::env::var("TARGET").unwrap()
    );
}
