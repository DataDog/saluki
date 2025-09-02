//! Dynamic configuration.
mod diff;
mod event;
mod handler;
mod provider;

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::{broadcast, Notify};

pub use self::diff::diff_config;
pub use self::event::ConfigChangeEvent;
pub use self::handler::{DynamicConfigurationHandler, DynamicConfigurationReceiver};
pub use self::provider::Provider;

/// Creates a new dynamic configuration channel.
///
/// This returns a sender and receiver pair that can be used to push and listen for dynamic configuration updates.
pub fn channel() -> (DynamicConfigurationHandler, DynamicConfigurationReceiver) {
    let values = Arc::new(ArcSwap::from_pointee(serde_json::Value::Null));
    let (sender, _) = broadcast::channel(100);
    let notifier = Arc::new(Notify::new());

    let handler = DynamicConfigurationHandler {
        values: values.clone(),
        notifier: notifier.clone(),
    };

    let receiver = DynamicConfigurationReceiver {
        values,
        sender,
        notifier,
    };

    (handler, receiver)
}
