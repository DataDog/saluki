use std::{
    future::Future,
    path::PathBuf,
    time::{Duration, Instant},
};

use airlock::{
    config::{DatadogIntakeConfig, MillstoneConfig},
    driver::{Driver, DriverConfig, DriverDetails, ExitStatus},
};
use saluki_error::{generic_error, GenericError};
use tokio::{select, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, info_span, warn, Instrument as _, Span};

use crate::correctness::{
    analysis::{AnalysisMode, AnalysisRunner, TracesAnalysisOptions},
    case::CorrectnessTestCase,
    config::Runtime,
};
use crate::{
    assertions::{AssertionResult, AssertionStatus},
    intake::{CollectedData, IntakeClient},
    ledger::ResourceLedger,
    reporter::{ErrorKind, PhaseTiming, TestResult},
    tasks::CaseTasks,
    test::TestContext,
    traffic::{self, Side},
};

/// Path where the original millstone config is mounted inside the shared millstone container.
///
/// This mirrors the path used by the single-millstone setup so the same bind-mount machinery works.
const MILLSTONE_CONFIG_INTERNAL: &str = "/etc/millstone/config.toml";
const MILLSTONE_BASELINE_COMPLETION_FILE: &str = "/tmp/millstone-baseline-complete";
const MILLSTONE_COMPARISON_COMPLETION_FILE: &str = "/tmp/millstone-comparison-complete";
const MILLSTONE_COMPLETION_FILE: &str = "/tmp/millstone-complete";

/// Where the test's traffic directory is mounted inside the shared millstone container.
const MILLSTONE_TRAFFIC_INTERNAL: &str = "/traffic";
const MILLSTONE_HEALTHCHECK_INTERVAL: Duration = Duration::from_secs(1);
const MILLSTONE_HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(1);
const MILLSTONE_HEALTHCHECK_RETRIES: i64 = 3;
const MILLSTONE_HEALTHCHECK_START_PERIOD: Duration = Duration::from_secs(900);
const MILLSTONE_HEALTHCHECK_START_INTERVAL: Duration = Duration::from_millis(250);

/// How long to wait after Millstone finishes sending before querying datadog-intake for data.
///
/// Millstone remains alive during this interval so the agents can resolve the sender's process identity while flushing
/// any remaining aggregated metrics. The value is slightly longer than a full aggregation bucket width.
const FLUSH_WAIT: Duration = Duration::from_secs(32);

/// Run a single correctness test and return a panoramic `TestResult`.
pub async fn run_correctness_test(case: CorrectnessTestCase, tctx: TestContext) -> TestResult {
    match case.config.runtime {
        Runtime::Docker => run_docker_correctness_test(case, tctx).await,
        Runtime::KubernetesInDocker => crate::correctness::k8s::run_k8s_correctness_test(case, tctx).await,
    }
}

async fn run_docker_correctness_test(case: CorrectnessTestCase, tctx: TestContext) -> TestResult {
    let name = case.name.clone();
    let started = Instant::now();

    let log_dir = tctx.log_dir().to_path_buf();

    // Phases are marked on the shared tracker so the runner can name the one a test was in when a
    // deadline fires, and so their timings survive a test that never returns.
    let phases = tctx.phases.clone();

    // Phase 1: spawn containers
    let phase = phases.enter("spawn_containers");
    let test_runner = match CorrectnessRunner::from_case(&case, tctx).await {
        Ok(r) => r,
        Err(e) => return make_error_result(name, started, phase.finish_and_collect(), e),
    };
    phase.finish();

    // Phase 2: collect data
    let phase = phases.enter("collect_data");
    let (baseline_data, comparison_data) = match test_runner.run().await {
        Ok(data) => data,
        Err(e) => return make_error_result(name, started, phase.finish_and_collect(), e),
    };
    phase.finish();

    // Persist what each side decoded before the analysis consumes it.
    let traffic_dir = traffic::traffic_dir(&log_dir);
    for (side, data) in [(Side::Baseline, &baseline_data), (Side::Comparison, &comparison_data)] {
        traffic::write_decoded(&traffic_dir, side, data);
    }

    // Phase 3: analysis
    let phase = phases.enter("analysis");
    let traces_options = match case.config.analysis_mode {
        AnalysisMode::Traces => Some(TracesAnalysisOptions {
            otlp_direct_analysis_mode: case.config.otlp_direct_analysis_mode,
            additional_span_ignore_fields: case.config.additional_span_ignore_fields.clone(),
        }),
        AnalysisMode::Events | AnalysisMode::Metrics | AnalysisMode::ServiceChecks => None,
    };
    let analysis_runner = AnalysisRunner::new(
        case.config.analysis_mode,
        baseline_data,
        comparison_data,
        traces_options,
    )
    .with_dogstatsd_forwarding_requirement(case.config.require_dogstatsd_forwarded_packets);
    let analysis_result = analysis_runner.run_analysis();
    let analysis_duration = phase.elapsed();
    let phase_timings = phase.finish_and_collect();

    let total_duration = started.elapsed();

    match analysis_result {
        Ok(()) => TestResult::from_assertions(
            name,
            total_duration,
            vec![AssertionResult {
                name: "telemetry matches".to_string(),
                status: AssertionStatus::Passed,
                message: "No difference detected between baseline and comparison.".to_string(),
                duration: analysis_duration,
            }],
            phase_timings,
        ),
        Err((e, details)) => {
            let full_message = format!("{:?}", e);
            TestResult::from_assertions(
                name,
                total_duration,
                vec![AssertionResult {
                    name: "telemetry matches".to_string(),
                    status: AssertionStatus::Failed,
                    message: full_message,
                    duration: analysis_duration,
                }],
                phase_timings,
            )
            .with_assertion_details(vec![details])
        }
    }
}

/// Builds a setup-error result carrying the timings of the phases the test finished before failing.
pub(crate) fn make_error_result(
    name: String, started: Instant, phase_timings: Vec<PhaseTiming>, e: GenericError,
) -> TestResult {
    TestResult::errored(
        name,
        ErrorKind::Setup,
        format!("{:?}", e),
        started.elapsed(),
        phase_timings,
    )
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
}

impl CorrectnessRunner {
    pub async fn from_case(case: &CorrectnessTestCase, tctx: TestContext) -> Result<Self, GenericError> {
        let baseline = crate::mounts::apply_target_mounts(
            case.target_driver_config(&case.config.baseline).await?,
            tctx.mounts_dir(),
        )?;
        let comparison = crate::mounts::apply_target_mounts(
            case.target_driver_config(&case.config.comparison).await?,
            tctx.mounts_dir(),
        )?;
        Ok(Self {
            datadog_intake_config: case.datadog_intake_config(),
            millstone_config: case.millstone_config(),
            baseline_target_driver_config: baseline,
            comparison_target_driver_config: comparison,
            tctx,
        })
    }

    /// Builds the group runner for the baseline agent containers (datadog-intake + target).
    async fn build_baseline_group_runner(&self, isolation_group_id: String) -> Result<GroupRunner, GenericError> {
        debug!("Creating baseline group runner...");

        let mut group_runner = GroupRunner::new(isolation_group_id, "baseline", &self.tctx);
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

        let mut group_runner = GroupRunner::new(isolation_group_id, "comparison", &self.tctx);

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

        // The capture lands in the test's log directory through a bind mount, so nothing has to be copied out of
        // the container afterwards.
        let host_traffic_dir = traffic::traffic_dir(self.tctx.log_dir());
        let capture_args = match std::fs::create_dir_all(&host_traffic_dir) {
            Ok(()) => format!(" --traffic-capture-dir {}", MILLSTONE_TRAFFIC_INTERNAL),
            Err(e) => {
                warn!(path = %host_traffic_dir.display(), error = %e, "Failed to create traffic capture directory. Continuing without an input capture.");
                String::new()
            }
        };

        let millstone_binary = self
            .millstone_config
            .binary_path
            .clone()
            .unwrap_or_else(|| "/usr/local/bin/millstone".to_string());

        // Substitute $GROUP in the bind-mounted config to produce two per-target configs, then run both Millstone
        // processes in parallel. Each process reports when it finishes sending and remains alive until the runner shuts
        // down the container.
        let cmd = format!(
            "sed 's/\\$GROUP/baseline/g' {cfg} > /tmp/millstone-baseline.toml || exit 1; \
             sed 's/\\$GROUP/comparison/g' {cfg} > /tmp/millstone-comparison.toml || exit 1; \
             {bin} /tmp/millstone-baseline.toml --completion-file {baseline_complete}{capture_args} & P1=$!; \
             {bin} /tmp/millstone-comparison.toml --completion-file {comparison_complete} & P2=$!; \
             trap 'trap - TERM INT; kill \"$P1\" \"$P2\" 2>/dev/null; \
                   wait \"$P1\" 2>/dev/null; wait \"$P2\" 2>/dev/null; exit 0' TERM INT; \
             while kill -0 \"$P1\" 2>/dev/null && kill -0 \"$P2\" 2>/dev/null; do \
                 if [ -f {baseline_complete} ] && [ -f {comparison_complete} ]; then \
                     touch {complete}; break; \
                 fi; \
                 sleep 0.1; \
             done; \
             while kill -0 \"$P1\" 2>/dev/null && kill -0 \"$P2\" 2>/dev/null; do sleep 1; done; \
             kill \"$P1\" \"$P2\" 2>/dev/null; \
             wait \"$P1\"; R1=$?; wait \"$P2\"; R2=$?; exit $((R1 | R2))",
            cfg = MILLSTONE_CONFIG_INTERNAL,
            bin = millstone_binary,
            baseline_complete = MILLSTONE_BASELINE_COMPLETION_FILE,
            comparison_complete = MILLSTONE_COMPARISON_COMPLETION_FILE,
            complete = MILLSTONE_COMPLETION_FILE,
            capture_args = capture_args,
        );

        let driver_config = DriverConfig::from_image("millstone", self.millstone_config.image.clone())
            .with_entrypoint(vec!["/bin/sh".to_string(), "-c".to_string()])
            .with_command(vec![cmd])
            .with_healthcheck(
                vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    format!("test -f {}", MILLSTONE_COMPLETION_FILE),
                ],
                MILLSTONE_HEALTHCHECK_INTERVAL,
                MILLSTONE_HEALTHCHECK_TIMEOUT,
                MILLSTONE_HEALTHCHECK_RETRIES,
                MILLSTONE_HEALTHCHECK_START_PERIOD,
                MILLSTONE_HEALTHCHECK_START_INTERVAL,
            )
            // Bind-mount the original millstone.yaml—same as the single-millstone setup.
            .with_bind_mount(&self.millstone_config.config_path, MILLSTONE_CONFIG_INTERNAL)
            // Only the baseline run captures its input: both runs send byte-identical payloads from the same seed.
            .with_bind_mount(&host_traffic_dir, MILLSTONE_TRAFFIC_INTERNAL)
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

        let mut group_runner = GroupRunner::new(isolation_group_id, "millstone", &self.tctx);
        group_runner.with_driver(driver_config)?;

        Ok(group_runner)
    }

    pub async fn run(self) -> Result<(CollectedData, CollectedData), GenericError> {
        // Register Millstone last so reclaim attempts its removal before the networks and volumes it uses.
        let baseline_isolation_group_id = self.tctx.ledger.isolation_group();
        let comparison_isolation_group_id = self.tctx.ledger.isolation_group();
        let millstone_isolation_group_id = self.tctx.ledger.isolation_group();

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
        let tasks = &self.tctx.tasks;
        let (baseline_collector, comparison_collector) = tokio::try_join!(
            run_in_background(tasks, &baseline_runner_span, baseline_group_runner.spawn()),
            run_in_background(tasks, &comparison_runner_span, comparison_group_runner.spawn()),
        )?;
        info!("Agent containers spawned successfully. Starting shared Millstone...");

        // Phase 4: Spawn the shared Millstone container and wait until both senders finish sending. The container
        // healthcheck reports completion while both sender processes remain alive.
        let millstone = run_in_background(tasks, &millstone_runner_span, millstone_group_runner.spawn_millstone())
            .await
            .map_err(|e| generic_error!("Failed to spawn shared millstone container: {}", e))?;
        debug!(
            "Millstone finished sending. Waiting {:?} for agents to flush...",
            FLUSH_WAIT
        );

        // Phase 5: Give agents time to flush all remaining aggregated metrics while Millstone remains alive.
        //
        // TODO: This should maybe be configurable, or perhaps we can figure out a better way to
        // determine when the next flush has happened... and further, we might not need to care
        // about this for particular analysis modes if the functionality we're testing doesn't rely
        // on flushing like metrics does.
        let millstone_exit = async {
            millstone
                .wait_for_container_exit()
                .await
                .unwrap_or_else(|e| ExitStatus::Failed {
                    code: -1,
                    error: format!("failed to wait for container exit: {}", e),
                })
        };
        wait_for_flush(&self.tctx.test_cancel_token(), millstone_exit).await?;

        // Phase 6: Collect data from both datadog-intake containers.
        info!("Collecting data from baseline and comparison intake containers...");
        let collect = |port| async move { CollectedData::collect(&IntakeClient::new(port)).await };
        tokio::try_join!(
            run_in_background(
                tasks,
                &baseline_runner_span,
                collect(baseline_collector.datadog_intake_port)
            ),
            run_in_background(
                tasks,
                &comparison_runner_span,
                collect(comparison_collector.datadog_intake_port)
            ),
        )
    }
}

/// Waits [`FLUSH_WAIT`] for the agents to flush, failing early on cancellation or if Millstone exits.
async fn wait_for_flush(
    cancel: &CancellationToken, millstone_exit: impl Future<Output = ExitStatus>,
) -> Result<(), GenericError> {
    select! {
        // Report cancellation even if Millstone exits in the same instant.
        biased;
        _ = cancel.cancelled() => Err(generic_error!("Cancelled while waiting {:?} for the agents to flush.", FLUSH_WAIT)),
        status = millstone_exit => match status {
            ExitStatus::Success => Err(generic_error!("Shared Millstone exited before telemetry collection completed.")),
            ExitStatus::Failed { code, error } => Err(generic_error!(
                "Shared Millstone exited before telemetry collection completed with code {}: {}",
                code,
                error
            )),
        },
        _ = sleep(FLUSH_WAIT) => Ok(()),
    }
}

/// Spawns `f` in `span` through the case's task owner and returns a future for its result.
fn run_in_background<F, T>(tasks: &CaseTasks, span: &Span, f: F) -> impl Future<Output = Result<T, GenericError>>
where
    F: Future<Output = Result<T, GenericError>> + Send + 'static,
    T: Send + 'static,
{
    let handle = tasks.spawn(f.instrument(span.clone()));
    async move {
        match handle.await {
            Ok(Some(result)) => result,
            Ok(None) => Err(generic_error!("Stopped before finishing: the test was cancelled.")),
            Err(e) => Err(generic_error!("Task panicked during normal operation: {}", e)),
        }
    }
}

/// Starts a group of containers; the runner reclaims their registered isolation group.
struct GroupRunner {
    isolation_group_id: String,
    runner_log_dir: PathBuf,
    drivers: Vec<Driver>,
    alpine_image: String,
    ledger: ResourceLedger,
    /// Shared with the task wrapper so interrupted creation marks the ledger before task shutdown.
    stop: CancellationToken,
}

impl GroupRunner {
    fn new(isolation_group_id: String, group_name: &'static str, tctx: &TestContext) -> Self {
        // Create a subdirectory for the isolation group's container logs.
        let group_log_dir = tctx.log_dir().join(group_name);

        info!(
            "Creating test group runner for target '{}'. Logs will be saved to {}",
            group_name,
            group_log_dir.display()
        );

        Self {
            isolation_group_id,
            runner_log_dir: group_log_dir,
            drivers: Vec::new(),
            alpine_image: tctx.settings.alpine_image.clone(),
            ledger: tctx.ledger.clone(),
            stop: tctx.tasks.stop.clone(),
        }
    }

    fn with_driver(&mut self, config: DriverConfig) -> Result<&mut Self, GenericError> {
        let config = config.with_alpine_image(self.alpine_image.clone());
        let driver =
            Driver::from_config(self.isolation_group_id.clone(), config)?.with_logging(self.runner_log_dir.clone());
        self.drivers.push(driver);
        Ok(self)
    }

    async fn spawn(self) -> Result<AgentGroupCollector, GenericError> {
        AgentGroupCollector::new(self.spawn_into().await?)
    }

    async fn spawn_millstone(self) -> Result<Driver, GenericError> {
        self.spawn_into()
            .await?
            .into_iter()
            .find(|spawned| spawned.driver.driver_id() == "millstone")
            .map(|spawned| spawned.driver)
            .ok_or_else(|| generic_error!("Failed to get millstone driver from shared millstone group."))
    }

    /// Starts each driver in this group in turn and waits for it to become healthy, stopping at the first failure.
    ///
    /// The runner reclaims the registered group, including resources from partial starts.
    async fn spawn_into(self) -> Result<Vec<SpawnedDriver>, GenericError> {
        let mut spawned = Vec::with_capacity(self.drivers.len());
        for mut driver in self.drivers {
            let driver_id = driver.driver_id();
            debug!(driver_id, "Starting container...");
            let details = self
                .ledger
                .start_container(&driver, &self.isolation_group_id, &self.stop)
                .await?;
            debug!(driver_id, "Container started. Waiting for it to become healthy...");

            driver.wait_for_container_healthy().await?;
            debug!(driver_id, "Container is healthy. Proceeding...");
            spawned.push(SpawnedDriver { driver, details });
        }
        Ok(spawned)
    }
}

/// A driver whose container is running and healthy.
struct SpawnedDriver {
    driver: Driver,
    details: DriverDetails,
}

/// Holds the intake port for one agent group after all containers have been spawned.
struct AgentGroupCollector {
    datadog_intake_port: u16,
}

impl AgentGroupCollector {
    fn new(spawned: Vec<SpawnedDriver>) -> Result<Self, GenericError> {
        let datadog_intake_port = spawned
            .iter()
            .find(|spawned| spawned.driver.driver_id() == "datadog-intake")
            .and_then(|spawned| spawned.details.try_get_exposed_port("tcp", 2049))
            .ok_or_else(|| generic_error!("Failed to get exposed port details for datadog-intake container."))?;

        Ok(Self { datadog_intake_port })
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::timeout;

    use super::*;
    use crate::intake::test_server::{install_crypto_provider, MockIntake};

    const BOUND: Duration = Duration::from_secs(5);

    /// Bounds paused-time tests while allowing [`FLUSH_WAIT`] to pass.
    const PAUSED_BOUND: Duration = Duration::from_secs(120);

    /// Runs [`wait_for_flush`] within the bound, returning its result and how long it took.
    async fn flush(
        cancel: &CancellationToken, millstone_exit: impl Future<Output = ExitStatus>,
    ) -> (Result<(), GenericError>, Duration) {
        let started = tokio::time::Instant::now();
        let flushed = timeout(PAUSED_BOUND, wait_for_flush(cancel, millstone_exit))
            .await
            .expect("the flush wait should finish within the bound");
        (flushed, started.elapsed())
    }

    async fn exits_after(after: Duration, status: ExitStatus) -> ExitStatus {
        sleep(after).await;
        status
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_flush_returns_when_the_test_is_cancelled() {
        let cancel = CancellationToken::new();
        let canceller = cancel.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs(1)).await;
            canceller.cancel();
        });

        let (flushed, elapsed) = flush(&cancel, std::future::pending()).await;

        let error = flushed.expect_err("a cancelled flush wait should fail").to_string();
        assert_eq!(
            error,
            format!("Cancelled while waiting {:?} for the agents to flush.", FLUSH_WAIT)
        );
        assert_eq!(elapsed, Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_flush_fails_when_millstone_exits() {
        let exit = exits_after(
            Duration::from_secs(1),
            ExitStatus::Failed {
                code: 3,
                error: "boom".to_string(),
            },
        );

        let (flushed, elapsed) = flush(&CancellationToken::new(), exit).await;

        let error = flushed
            .expect_err("a Millstone exit should fail the flush wait")
            .to_string();
        assert_eq!(
            error,
            "Shared Millstone exited before telemetry collection completed with code 3: boom"
        );
        assert_eq!(elapsed, Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_flush_waits_out_the_flush() {
        let (flushed, elapsed) = flush(&CancellationToken::new(), std::future::pending()).await;

        flushed.expect("an undisturbed flush wait should succeed");
        assert_eq!(elapsed, FLUSH_WAIT);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_flush_reports_cancellation_when_millstone_exits_at_the_same_time() {
        let cancel = CancellationToken::new();
        cancel.cancel();

        for _ in 0..100 {
            let (flushed, _) = flush(&cancel, std::future::ready(ExitStatus::Success)).await;

            let error = flushed.expect_err("a cancelled flush wait should fail").to_string();
            assert!(error.starts_with("Cancelled while waiting"), "{error}");
        }
    }

    #[tokio::test]
    async fn a_stalled_collection_stops_when_the_test_is_cancelled() {
        install_crypto_provider();
        let intake = MockIntake::unresponsive().await;
        let test_token = CancellationToken::new();
        let tasks = CaseTasks::new(&test_token);
        let port = intake.port();
        let collected = run_in_background(&tasks, &Span::none(), async move {
            CollectedData::collect(&IntakeClient::new(port)).await
        });
        let canceller = test_token.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            canceller.cancel();
        });

        let collected = timeout(BOUND, collected)
            .await
            .expect("the collection should stop within the bound");

        let Err(error) = collected else {
            panic!("a stopped collection should fail");
        };
        assert_eq!(error.to_string(), "Stopped before finishing: the test was cancelled.");
        assert!(
            intake.connections() > 0,
            "the collection should have reached the intake"
        );
    }
}
