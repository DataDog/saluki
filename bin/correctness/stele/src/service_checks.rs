use serde::{Deserialize, Serialize};

/// A simplified service check representation.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ServiceCheck {
    name: String,
    status: u8,
    hostname: String,
    message: String,
    tags: Vec<String>,
    /// Unix timestamp in seconds from the `d:` field, or None if not present.
    ///
    /// NOTE: Do not compare this field directly without first normalizing pipeline-generated
    /// fill-in values — see `ServiceChecksAnalyzer` for details.
    pub timestamp: Option<u64>,
}

impl ServiceCheck {
    /// Returns the name of the service check.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the status of the service check (0=OK, 1=Warning, 2=Critical, 3=Unknown).
    pub fn status(&self) -> u8 {
        self.status
    }

    /// Returns the hostname of the service check.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Returns the message associated with the service check.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the tags of the service check.
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    /// Returns the timestamp of the service check (Unix seconds), or None if not set.
    pub fn timestamp(&self) -> Option<u64> {
        self.timestamp
    }

    /// Builds a `ServiceCheck` from the fields sent to `/api/v1/check_run`.
    pub fn from_check_run(
        name: String, status: u8, hostname: String, message: String, mut tags: Vec<String>, timestamp: Option<u64>,
    ) -> Self {
        tags.sort_unstable();
        Self {
            name,
            status,
            hostname,
            message,
            tags,
            timestamp,
        }
    }
}
