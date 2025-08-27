//! Defines the event type for configuration changes.

use serde_json::Value as JsonValue;

/// An event that occurs when the configuration changes.
#[derive(Debug, Clone)]
pub enum ConfigChangeEvent {
    /// A snapshot of the configuration has been received.
    Snapshot,
    /// An update to the configuration has been received.
    Update {
        /// The key that was updated.
        key: String,
        /// The new value.
        value: JsonValue,
    },
}
