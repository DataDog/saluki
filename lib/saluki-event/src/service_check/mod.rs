//! Service checks.

use stringtheory::MetaString;

use serde::{Serialize, Serializer};
/// Service status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, Serialize)]
pub struct ServiceCheck {
    #[serde(rename = "check")]
    name: MetaString,
    status: CheckStatus,
    timestamp: Option<u64>,
    hostname: Option<MetaString>,
    message: Option<MetaString>,
    tags: Option<Vec<MetaString>>,
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
        self.hostname.as_deref()
    }

    /// Returns the message associated with the check.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Returns the tags associated with the check.
    pub fn tags(&self) -> Option<&[MetaString]> {
        self.tags.as_deref()
    }

    /// Creates a `ServiceCheck` from the given name and status
    pub fn new(name: &str, status: CheckStatus) -> Self {
        Self {
            name: name.into(),
            status,
            timestamp: None,
            hostname: None,
            message: None,
            tags: None,
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
        self.hostname = hostname.into();
        self
    }

    /// Set the tags of the service check
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_tags(mut self, tags: impl Into<Option<Vec<MetaString>>>) -> Self {
        self.tags = tags.into();
        self
    }

    /// Set the message of the service check
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_message(mut self, message: impl Into<Option<MetaString>>) -> Self {
        self.message = message.into();
        self
    }
}

impl CheckStatus {
    /// Convert Check Status to u8 representation.
    pub fn as_u8(&self) -> u8 {
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
