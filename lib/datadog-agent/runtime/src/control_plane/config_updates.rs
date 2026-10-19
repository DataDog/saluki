//! Supervised worker that keeps the configuration current.

use std::future::Future;

use async_trait::async_trait;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{InitializationError, Supervisable, SupervisorFuture};
use saluki_error::{ErrorContext as _, GenericError};
use tokio::select;

/// Applies the configuration updates that a remote agent's configuration system receives from the Datadog Agent.
pub trait ConfigUpdatesRunner: Send + Sync + 'static {
    /// Applies each update from the Datadog Agent until the configuration stream closes.
    ///
    /// Each call **MUST** continue from where the previous call stopped, so that the caller can run this again after a
    /// call fails or panics.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration stream closes. If the stream can't reopen, each later call **SHOULD**
    /// fail in the same way at once.
    fn run(&self) -> impl Future<Output = Result<(), GenericError>> + Send + 'static;
}

/// A worker that applies configuration updates from the Datadog Agent.
///
/// The worker runs until it is shut down, or until the Agent configuration stream closes, in which case it fails. Each
/// restart continues with the same stream, so a restart recovers from a panic while an update is applied. A restart
/// does not recover from a closed stream: the stream does not reopen, so the restarted worker fails again at once.
pub struct ConfigUpdatesWorker<U>
where
    U: ConfigUpdatesRunner,
{
    updates: U,
}

impl<U> ConfigUpdatesWorker<U>
where
    U: ConfigUpdatesRunner,
{
    /// Creates a new [`ConfigUpdatesWorker`] that applies the given updates.
    pub fn new(updates: U) -> Self {
        Self { updates }
    }
}

#[async_trait]
impl<U> Supervisable for ConfigUpdatesWorker<U>
where
    U: ConfigUpdatesRunner,
{
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
