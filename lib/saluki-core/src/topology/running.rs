use saluki_error::GenericError;
use tokio::task::JoinHandle;

use super::shutdown::ComponentShutdownCoordinator;

pub struct RunningTopology {
    shutdown_coordinator: ComponentShutdownCoordinator,
    source_handles: Vec<JoinHandle<Result<(), ()>>>,
    transform_handles: Vec<JoinHandle<Result<(), ()>>>,
    destination_handles: Vec<JoinHandle<Result<(), ()>>>,
}

impl RunningTopology {
    pub fn from_parts(
        shutdown_coordinator: ComponentShutdownCoordinator, source_handles: Vec<JoinHandle<Result<(), ()>>>,
        transform_handles: Vec<JoinHandle<Result<(), ()>>>, destination_handles: Vec<JoinHandle<Result<(), ()>>>,
    ) -> Self {
        Self {
            shutdown_coordinator,
            source_handles,
            transform_handles,
            destination_handles,
        }
    }

    pub async fn shutdown(self) -> Result<(), GenericError> {
        // Trigger shutdown of sources, which will then cascade to the downstream components connected to those sources,
        // eventually leading to all components shutting down.
        self.shutdown_coordinator.shutdown();

        for handle in self.source_handles {
            let _ = handle.await?;
        }

        for handle in self.transform_handles {
            let _ = handle.await?;
        }

        for handle in self.destination_handles {
            let _ = handle.await?;
        }

        Ok(())
    }
}
