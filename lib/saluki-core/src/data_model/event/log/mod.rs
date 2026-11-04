//! Logs.

use std::collections::HashMap;
use std::num::NonZeroU64;

use serde_json::Value as JsonValue;
use stringtheory::MetaString;

use crate::data_model::tags::TagSet;

/// A log event.
#[derive(Clone, Debug, PartialEq)]
pub struct Log {
    /// Log message body.
    message: MetaString,
    /// When the log entered the pipeline, in nanoseconds since the Unix epoch, if its source recorded it.
    ingestion_timestamp: Option<NonZeroU64>,
    /// Log status, as the producer of the log gives it (for example, `"info"`, `"warn"`, or `"Fatal"`).
    ///
    /// It isn't normalized, and is empty when the producer gave no status.
    status: MetaString,
    /// Log source
    source: Option<MetaString>,
    /// Hostname associated with the log.
    hostname: MetaString,
    /// Service associated with the log.
    service: MetaString,
    /// Tags of the log.
    tags: TagSet,
    /// Additional properties of the log.
    additional_properties: HashMap<MetaString, JsonValue>,
}

impl Log {
    /// Creates a new `Log` with the given message.
    pub fn new(message: impl Into<MetaString>) -> Self {
        Self {
            message: message.into(),
            ingestion_timestamp: None,
            status: MetaString::empty(),
            source: None,
            hostname: MetaString::empty(),
            service: MetaString::empty(),
            tags: TagSet::default(),
            additional_properties: HashMap::new(),
        }
    }

    /// Sets the log message.
    pub fn with_message(mut self, message: impl Into<MetaString>) -> Self {
        self.message = message.into();
        self
    }

    /// Sets when the log entered the pipeline, in nanoseconds since the Unix epoch.
    ///
    /// Sources set this when they create a log. It isn't the time of the log itself, which belongs to its properties,
    /// but the time it was received, for measuring how long logs take to be sent. A timestamp of 0 means unknown.
    pub fn with_ingestion_timestamp(mut self, timestamp_ns: u64) -> Self {
        self.ingestion_timestamp = NonZeroU64::new(timestamp_ns);
        self
    }

    /// Sets the log status.
    ///
    /// The status is kept as it is given, without normalization. An empty status means the log has none.
    pub fn with_status(mut self, status: impl Into<MetaString>) -> Self {
        self.status = status.into();
        self
    }

    /// Sets the log source.
    pub fn with_source(mut self, source: impl Into<Option<MetaString>>) -> Self {
        self.source = source.into();
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
    pub fn with_tags(mut self, tags: impl Into<Option<TagSet>>) -> Self {
        self.tags = tags.into().unwrap_or_else(TagSet::default);
        self
    }

    /// Sets the additional properties map.
    pub fn with_additional_properties(
        mut self, additional_properties: impl Into<Option<HashMap<MetaString, JsonValue>>>,
    ) -> Self {
        self.additional_properties = additional_properties.into().unwrap_or_default();
        self
    }

    /// Returns the message string slice.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns when the log entered the pipeline, in nanoseconds since the Unix epoch, if its source recorded it.
    pub fn ingestion_timestamp(&self) -> Option<u64> {
        self.ingestion_timestamp.map(NonZeroU64::get)
    }

    /// Returns the log status, or an empty string if the log has none.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Returns the log source, if set.
    pub fn source(&self) -> &Option<MetaString> {
        &self.source
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
    pub fn tags(&self) -> &TagSet {
        &self.tags
    }

    /// Returns the additional properties map.
    pub fn additional_properties(&self) -> &HashMap<MetaString, JsonValue> {
        &self.additional_properties
    }
}
