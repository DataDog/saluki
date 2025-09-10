//! Logs.

use std::num::NonZeroU64;

use saluki_context::tags::SharedTagSet;
use stringtheory::MetaString;

/// A log event.
#[derive(Clone, Debug, PartialEq)]
pub struct Log {
    /// Log message body.
    message: MetaString,
    /// Log status/severity (e.g., "info", "warn", "error").
    status: Option<LogStatus>,
    /// Timestamp.
    timestamp: Option<NonZeroU64>,
    /// Hostname associated with the log.
    hostname: MetaString,
    /// Service associated with the log.
    service: MetaString,
    /// Source of the log.
    source: MetaString,
    /// Tags of the log.
    tags: SharedTagSet,
}

/// Log status.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LogStatus {
    /// Emergency status.
    Emergency,
    /// Alert status.
    Alert,
    /// Critical status.
    Critical,
    /// Error status.
    Error,
    /// Warning status.
    Warning,
    /// Notice status.
    Notice,
    /// Info status.
    Info,
    /// Debug status.
    Debug,
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

    /// Sets the log status.
    pub fn with_status(mut self, status: impl Into<Option<LogStatus>>) -> Self {
        self.status = status.into();
        self
    }

    /// Set the timestamp.
    ///
    /// Represented as a Unix timestamp, or the number of seconds since the Unix epoch.
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

    /// Sets the log source.
    pub fn with_source(mut self, source: impl Into<Option<MetaString>>) -> Self {
        self.source = source.into().unwrap_or_else(MetaString::empty);
        self
    }

    /// Sets the tags string.
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

    /// Returns the log status, if set.
    pub fn status(&self) -> Option<LogStatus> {
        self.status
    }

    /// Returns the hostname, if set.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Returns the service name, if set.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Returns the log source, if set.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns the tags, if set.
    pub fn tags(&self) -> &SharedTagSet {
        &self.tags
    }
}
