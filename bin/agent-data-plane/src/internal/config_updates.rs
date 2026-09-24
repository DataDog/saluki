//! Supervised worker that keeps the configuration current.

use agent_data_plane_config_system::ConfigurationUpdates;
use async_trait::async_trait;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{InitializationError, Supervisable, SupervisorFuture};
use saluki_error::ErrorContext as _;
use tokio::select;

/// A worker that applies configuration updates from the Datadog Agent.
///
/// The worker runs until it is shut down, or until the Agent configuration stream closes, in which case it fails. Each
/// restart continues with the same stream, so a restart recovers from a panic while an update is applied. A restart
/// does not recover from a closed stream: the stream does not reopen, so the restarted worker fails again at once.
pub struct ConfigUpdatesWorker {
    updates: ConfigurationUpdates,
}

impl ConfigUpdatesWorker {
    /// Creates a new [`ConfigUpdatesWorker`] that applies the given updates.
    pub fn new(updates: ConfigurationUpdates) -> Self {
        Self { updates }
    }
}

#[async_trait]
impl Supervisable for ConfigUpdatesWorker {
    fn name(&self) -> &str {
        "config-updates"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let updates = self.updates.run();

        Ok(Box::pin(async move {
            select! {
                _ = process_shutdown => Ok(()),
                result = updates => result.error_context("Stopped applying configuration updates from the Datadog Agent."),
            }
        }))
    }
}
