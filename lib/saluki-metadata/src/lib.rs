#[allow(dead_code)]
mod details {
    include!(concat!(env!("OUT_DIR"), "/details.rs"));
}

static VERSION_DETAILS: AppDetails = AppDetails {
    name: details::DETECTED_APP_NAME,
    short_name: details::DETECTED_APP_SHORT_NAME,
    git_hash: details::DETECTED_GIT_HASH,
    version: Version::new(
        details::DETECTED_APP_VERSION,
        details::DETECTED_APP_VERSION_MAJOR,
        details::DETECTED_APP_VERSION_MINOR,
        details::DETECTED_APP_VERSION_PATCH,
    ),
    build_time: details::DETECTED_APP_BUILD_TIME,
    target_arch: details::DETECTED_TARGET_ARCH,
};

/// Gets the detected details for this application.
///
/// This includes basic information like the application name and semantic version, and information that might otherwise
/// fall under the general umbrella of "build metadata."
pub fn get_app_details() -> &'static AppDetails {
    &VERSION_DETAILS
}

/// Application details.
///
/// # Configuration
///
/// This struct is generated at build time and contains information about the detected application name and version
/// based on detected environment variables. The following environment variables are used:
///
/// - `APP_NAME`: The name of the application. If this is not set, the default value is "unknown".
/// - `APP_SHORT_NAME`: A short name for the application. If this is not set, the default value is "unknown".
/// - `APP_VERSION`: The version of the application. If this is not set, the default value is "0.0.0".
/// - `APP_GIT_HASH`: The Git hash of the application. If this is not set, the default value is "unknown".
/// - `APP_BUILD_TIME`: The build time of the application. If this is not set, the default value is "0000-00-00 00:00:00".
/// - `TARGET`: The target architecture of the application. If this is not set, the default value is "unknown-arch".
///
/// Environmment variables prefixed with `APP_` are expected to be set by the build script/tooling, while others are
/// provided automatically by Cargo.
///
/// The version string will be treated as a semantic version, and will be split on periods to extract the major, minor,
/// and patch numbers. Additionally, the patch number will be split on hyphens to remove any pre-release or build
/// metadata.
pub struct AppDetails {
    name: &'static str,
    short_name: &'static str,
    git_hash: &'static str,
    version: Version,
    build_time: &'static str,
    target_arch: &'static str,
}

impl AppDetails {
    /// Returns the detected application name.
    ///
    /// This is typically the name of the binary or executable. If the name cannot be detected, this will return "unknown".
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Returns the detected application short name.
    ///
    /// This is typically a shorter version of the name of the binary or executable, such as an acronym variant of the
    /// full name. If the short name cannot be detected, this will return "unknown".
    pub fn short_name(&self) -> &'static str {
        self.short_name
    }

    /// Returns the detected Git hash.
    ///
    /// If the Git hash cannot be detected, this will return "unknown".
    pub fn git_hash(&self) -> &'static str {
        self.git_hash
    }

    /// Returns the detected application version.
    ///
    /// If the version cannot be detected, this will return a version equivalent to `0.0.0`.
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Returns the detected build time.
    ///
    /// If the build time cannot be detected, this will return "0000-00-00 00:00:00".
    pub fn build_time(&self) -> &'static str {
        self.build_time
    }

    /// Returns the detected target architecture.
    ///
    /// This returns a "target triple", which is a string that generally has _four_ components: the processor
    /// architecture (x86-64, ARM64, etc), the "vendor" ("apple", "pc", etc), the operating system ("linux", "windows",
    /// "darwin", etc) and environment/ABI ("gnu", "musl", etc).
    ///
    /// The environment/ABI component can sometimes be omitted in scenarios where there are no meaningful distinctions
    /// for the given operating system.
    ///
    /// If the target architecture cannot be detected, this will return "unknown-arch".
    pub fn target_arch(&self) -> &'static str {
        self.target_arch
    }
}

/// A simple representation of a semantic version.
pub struct Version {
    raw: &'static str,
    major: u32,
    minor: u32,
    patch: u32,
}

impl Version {
    const fn new(raw: &'static str, major: u32, minor: u32, patch: u32) -> Self {
        Self {
            raw,
            major,
            minor,
            patch,
        }
    }

    /// Returns the raw version string.
    pub fn raw(&self) -> &'static str {
        self.raw
    }

    /// Returns the major version number.
    ///
    /// If the major version number is not present in the version string, this will return `0`.
    pub fn major(&self) -> u32 {
        self.major
    }

    /// Returns the minor version number.
    ///
    /// If the minor version number is not present in the version string, this will return `0`.
    pub fn minor(&self) -> u32 {
        self.minor
    }

    /// Returns the patch version number.
    ///
    /// If the patch version number is not present in the version string, this will return `0`.
    pub fn patch(&self) -> u32 {
        self.patch
    }
}
