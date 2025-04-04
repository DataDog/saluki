use std::time::Duration;

use async_trait::async_trait;
use saluki_error::GenericError;
use tokio::{sync::mpsc, time::sleep};
use tracing::{debug, error};

use super::metadata::MetadataOperation;

#[cfg(target_os = "linux")]
mod cgroups;
#[cfg(target_os = "linux")]
pub use self::cgroups::CgroupsMetadataCollector;

mod containerd;
pub use self::containerd::ContainerdMetadataCollector;

mod remote_agent;
pub use self::remote_agent::{RemoteAgentTaggerMetadataCollector, RemoteAgentWorkloadMetadataCollector};

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

/// A worker that runs a metadata collector.
pub struct MetadataCollectorWorker {
    collector: Box<dyn MetadataCollector + Send>,
}

impl MetadataCollectorWorker {
    /// Create a new `MetadataCollectorWorker` based on the given `collector`.
    pub fn new<MC>(collector: MC) -> Self
    where
        MC: MetadataCollector + Send + 'static,
    {
        Self {
            collector: Box::new(collector),
        }
    }

    /// Runs the collector, watching for metadata changes.
    ///
    /// This method will run indefinitely, watching for metadata changes and sending them to the given `operations_tx`. If
    /// an error is encountered during the call to [`MetadataCollector::watch`], it will be logged. Watching is always
    /// retried regardless of the return value.
    pub async fn run(mut self, mut operations_tx: mpsc::Sender<MetadataOperation>) {
        debug!(
            collector_name = self.collector.name(),
            "Starting metadata collector worker."
        );

        // We do this so that if the watch call happens to return in a retriable way (like if the stream ends
        // prematurely but without a true _error_, so `Ok(())` is returned), we can just start watching again.
        loop {
            if let Err(e) = self.collector.watch(&mut operations_tx).await {
                error!(error = %e, collector_name = self.collector.name(), "Failed to collect metadata. Sleeping for 5 seconds before retrying...");

                sleep(Duration::from_secs(5)).await;
            }
        }
    }
}
