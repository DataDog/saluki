use std::{collections::HashMap, time::Duration};

use memory_accounting::allocator::{AllocationGroupRegistry, AllocationStatsSnapshot};
use saluki_app::prelude::{fatal_and_exit, initialize_logging};
use saluki_core::runtime::{ProcessShutdown, Supervisable, Supervisor, SupervisorFuture};
use saluki_error::GenericError;
use tokio::{pin, select};
use tracing::{error, info};

#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<std::alloc::System> =
    memory_accounting::allocator::TrackingAllocator::new(std::alloc::System);

#[tokio::main]
async fn main() -> Result<(), GenericError> {
    if let Err(e) = initialize_logging(None) {
        fatal_and_exit(format!("failed to initialize logging: {}", e));
        return Ok(());
    };

    std::thread::spawn(|| log_alloc_groups());

    let mut supervisor = Supervisor::new("topology")?;
    supervisor.add_worker(WithDelay::never("infinite"));
    supervisor.add_worker(WithDelay::panic("delayed-panic", Duration::from_secs(9)));

    let mut nested_supervisor = Supervisor::new("nested")?;
    nested_supervisor.add_worker(WithDelay::success("delayed-success", Duration::from_secs(6)));
    nested_supervisor.add_worker(WithDelay::failure("delayed-failure", "failed", Duration::from_secs(15)));
    supervisor.add_worker(nested_supervisor);

    let mut nested_alloc_supervisor = Supervisor::new("nested-alloc")?;
    nested_alloc_supervisor.add_worker(SlowAlloc);
    //nested_alloc_supervisor.add_worker(WithDelay::success("delayed-success", Duration::from_secs(6)));
    supervisor.add_worker(nested_alloc_supervisor);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    let sup_handle = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

    let shutdown_delay = Duration::from_secs(30);
    info!("Waiting {:?} before shutting down...", shutdown_delay);
    tokio::time::sleep(shutdown_delay).await;

    info!("Sending shutdown signal...");
    let _ = shutdown_tx.send(());

    info!("Waiting for supervisor to finish...");
    match sup_handle.await {
        Ok(Ok(())) => info!("Supervisor completed successfully."),
        Ok(Err(e)) => error!("Supervisor task failed: {}", e),
        Err(e) => error!("Supervisor task panicked: {}", e),
    }

    Ok(())
}

struct WithDelay {
    worker_name: &'static str,
    result: Result<(), &'static str>,
    panic: bool,
    delay: Duration,
}

impl WithDelay {
    fn success(worker_name: &'static str, delay: Duration) -> Self {
        Self {
            worker_name,
            result: Ok(()),
            panic: false,
            delay,
        }
    }

    fn failure(worker_name: &'static str, msg: &'static str, delay: Duration) -> Self {
        Self {
            worker_name,
            result: Err(msg),
            panic: false,
            delay,
        }
    }

    fn panic(worker_name: &'static str, delay: Duration) -> Self {
        Self {
            worker_name,
            result: Ok(()),
            panic: true,
            delay,
        }
    }

    fn never(worker_name: &'static str) -> Self {
        Self {
            worker_name,
            result: Ok(()),
            panic: false,
            delay: Duration::from_secs(0),
        }
    }
}

impl Supervisable for WithDelay {
    fn name(&self) -> &str {
        self.worker_name
    }

    fn initialize(&self, mut process_shutdown: ProcessShutdown) -> Option<SupervisorFuture> {
        let worker_name = self.worker_name;
        let delay = self.delay;
        let result = self.result;
        let panic = self.panic;
        Some(Box::pin(async move {
            info!(worker_name, "Worker started.");
            let work = if delay.is_zero() {
                tokio::time::sleep(Duration::MAX)
            } else {
                tokio::time::sleep(delay)
            };

            pin!(work);

            select! {
                _ = &mut work => {
                    info!(worker_name, "Worker delay passed.");
                    if panic {
                        panic!("panic boom boom boom");
                    }
                }
                _ = process_shutdown.wait_for_shutdown() => {
                    info!(worker_name, "Worker received shutdown signal.");
                }
            }

            result.map_err(GenericError::msg)
        }))
    }
}

struct SlowAlloc;

impl Supervisable for SlowAlloc {
    fn name(&self) -> &str {
        "slow_alloc"
    }

    fn initialize(&self, mut process_shutdown: ProcessShutdown) -> Option<SupervisorFuture> {
        let worker_name = self.name().to_string();
        Some(Box::pin(async move {
            info!(worker_name, "Worker started.");

            let mut items = Vec::new();
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            let shutdown = process_shutdown.wait_for_shutdown();
            pin!(shutdown);

            loop {
                select! {
                    _ = &mut shutdown => {
                        info!(worker_name, "Worker received shutdown signal.");
                        break;
                    }
                    _ = interval.tick() => {
                        items.push(String::from("hello, world! it's me!"));
                    }
                }
            }

            Ok(())
        }))
    }
}

fn log_alloc_groups() {
    loop {
        info!("Current allocation group stats:");
        let alloc_registry = AllocationGroupRegistry::global();
        alloc_registry.visit_allocation_groups(|group, stats| {
            let empty = AllocationStatsSnapshot::empty();
            let delta_snapshot = stats.snapshot_delta(&empty);
            info!(
                "  - {}: objects={} bytes={}",
                group,
                delta_snapshot.live_objects(),
                delta_snapshot.live_bytes()
            );
        });

        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}
