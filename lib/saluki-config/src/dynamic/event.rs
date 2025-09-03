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
