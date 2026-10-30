//! Logs.

use std::collections::HashMap;

use serde_json::Value as JsonValue;
use stringtheory::MetaString;

use crate::data_model::tags::TagSet;

/// A log event.
#[derive(Clone, Debug, PartialEq)]
pub struct Log {
    /// Log message body, or the log's rendered content when `message_is_content` is set.
    message: MetaString,
    /// Whether `message` is the log's rendered content: the message as it's sent, with the log's properties in it.
    message_is_content: bool,
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
            message_is_content: false,
            status: MetaString::empty(),
            source: None,
            hostname: MetaString::empty(),
            service: MetaString::empty(),
            tags: TagSet::default(),
            additional_properties: HashMap::new(),
        }
    }

    /// Replaces the message with the log's rendered content.
    ///
    /// The content is the message as it's sent, with the log's properties already in it, such as the JSON object that
    /// the Agent sends for an OTLP log. The properties of the log are left as they are.
    pub fn with_rendered_content(mut self, content: impl Into<MetaString>) -> Self {
        self.message = content.into();
        self.message_is_content = true;
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
    ///
    /// When [`message_is_content`][Self::message_is_content] is `true`, this is the log's rendered content.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns `true` if the message is the log's rendered content, set by
    /// [`with_rendered_content`][Self::with_rendered_content].
    pub fn message_is_content(&self) -> bool {
        self.message_is_content
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;
    use stringtheory::MetaString;

    use super::Log;

    #[test]
    fn rendered_content_replaces_the_message_and_keeps_the_properties() {
        let mut properties = HashMap::new();
        properties.insert(MetaString::from_static("key"), json!("value"));
        let log = Log::new("hello").with_additional_properties(properties.clone());
        assert!(!log.message_is_content());
        assert_eq!(log.message(), "hello");

        let log = log.with_rendered_content(r#"{"key":"value","message":"hello"}"#);
        assert!(log.message_is_content());
        assert_eq!(log.message(), r#"{"key":"value","message":"hello"}"#);
        assert_eq!(log.additional_properties(), &properties);
    }
}
