use std::time::Duration;

use saluki_core::runtime::{ProcessShutdown, Supervisable, Supervisor, SupervisorFuture};
use saluki_error::GenericError;
use tokio::{pin, select};
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<(), GenericError> {
    tracing_subscriber::fmt::fmt()
        .with_ansi(true)
        .compact()
        .with_max_level(tracing::Level::INFO)
        .init();

    // Create our parent supervisor, which acts as the entrypoint to the supervision tree.
    //
    // We add a dedicated worker to this supervisor, and then also add nested supervisors to approximate how other
    // subsystems/services would compose their supervision trees into the overall application's supervision tree.
    let mut supervisor = Supervisor::new("root-sup")?;
    supervisor.add_worker(MockWorker::never("root-long-running"));

    // Create some nested supervisors: an administrative API service, and a telemetry service.
    //
    // These supervisors will be configured to simulate periodic failures which the supervisor will attempt
    // to recover from.
    let mut admin_api_supervisor = Supervisor::new("admin-api-sup")?;
    admin_api_supervisor.add_worker(MockWorker::never("admin-api-tcp-listener"));
    admin_api_supervisor.add_worker(MockWorker::failure(
        "admin-api-conn-handler",
        "failed to return response",
        Duration::from_secs(7),
    ));
    supervisor.add_worker(admin_api_supervisor);

    let mut telemetry_supervisor = Supervisor::new("telemetry-sup")?;
    telemetry_supervisor.add_worker(MockWorker::panic("telemetry-collector", Duration::from_secs(3)));
    telemetry_supervisor.add_worker(MockWorker::never("telemetry-flusher").with_shutdown_delay(Duration::from_secs(2)));
    supervisor.add_worker(telemetry_supervisor);

    // Configure our shutdown handle -- which we'll manually signal after 30 seconds -- and spawn the parent supervisor.
    //
    // Based on the timing of the simulated failures, we should see a few things happen:
    //
    // - the admin API supervisor will periodically restart the failed connection handler worker
    // - the telemetry supervisor will periodically restart the panicked collector worker
    // - since the telemetry collector worker fails too often, the telemetry supervisor will eventually shutdown due to
    //   its restart strategy, which the parent supervisor will detect, causing it to restart the entire telemetry
    //   supervisor
    // - when the telemetry supervisor shuts down, we should see the flusher worker shutdown as well, since we
    //   shutdown all other workers on the telemetry supervisor (in an orderly fashion) before shutting down the
    //   supervisor itself
    // - since shutdown is orderly, we can notice a small delay when the supervisor shuts down due to having to wait
    //   for the flusher to complete (2 seconds)
    // - this will keep repeating for the telemetry supervisor because we only allow one restart every 5 seconds before
    //   triggering shutdown (the collector panics every 3 seconds)
    // - however, since telemetry supervisor is only restarted every 6 seconds (two failures -> 6 seconds of runtime),
    //   the parent supervisor doesn't reach _its_ limit of 1 restart every 5 seconds so the parent supervisor keeps
    //   restarting the telemetry supervisor
    // - we eventually hit 30 seconds of runtime, and then trigger shutdown which cascades from the parent supervisor
    //   to its workers, and so on, until shutdown is complete
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

struct MockWorker {
    worker_name: &'static str,
    result: Result<(), &'static str>,
    panic: bool,
    delay: Duration,
    shutdown_delay: Option<Duration>,
}

impl MockWorker {
    /// Creates a worker that returns a failure result after the given delay.
    fn failure(worker_name: &'static str, msg: &'static str, delay: Duration) -> Self {
        Self {
            worker_name,
            result: Err(msg),
            panic: false,
            delay,
            shutdown_delay: None,
        }
    }

    /// Creates a worker that panics after the given delay.
    fn panic(worker_name: &'static str, delay: Duration) -> Self {
        Self {
            worker_name,
            result: Ok(()),
            panic: true,
            delay,
            shutdown_delay: None,
        }
    }

    /// Creates a worker that never completes.
    fn never(worker_name: &'static str) -> Self {
        Self {
            worker_name,
            result: Ok(()),
            panic: false,
            delay: Duration::from_secs(0),
            shutdown_delay: None,
        }
    }

    /// Sets the shutdown delay for the worker.
    fn with_shutdown_delay(mut self, delay: Duration) -> Self {
        self.shutdown_delay = Some(delay);
        self
    }
}

impl Supervisable for MockWorker {
    fn name(&self) -> &str {
        self.worker_name
    }

    fn initialize(&self, mut process_shutdown: ProcessShutdown) -> Option<SupervisorFuture> {
        let worker_name = self.worker_name;
        let delay = self.delay;
        let result = self.result;
        let panic = self.panic;
        let shutdown_delay = self.shutdown_delay;

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
                        panic!("Worker hit unrecoverable error.");
                    }
                }
                _ = process_shutdown.wait_for_shutdown() => {
                    info!(worker_name, "Worker received shutdown signal.");
                }
            }

            if let Some(delay) = shutdown_delay {
                tokio::time::sleep(delay).await;
            }

            info!(worker_name, "Worker shutting down.");

            result.map_err(GenericError::msg)
        }))
    }
}
