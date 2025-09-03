//! Logs.

use std::num::NonZeroU64;

use saluki_context::tags::SharedTagSet;
use stringtheory::MetaString;

/// Internal representation of a log event aligned to Datadog logs payload fields https://docs.datadoghq.com/api/latest/logs/#log-object.
#[derive(Clone, Debug, PartialEq)]
pub struct Log {
    /// Log message body.
    message: MetaString,
    /// Log status/severity (e.g., "info", "warn", "error").
    status: Option<MetaString>,
    /// Timestamp in seconds since Unix epoch.
    timestamp: Option<NonZeroU64>,
    /// Hostname associated with the log.
    hostname: MetaString,
    /// Service associated with the log.
    service: MetaString,
    /// Source of the log (ddsource).
    source: MetaString,
    /// Comma-separated tags (ddtags), format: key:value,key:value.
    tags: SharedTagSet,
}

impl Log {
    /// Creates a new `Log` with the given message.
    pub fn new(message: impl Into<MetaString>) -> Self {
        Self {
            message: message.into(),
            status: None,
            timestamp: None,
            hostname: MetaString::empty(),
            service: MetaString::empty(),
            source: MetaString::empty(),
            tags: SharedTagSet::default(),
        }
    }

    /// Sets the log status/severity.
    pub fn with_status(mut self, status: impl Into<Option<MetaString>>) -> Self {
        self.status = status.into();
        self
    }

    /// Sets the timestamp in seconds since Unix epoch. `None` clears the timestamp.
    pub fn with_timestamp_unix_s(mut self, ts: impl Into<Option<u64>>) -> Self {
        self.timestamp = ts.into().and_then(NonZeroU64::new);
        self
    }

    /// Sets the hostname.
    pub fn with_hostname(mut self, hostname: impl Into<Option<MetaString>>) -> Self {
        self.hostname = hostname.into().unwrap_or_else(MetaString::empty);
        self
    }

    /// Sets the service name.
    pub fn with_service(mut self, service: impl Into<Option<MetaString>>) -> Self {
        self.service = service.into().unwrap_or_else(MetaString::empty);
        self
    }

    /// Sets the log source (ddsource).
    pub fn with_source(mut self, source: impl Into<Option<MetaString>>) -> Self {
        self.source = source.into().unwrap_or_else(MetaString::empty);
        self
    }

    /// Sets the comma-separated Datadog tags string (ddtags).
    pub fn with_ddtags(mut self, ddtags: impl Into<Option<SharedTagSet>>) -> Self {
        self.tags = ddtags.into().unwrap_or_else(SharedTagSet::default);
        self
    }

    /// Returns the message string slice.
    pub fn message(&self) -> &str {
        &self.message
    }
    /// Returns the timestamp in seconds, if set.
    pub fn timestamp_unix_s(&self) -> Option<u64> {
        self.timestamp.map(|v| v.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log() {
        let log = Log::new("test message");
        assert_eq!(log.message(), "test message");
    }
}
