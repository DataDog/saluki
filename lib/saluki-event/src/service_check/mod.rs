//! Service checks.

// TODO: Switch usages of `String` to `MetaString` since we should generally be able to intern these strings as they
// originate in in the DogStatsD codec, where interning is already taking place.

/// Service status.
#[derive(Clone, Copy, Debug)]
pub enum CheckStatus {
    /// The service is operating normally.
    Ok,

    /// The service is in a warning state.
    Warn,

    /// The service is in a critical state.
    Critical,

    /// The service is in an unknown state.
    Unknown,
}

/// A service check.
///
/// Service checks represent the status of a service at a particular point in time. Checks are simplistic, with a basic
/// message, status enum (OK vs warning vs critical, etc), timestamp, and tags.
#[derive(Clone, Debug)]
pub struct ServiceCheck {
    name: String,
    status: CheckStatus,
    timestamp: u64,
    host: String,
    message: String,
    tags: Vec<String>,
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
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Returns the host where the check originated from.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Returns the message associated with the check.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the tags associated with the check.
    pub fn tags(&self) -> &[String] {
        &self.tags
    }
}
