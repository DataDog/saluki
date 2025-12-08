//! Single check autodiscovery provider.
//!
//! This module provides an autodiscovery provider that emits a single `CheckSchedule` event
//! for a pre-configured check.

use async_trait::async_trait;
use saluki_env::autodiscovery::{AutodiscoveryEvent, AutodiscoveryProvider, CheckConfig};
use tokio::sync::broadcast::{self, Receiver, Sender};

/// An autodiscovery provider that emits a single `CheckSchedule` event.
///
/// This provider is used by the check-runner to inject a single check configuration
/// into the checks source without requiring a full autodiscovery system.
#[derive(Clone)]
pub struct SingleCheckAutodiscoveryProvider {
    sender: Sender<AutodiscoveryEvent>,
    check_config: CheckConfig,
}

impl SingleCheckAutodiscoveryProvider {
    /// Creates a new `SingleCheckAutodiscoveryProvider` with the given check configuration.
    pub fn new(check_config: CheckConfig) -> Self {
        // Create a broadcast channel with capacity for the single event
        let (sender, _) = broadcast::channel(1);

        Self {
            sender,
            check_config,
        }
    }
}

#[async_trait]
impl AutodiscoveryProvider for SingleCheckAutodiscoveryProvider {
    async fn subscribe(&self) -> Option<Receiver<AutodiscoveryEvent>> {
        // Create the receiver first, then send the event
        // This ensures the event is received by the subscriber
        let receiver = self.sender.subscribe();

        // Send the CheckSchedule event
        let _ = self.sender.send(AutodiscoveryEvent::CheckSchedule {
            config: self.check_config.clone(),
        });

        Some(receiver)
    }
}
