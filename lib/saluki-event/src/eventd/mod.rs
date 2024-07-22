//! Events.

// TODO: Switch usages of `String` to `MetaString` since we should generally be able to intern these strings as they
// originate in in the DogStatsD codec, where interning is already taking place.

/// Alert type.
#[derive(Clone, Copy, Debug)]
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

/// Event priority.
#[derive(Clone, Copy, Debug)]
pub enum Priority {
    /// The event has normal priority.
    Normal,

    /// The event has low priority.
    Low,
}

/// EventD is an object that can be posted to the DataDog event stream.
#[derive(Clone, Debug)]
pub struct EventD {
    title: String,
    text: String,
    timestamp: u64,
    host: String,
    aggregation_key: String,
    priority: Priority,
    source_type_name: String,
    alert_type: AlertType,
    tags: Vec<String>,
}

impl EventD {
    /// Gets the title of the event.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Gets the text of the event.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Gets the host where the event originated from.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Gets the aggregation key of the event.
    pub fn aggregation_key(&self) -> &str {
        &self.aggregation_key
    }

    /// Gets the priority of the event.
    pub fn priority(&self) -> Priority {
        self.priority
    }

    /// Gets the source type name of the event.
    pub fn source_type_name(&self) -> &str {
        &self.source_type_name
    }

    /// Gets the alert type of the event.
    pub fn alert_type(&self) -> AlertType {
        self.alert_type
    }

    /// Gets the timestamp of the event.
    ///
    /// This is a Unix timestamp, or the number of seconds since the Unix epoch.
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Gets the tags associated with the event.
    pub fn tags(&self) -> &[String] {
        &self.tags
    }
}
