//! Defines the event type for configuration changes.

use facet_value::Value;

/// An event that occurs when the configuration changes.
#[derive(Debug, Clone)]
pub struct ConfigChangeEvent {
    /// The key that changed.
    pub key: String,
    /// The previous value, if any.
    pub old_value: Option<Value>,
    /// The new value.
    pub new_value: Option<Value>,
}

/// An update message for the dynamic configuration state, sent from the config stream to the updater task.
#[derive(Clone, Debug)]
pub enum ConfigUpdate {
    /// A complete snapshot of the configuration.
    ///
    /// The existing state should be replaced.
    Snapshot(Value),
    /// A partial update for a single key-value pair.
    ///
    /// This should be merged into the existing state.
    Partial {
        /// The key to update.
        key: String,
        /// The new value.
        value: Value,
    },
}
