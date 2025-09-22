//! Logs.

use std::collections::HashMap;

use saluki_context::tags::SharedTagSet;
use serde_json::Value as JsonValue;
use stringtheory::MetaString;

/// A log event.
#[derive(Clone, Debug, PartialEq)]
pub struct Log {
    /// Log message body.
    message: MetaString,
    /// Log status/severity (e.g., "info", "warn", "error").
    status: Option<LogStatus>,
    /// Hostname associated with the log.
    hostname: MetaString,
    /// Service associated with the log.
    service: MetaString,
    /// Tags of the log.
    tags: SharedTagSet,
    /// Additional properties of the log.
    additional_properties: HashMap<String, JsonValue>,
}

/// Log status.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LogStatus {
    /// Trace status.
    Trace,
    /// Emergency status.
    Emergency,
    /// Alert status.
    Alert,
    /// Fatal status.
    Fatal,
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
            hostname: MetaString::empty(),
            service: MetaString::empty(),
            tags: SharedTagSet::default(),
            additional_properties: HashMap::new(),
        }
    }

    /// Sets the log status.
    pub fn with_status(mut self, status: impl Into<Option<LogStatus>>) -> Self {
        self.status = status.into();
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

    /// Sets the tags string.
    pub fn with_ddtags(mut self, ddtags: impl Into<Option<SharedTagSet>>) -> Self {
        self.tags = ddtags.into().unwrap_or_else(SharedTagSet::default);
        self
    }

    /// Sets the addtional properties map.
    pub fn with_additional_properties(
        mut self, additional_properties: impl Into<Option<HashMap<String, JsonValue>>>,
    ) -> Self {
        self.additional_properties = additional_properties.into().unwrap_or_default();
        self
    }

    /// Returns the message string slice.
    pub fn message(&self) -> &str {
        &self.message
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

    /// Returns the tags, if set.
    pub fn tags(&self) -> &SharedTagSet {
        &self.tags
    }

    /// Returns the additional properties map.
    pub fn additional_properties(&self) -> &HashMap<String, JsonValue> {
        &self.additional_properties
    }
}
