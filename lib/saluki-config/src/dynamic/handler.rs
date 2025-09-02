//! A handler for dynamic configuration.
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::{broadcast, Notify};

use super::event::ConfigChangeEvent;

/// The receiving side of a dynamic configuration channel.
///
/// This is used by the `ConfigurationLoader` to listen for updates from a dynamic source.
#[derive(Clone, Debug)]
pub struct DynamicConfigurationReceiver {
    /// The shared configuration values.
    pub values: Arc<ArcSwap<serde_json::Value>>,
    /// The broadcast sender for configuration change events.
    pub sender: broadcast::Sender<ConfigChangeEvent>,
    /// A notifier for waiting for dynamic configuration changes.
    pub notifier: Arc<Notify>,
}

impl DynamicConfigurationReceiver {
    /// Waits for a dynamic configuration update to be received.
    pub async fn wait_for_update(&self) {
        self.notifier.notified().await;
    }
}

/// The sending side of a dynamic configuration channel.
///
/// This is used by dynamic configuration sources to push new configuration states into the system.
#[derive(Clone, Debug)]
pub struct DynamicConfigurationHandler {
    /// The shared configuration values.
    pub values: Arc<ArcSwap<serde_json::Value>>,
    /// A notifier for pinging the dynamic configuration receiver.
    pub notifier: Arc<Notify>,
}

impl DynamicConfigurationHandler {
    /// Updates the dynamic configuration with a new value.
    pub fn update(&self, value: serde_json::Value) {
        self.values.store(Arc::new(value));
        self.notifier.notify_waiters();
    }
}
