//! Events.

// TODO: Switch usages of `String` to `MetaString` since we should generally be able to intern these strings as they
// originate in in the DogStatsD codec, where interning is already taking place.

use std::fmt;

use serde::{Serialize, Serializer};

/// Value supplied used to specify a low priority event
pub const PRIORITY_LOW: &str = "low";

/// Value used to specify an error alert.
pub const ALERT_TYPE_ERROR: &str = "error";

/// Value used to specify a warning alert.
pub const ALERT_TYPE_WARNING: &str = "warning";

/// Value used to specify a success alert.
pub const ALERT_TYPE_SUCCESS: &str = "success";

/// Alert type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertType {
    /// Indicates an informational event.
    Info,

    /// Indicates an error event.
    Error,

    /// Indicates a warning event.
    Warning,

    /// Indicates a successful event.
    Success,
}

impl fmt::Display for AlertType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            AlertType::Info => "info",
            AlertType::Error => "error",
            AlertType::Warning => "warning",
            AlertType::Success => "success",
        };
        write!(f, "{}", s)
    }
}

impl Serialize for AlertType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.stringify())
    }
}

impl AlertType {
    /// Creates an AlertType from a string.
    ///
    /// Defaults to an informational alert.
    pub fn try_from_string(alert_type: &str) -> Option<Self> {
        match alert_type {
            ALERT_TYPE_ERROR => Some(AlertType::Error),
            ALERT_TYPE_WARNING => Some(AlertType::Warning),
            ALERT_TYPE_SUCCESS => Some(AlertType::Success),
            _ => Some(AlertType::Info),
        }
    }

    /// Returns stringified name of the AlertType variant.
    pub fn stringify(self) -> &'static str {
        match self {
            AlertType::Info => "info",
            AlertType::Error => "error",
            AlertType::Warning => "warning",
            AlertType::Success => "success",
        }
    }
}

/// Event priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// The event has normal priority.
    Normal,

    /// The event has low priority.
    Low,
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Priority::Normal => "normal",
            Priority::Low => "low",
        };
        write!(f, "{}", s)
    }
}

impl Serialize for Priority {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.stringify())
    }
}

impl Priority {
    /// Creates an event Priority from a string.
    ///
    /// Defaults to  normal priority.
    pub fn try_from_string(priority: &str) -> Option<Self> {
        match priority {
            PRIORITY_LOW => Some(Priority::Low),
            _ => Some(Priority::Normal),
        }
    }

    /// Returns stringified name of the Priority variant.
    pub fn stringify(self) -> &'static str {
        match self {
            Priority::Normal => "normal",
            Priority::Low => "low",
        }
    }
}

/// EventD is an object that can be posted to the DataDog event stream.
#[derive(Clone, Debug, Serialize)]
pub struct EventD {
    title: String,
    text: String,
    timestamp: Option<u64>,
    hostname: Option<String>,
    aggregation_key: Option<String>,
    priority: Option<Priority>,
    source_type_name: Option<String>,
    alert_type: Option<AlertType>,
    tags: Option<Vec<String>>,
}

impl EventD {
    /// Returns the title of the event.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns the text of the event.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the host where the event originated from.
    pub fn hostname(&self) -> Option<&str> {
        self.hostname.as_deref()
    }

    /// Returns the aggregation key of the event.
    pub fn aggregation_key(&self) -> Option<&str> {
        self.aggregation_key.as_deref()
    }

    /// Returns the priority of the event.
    pub fn priority(&self) -> Option<Priority> {
        self.priority
    }

    /// Returns the source type name of the event.
    pub fn source_type_name(&self) -> Option<&str> {
        self.source_type_name.as_deref()
    }

    /// Returns the alert type of the event.
    pub fn alert_type(&self) -> Option<AlertType> {
        self.alert_type
    }

    /// Returns the timestamp of the event.
    ///
    /// This is a Unix timestamp, or the number of seconds since the Unix epoch.
    pub fn timestamp(&self) -> Option<u64> {
        self.timestamp
    }

    /// Returns the tags associated with the event.
    pub fn tags(&self) -> Option<&[String]> {
        self.tags.as_deref()
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

    /// Set the timestamp.
    ///
    /// Represented as a Unix timestamp, or the number of seconds since the Unix epoch.
    pub fn set_timestamp(&mut self, timestamp: impl Into<Option<u64>>) {
        self.timestamp = timestamp.into();
    }

    /// Set the hostname where the event originated from.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_hostname(mut self, hostname: impl Into<Option<String>>) -> Self {
        self.hostname = hostname.into();
        self
    }

    /// Set the hostname where the event originated from.
    pub fn set_hostname(&mut self, hostname: impl Into<Option<String>>) {
        self.hostname = hostname.into();
    }

    /// Set the aggregation key of the event
    ///
    /// Aggregation key is use to group events together in the event stream.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_aggregation_key(mut self, hostname: impl Into<Option<String>>) -> Self {
        self.hostname = hostname.into();
        self
    }

    /// Set the hostname where the event originated from.
    ///
    /// Aggregation key is use to group events together in the event stream.
    pub fn set_aggregation_key(&mut self, hostname: impl Into<Option<String>>) {
        self.hostname = hostname.into();
    }

    /// Set the priority of the event
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_priority(mut self, priority: impl Into<Option<Priority>>) -> Self {
        self.priority = priority.into();
        self
    }

    /// Set the priority of the event
    pub fn set_priority(&mut self, priority: impl Into<Option<Priority>>) {
        self.priority = priority.into();
    }

    /// Set the source type name of the event
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_source_type_name(mut self, source_type_name: impl Into<Option<String>>) -> Self {
        self.source_type_name = source_type_name.into();
        self
    }

    /// Set the source type name of the event
    pub fn set_source_type_name(&mut self, source_type_name: impl Into<Option<String>>) {
        self.source_type_name = source_type_name.into();
    }

    /// Set the alert type of the event
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_alert_type(mut self, alert_type: impl Into<Option<AlertType>>) -> Self {
        self.alert_type = alert_type.into();
        self
    }

    /// Set the alert type name of the event
    pub fn set_alert_type(&mut self, alert_type: impl Into<Option<AlertType>>) {
        self.alert_type = alert_type.into();
    }

    /// Set the tags of the event
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_tags(mut self, tags: impl Into<Option<Vec<String>>>) -> Self {
        self.tags = tags.into();
        self
    }

    /// Set the tags of the event
    pub fn set_tags(&mut self, tags: impl Into<Option<Vec<String>>>) {
        self.tags = tags.into();
    }

    /// Creates an `EventD` from the given title and text.
    ///
    /// Defaults to an informational alert with normal priority.
    pub fn new(title: &str, text: &str) -> Self {
        Self {
            title: title.to_string(),
            text: text.to_string(),
            timestamp: None,
            hostname: None,
            aggregation_key: None,
            priority: Some(Priority::Normal),
            source_type_name: None,
            alert_type: Some(AlertType::Info),
            tags: None,
        }
    }
}
