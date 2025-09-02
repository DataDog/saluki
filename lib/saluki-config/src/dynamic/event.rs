//! Defines the event type for configuration changes.

use serde_json::Value as JsonValue;

/// An event that occurs when the configuration changes.
#[derive(Debug, Clone)]
pub enum ConfigChangeEvent {
    /// A configuration key was added.
    Added {
        /// The key that was added.
        key: String,
        /// The new value.
        value: JsonValue,
    },
    /// A configuration key was modified.
    Modified {
        /// The key that was updated.
        key: String,
        /// The old value.
        old_value: JsonValue,
        /// The new value.
        new_value: JsonValue,
    },
}
