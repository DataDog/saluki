//! Defines the event type for configuration changes.

use serde_json::Value as JsonValue;

/// An event that occurs when the configuration changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChangeEvent {
    /// The key that changed.
    pub key: String,
    /// The previous value, if any.
    pub old_value: Option<JsonValue>,
    /// The new value.
    pub new_value: Option<JsonValue>,
}

/// An update message for the dynamic configuration state, sent from the config stream to the updater task.
#[derive(Clone, Debug)]
pub enum ConfigUpdate {
    /// A complete snapshot of the configuration.
    ///
    /// The existing state should be replaced.
    Snapshot(serde_json::Value),
    /// A partial update for a single key-value pair.
    ///
    /// This should be merged into the existing state.
    Partial {
        /// The key to update.
        key: String,
        /// The new value.
        value: serde_json::Value,
    },
}
