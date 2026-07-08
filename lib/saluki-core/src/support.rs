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
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SubsystemIdentifier {
    segments: SmallVec<[MetaString; 6]>,
}

impl SubsystemIdentifier {
    /// Creates a `SubsystemIdentifier` from a number of segments.
    ///
    /// Every segment is sanitized/normalized first.
    pub fn from_segments<I, S>(segments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            segments: segments.into_iter().map(|s| get_sanitized_name(s.as_ref())).collect(),
        }
    }

    /// Consumes the identifier and returns a new one with the given segment appended.
    ///
    /// The segment is sanitized/normalized first.
    pub fn child<S: AsRef<str>>(mut self, segment: S) -> Self {
        self.segments.push(get_sanitized_name(segment.as_ref()));
        self
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
}
