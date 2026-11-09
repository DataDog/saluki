//! Service checks.

use saluki_common::iter::ReusableDeduplicator;
use serde::{ser::SerializeMap as _, Serialize, Serializer};
use stringtheory::MetaString;

use crate::data_model::tags::TagSet;

/// Service status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckStatus {
    /// The service is operating normally.
    Ok,

    /// The service is in a warning state.
    Warning,

    /// The service is in a critical state.
    Critical,

    /// The service is in an unknown state.
    Unknown,
}

/// A service check.
///
/// Service checks represent the status of a service at a particular point in time. Checks are simplistic, with a basic
/// message, status enum (OK vs warning vs critical, etc), timestamp, and tags.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceCheck {
    name: MetaString,
    status: CheckStatus,
    timestamp: Option<u64>,
    /// `None` if no hostname was set, or `Some` with the hostname, which may be explicitly empty.
    hostname: Option<MetaString>,
    message: MetaString,
    tags: TagSet,
    origin_tags: TagSet,
}

impl ServiceCheck {
    /// Returns the name of the check.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the status of the check.
    pub fn status(&self) -> CheckStatus {
        self.status
    }

    /// Returns the timestamp of the check.
    ///
    /// This is a Unix timestamp, or the number of seconds since the Unix epoch.
    pub fn timestamp(&self) -> Option<u64> {
        self.timestamp
    }

    /// Returns the host where the check originated from.
    ///
    /// Returns `None` if no hostname was set. An explicitly empty hostname is returned as `Some("")`, which is
    /// distinct from no hostname: for example, it shouldn't be replaced with a default hostname.
    pub fn hostname(&self) -> Option<&str> {
        self.hostname.as_deref()
    }

    /// Returns the message associated with the check.
    pub fn message(&self) -> Option<&str> {
        if self.message.is_empty() {
            None
        } else {
            Some(&self.message)
        }
    }

    /// Returns the tags associated with the check.
    pub fn tags(&self) -> &TagSet {
        &self.tags
    }

    /// Returns the origin tags associated with the check.
    pub fn origin_tags(&self) -> &TagSet {
        &self.origin_tags
    }

    /// Creates a `ServiceCheck` from the given name and status
    pub fn new(name: impl Into<MetaString>, status: CheckStatus) -> Self {
        Self {
            name: name.into(),
            status,
            timestamp: None,
            hostname: None,
            message: MetaString::empty(),
            tags: TagSet::default(),
            origin_tags: TagSet::default(),
        }
    }

    /// Set the timestamp.
    ///
    /// Represented as a Unix timestamp, or the number of seconds since the Unix epoch.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_timestamp(mut self, timestamp: impl Into<Option<u64>>) -> Self {
        self.timestamp = timestamp.into();
        self
    }

    /// Set the hostname where the service check originated from.
    ///
    /// `None` clears the hostname. `Some("")` sets an explicitly empty hostname, which is distinct from no hostname.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_hostname(mut self, hostname: impl Into<Option<MetaString>>) -> Self {
        self.hostname = hostname.into();
        self
    }

    /// Set the hostname where the service check originated from.
    ///
    /// `None` clears the hostname. `Some("")` sets an explicitly empty hostname, which is distinct from no hostname.
    pub fn set_hostname(&mut self, hostname: impl Into<Option<MetaString>>) {
        self.hostname = hostname.into();
    }

    /// Set the tags of the service check
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_tags(mut self, tags: impl Into<TagSet>) -> Self {
        self.tags = tags.into();
        self
    }

    /// Set the message of the service check
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_message(mut self, message: impl Into<Option<MetaString>>) -> Self {
        self.message = match message.into() {
            Some(message) => message,
            None => MetaString::empty(),
        };
        self
    }

    /// Set the origin tags of the service check
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_origin_tags(mut self, origin_tags: impl Into<TagSet>) -> Self {
        self.origin_tags = origin_tags.into();
        self
    }
}

impl Serialize for ServiceCheck {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("check", &self.name)?;
        // An explicitly empty hostname is still written, as the Datadog Agent does.
        if let Some(hostname) = &self.hostname {
            map.serialize_entry("host_name", hostname)?;
        }
        if !self.message.is_empty() {
            map.serialize_entry("message", &self.message)?;
        }
        map.serialize_entry("status", &self.status)?;

        let tags = DeduplicatedTagsSerializable {
            tags: &self.tags,
            origin_tags: &self.origin_tags,
        };
        map.serialize_entry("tags", &tags)?;

        if let Some(timestamp) = self.timestamp.as_ref() {
            map.serialize_entry("timestamp", timestamp)?;
        }
        map.end()
    }
}

impl CheckStatus {
    /// Returns the integer representation of this status.
    pub const fn as_u8(&self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Warning => 1,
            Self::Critical => 2,
            Self::Unknown => 3,
        }
    }
}

impl Serialize for CheckStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(self.as_u8())
    }
}

/// Error type for parsing CheckStatus.
#[derive(Debug, Clone)]
pub struct ParseCheckStatusError;

impl std::fmt::Display for ParseCheckStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid check status")
    }
}

impl std::error::Error for ParseCheckStatusError {}

impl TryFrom<u8> for CheckStatus {
    type Error = ParseCheckStatusError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Ok),
            1 => Ok(Self::Warning),
            2 => Ok(Self::Critical),
            3 => Ok(Self::Unknown),
            _ => Err(ParseCheckStatusError),
        }
    }
}

// Helper type to let us serialize deduplicated tags.
struct DeduplicatedTagsSerializable<'a> {
    tags: &'a TagSet,
    origin_tags: &'a TagSet,
}

impl<'a> Serialize for DeduplicatedTagsSerializable<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let chained_tags = self.tags.into_iter().chain(self.origin_tags);

        let mut tags_deduplicator = ReusableDeduplicator::new();
        let deduplicated_tags = tags_deduplicator.deduplicated(chained_tags);
        serializer.collect_seq(deduplicated_tags)
    }
}

#[cfg(test)]
mod tests {
    use stringtheory::MetaString;

    use super::{CheckStatus, ServiceCheck};

    #[test]
    fn unset_and_explicitly_empty_hostnames_are_distinct() {
        let unset = ServiceCheck::new("check", CheckStatus::Ok);
        assert_eq!(unset.hostname(), None);

        let empty = ServiceCheck::new("check", CheckStatus::Ok).with_hostname(MetaString::empty());
        assert_eq!(empty.hostname(), Some(""));

        let cleared = empty.with_hostname(None);
        assert_eq!(cleared.hostname(), None);
    }

    #[test]
    fn serializes_explicitly_empty_hostname() {
        // Like the Datadog Agent, an explicitly empty hostname is written as an empty `host_name`, while an unset
        // hostname is omitted.
        let unset = serde_json::to_value(ServiceCheck::new("check", CheckStatus::Ok)).expect("should serialize");
        assert!(unset.get("host_name").is_none());

        let empty = ServiceCheck::new("check", CheckStatus::Ok).with_hostname(MetaString::empty());
        let empty = serde_json::to_value(empty).expect("should serialize");
        assert_eq!(empty["host_name"], "");
    }
}
