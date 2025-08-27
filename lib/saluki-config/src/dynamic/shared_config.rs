use std::sync::Arc;

use arc_swap::ArcSwap;
use serde_json::Value as JsonValue;
use tokio::sync::broadcast;

use super::ConfigChangeEvent;

/// A shared configuration object.
///
/// This object is used to share configuration values between threads. It is used to store the configuration values in
/// an atomic way, and to notify components when a configuration value changes.
#[derive(Clone, Debug)]
pub struct SharedConfig {
    /// The shared configuration values.
    pub values: Arc<ArcSwap<JsonValue>>,

    /// The broadcast sender for configuration change events.
    pub sender: broadcast::Sender<ConfigChangeEvent>,
}
