//! Service checks.

use saluki_context::tags::{SharedTagSet, TagsExt};
use serde::{ser::SerializeMap as _, Serialize, Serializer};
use stringtheory::MetaString;

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
    hostname: MetaString,
    message: MetaString,
    tags: SharedTagSet,
    origin_tags: SharedTagSet,
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
    pub fn hostname(&self) -> Option<&str> {
        if self.hostname.is_empty() {
            None
        } else {
            Some(&self.hostname)
        }
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
    pub fn tags(&self) -> &SharedTagSet {
        &self.tags
    }

    /// Returns the origin tags associated with the check.
    pub fn origin_tags(&self) -> &SharedTagSet {
        &self.origin_tags
    }

    /// Creates a `ServiceCheck` from the given name and status
    pub fn new(name: impl Into<MetaString>, status: CheckStatus) -> Self {
        Self {
            name: name.into(),
            status,
            timestamp: None,
            hostname: MetaString::empty(),
            message: MetaString::empty(),
            tags: SharedTagSet::default(),
            origin_tags: SharedTagSet::default(),
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
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_hostname(mut self, hostname: impl Into<Option<MetaString>>) -> Self {
        self.hostname = match hostname.into() {
            Some(hostname) => hostname,
            None => MetaString::empty(),
        };
        self
    }

    /// Set the tags of the service check
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_tags(mut self, tags: impl Into<SharedTagSet>) -> Self {
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
    pub fn with_origin_tags(mut self, origin_tags: SharedTagSet) -> Self {
        self.origin_tags = origin_tags;
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
        if !self.hostname.is_empty() {
            map.serialize_entry("host_name", &self.hostname)?;
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
    tags: &'a SharedTagSet,
    origin_tags: &'a SharedTagSet,
}

impl<'a> Serialize for DeduplicatedTagsSerializable<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let deduplicated_tags = self.tags.into_iter().chain(self.origin_tags).deduplicated();
        serializer.collect_seq(deduplicated_tags)
    }
}
