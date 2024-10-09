//! Events.

use std::{fmt, num::NonZeroU64};

use serde::{Serialize, Serializer};
use stringtheory::MetaString;

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
        serializer.serialize_str(self.as_str())
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

    fn as_str(self) -> &'static str {
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
        serializer.serialize_str(self.as_str())
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

    fn as_str(self) -> &'static str {
        match self {
            Priority::Normal => "normal",
            Priority::Low => "low",
        }
    }
}

/// EventD is an object that can be posted to the DataDog event stream.
#[derive(Clone, Debug, Serialize)]
pub struct EventD {
    title: MetaString,
    text: MetaString,
    timestamp: Option<NonZeroU64>,
    #[serde(skip_serializing_if = "MetaString::is_empty")]
    hostname: MetaString,
    #[serde(skip_serializing_if = "MetaString::is_empty")]
    aggregation_key: MetaString,
    priority: Option<Priority>,
    #[serde(skip_serializing_if = "MetaString::is_empty")]
    source_type_name: MetaString,
    alert_type: Option<AlertType>,
    tags: Option<Vec<MetaString>>,
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
        if self.hostname.is_empty() {
            None
        } else {
            Some(&self.hostname)
        }
    }

    /// Returns the aggregation key of the event.
    pub fn aggregation_key(&self) -> Option<&str> {
        if self.aggregation_key.is_empty() {
            None
        } else {
            Some(&self.aggregation_key)
        }
    }

    /// Returns the priority of the event.
    pub fn priority(&self) -> Option<Priority> {
        self.priority
    }

    /// Returns the source type name of the event.
    pub fn source_type_name(&self) -> Option<&str> {
        if self.source_type_name.is_empty() {
            None
        } else {
            Some(&self.source_type_name)
        }
    }

    /// Returns the alert type of the event.
    pub fn alert_type(&self) -> Option<AlertType> {
        self.alert_type
    }

    /// Returns the timestamp of the event.
    ///
    /// This is a Unix timestamp, or the number of seconds since the Unix epoch.
    pub fn timestamp(&self) -> Option<u64> {
        self.timestamp.map(|ts| ts.get())
    }

    /// Returns the tags associated with the event.
    pub fn tags(&self) -> Option<&[MetaString]> {
        self.tags.as_deref()
    }

    /// Set the timestamp.
    ///
    /// Represented as a Unix timestamp, or the number of seconds since the Unix epoch.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_timestamp(mut self, timestamp: impl Into<Option<u64>>) -> Self {
        self.timestamp = timestamp.into().and_then(NonZeroU64::new);
        self
    }

    /// Set the timestamp.
    ///
    /// Represented as a Unix timestamp, or the number of seconds since the Unix epoch.
    pub fn set_timestamp(&mut self, timestamp: impl Into<Option<u64>>) {
        self.timestamp = timestamp.into().and_then(NonZeroU64::new);
    }

    /// Set the hostname where the event originated from.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_hostname(mut self, hostname: impl Into<Option<MetaString>>) -> Self {
        self.hostname = match hostname.into() {
            Some(s) => s,
            None => MetaString::empty(),
        };
        self
    }

    /// Set the hostname where the event originated from.
    pub fn set_hostname(&mut self, hostname: impl Into<Option<MetaString>>) {
        self.hostname = match hostname.into() {
            Some(s) => s,
            None => MetaString::empty(),
        };
    }

    /// Set the aggregation key of the event.
    ///
    /// Aggregation key is use to group events together in the event stream.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_aggregation_key(mut self, aggregation_key: impl Into<Option<MetaString>>) -> Self {
        self.aggregation_key = match aggregation_key.into() {
            Some(s) => s,
            None => MetaString::empty(),
        };
        self
    }

    /// Set the hostname where the event originated from.
    ///
    /// Aggregation key is use to group events together in the event stream.
    pub fn set_aggregation_key(&mut self, aggregation_key: impl Into<Option<MetaString>>) {
        self.aggregation_key = match aggregation_key.into() {
            Some(s) => s,
            None => MetaString::empty(),
        };
    }

    /// Set the priority of the event.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_priority(mut self, priority: impl Into<Option<Priority>>) -> Self {
        self.priority = priority.into();
        self
    }

    /// Set the priority of the event.
    pub fn set_priority(&mut self, priority: impl Into<Option<Priority>>) {
        self.priority = priority.into();
    }

    /// Set the source type name of the event.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_source_type_name(mut self, source_type_name: impl Into<Option<MetaString>>) -> Self {
        self.source_type_name = match source_type_name.into() {
            Some(s) => s,
            None => MetaString::empty(),
        };
        self
    }

    /// Set the source type name of the event.
    pub fn set_source_type_name(&mut self, source_type_name: impl Into<Option<MetaString>>) {
        self.source_type_name = match source_type_name.into() {
            Some(s) => s,
            None => MetaString::empty(),
        };
    }

    /// Set the alert type of the event.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_alert_type(mut self, alert_type: impl Into<Option<AlertType>>) -> Self {
        self.alert_type = alert_type.into();
        self
    }

    /// Set the alert type name of the event.
    pub fn set_alert_type(&mut self, alert_type: impl Into<Option<AlertType>>) {
        self.alert_type = alert_type.into();
    }

    /// Set the tags of the event
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_tags(mut self, tags: impl Into<Option<Vec<MetaString>>>) -> Self {
        self.tags = tags.into();
        self
    }

    /// Set the tags of the event.
    pub fn set_tags(&mut self, tags: impl Into<Option<Vec<MetaString>>>) {
        self.tags = tags.into();
    }

    /// Creates an `EventD` from the given title and text.
    ///
    /// Defaults to an informational alert with normal priority.
    pub fn new(title: &str, text: &str) -> Self {
        Self {
            title: title.into(),
            text: text.into(),
            timestamp: None,
            hostname: MetaString::empty(),
            aggregation_key: MetaString::empty(),
            priority: Some(Priority::Normal),
            source_type_name: MetaString::empty(),
            alert_type: Some(AlertType::Info),
            tags: None,
        }
    }
}
