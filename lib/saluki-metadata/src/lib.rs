#[allow(dead_code)]
mod details {
    include!(concat!(env!("OUT_DIR"), "/details.rs"));
}

static VERSION_DETAILS: AppDetails = AppDetails {
    full_name: details::DETECTED_APP_FULL_NAME,
    short_name: details::DETECTED_APP_SHORT_NAME,
    identifier: details::DETECTED_APP_IDENTIFIER,
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
/// - `APP_FULL_NAME`: Application's full name. If this is not set, the default value is "unknown".
/// - `APP_SHORT_NAME`: Application's short name. If this is not set, the default value is "unknown".
/// - `APP_IDENTIFIER`: Application's identifier. If this is not set, the default value is "unknown".
/// - `APP_VERSION`: Version of the application. If this is not set, the default value is "0.0.0".
/// - `APP_GIT_HASH`: Git hash of the application. If this is not set, the default value is "unknown".
/// - `APP_BUILD_TIME`: Build time of the application. If this is not set, the default value is "0000-00-00 00:00:00".
/// - `TARGET`: Target architecture of the application. If this is not set, the default value is "unknown-arch".
///
/// Environment variables prefixed with `APP_` are expected to be set by the build script/tooling, while others are
/// provided automatically by Cargo.
///
/// The version string will be treated as a semantic version, and will be split on periods to extract the major, minor,
/// and patch numbers. Additionally, the patch number will be split on hyphens to remove any pre-release or build
/// metadata.
pub struct AppDetails {
    full_name: &'static str,
    short_name: &'static str,
    identifier: &'static str,
    git_hash: &'static str,
    version: Version,
    build_time: &'static str,
    target_arch: &'static str,
}

impl AppDetails {
    /// Returns the application's full name.
    ///
    /// This is typically a human-friendly/"pretty" name of the binary/executable, such as "Agent Data Plane".
    ///
    /// If the full name could not be detected, this will return "unknown".
    pub fn full_name(&self) -> &'static str {
        self.full_name
    }

    /// Returns the application's short name.
    ///
    /// This is typically a shorter version of the name of the binary/executable, such as "Data Plane" or
    /// "DATAPLANE".
    ///
    /// If the short name could not be detected, this will return "unknown".
    pub fn short_name(&self) -> &'static str {
        self.short_name
    }

    /// Returns the application's identifier.
    ///
    /// This is typically a very condensed form of the name of the binary/executable, like an acronym, such as "adp" or
    /// "ADP".
    ///
    /// If the identifier could not be detected, this will return "unknown".
    pub fn identifier(&self) -> &'static str {
        self.identifier
    }

    /// Returns the Git hash used to build the application.
    ///
    /// If the Git hash could not be detected, this will return "unknown".
    pub fn git_hash(&self) -> &'static str {
        self.git_hash
    }

    /// Returns the application's version.
    ///
    /// If the version could not be detected, this will return a version equivalent to `0.0.0`.
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Returns the build time of the application.
    ///
    /// If the build time could not be detected, this will return "0000-00-00 00:00:00".
    pub fn build_time(&self) -> &'static str {
        self.build_time
    }

    /// Returns the target architecture of the application.
    ///
    /// This returns a "target triple", which is a string that generally has _four_ components: the processor
    /// architecture (x86-64, ARM64, etc), the "vendor" ("apple", "pc", etc), the operating system ("linux", "windows",
    /// "darwin", etc) and environment/ABI ("gnu", "musl", etc).
    ///
    /// The environment/ABI component can sometimes be omitted in scenarios where there are no meaningful distinctions
    /// for the given operating system.
    ///
    /// If the target architecture could not be detected, this will return "unknown-arch".
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
