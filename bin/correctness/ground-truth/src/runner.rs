use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    task::{ready, Context, Poll},
    time::Duration,
};

use airlock::{
    config::{ADPConfig, DSDConfig, MetricsIntakeConfig, MillstoneConfig},
    driver::{Driver, DriverConfig, DriverDetails, ExitStatus},
};
use rand::{
    distributions::{Alphanumeric, DistString as _},
    thread_rng,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use stele::Metric;
use tokio::{select, task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, info_span, Instrument as _, Span};

use crate::{analysis::RawTestResults, config::Cli, sync::Coordinator};

pub struct TestRunner {
    adp_config: ADPConfig,
    dsd_config: DSDConfig,
    metrics_intake_config: MetricsIntakeConfig,
    millstone_config: MillstoneConfig,

    cancel_token: CancellationToken,
    dsd_coordinator: Coordinator,
    adp_coordinator: Coordinator,
    log_base_dir: PathBuf,
}

impl TestRunner {
    pub fn from_cli(cli: &Cli) -> Self {
        Self {
            adp_config: cli.adp_config(),
            dsd_config: cli.dsd_config(),
            metrics_intake_config: cli.metrics_intake_config(),
            millstone_config: cli.millstone_config(),
            cancel_token: CancellationToken::new(),
            dsd_coordinator: Coordinator::new(),
            adp_coordinator: Coordinator::new(),
            log_base_dir: PathBuf::from("/tmp/ground-truth"),
        }
    }

    async fn build_dsd_group_runner(&self, isolation_group_id: String) -> Result<GroupRunner, GenericError> {
        debug!("Creating DSD group runner...");

        let mut group_runner = GroupRunner::new(
            isolation_group_id,
            "dsd",
            self.log_base_dir.clone(),
            self.dsd_coordinator.clone(),
            self.cancel_token.child_token(),
        );

        let dogstatsd_config = DriverConfig::dogstatsd(self.dsd_config.clone())
            .await?
            // We _have_ to pass the API key as an environment variable, otherwise the container won't cleanly start.
            .with_env_var("DD_API_KEY", "dummy-api-key-correctness-testing");

        group_runner
            .with_driver(DriverConfig::metrics_intake(self.metrics_intake_config.clone()).await?)?
            .with_driver(dogstatsd_config)?
            .with_driver(DriverConfig::millstone(self.millstone_config.clone()).await?)?;

        Ok(group_runner)
    }

    async fn build_adp_group_runner(&self, isolation_group_id: String) -> Result<GroupRunner, GenericError> {
        debug!("Creating ADP group runner...");

        let mut group_runner = GroupRunner::new(
            isolation_group_id,
            "adp",
            self.log_base_dir.clone(),
            self.adp_coordinator.clone(),
            self.cancel_token.child_token(),
        );

        let adp_config = DriverConfig::agent_data_plane(self.adp_config.clone())
            .await?
            .with_env_var("DD_API_KEY", "dummy-api-key-correctness-testing")
            .with_env_var("DD_ADP_USE_NOOP_WORKLOAD_PROVIDER", "true")
            .with_env_var("DD_TELEMETRY_ENABLED", "true")
            .with_env_var("DD_PROMETHEUS_LISTEN_ADDR", "tcp://0.0.0.0:6000")
            .with_exposed_port("tcp", 6000);

        group_runner
            .with_driver(DriverConfig::metrics_intake(self.metrics_intake_config.clone()).await?)?
            .with_driver(adp_config)?
            .with_driver(DriverConfig::millstone(self.millstone_config.clone()).await?)?;

        Ok(group_runner)
    }

    async fn unwrap_or_shutdown<T>(
        &mut self, step_id: &'static str, dsd_result: Result<T, GenericError>, adp_result: Result<T, GenericError>,
    ) -> Result<(T, T), GenericError> {
        match (dsd_result, adp_result) {
            (Ok(dsd_result), Ok(adp_result)) => Ok((dsd_result, adp_result)),

            // If either side failed, we need to shutdown everything before returning the error.
            (maybe_dsd_error, maybe_adp_error) => {
                // Trigger any spawned drivers to shutdown and cleanup, and wait for that to happen.
                self.cancel_token.cancel();
                self.dsd_coordinator.wait().await;
                self.adp_coordinator.wait().await;

                // Figure out which side failed to initially spawn successfully, and return the appropriate
                // error. If both failed, then we log both errors and return a generic error instead.
                match (maybe_dsd_error, maybe_adp_error) {
                    (Ok(_), Err(adp_error)) => {
                        error!("DogStatsD group runner completed step '{}' successfully, but Agent Data Plane group runner encountered an error.", step_id);
                        Err(adp_error)
                    }
                    (Err(dsd_error), Ok(_)) => {
                        error!("Agent Data Plane group runner completed step '{}' successfully, but DogStatsD group runner encountered an error.", step_id);
                        Err(dsd_error)
                    }
                    (Err(dsd_error), Err(adp_error)) => {
                        error!(error = %dsd_error, "DogStatsD group runner encountered an error at step '{}'.", step_id);
                        error!(error = %adp_error, "Agent Data Plane group runner encountered an error at step '{}'.", step_id);
                        Err(generic_error!("Failed to complete step '{}'.", step_id))
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    pub async fn run(mut self) -> Result<RawTestResults, GenericError> {
        let dsd_isolation_group_id = generate_isolation_group_id();
        let adp_isolation_group_id = generate_isolation_group_id();

        let dsd_runner_span = info_span!(
            "runner",
            isolation_group_id = dsd_isolation_group_id.clone(),
            test_id = "dsd"
        );
        let adp_runner_span = info_span!(
            "runner",
            isolation_group_id = adp_isolation_group_id.clone(),
            test_id = "adp"
        );

        // Build a group runner for both DogStatsD and Agent Data Plane, which handles the complexities of spawning and
        // monitoring the containers, as well as cleaning them up after failure and/or when we're done.
        let dsd_group_runner = self.build_dsd_group_runner(dsd_isolation_group_id.clone()).await?;
        let adp_group_runner = self.build_adp_group_runner(adp_isolation_group_id.clone()).await?;

        // Do the initial spawn of both group runners, which should get all relevant containers spawned and running in
        // the correct order, and so on.
        info!("Spawning containers for DogStatsD and Agent Data Plane...");
        let dsd_spawn_result = run_in_background(&dsd_runner_span, dsd_group_runner.spawn());
        let adp_spawn_result = run_in_background(&adp_runner_span, adp_group_runner.spawn());

        // Everything is running, so just wait for the results (or an error) to come back from both group runners.
        let (dsd_result_collector, adp_result_collector) = self
            .unwrap_or_shutdown("spawn_containers", dsd_spawn_result.await, adp_spawn_result.await)
            .await?;
        info!("Containers spawned successfully. Waiting for results...");

        let maybe_dsd_results = run_in_background(&dsd_runner_span, dsd_result_collector.wait_for_results());
        let maybe_adp_results = run_in_background(&adp_runner_span, adp_result_collector.wait_for_results());

        let (dsd_results, adp_results) = self
            .unwrap_or_shutdown("collect_results", maybe_dsd_results.await, maybe_adp_results.await)
            .await?;

        // We've gotten our results back, so signal to any remaining containers that they can shutdown now.
        info!("Cleaning up remaining containers and resources...");
        self.cancel_token.cancel();
        self.dsd_coordinator.wait().await;
        self.adp_coordinator.wait().await;

        debug!("Cleaning up DogStatsD-related resources...");
        if let Err(e) = Driver::clean_related_resources(dsd_isolation_group_id).await {
            error!(error = %e, "Failed to clean up DogStatsD-related resources. Manual cleanup may be required.");
        }

        debug!("Cleaning up Agent Data Plane-related resources...");
        if let Err(e) = Driver::clean_related_resources(adp_isolation_group_id).await {
            error!(error = %e, "Failed to clean up Agent Data Plane-related resources. Manual cleanup may be required.");
        }

        info!("Cleanup complete.");

        Ok(RawTestResults::new(dsd_results, adp_results))
    }
}

fn run_in_background<F, T>(span: &Span, f: F) -> SpawnedResult<T>
where
    F: Future<Output = Result<T, GenericError>> + Send + 'static,
    T: Send + 'static,
{
    let handle = tokio::spawn(f.instrument(span.clone()));
    SpawnedResult { handle }
}

struct SpawnedResult<T> {
    handle: JoinHandle<Result<T, GenericError>>,
}

impl<T> Future for SpawnedResult<T>
where
    T: Send + 'static,
{
    type Output = Result<T, GenericError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `JoinHandle<T>` is `Unpin`, which means we're `Unpin`, so we can safely project the inner
        // `JoinHandle` field.
        let handle = unsafe { self.map_unchecked_mut(|sr| &mut sr.handle) };
        match ready!(handle.poll(cx)) {
            Ok(result) => Poll::Ready(result),
            Err(e) => Poll::Ready(Err(generic_error!("Task panicked during normal operation: {}", e))),
        }
    }
}

/// Manages the lifecycle of a group of containers related to a specific SUT.
struct GroupRunner {
    isolation_group_id: String,
    runner_log_dir: PathBuf,
    drivers: Vec<Driver>,
    coordinator: Coordinator,
    cancel_token: CancellationToken,
}

impl GroupRunner {
    fn new(
        isolation_group_id: String, test_id: &'static str, log_base_dir: PathBuf, coordinator: Coordinator,
        cancel_token: CancellationToken,
    ) -> Self {
        let runner_log_dir = log_base_dir.join(isolation_group_id.clone());

        info!(
            "Creating test group runner for test type '{}'. Logs will be saved to {}.",
            test_id,
            runner_log_dir.display()
        );

        Self {
            isolation_group_id,
            runner_log_dir,
            drivers: Vec::new(),
            coordinator,
            cancel_token,
        }
    }

    fn with_driver(&mut self, config: DriverConfig) -> Result<&mut Self, GenericError> {
        let driver =
            Driver::from_config(self.isolation_group_id.clone(), config)?.with_logging(self.runner_log_dir.clone());
        self.drivers.push(driver);
        Ok(self)
    }

    async fn spawn(mut self) -> Result<ResultCollector, GenericError> {
        let mut driver_handles = Vec::new();

        // Spawn all of our drivers, short-circuiting if any of them fail to start.
        //
        // We capture the error if any of them _do_ happen to fail, and return it only after we have attempted to clean
        // up any drivers that were successfully spawned.
        let mut maybe_spawn_error = None;
        for driver in self.drivers {
            let driver_token = self.cancel_token.child_token();
            match spawn_driver_with_details(driver, &mut self.coordinator, driver_token).await {
                Ok(handle) => {
                    driver_handles.push(handle);
                }
                Err(e) => {
                    maybe_spawn_error = Some(e);
                    break;
                }
            }
        }

        let mut spawned_drivers = SpawnedDrivers::new(self.coordinator, self.cancel_token);
        for driver_handle in driver_handles {
            spawned_drivers.add_driver_handle(driver_handle);
        }

        match maybe_spawn_error {
            Some(e) => {
                debug!("Encountered error while spawning drivers. Cleaning up any successfully spawned drivers...");

                // We encountered an error while spawning drivers, so we need to clean up any drivers that were successfully
                // spawned before returning the error. We simply use our existing `SpawnedDrivers` to carry that out.
                let driver_results = spawned_drivers.stop_and_wait().await;
                if !driver_results.all_succeeded() {
                    error!("Failed to stop spawned drivers cleanly after encountering error during spawn.");
                    for (driver_id, status) in driver_results.failures() {
                        error!(driver_id, %status, "Driver failed to stop cleanly.");
                    }
                }

                debug!("Successfully cleaned up any spawned drivers.");

                Err(e)
            }
            None => ResultCollector::new(spawned_drivers),
        }
    }
}

#[derive(Default)]
pub struct DriverResults {
    results: HashMap<&'static str, ExitStatus>,
}

impl DriverResults {
    pub fn add_driver_result(&mut self, driver_id: &'static str, exit_status: ExitStatus) {
        self.results.insert(driver_id, exit_status);
    }

    pub fn all_succeeded(&self) -> bool {
        self.results
            .iter()
            .all(|(_, status)| matches!(status, ExitStatus::Success))
    }

    pub fn failures(&self) -> impl Iterator<Item = (&'static str, &ExitStatus)> {
        self.results
            .iter()
            .filter(|(_, status)| !matches!(status, ExitStatus::Success))
            .map(|(id, status)| (*id, status))
    }
}

struct ResultCollector {
    millstone_handle: DriverHandle,
    metrics_intake_port: u16,
}

impl ResultCollector {
    fn new(mut spawned_drivers: SpawnedDrivers) -> Result<Self, GenericError> {
        let millstone_handle = spawned_drivers
            .take_driver_handle("millstone")
            .ok_or_else(|| generic_error!("Failed to get millstone driver handle."))?;
        let metrics_intake_port = spawned_drivers
            .get_driver_details("metrics-intake")
            .and_then(|details| details.try_get_exposed_port("tcp", 2049))
            .ok_or_else(|| generic_error!("Failed to get exposed port details for metrics-intake container"))?;

        Ok(Self {
            millstone_handle,
            metrics_intake_port,
        })
    }

    async fn wait_for_results(self) -> Result<Vec<Metric>, GenericError> {
        debug!("Waiting for millstone container to complete...");
        // Wait for millstone to complete, since that signals that all metrics have been _sent_ to the target.
        if let ExitStatus::Failed { code, error } = self.millstone_handle.wait().await {
            return Err(generic_error!("Failed to drive millstone to completion; process exited with non-zero exit code ({}). Error message: {}", code, error));
        }
        debug!(
            "Millstone container stopped successfully. Waiting for flush interval to elapse before dumping metrics..."
        );

        // Now we'll briefly wait (for the duration of an aggregation flush interval, plus a little extra) before dumping the metrics from
        // metrics-intake, to ensure everything from the target has been flushed out.
        sleep(Duration::from_secs(32)).await;

        let client = reqwest::Client::new();
        let metrics = client
            .get(format!("http://localhost:{}/metrics/dump", self.metrics_intake_port))
            .send()
            .await
            .error_context("Failed to call metrics dump endpoint on metrics-intake server.")?
            .json::<Vec<Metric>>()
            .await
            .error_context("Failed to decode dumped metrics from metrics-intake response.")?;

        debug!("Metrics dumped successfully.");

        Ok(metrics)
    }
}

struct SpawnedDrivers {
    coordinator: Coordinator,
    cancel_token: CancellationToken,
    handles: Vec<DriverHandle>,
}

impl SpawnedDrivers {
    fn new(coordinator: Coordinator, cancel_token: CancellationToken) -> Self {
        Self {
            coordinator,
            cancel_token,
            handles: Vec::new(),
        }
    }

    fn add_driver_handle(&mut self, driver_handle: DriverHandle) {
        self.handles.push(driver_handle);
    }

    fn get_driver_details(&self, driver_id: &'static str) -> Option<&DriverDetails> {
        self.handles
            .iter()
            .find(|handle| handle.driver_id == driver_id)
            .map(|handle| &handle.details)
    }

    fn take_driver_handle(&mut self, driver_id: &'static str) -> Option<DriverHandle> {
        let handle_idx = self.handles.iter().position(|handle| handle.driver_id == driver_id)?;

        Some(self.handles.remove(handle_idx))
    }

    async fn stop_and_wait(mut self) -> DriverResults {
        // Trigger all drivers to stop and wait for them to complete.
        self.cancel_token.cancel();
        debug!("Triggered shutdown signal. Waiting for drivers to stop...");

        self.coordinator.wait().await;

        // Go through and collect the return value of each driver's management task to see if they exited cleanly or not.
        let mut driver_results = DriverResults::default();

        for handle in self.handles {
            let driver_id = handle.driver_id;
            let exit_status = handle.wait().await;

            driver_results.add_driver_result(driver_id, exit_status);
        }

        driver_results
    }
}

struct DriverHandle {
    driver_id: &'static str,
    details: DriverDetails,
    handle: JoinHandle<ExitStatus>,
}

impl DriverHandle {
    async fn wait(self) -> ExitStatus {
        match self.handle.await {
            Ok(exit_status) => exit_status,
            Err(e) => {
                error!(error = %e, "Driver management task panicked during normal operation!");
                ExitStatus::Failed {
                    code: -1,
                    error: format!("driver management task panicked: {}", e),
                }
            }
        }
    }
}

async fn spawn_driver_with_details(
    mut driver: Driver, coordinator: &mut Coordinator, cancel_token: CancellationToken,
) -> Result<DriverHandle, GenericError> {
    let driver_id = driver.driver_id();
    debug!(driver_id, "Starting container...");

    // Start the container and wait for it to become healthy.
    let details = driver.start().await?;
    debug!(driver_id, "Container started. Waiting for it to become healthy...");

    driver.wait_for_container_healthy().await?;
    debug!(driver_id, "Container is healthy. Proceeding...");

    // Spawn a management task that will ensure the container runs until the shutdown signal is received, or the
    // container stops, whichever comes first.
    let task_token = coordinator.register();
    let handle = tokio::spawn(async move {
        // Re-bind our task handle here to ensure it moves and thus drops when the task completes.
        let _token = task_token;

        select! {
            result = driver.wait_for_container_exit() => {
                debug!(driver_id, "Container exited.");

                if let Err(e) = driver.cleanup().await {
                    error!(driver_id, error = %e, "Failed to cleanup container.");
                }

                debug!(driver_id, "Container cleanup complete.");

                match result {
                    Ok(exit_status) => exit_status,
                    Err(e) => ExitStatus::Failed { code: -1, error: format!("failed to wait for container exit: {}", e) },
                }
            },
            _ = cancel_token.cancelled() => {
                debug!(driver_id, "Container stopping due to shutdown signal.");
                if let Err(e) = driver.cleanup().await {
                    error!(driver_id, error = %e, "Failed to cleanup container.");
                }
                debug!(driver_id, "Container cleanup complete.");

                ExitStatus::Success
            },
        }
    }.in_current_span());

    Ok(DriverHandle {
        driver_id,
        details,
        handle,
    })
}

/// Generates a random 16-character alphanumeric string suitable for use as an isolation group ID.
fn generate_isolation_group_id() -> String {
    Alphanumeric.sample_string(&mut thread_rng(), 8)
}
