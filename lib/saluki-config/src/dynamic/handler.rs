//! A handler for dynamic configuration.
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::{broadcast, Notify};

use super::event::ConfigChangeEvent;

/// A dynamic configuration handler.
#[derive(Clone, Debug)]
pub struct DynamicConfigurationHandler {
    /// The shared configuration values.
    pub values: Arc<ArcSwap<serde_json::Value>>,
    /// The broadcast sender for configuration change events.
    pub sender: broadcast::Sender<ConfigChangeEvent>,
    /// A notifier for waiting for dynamic configuration changes.
    pub notifier: Arc<Notify>,
}
