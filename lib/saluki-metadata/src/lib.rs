use std::sync::OnceLock;

use serde::{Serialize, Serializer};

#[allow(dead_code)]
mod details {
    include!(concat!(env!("OUT_DIR"), "/details.rs"));
}

static APP_DETAILS: OnceLock<AppDetails> = OnceLock::new();

/// Details reported before an application has registered its own.
static UNREGISTERED_APP_DETAILS: AppDetails =
    AppDetails::new("unknown", "unknown", "unknown", Version::new("0.0.0", 0, 0, 0));

/// Gets the details for this application.
///
/// This includes basic information like the application name and semantic version, and information that might otherwise
/// fall under the general umbrella of "build metadata."
///
/// If the application hasn't registered its details through [`set_app_details`], the name and version fields report
/// `"unknown"` and `0.0.0` respectively. Build metadata is always populated, since it's captured at compile time.
pub fn get_app_details() -> &'static AppDetails {
    APP_DETAILS.get().unwrap_or(&UNREGISTERED_APP_DETAILS)
}

/// Registers the details for this application.
///
/// Binaries declare their details with [`declare_app_details!`] and register them here, which should be the first
/// thing `main` does: anything read through [`get_app_details`] beforehand reports an unknown application.
///
/// # Panics
///
/// Panics if the details have already been registered.
pub fn set_app_details(details: AppDetails) {
    assert!(
        APP_DETAILS.set(details).is_ok(),
        "application details have already been registered"
    );
}

/// Declares the details for the application being compiled.
///
/// The caller supplies the three names that identify the application. Everything else is filled in automatically: the
/// version comes from the calling crate's Cargo manifest, and the build metadata from `saluki-metadata`'s own build
/// script.
///
/// # Examples
///
/// ```
/// # use saluki_metadata::{declare_app_details, AppDetails};
/// pub const APP_DETAILS: AppDetails = declare_app_details!(
///     full_name = "Example Application",
///     short_name = "example",
///     identifier = "ex",
/// );
/// ```
#[macro_export]
macro_rules! declare_app_details {
    (full_name = $full_name:expr, short_name = $short_name:expr, identifier = $identifier:expr $(,)?) => {
        $crate::AppDetails::new(
            $full_name,
            $short_name,
            $identifier,
            $crate::Version::new(
                env!("CARGO_PKG_VERSION"),
                $crate::const_parse_u32(env!("CARGO_PKG_VERSION_MAJOR")),
                $crate::const_parse_u32(env!("CARGO_PKG_VERSION_MINOR")),
                $crate::const_parse_u32(env!("CARGO_PKG_VERSION_PATCH")),
            ),
        )
    };
}

/// Parses a `u32` from a string in a `const` context.
///
/// Only intended for the version components that Cargo hands us, which are always plain decimal numbers. Anything else
/// fails the build.
#[doc(hidden)]
pub const fn const_parse_u32(s: &str) -> u32 {
    match u32::from_str_radix(s, 10) {
        Ok(value) => value,
        Err(_) => panic!("version component was not a number"),
    }
}

/// Application details.
///
/// # Configuration
///
/// The name and version fields identify a specific application, so they're declared by the binary itself through
/// [`declare_app_details!`] and registered with [`set_app_details`]. The version comes from that binary's Cargo
/// manifest.
///
/// The remaining fields describe the build rather than the application, so they're captured at compile time from the
/// following environment variables:
///
/// - `APP_GIT_HASH`: Git hash of the application. If this isn't set, the default value is `"unknown"`.
/// - `APP_BUILD_TIME`: Build time of the application. If this isn't set, the default value is `"0000-00-00 00:00:00"`.
/// - `APP_DEV_BUILD`: Whether the application is a development build. If this isn't set, the default value is `true`.
/// - `TARGET`: Target architecture of the application. If this isn't set, the default value is `"unknown-arch"`.
///
/// Environment variables prefixed with `APP_` are expected to be set by the build script/tooling, while others are
/// provided automatically by Cargo.
#[derive(Serialize)]
pub struct AppDetails {
    full_name: &'static str,
    short_name: &'static str,
    identifier: &'static str,
    git_hash: &'static str,
    version: Version,
    build_time: &'static str,
    dev_build: bool,
    target_arch: &'static str,
}

impl AppDetails {
    /// Creates a new `AppDetails` from the given application identity.
    ///
    /// Build metadata is filled in from the values captured at compile time, so callers can't get it wrong. Prefer
    /// [`declare_app_details!`], which also derives the version from the calling crate's Cargo manifest.
    pub const fn new(
        full_name: &'static str, short_name: &'static str, identifier: &'static str, version: Version,
    ) -> Self {
        Self {
            full_name,
            short_name,
            identifier,
            version,
            git_hash: details::DETECTED_GIT_HASH,
            build_time: details::DETECTED_APP_BUILD_TIME,
            dev_build: details::DETECTED_APP_DEV_BUILD,
            target_arch: details::DETECTED_TARGET_ARCH,
        }
    }

    /// Returns the application's full name.
    ///
    /// This is typically a human-friendly/"pretty" name of the binary/executable, such as `"Agent Data Plane"`.
    ///
    /// If the application hasn't registered its details, this will return `"unknown"`.
    pub fn full_name(&self) -> &'static str {
        self.full_name
    }

    /// Returns the application's short name.
    ///
    /// This is typically a shorter version of the name of the binary/executable, such as `"Data Plane"` or `"DATAPLANE"`.
    ///
    /// If the application hasn't registered its details, this will return `"unknown"`.
    pub fn short_name(&self) -> &'static str {
        self.short_name
    }

    /// Returns the application's identifier.
    ///
    /// This is typically a very condensed form of the name of the binary/executable, like an acronym, such as `"adp"`
    /// or `"ADP"`.
    ///
    /// If the application hasn't registered its details, this will return `"unknown"`.
    pub fn identifier(&self) -> &'static str {
        self.identifier
    }

    /// Returns the Git hash used to build the application.
    ///
    /// If the Git hash couldn't be detected, this will return `"unknown"`.
    pub fn git_hash(&self) -> &'static str {
        self.git_hash
    }

    /// Returns the application's version.
    ///
    /// If the application hasn't registered its details, this will return a version equivalent to `"0.0.0"`.
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Returns the build time of the application.
    ///
    /// If the build time couldn't be detected, this will return `"0000-00-00 00:00:00"`.
    pub fn build_time(&self) -> &'static str {
        self.build_time
    }

    /// Returns `true` if this application is a development build.
    ///
    /// Development builds generally encompass all local builds, and any CI builds which aren't related to versioned
    /// artifacts intended for public release.
    ///
    /// If the development build flag couldn't be detected, this will return `true`.
    pub fn is_dev_build(&self) -> bool {
        self.dev_build
    }

    /// Returns the target architecture of the application.
    ///
    /// This returns a _target triple_, which is a string that generally has _four_ components: the processor
    /// architecture (x86-64, ARM64, etc), vendor (`"apple"`, `"pc"`, etc), operating system (`"linux"`, `"windows"`,
    /// `"darwin"`, etc) and environment/ABI (`"gnu"`, `"musl"`, etc).
    ///
    /// The environment/ABI component can sometimes be omitted in scenarios where there are no meaningful distinctions
    /// for the given operating system.
    ///
    /// If the target architecture couldn't be detected, this will return `"unknown-arch"`.
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
    /// Creates a new `Version` from the given raw string and its component numbers.
    pub const fn new(raw: &'static str, major: u32, minor: u32, patch: u32) -> Self {
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
    /// If the major version number isn't present in the version string, this will return `0`.
    pub fn major(&self) -> u32 {
        self.major
    }

    /// Returns the minor version number.
    ///
    /// If the minor version number isn't present in the version string, this will return `0`.
    pub fn minor(&self) -> u32 {
        self.minor
    }

    /// Returns the patch version number.
    ///
    /// If the patch version number isn't present in the version string, this will return `0`.
    pub fn patch(&self) -> u32 {
        self.patch
    }
}

impl Serialize for Version {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Redirect serialization entirely to the 'raw' string slice
        serializer.serialize_str(self.raw)
    }
}
