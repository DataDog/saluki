//! Cross-cutting support types shared across subsystems.

use std::fmt;

use smallvec::SmallVec;
use stringtheory::MetaString;

use crate::runtime::get_sanitized_name;

/// A fully qualified, canonical identifier for a uniquely addressable subsystem within the overall system.
///
/// `SubsystemIdentifier` is meant to represent a unique identifier for any "subsystem" in a Saluki-based data plane
/// such that it is already sanitized, normalized, and ready to be used in the various registries and areas where unique
/// names are required: health registry, resource account, supervision trees, and more.
///
/// Segments are sanitized as they are added, and any segment that sanitizes to an empty string is dropped. A
/// `SubsystemIdentifier` therefore never contains an empty segment, and its rendered form never has a leading,
/// trailing, or doubled separator.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SubsystemIdentifier {
    segments: SmallVec<[MetaString; 6]>,
}

impl SubsystemIdentifier {
    /// Creates a `SubsystemIdentifier` from a number of segments.
    ///
    /// Each segment is sanitized/normalized independently, and any segment that sanitizes to an empty string is
    /// dropped.
    pub fn from_segments<I, S>(segments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            segments: segments
                .into_iter()
                .map(|s| get_sanitized_name(s.as_ref()))
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }

    /// Creates a `SubsystemIdentifier` from a single dotted string, treating each period as a segment separator.
    ///
    /// The input is split on `.`, then each segment is sanitized/normalized independently (matching
    /// [`from_segments`][Self::from_segments]) and empty segments are dropped. This is the correct constructor when the
    /// caller already holds an assembled, dotted name (for example `topology.primary.sources.in`); passing such a
    /// string to [`from_segments`][Self::from_segments] as a lone segment would instead collapse the periods into
    /// underscores.
    pub fn from_dotted(dotted: &str) -> Self {
        Self::from_segments(dotted.split('.'))
    }

    /// Consumes the identifier and returns a new one with the given segment appended.
    ///
    /// The segment is sanitized/normalized first; if it sanitizes to empty, the identifier is returned unchanged.
    pub fn child<S: AsRef<str>>(mut self, segment: S) -> Self {
        let segment = get_sanitized_name(segment.as_ref());
        if !segment.is_empty() {
            self.segments.push(segment);
        }
        self
    }

    /// Returns `true` if `other` is a strict descendant of this identifier.
    ///
    /// A descendant has this identifier as a complete leading run of segments and at least one additional segment. For
    /// example, `env_provider.workload` is an ancestor of `env_provider.workload.remote_agent` but not of
    /// `env_provider.workloads` (segments are compared whole, not as string prefixes), nor of `env_provider.workload`
    /// itself (the match must be strict).
    pub fn is_ancestor_of(&self, other: &SubsystemIdentifier) -> bool {
        other.segments.len() > self.segments.len() && other.segments[..self.segments.len()] == self.segments[..]
    }
}

impl fmt::Display for SubsystemIdentifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (idx, segment) in self.segments.iter().enumerate() {
            if idx > 0 {
                write!(f, ".")?;
            }
            write!(f, "{}", segment)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::SubsystemIdentifier;

    #[test]
    fn general_segments_are_sanitized() {
        let identifier = SubsystemIdentifier::from_segments(["env_provider", "workload", "remote-agent"]);
        assert_eq!(identifier.to_string(), "env_provider.workload.remote_agent");
    }

    #[test]
    fn from_dotted_splits_and_sanitizes() {
        // Periods separate segments, and each segment is sanitized independently.
        let identifier = SubsystemIdentifier::from_dotted("topology.primary.sources.dsd-in");
        assert_eq!(identifier.to_string(), "topology.primary.sources.dsd_in");

        // A lone segment via `from_segments` collapses periods instead of splitting on them.
        let collapsed = SubsystemIdentifier::from_segments(["topology.primary"]);
        assert_eq!(collapsed.to_string(), "topology_primary");
    }

    #[test]
    fn empty_segments_are_dropped() {
        // Leading, trailing, and doubled separators produce empty segments, which are dropped so the rendered form
        // never has stray separators. This keeps the identifier consistent with the process-tree name sanitizer.
        assert_eq!(SubsystemIdentifier::from_dotted(".a..b.").to_string(), "a.b");
        assert_eq!(SubsystemIdentifier::from_segments(["a", "", "b"]).to_string(), "a.b");

        // A segment consisting only of trimmable characters sanitizes to empty and is likewise dropped, including via
        // `child`.
        assert_eq!(SubsystemIdentifier::from_segments(["a", "--", "b"]).to_string(), "a.b");
        assert_eq!(SubsystemIdentifier::from_segments(["a"]).child("--").to_string(), "a");
    }

    #[test]
    fn is_ancestor_of() {
        let identifier = SubsystemIdentifier::from_segments(["env_provider", "workload"]);
        let ancestor_of = |s: &str| identifier.is_ancestor_of(&SubsystemIdentifier::from_dotted(s));

        // Strict descendants match, at any depth.
        assert!(ancestor_of("env_provider.workload.remote_agent"));
        assert!(ancestor_of("env_provider.workload.remote_agent.aggregator"));

        // The identifier is not an ancestor of itself.
        assert!(!ancestor_of("env_provider.workload"));

        // Segments are compared whole, so a longer segment sharing a prefix does not match.
        assert!(!ancestor_of("env_provider.workloads.remote_agent"));

        // Unrelated names, and names for which the identifier is a suffix rather than a prefix, do not match.
        assert!(!ancestor_of("topology.primary.sources.in"));
        assert!(!ancestor_of("workload.remote_agent"));
    }
}
