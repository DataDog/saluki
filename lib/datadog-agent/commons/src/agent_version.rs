//! Agent version detection.
//!
//! Agent Data Plane is designed to be a drop-in replacement for specific pieces of Datadog Agent functionality, and
//! some of that behavior is version-dependent: a given Agent version may emit an output field, payload shape, or tag
//! that an earlier version did not. To stay byte-for-byte compatible with the Agent it is bundled with, ADP needs to
//! know which Agent version it is paired with.
//!
//! The Agent version is provided via the `DD_AGENT_VERSION` environment variable at *build time*, baked into the
//! binary by `build.rs`. The converged Agent image build passes it as a Docker build argument, so every ADP binary
//! carries a fixed, immutable version string that reflects the Agent it was compiled alongside.
//!
//! When `DD_AGENT_VERSION` is absent at build time (for example, in local development or standalone builds without a
//! bundled Agent), the version is treated as unknown and all version gates default to the most recent behavior.
//!
//! # Design
//!
//! This is intentionally a lightweight, hand-rolled version parser rather than a full semantic-versioning
//! implementation: the values we compare against are simple `MAJOR.MINOR.PATCH` release tags (optionally carrying a
//! flavor or pre-release suffix such as `-full` or `-devel`), and we only ever need a "meets this minimum" comparison.

include!(concat!(env!("OUT_DIR"), "/details.rs"));

/// Raw Agent version string that ADP was built against, as detected at build time.
///
/// This is the verbatim `DD_AGENT_VERSION` value (e.g. `"7.81.0-full"` or `"nightly"`), suitable for display in
/// diagnostics. Returns `None` when `DD_AGENT_VERSION` was not set at build time.
pub fn version_string() -> Option<&'static str> {
    if DETECTED_AGENT_VERSION.is_empty() {
        None
    } else {
        Some(DETECTED_AGENT_VERSION)
    }
}

/// Version of the Datadog Agent that ADP is paired with, as detected at build time.
///
/// Returns `None` when `DD_AGENT_VERSION` was not set at build time.
pub fn version() -> Option<AgentVersion> {
    if DETECTED_AGENT_VERSION.is_empty() {
        return None;
    }
    Some(AgentVersion {
        major: DETECTED_AGENT_VERSION_MAJOR,
        minor: DETECTED_AGENT_VERSION_MINOR,
        patch: DETECTED_AGENT_VERSION_PATCH,
        is_dev: DETECTED_AGENT_IS_DEV,
    })
}

/// Returns `true` if the Agent version is at least `major.minor.patch`.
///
/// When the Agent version is unknown (i.e. `DD_AGENT_VERSION` was not set at build time), this returns `true`:
/// absent an explicit older-version signal, ADP defaults to the most recent behavior.
pub fn meets(major: u64, minor: u64, patch: u64) -> bool {
    version().is_none_or(|v| v.meets(major, minor, patch))
}

/// Version of the Datadog Agent that ADP is paired with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentVersion {
    major: u64,
    minor: u64,
    patch: u64,
    is_dev: bool,
}

impl AgentVersion {
    /// Parses an Agent version from a version string.
    ///
    /// Accepts values such as `7.81.0`, `7.81.0-full`, `7.83.0-devel`, and bare development markers such as `nightly`.
    /// Development/pre-release builds (identified by a `devel`, `dev`, `nightly`, or `master` marker) are considered
    /// newer than every numbered release, since they track the bleeding edge of Agent behavior.
    ///
    /// Returns `None` if the string is empty or carries neither a parseable `MAJOR` component nor a development marker.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }

        let is_dev = ["devel", "dev", "nightly", "master"]
            .iter()
            .any(|marker| raw.contains(marker));

        // Take the leading `MAJOR.MINOR.PATCH` portion, discarding any flavor/pre-release suffix (`-full`, `-devel`,
        // `+git...`, etc.).
        let version_part = raw.split(['-', '+']).next().unwrap_or("");
        let mut components = version_part.split('.');
        let major = components.next().and_then(|c| c.parse().ok());
        let minor = components.next().and_then(|c| c.parse().ok()).unwrap_or(0);
        let patch = components.next().and_then(|c| c.parse().ok()).unwrap_or(0);

        match (major, is_dev) {
            (Some(major), _) => Some(Self {
                major,
                minor,
                patch,
                is_dev,
            }),
            // A development marker with no numeric version (e.g. `nightly`) is still meaningful: it is newer than
            // everything.
            (None, true) => Some(Self {
                major: 0,
                minor: 0,
                patch: 0,
                is_dev: true,
            }),
            (None, false) => None,
        }
    }

    /// Returns `true` if this version is at least `major.minor.patch`.
    ///
    /// Development/pre-release builds always satisfy the check, as they track behavior ahead of any numbered release.
    pub fn meets(&self, major: u64, minor: u64, patch: u64) -> bool {
        if self.is_dev {
            return true;
        }
        (self.major, self.minor, self.patch) >= (major, minor, patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_release() {
        let version = AgentVersion::parse("7.81.0").expect("should parse");
        assert_eq!(
            version,
            AgentVersion {
                major: 7,
                minor: 81,
                patch: 0,
                is_dev: false
            }
        );
    }

    #[test]
    fn parses_release_with_flavor_suffix() {
        let version = AgentVersion::parse("7.81.0-full").expect("should parse");
        assert_eq!(
            version,
            AgentVersion {
                major: 7,
                minor: 81,
                patch: 0,
                is_dev: false
            }
        );
    }

    #[test]
    fn parses_dev_build() {
        let version = AgentVersion::parse("7.83.0-devel+git.20.5723c24").expect("should parse");
        assert!(version.is_dev);
        assert_eq!(version.major, 7);
        assert_eq!(version.minor, 83);
    }

    #[test]
    fn parses_bare_dev_marker() {
        let version = AgentVersion::parse("nightly").expect("should parse");
        assert!(version.is_dev);
    }

    #[test]
    fn rejects_empty_and_garbage() {
        assert_eq!(AgentVersion::parse(""), None);
        assert_eq!(AgentVersion::parse("   "), None);
        assert_eq!(AgentVersion::parse("not-a-version"), None);
    }

    #[test]
    fn meets_compares_release_versions() {
        let v = AgentVersion::parse("7.81.0").unwrap();
        assert!(v.meets(7, 80, 0));
        assert!(v.meets(7, 81, 0));
        assert!(!v.meets(7, 82, 0));
        assert!(!v.meets(8, 0, 0));
    }

    #[test]
    fn dev_builds_meet_every_gate() {
        let v = AgentVersion::parse("nightly").unwrap();
        assert!(v.meets(7, 82, 0));
        assert!(v.meets(99, 0, 0));

        let v = AgentVersion::parse("7.83.0-devel").unwrap();
        assert!(v.meets(7, 82, 0));
        assert!(v.meets(99, 0, 0));
    }
}
