//! Event types
mod priority;
pub use self::priority::*;

mod alert;
pub use self::alert::*;

/// EventD is an object that can be posted to the DataDog event stream.
#[derive(Clone, Debug)]
pub struct EventD {
    title: String,
    text: String,
    timestamp: u64,
    host: String,
    aggregation_key: String,
    priority: EventPriority,
    source_type_name: String,
    alert_type: EventAlertType,
    tags: Vec<String>,
}

impl EventD {
    /// Gets a reference to the title.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Gets a reference to the text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Gets a reference to the host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Gets a reference to the aggregation key.
    pub fn aggregation_key(&self) -> &str {
        &self.aggregation_key
    }

    /// Gets a reference to the priority.
    pub fn priority(&self) -> &EventPriority {
        &self.priority
    }

    /// Gets a reference to the source type name.
    pub fn source_type_name(&self) -> &str {
        &self.source_type_name
    }

    /// Gets a reference to the alert type
    pub fn alert_type(&self) -> &EventAlertType {
        &self.alert_type
    }

    /// Gets a reference to the timestamp.
    pub fn timestamp(&self) -> &u64 {
        &self.timestamp
    }

    /// Gets a reference to the tags.
    pub fn tags(&self) -> &Vec<String> {
        &self.tags
    }
}
