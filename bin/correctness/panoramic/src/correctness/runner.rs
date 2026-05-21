use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    task::{ready, Context, Poll},
    time::{Duration, Instant},
};

use airlock::{
    config::{DatadogIntakeConfig, MillstoneConfig},
    driver::{Driver, DriverConfig, DriverDetails, ExitStatus},
};
use rand::{distr::SampleString as _, rng};
use rand_distr::Alphanumeric;
use saluki_error::{generic_error, GenericError};
use tokio::{select, task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, info_span, Instrument as _, Span};

use crate::correctness::{
    analysis::{AnalysisMode, AnalysisRunner, CollectedData, TracesAnalysisOptions},
    config::{Config, Runtime},
    sync::Coordinator,
};
use crate::{
    assertions::AssertionResult,
    reporter::{PhaseTiming, TestResult},
    test::TestContext,
};

/// Path where the original millstone config is mounted inside the shared millstone container.
///
/// This mirrors the path used by the single-millstone setup so the same bind-mount machinery works.
const MILLSTONE_CONFIG_INTERNAL: &str = "/etc/millstone/config.toml";

/// How long to wait after millstone exits before querying datadog-intake for data.
///
/// This gives the agents time to flush any remaining aggregated metrics after millstone stops
/// sending. The value is slightly longer than a full aggregation bucket width.
const FLUSH_WAIT: Duration = Duration::from_secs(32);

/// Run a single correctness test and return a panoramic `TestResult`.
pub async fn run_correctness_test(name: String, config: Config, tctx: TestContext) -> TestResult {
    match config.runtime {
        Runtime::Docker => run_docker_correctness_test(name, config, tctx).await,
        Runtime::KubernetesInDocker => crate::correctness::k8s::run_k8s_correctness_test(name, config, tctx).await,
    }
}

async fn run_docker_correctness_test(name: String, config: Config, tctx: TestContext) -> TestResult {
    let started = Instant::now();

    // Phase 1: spawn containers
    let spawn_start = Instant::now();
    let test_runner = match CorrectnessRunner::from_config(&config, tctx).await {
        Ok(r) => r,
        Err(e) => return make_error_result(name, started, "spawn_containers", e),
    };
    let spawn_duration = spawn_start.elapsed();

    // Phase 2: collect data
    let collect_start = Instant::now();
    let (baseline_data, comparison_data) = match test_runner.run().await {
        Ok(data) => data,
        Err(e) => return make_error_result(name, started, "collect_data", e),
    };
    let collect_duration = collect_start.elapsed();

    // Phase 3: analysis
    let analysis_start = Instant::now();
    let traces_options = match config.analysis_mode {
        AnalysisMode::Traces => Some(TracesAnalysisOptions {
            otlp_direct_analysis_mode: config.otlp_direct_analysis_mode,
            additional_span_ignore_fields: config.additional_span_ignore_fields.clone(),
        }),
        AnalysisMode::Events | AnalysisMode::Metrics | AnalysisMode::ServiceChecks => None,
    };
    let analysis_runner = AnalysisRunner::new(config.analysis_mode, baseline_data, comparison_data, traces_options);
    let analysis_runner =
        analysis_runner
            .with_dogstatsd_forwarding_requirement(config.require_dogstatsd_forwarded_packets)
            .with_dogstatsd_forwarding_batch_requirement(config.require_dogstatsd_forwarded_packet_batches)
            .with_dogstatsd_forwarding_comparison_mode(config.dogstatsd_forwarding_comparison_mode);
    let analysis_result = analysis_runner.run_analysis();
    let analysis_duration = analysis_start.elapsed();

    let total_duration = started.elapsed();

    let phase_timings = vec![
        PhaseTiming {
            phase: "spawn_containers".to_string(),
            duration: spawn_duration,
        },
        PhaseTiming {
            phase: "collect_data".to_string(),
            duration: collect_duration,
        },
        PhaseTiming {
            phase: "analysis".to_string(),
            duration: analysis_duration,
        },
    ];

    match analysis_result {
        Ok(()) => TestResult {
            name,
            passed: true,
            duration: total_duration,
            assertion_results: vec![AssertionResult {
                name: "telemetry matches".to_string(),
                passed: true,
                message: "No difference detected between baseline and comparison.".to_string(),
                duration: analysis_duration,
            }],
            error: None,
            phase_timings,
            assertion_details: vec![],
        },
        Err((e, details)) => {
            let full_message = format!("{:?}", e);
            let summary = full_message.lines().next().unwrap_or(&full_message).to_string();
            TestResult {
                name,
                passed: false,
                duration: total_duration,
                assertion_results: vec![AssertionResult {
                    name: "telemetry matches".to_string(),
                    passed: false,
                    message: full_message,
                    duration: analysis_duration,
                }],
                error: Some(summary),
                phase_timings,
                assertion_details: vec![details],
            }
        }
    }
}

/// Cleans up Docker volumes and networks for all three isolation groups.
///
/// Containers are already removed by the coordinator waits on every exit path; this handles the
/// volumes and networks that `Driver::cleanup` doesn't remove. Safe to call even if a group was
/// never fully started: Docker returns 404 for unknown resources and we log and continue.
async fn cleanup_groups(baseline_id: &str, comparison_id: &str, millstone_id: &str) {
    for id in [baseline_id, comparison_id, millstone_id] {
        if let Err(e) = Driver::clean_related_resources(id.to_string()).await {
            error!(error = %e, "Failed to clean up isolation group '{}'. Manual cleanup may be required.", id);
        }
    }
}

pub(crate) fn make_error_result(name: String, started: Instant, phase: &str, e: GenericError) -> TestResult {
    TestResult {
        name,
        passed: false,
        duration: started.elapsed(),
        assertion_results: vec![],
        error: Some(format!("{:?}", e)),
        phase_timings: vec![PhaseTiming {
            phase: phase.to_string(),
            duration: started.elapsed(),
        }],
        assertion_details: vec![],
    }
}

/// Manages the state and program flow of running a *correctness* test.
///
/// In a correctness test, two isolated groups of containers are created. One containing the Agent
/// alone (baseline), and the other containing ADP and the Agent working together (comparison). A
/// single shared millstone container runs two parallel millstone processes—one targeting each
/// agent—to ensure both agents receive bitwise-identical inputs from the same seed.
pub struct CorrectnessRunner {
    datadog_intake_config: DatadogIntakeConfig,
    millstone_config: MillstoneConfig,
    baseline_target_driver_config: DriverConfig,
    comparison_target_driver_config: DriverConfig,
    tctx: TestContext,
    baseline_coordinator: Coordinator,
    comparison_coordinator: Coordinator,
    millstone_coordinator: Coordinator,
}

impl CorrectnessRunner {
    pub async fn from_config(config: &Config, tctx: TestContext) -> Result<Self, GenericError> {
        let baseline =
            crate::mounts::apply_target_mounts(config.baseline_target_driver_config().await?, tctx.mounts_dir())?;
        let comparison =
            crate::mounts::apply_target_mounts(config.comparison_target_driver_config().await?, tctx.mounts_dir())?;
        Ok(Self {
            datadog_intake_config: config.datadog_intake_config(),
            millstone_config: config.millstone_config(),
            baseline_target_driver_config: baseline,
            comparison_target_driver_config: comparison,
            tctx,
            baseline_coordinator: Coordinator::new(),
            comparison_coordinator: Coordinator::new(),
            millstone_coordinator: Coordinator::new(),
        })
    }

    /// Builds the group runner for the baseline agent containers (datadog-intake + target).
    async fn build_baseline_group_runner(&self, isolation_group_id: String) -> Result<GroupRunner, GenericError> {
        debug!("Creating baseline group runner...");

        let mut group_runner = GroupRunner::new(
            isolation_group_id,
            "baseline",
            self.tctx.log_dir().to_path_buf(),
            self.baseline_coordinator.clone(),
            // Pass a child from the test context so that a cancellation from above will affect the group runners.
            self.tctx.test_cancel_token().child_token(),
        );
        group_runner
            .with_driver(DriverConfig::datadog_intake(self.datadog_intake_config.clone()).await?)?
            // Give the agent a "baseline" alias on its network so the shared millstone can
            // address it unambiguously when connected to both agent networks.
            .with_driver(
                self.baseline_target_driver_config
                    .clone()
                    .with_network_alias("baseline"),
            )?;

        Ok(group_runner)
    }

    /// Builds the group runner for the comparison agent containers (datadog-intake + target).
    async fn build_comparison_group_runner(&self, isolation_group_id: String) -> Result<GroupRunner, GenericError> {
        debug!("Creating comparison group runner...");

        let mut group_runner = GroupRunner::new(
            isolation_group_id,
            "comparison",
            self.tctx.log_dir().to_path_buf(),
            self.comparison_coordinator.clone(),
            // Pass a child from the test context so that a cancellation from above will affect the group runners.
            self.tctx.test_cancel_token().child_token(),
        );

        group_runner
            .with_driver(DriverConfig::datadog_intake(self.datadog_intake_config.clone()).await?)?
            // Give the agent a "comparison" alias on its network so the shared millstone can
            // address it unambiguously when connected to both agent networks.
            .with_driver(
                self.comparison_target_driver_config
                    .clone()
                    .with_network_alias("comparison"),
            )?;

        Ok(group_runner)
    }

    /// Builds the group runner for the shared millstone container.
    ///
    /// The millstone container is set up identically to the original single-millstone setup:
    /// `millstone.yaml` is bind-mounted at the same path as before. The shell entrypoint then
    /// uses `sed` to derive two per-target configs in `/tmp`—one with `baseline` addresses and
    /// one with `comparison` addresses—before launching both millstone processes in parallel.
    ///
    /// Two `sed` substitutions cover all target types:
    /// - DSD socket targets: `/airlock/` → `/{group}-airlock/`
    /// - TCP/gRPC targets: `://target` → `://{group}`
    ///
    /// Both agents are healthy before this container starts, so no startup wait is needed.
    async fn build_shared_millstone_group_runner(
        &self, isolation_group_id: String, baseline_isolation_group_id: &str, comparison_isolation_group_id: &str,
    ) -> Result<GroupRunner, GenericError> {
        debug!("Creating shared millstone group runner...");

        let millstone_binary = self
            .millstone_config
            .binary_path
            .clone()
            .unwrap_or_else(|| "/usr/local/bin/millstone".to_string());

        // Substitute $GROUP in the bind-mounted config to produce two per-target configs, then
        // run both millstone processes in parallel. `exec sh -c '...'` ensures both seds complete
        // before either millstone process starts.
        let cmd = format!(
            "sed 's/\\$GROUP/baseline/g' {cfg} > /tmp/millstone-baseline.toml && \
             sed 's/\\$GROUP/comparison/g' {cfg} > /tmp/millstone-comparison.toml && \
             exec sh -c '{bin} /tmp/millstone-baseline.toml & P1=$!; \
                          {bin} /tmp/millstone-comparison.toml & P2=$!; \
                          wait $P1; R1=$?; wait $P2; R2=$?; exit $((R1 | R2))'",
            cfg = MILLSTONE_CONFIG_INTERNAL,
            bin = millstone_binary,
        );

        let driver_config = DriverConfig::from_image("millstone", self.millstone_config.image.clone())
            .with_entrypoint(vec!["/bin/sh".to_string(), "-c".to_string()])
            .with_command(vec![cmd])
            // Bind-mount the original millstone.yaml—same as the single-millstone setup.
            .with_bind_mount(&self.millstone_config.config_path, MILLSTONE_CONFIG_INTERNAL)
            // Mount both agent isolation-group volumes so millstone can reach their DSD sockets.
            .with_volume_mount(format!("airlock-{}", baseline_isolation_group_id), "/baseline-airlock")
            .with_volume_mount(
                format!("airlock-{}", comparison_isolation_group_id),
                "/comparison-airlock",
            )
            // Connect to both agent networks so millstone can resolve "baseline" and "comparison"
            // hostnames for TCP/gRPC targets.
            .with_network(format!("airlock-{}", baseline_isolation_group_id))
            .with_network(format!("airlock-{}", comparison_isolation_group_id));

        let mut group_runner = GroupRunner::new(
            isolation_group_id,
            "millstone",
            self.tctx.log_dir().to_path_buf(),
            self.millstone_coordinator.clone(),
            self.tctx.test_cancel_token().child_token(),
        );
        group_runner.with_driver(driver_config)?;

        Ok(group_runner)
    }

    async fn unwrap_or_shutdown<T>(
        &mut self, step_id: &'static str, baseline_result: Result<T, GenericError>,
        comparison_result: Result<T, GenericError>,
    ) -> Result<(T, T), GenericError> {
        match (baseline_result, comparison_result) {
            (Ok(baseline_result), Ok(comparison_result)) => Ok((baseline_result, comparison_result)),

            // If either side failed, we need to shutdown everything before returning the error.
            (maybe_baseline_error, maybe_comparison_error) => {
                // Trigger any spawned drivers to shutdown and cleanup, and wait for that to happen.
                self.tctx.test_cancel_token().cancel();
                self.baseline_coordinator.wait().await;
                self.comparison_coordinator.wait().await;
                self.millstone_coordinator.wait().await;

                // Figure out which side failed to initially spawn successfully, and return the appropriate
                // error. If both failed, then we log both errors and return a generic error instead.
                match (maybe_baseline_error, maybe_comparison_error) {
                    (Ok(_), Err(comparison_error)) => {
                        error!("Baseline group runner completed step '{}' successfully, but comparison group runner encountered an error.", step_id);
                        Err(comparison_error)
                    }
                    (Err(baseline_error), Ok(_)) => {
                        error!("Comparison group runner completed step '{}' successfully, but baseline group runner encountered an error.", step_id);
                        Err(baseline_error)
                    }
                    (Err(baseline_error), Err(comparison_error)) => {
                        error!(error = ?baseline_error, "Baseline group runner encountered an error at step '{}'.", step_id);
                        error!(error = ?comparison_error, "Comparison group runner encountered an error at step '{}'.", step_id);
                        Err(generic_error!("Failed to complete step '{}'.", step_id))
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    pub async fn run(mut self) -> Result<(CollectedData, CollectedData), GenericError> {
        let baseline_isolation_group_id = generate_isolation_group_id();
        let comparison_isolation_group_id = generate_isolation_group_id();
        let millstone_isolation_group_id = generate_isolation_group_id();

        let baseline_runner_span = info_span!(
            "runner",
            isolation_group_id = baseline_isolation_group_id.clone(),
            test_id = "baseline"
        );
        let comparison_runner_span = info_span!(
            "runner",
            isolation_group_id = comparison_isolation_group_id.clone(),
            test_id = "comparison"
        );
        let millstone_runner_span = info_span!(
            "runner",
            isolation_group_id = millstone_isolation_group_id.clone(),
            test_id = "millstone"
        );

        // Phase 1: Build all three group runners.
        let baseline_group_runner = self
            .build_baseline_group_runner(baseline_isolation_group_id.clone())
            .await?;
        let comparison_group_runner = self
            .build_comparison_group_runner(comparison_isolation_group_id.clone())
            .await?;
        let millstone_group_runner = self
            .build_shared_millstone_group_runner(
                millstone_isolation_group_id.clone(),
                &baseline_isolation_group_id,
                &comparison_isolation_group_id,
            )
            .await?;

        // Phase 3: Spawn both agent groups in parallel and wait for them to become healthy.
        //
        // The agent volumes and networks are created as part of this step. We must wait for the
        // agent groups before starting millstone so that the volumes exist when the millstone
        // container mounts them, and the networks exist when it connects to them.
        info!("Spawning containers for baseline and comparison targets...");
        let baseline_spawn_result = run_in_background(&baseline_runner_span, baseline_group_runner.spawn());
        let comparison_spawn_result = run_in_background(&comparison_runner_span, comparison_group_runner.spawn());

        let (baseline_collector, comparison_collector) = match self
            .unwrap_or_shutdown(
                "spawn_agent_containers",
                baseline_spawn_result.await,
                comparison_spawn_result.await,
            )
            .await
        {
            Ok(pair) => pair,
            Err(e) => {
                cleanup_groups(
                    &baseline_isolation_group_id,
                    &comparison_isolation_group_id,
                    &millstone_isolation_group_id,
                )
                .await;
                return Err(e);
            }
        };
        info!("Agent containers spawned successfully. Starting shared millstone...");

        // Phase 4: Spawn the shared millstone container now that both agent volumes exist.
        let millstone_waiter =
            match run_in_background(&millstone_runner_span, millstone_group_runner.spawn_millstone()).await {
                Ok(w) => w,
                Err(e) => {
                    self.tctx.test_cancel_token().cancel();
                    self.baseline_coordinator.wait().await;
                    self.comparison_coordinator.wait().await;
                    self.millstone_coordinator.wait().await;
                    cleanup_groups(
                        &baseline_isolation_group_id,
                        &comparison_isolation_group_id,
                        &millstone_isolation_group_id,
                    )
                    .await;
                    return Err(generic_error!("Failed to spawn shared millstone container: {}", e));
                }
            };
        info!("Shared millstone started. Waiting for it to complete...");

        // Phase 5: Wait for millstone to finish both parallel runs.
        if let ExitStatus::Failed { code, error } = millstone_waiter.millstone_handle.wait().await {
            self.tctx.test_cancel_token().cancel();
            self.baseline_coordinator.wait().await;
            self.comparison_coordinator.wait().await;
            self.millstone_coordinator.wait().await;
            cleanup_groups(
                &baseline_isolation_group_id,
                &comparison_isolation_group_id,
                &millstone_isolation_group_id,
            )
            .await;
            return Err(generic_error!(
                "Shared millstone exited with non-zero exit code ({}). Error: {}",
                code,
                error
            ));
        }
        debug!(
            "Shared millstone completed. Waiting {:?} for agents to flush...",
            FLUSH_WAIT
        );

        // Phase 6: Give agents time to flush all remaining aggregated metrics.
        //
        // TODO: This should maybe be configurable, or perhaps we can figure out a better way to
        // determine when the next flush has happened... and further, we might not need to care
        // about this for particular analysis modes if the functionality we're testing doesn't rely
        // on flushing like metrics does.
        sleep(FLUSH_WAIT).await;

        // Phase 7: Collect data from both datadog-intake containers, then shut everything down.
        info!("Collecting data from baseline and comparison intake containers...");
        let maybe_baseline_data = run_in_background(
            &baseline_runner_span,
            CollectedData::for_port(baseline_collector.datadog_intake_port),
        );
        let maybe_comparison_data = run_in_background(
            &comparison_runner_span,
            CollectedData::for_port(comparison_collector.datadog_intake_port),
        );

        let (baseline_data, comparison_data) = match self
            .unwrap_or_shutdown("collect_data", maybe_baseline_data.await, maybe_comparison_data.await)
            .await
        {
            Ok(pair) => pair,
            Err(e) => {
                cleanup_groups(
                    &baseline_isolation_group_id,
                    &comparison_isolation_group_id,
                    &millstone_isolation_group_id,
                )
                .await;
                return Err(e);
            }
        };

        // Signal all remaining containers to shut down and wait for them.
        info!("Cleaning up remaining containers and resources...");
        // TODO: is it correct to be using tctx.test_cancel_token for this? Or is this cancel() call necessary?
        self.tctx.test_cancel_token().cancel();
        self.baseline_coordinator.wait().await;
        self.comparison_coordinator.wait().await;
        self.millstone_coordinator.wait().await;

        cleanup_groups(
            &baseline_isolation_group_id,
            &comparison_isolation_group_id,
            &millstone_isolation_group_id,
        )
        .await;

        info!("Cleanup complete.");

        Ok((baseline_data, comparison_data))
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
        isolation_group_id: String, group_name: &'static str, log_dir: PathBuf, coordinator: Coordinator,
        cancel_token: CancellationToken,
    ) -> Self {
        // Create a subdirectory for the isolation group's container logs.
        let group_log_dir = log_dir.join(group_name);

        info!(
            "Creating test group runner for target '{}'. Logs will be saved to {}",
            group_name,
            group_log_dir.display()
        );

        Self {
            isolation_group_id,
            runner_log_dir: group_log_dir,
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

    async fn spawn(self) -> Result<AgentGroupCollector, GenericError> {
        AgentGroupCollector::new(self.spawn_into().await?)
    }

    /// Spawns all drivers in this group and returns the resulting [`SpawnedDrivers`].
    ///
    /// If any driver fails to start, any already-started drivers are cleaned up and the error is
    /// returned. This is the shared core of [`spawn`][Self::spawn] and
    /// [`spawn_millstone`][Self::spawn_millstone].
    async fn spawn_into(mut self) -> Result<SpawnedDrivers, GenericError> {
        let mut driver_handles = Vec::new();

        // Spawn all of our drivers, short-circuiting if any of them fail to start.
        //
        // We capture the error if any of them _do_ happen to fail, and return it only after we
        // have attempted to clean up any drivers that were successfully spawned.
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
            None => Ok(spawned_drivers),
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

/// Holds the intake port for one agent group after all containers have been spawned.
///
/// The millstone waiter is managed separately; this type is only responsible for tracking the
/// port needed to collect data from `datadog-intake`.
struct AgentGroupCollector {
    datadog_intake_port: u16,
}

impl AgentGroupCollector {
    fn new(spawned_drivers: SpawnedDrivers) -> Result<Self, GenericError> {
        let datadog_intake_port = spawned_drivers
            .get_driver_details("datadog-intake")
            .and_then(|details| details.try_get_exposed_port("tcp", 2049))
            .ok_or_else(|| generic_error!("Failed to get exposed port details for datadog-intake container."))?;

        Ok(Self { datadog_intake_port })
    }
}

/// Holds the millstone driver handle for the shared millstone group.
///
/// The caller must wait on `millstone_handle` to detect when millstone has finished sending
/// traffic to both agents.
struct MillstoneWaiter {
    millstone_handle: DriverHandle,
}

impl MillstoneWaiter {
    fn new(mut spawned_drivers: SpawnedDrivers) -> Result<Self, GenericError> {
        let millstone_handle = spawned_drivers
            .take_driver_handle("millstone")
            .ok_or_else(|| generic_error!("Failed to get millstone driver handle from shared millstone group."))?;

        Ok(Self { millstone_handle })
    }
}

impl GroupRunner {
    async fn spawn_millstone(self) -> Result<MillstoneWaiter, GenericError> {
        MillstoneWaiter::new(self.spawn_into().await?)
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

/// Generates a random 8-character alphanumeric string suitable for use as an isolation group ID.
fn generate_isolation_group_id() -> String {
    Alphanumeric.sample_string(&mut rng(), 8)
}
