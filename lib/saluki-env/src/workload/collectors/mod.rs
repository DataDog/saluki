//! Workload metadata collection.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use saluki_core::runtime::{InitializationError, ProcessShutdown, Supervisable, SupervisorFuture};
use saluki_error::GenericError;
use tokio::{
    pin, select,
    sync::{mpsc, Mutex},
    time::sleep,
};
use tracing::{debug, error};

use super::metadata::MetadataOperation;

#[cfg(target_os = "linux")]
mod cgroups;
#[cfg(target_os = "linux")]
pub use self::cgroups::CgroupsMetadataCollector;

mod containerd;
pub use self::containerd::ContainerdMetadataCollector;

/// A metadata collector.
///
/// Metadata collectors are responsible for collecting metadata from the environment, both at startup and over time as
/// changes to the workload occur. This metadata can represent many things, from basic key/value pairs about specific
/// entities, to fixed relationships between entities, to more dynamic information like the current state of a workload.
#[async_trait]
pub trait MetadataCollector {
    /// Get the name of this collector.
    fn name(&self) -> &'static str;

    /// Watch for metadata changes.
    async fn watch(&mut self, operations_tx: &mut mpsc::Sender<MetadataOperation>) -> Result<(), GenericError>;
}

/// A supervised worker that drives a [`MetadataCollector`].
///
/// The worker holds the collector and its dedicated [`MetadataOperation`] sender, and on initialization runs
/// `watch` in a loop. The loop retries internally on transient errors with a short backoff so that natural stream
/// cycling against the upstream API doesn't trip the parent supervisor's restart budget; the supervisor retains
/// responsibility for catching panics and restarting the worker as a whole.
pub struct MetadataCollectorWorker {
    name: &'static str,
    state: Arc<Mutex<MetadataCollectorState>>,
}

struct MetadataCollectorState {
    collector: Box<dyn MetadataCollector + Send>,
    operations_tx: mpsc::Sender<MetadataOperation>,
}

impl MetadataCollectorWorker {
    /// Create a new `MetadataCollectorWorker` from the given `collector` and operations sender.
    pub fn new<MC>(collector: MC, operations_tx: mpsc::Sender<MetadataOperation>) -> Self
    where
        MC: MetadataCollector + Send + 'static,
    {
        let name = collector.name();
        Self {
            name,
            state: Arc::new(Mutex::new(MetadataCollectorState {
                collector: Box::new(collector),
                operations_tx,
            })),
        }
    }
}

#[async_trait]
impl Supervisable for MetadataCollectorWorker {
    fn name(&self) -> &str {
        self.name
    }

    async fn initialize(&self, process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        let state = Arc::clone(&self.state);
        Ok(Box::pin(async move {
            let mut state_guard = state.lock_owned().await;
            run_collector(&mut state_guard, process_shutdown).await;
            Ok(())
        }))
    }
}

async fn run_collector(state: &mut MetadataCollectorState, mut process_shutdown: ProcessShutdown) {
    debug!(
        collector_name = state.collector.name(),
        "Starting metadata collector worker."
    );

    let shutdown = process_shutdown.wait_for_shutdown();
    pin!(shutdown);

    let MetadataCollectorState {
        collector,
        operations_tx,
    } = state;

    loop {
        select! {
            _ = &mut shutdown => break,
            result = collector.watch(operations_tx) => {
                if let Err(e) = result {
                    error!(
                        error = %e,
                        collector_name = collector.name(),
                        "Failed to collect metadata. Sleeping 2s before retrying...",
                    );

                    select! {
                        _ = &mut shutdown => break,
                        _ = sleep(Duration::from_secs(2)) => {},
                    }
                }
            }
        }
    }

    debug!(collector_name = collector.name(), "Metadata collector worker stopped.");
}
