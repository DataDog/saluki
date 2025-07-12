use std::time::Duration;

use saluki_app::prelude::{fatal_and_exit, initialize_logging};
use saluki_core::runtime::{ProcessShutdown, Supervisable, Supervisor, SupervisorFuture};
use saluki_error::GenericError;
use tokio::{pin, select};
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<(), GenericError> {
    if let Err(e) = initialize_logging(None) {
        fatal_and_exit(format!("failed to initialize logging: {}", e));
        return Ok(());
    };

    let mut supervisor = Supervisor::new("root")?;
    supervisor.add_worker(WithDelay::never("infinite"));
    supervisor.add_worker(WithDelay::panic("delayed-panic", Duration::from_secs(9)));

    let mut nested_supervisor = Supervisor::new("nested")?;
    nested_supervisor.add_worker(WithDelay::success("delayed-success", Duration::from_secs(6)));
    nested_supervisor.add_worker(WithDelay::failure("delayed-failure", "failed", Duration::from_secs(15)));
    supervisor.add_worker(nested_supervisor);

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
