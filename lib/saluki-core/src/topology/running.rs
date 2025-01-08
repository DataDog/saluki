use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use saluki_error::{generic_error, GenericError};
use tokio::{
    pin,
    runtime::Runtime,
    select,
    task::{Id, JoinError, JoinSet},
    time::{interval, sleep},
};
use tracing::{debug, error, info, warn};

use super::{shutdown::ComponentShutdownCoordinator, ComponentId};

/// A running topology.
pub struct RunningTopology {
    thread_pool: Runtime,
    shutdown_coordinator: ComponentShutdownCoordinator,
    component_tasks: JoinSet<Result<(), GenericError>>,
    component_task_map: HashMap<Id, ComponentId>,
}

impl RunningTopology {
    /// Creates a new `RunningTopology`.
    pub(super) fn from_parts(
        thread_pool: Runtime, shutdown_coordinator: ComponentShutdownCoordinator,
        component_tasks: JoinSet<Result<(), GenericError>>, component_task_map: HashMap<Id, ComponentId>,
    ) -> Self {
        Self {
            thread_pool,
            shutdown_coordinator,
            component_tasks,
            component_task_map,
        }
    }

    /// Waits for any spawned component to unexpectedly finish.
    ///
    /// This can be used in a loop to determine if any component has unexpectedly finished before shutdown was
    /// initiated, which usually represents an error or bug with the component.
    pub async fn wait_for_unexpected_finish(&mut self) {
        let task_result = self
            .component_tasks
            .join_next_with_id()
            .await
            .expect("no components to wait for");

        // We call `handle_task_result` to log everything the same as when calling `shutdown`/`shutdown_with_timeout`,
        // with an "unexpected" flag to indicate that this should be considered an unexpected finish if it did happen to
        // finish "successfully", which adjusts the logging accordingly.
        handle_task_result(&mut self.component_task_map, task_result, true);
    }

    /// Triggers the topology to shutdown, waiting until all components have stopped.
    ///
    /// This will wait indefinitely for all components to stop. If graceful shutdown with an upper bound is desired, use
    /// [`shutdown_with_timeout`][Self::shutdown_with_timeout] instead.
    ///
    /// # Errors
    ///
    /// If the topology fails to shutdown cleanly due to an error in a component, an error will be returned.
    pub async fn shutdown(self) -> Result<(), GenericError> {
        self.shutdown_with_timeout(Duration::MAX).await
    }

    /// Triggers the topology to shutdown, waiting until all components have stopped or the timeout has elapsed.
    ///
    /// # Errors
    ///
    /// If the topology fails to shutdown cleanly, either due to an error in a component or the timeout being reached,
    /// an error will be returned.
    pub async fn shutdown_with_timeout(mut self, timeout: Duration) -> Result<(), GenericError> {
        let shutdown_deadline = Instant::now() + timeout;

        let shutdown_timeout = sleep(timeout);
        pin!(shutdown_timeout);

        let mut progress_interval = interval(Duration::from_secs(5));
        progress_interval.tick().await;

        // Trigger shutdown of sources, which will then cascade to the downstream components connected to those sources,
        // eventually leading to all components shutting down.
        self.shutdown_coordinator.shutdown();

        let mut stopped_cleanly = true;

        loop {
            select! {
                // A task finished.
                maybe_task_result = self.component_tasks.join_next_with_id() => match maybe_task_result {
                    None => {
                        info!("All components stopped.");
                        break;
                    },
                    Some(component_result) => if !handle_task_result(&mut self.component_task_map, component_result, false) {
                        stopped_cleanly = false;
                    },
                },

                // Emit some information about which components we're still waiting on to shutdown.
                _ = progress_interval.tick() => {
                    let mut remaining_components = self.component_task_map.values()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>();
                    remaining_components.sort();
                    let remaining_time = shutdown_deadline.saturating_duration_since(Instant::now());

                    info!("Waiting for the remaining component(s) to stop: {}. {} seconds remaining.", remaining_components.join(", "), remaining_time.as_secs_f64().round() as u64);
                },

                // Shutdown timeout was reached.
                _ = &mut shutdown_timeout => {
                    warn!("Forcefully stopping topology after shutdown grace period.");
                    stopped_cleanly = false;
                    break;
                },
            }
        }

        // Trigger the thread pool to shutdown, which will stop the runtime and all tasks.
        //
        // We do this without waiting.
        self.thread_pool.shutdown_background();

        if stopped_cleanly {
            Ok(())
        } else {
            Err(generic_error!("Topology failed to shutdown cleanly."))
        }
    }
}

/// Handles the result of a component task finishing, logging the result and removing the task from our map of running components.
///
/// If the task stopped successfully, `true` is returned.
fn handle_task_result(
    component_task_map: &mut HashMap<Id, ComponentId>, task_result: Result<(Id, Result<(), GenericError>), JoinError>,
    unexpected: bool,
) -> bool {
    let (task_id, stopped_successfully) = match task_result {
        Ok((id, component_result)) => {
            let component_id = component_task_map.get(&id).expect("component ID not found");
            match component_result {
                Ok(()) => {
                    if unexpected {
                        warn!(%component_id, "Component unexpectedly finished.");
                    } else {
                        debug!(%component_id, "Component stopped.");
                    }
                    (id, true)
                }
                Err(e) => {
                    error!(%component_id, error = %e, "Component stopped with error.");
                    (id, false)
                }
            }
        }
        Err(e) => {
            let id = e.id();
            let component_id = component_task_map.get(&id).expect("component ID not found");
            error!(%component_id, error = %e, "Component task failed unexpectedly.");
            (id, false)
        }
    };

    component_task_map.remove(&task_id);
    stopped_successfully
}
