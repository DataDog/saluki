//! Test execution shared by TUI and plain output.
//!
//! The runner sends progress and results to the run collector as events.

use std::num::NonZeroUsize;
use std::sync::RwLock;
use std::{
    collections::HashMap,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use airlock::driver::{ContainerOs, Driver, DriverConfig, DriverDetails};
use bollard::{container::LogOutput, errors::Error as DockerError};
use futures::stream::{self, StreamExt as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::pin;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::deadline::{Budget, Deadline};
use crate::ledger::RECLAIM_TIME;
use crate::probe::probe;
use crate::record::{ActiveStep, CaseRecord, StepStatus, Verdict};
use crate::tasks::TASK_STOP_TIME;
use crate::test::{RunnerSettings, Test, TestContext};
use crate::{
    assertions::{AssertionContext, LogBuffer, TargetCommand},
    config::{parse_file_spec, parse_port_spec},
    events::TestEvent,
    intake::IntakeClient,
    integration::IntegrationTestCase,
    reporter::{
        ErrorKind, Issue, IssueKind, PhaseTiming, PhaseTracker, StopCause, StoppedBy, TestResult, TimeoutAttribution,
        UNKNOWN_PHASE,
    },
};

/// A sender that the `Runner` can use to send `TestEvent`s.
pub(crate) type EventSender = mpsc::UnboundedSender<TestEvent>;

/// How long a cancelled or timed-out test has to return its result before the runner drops it.
const GRACE_TIME: Duration = Duration::from_secs(30);

/// The amount of time the intake sidecar has to report healthy before the test fails.
const INTAKE_HEALTH_TIMEOUT: Duration = Duration::from_secs(60);

/// Time allowed to resolve the target's dynamic variables and read them back.
const DYNAMIC_VARS_TIMEOUT: Duration = Duration::from_secs(30);

/// The amount of time the post-assertion intake capture has to collect from the intake sidecar.
const INTAKE_CAPTURE_BUDGET: Duration = Duration::from_secs(10);

/// Maps shared `DD_DATA_PLANE_*` test env keys to their Windows-image-native nested form.
///
/// ADP reads its own configuration as a nested object: a key like `data_plane.enabled` maps to
/// the env var `DD_DATA_PLANE__ENABLED` (single underscore between path levels, double
/// underscore separating the `data_plane` prefix from the inner path). The Core Agent's config
/// loader uses single underscores throughout and exposes the same setting as
/// `DD_DATA_PLANE_ENABLED`.
///
/// On the `linux` runtime the test target is the converged Datadog Agent image, where
/// an s6 init script reads each `DD_DATA_PLANE_*` env var and re-exports it under the nested
/// name before launching ADP. Tests therefore write the flat shape (`DD_DATA_PLANE_ENABLED`)
/// in their YAML, which works on `linux` and `mac` (the Unix runner has its own translator).
///
/// On the Windows runtime the test target is an ADP-only image we build in this repository. There is
/// no s6 entrypoint and ADP only reads the nested form, so we replay the same translation here
/// before the env vars cross into the container. Keeping the table in the harness lets test
/// YAML files stay portable across all three runtimes; only entries that are actually used
/// today are listed, and any new shared `DD_DATA_PLANE_*` key needs a one-line addition here.
const LINUX_ADP_CLI_BINARY_PATH: &str = "/opt/datadog-agent/embedded/bin/agent-data-plane";
const WINDOWS_ADP_CLI_BINARY_PATH: &str = r"C:\adp\agent-data-plane.exe";
const LINUX_CORE_AGENT_CLI_BINARY_PATH: &str = "/opt/datadog-agent/bin/agent/agent";
const WINDOWS_CORE_AGENT_CLI_BINARY_PATH: &str = r"C:\Program Files\Datadog\Datadog Agent\bin\agent.exe";

const WINDOWS_ENV_ALIASES: &[(&str, &str)] = &[
    ("DD_DATA_PLANE_ENABLED", "DD_DATA_PLANE__ENABLED"),
    ("DD_DATA_PLANE_STANDALONE_MODE", "DD_DATA_PLANE__STANDALONE_MODE"),
    (
        "DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT",
        "DD_DATA_PLANE__USE_NEW_CONFIG_STREAM_ENDPOINT",
    ),
    (
        "DD_DATA_PLANE_REMOTE_AGENT_ENABLED",
        "DD_DATA_PLANE__REMOTE_AGENT_ENABLED",
    ),
    ("DD_DATA_PLANE_DOGSTATSD_ENABLED", "DD_DATA_PLANE__DOGSTATSD__ENABLED"),
    ("DD_DATA_PLANE_OTLP_ENABLED", "DD_DATA_PLANE__OTLP__ENABLED"),
    (
        "DD_DATA_PLANE_OTLP_PROXY_ENABLED",
        "DD_DATA_PLANE__OTLP__PROXY__ENABLED",
    ),
    (
        "DD_DATA_PLANE_OTLP_PROXY_TRACES_ENABLED",
        "DD_DATA_PLANE__OTLP__PROXY__TRACES__ENABLED",
    ),
    (
        "DD_DATA_PLANE_OTLP_PROXY_METRICS_ENABLED",
        "DD_DATA_PLANE__OTLP__PROXY__METRICS__ENABLED",
    ),
    (
        "DD_DATA_PLANE_OTLP_PROXY_LOGS_ENABLED",
        "DD_DATA_PLANE__OTLP__PROXY__LOGS__ENABLED",
    ),
    ("DD_DATA_PLANE_LOG_FILE", "DD_DATA_PLANE__LOG_FILE"),
];

fn container_adp_cli_command(target_os: ContainerOs) -> TargetCommand {
    let binary_path = match target_os {
        ContainerOs::Linux => LINUX_ADP_CLI_BINARY_PATH,
        ContainerOs::Windows => WINDOWS_ADP_CLI_BINARY_PATH,
    };

    TargetCommand::new(vec![binary_path.to_string()])
}

fn container_core_agent_cli_command(target_os: ContainerOs) -> TargetCommand {
    let binary_path = match target_os {
        ContainerOs::Linux => LINUX_CORE_AGENT_CLI_BINARY_PATH,
        ContainerOs::Windows => WINDOWS_CORE_AGENT_CLI_BINARY_PATH,
    };

    TargetCommand::new(vec![binary_path.to_string()])
}

fn build_port_mappings_for_runtime(
    runtime: &str, exposed_ports: &[String], details: &DriverDetails,
) -> HashMap<String, u16> {
    let mut mappings = HashMap::new();

    for port_spec in exposed_ports {
        if let Ok((port, protocol)) = parse_port_spec(port_spec) {
            if runtime == crate::config::WINDOWS_RUNTIME {
                mappings.insert(format!("{}/{}", port, protocol), port);
            } else if let Some(host_port) = details.try_get_exposed_port(protocol, port) {
                mappings.insert(format!("{}/{}", port, protocol), host_port);
            }
        }
    }

    mappings
}

fn normalize_env_for_runtime(mut env: HashMap<String, String>, runtime: &str) -> HashMap<String, String> {
    if runtime == crate::config::WINDOWS_RUNTIME {
        for (source, target) in WINDOWS_ENV_ALIASES {
            if let Some(value) = env.get(*source).cloned() {
                env.entry((*target).to_string()).or_insert(value);
            }
        }

        // As of Agent 7.80, the Core Agent only honors `data_plane.enabled` on Linux (and later
        // macOS); on every other platform `sanitizeDataPlaneConfig` installs an authoritative
        // `data_plane.enabled=false` override unless `DD_DATA_PLANE_FORCE_ENABLE=true` is set. The
        // Core Agent runs inside the Windows target container (started by the ADP entrypoint) and
        // reads this flat env var, so without it the Agent streams `data_plane.enabled=false` to ADP
        // and ADP exits with "Agent Data Plane is not enabled." Mirror the macOS host-process path
        // (see `unix_runner::build_core_agent_forced_env`) and force-enable when ADP is requested.
        if env.get("DD_DATA_PLANE_ENABLED").is_some_and(|value| value == "true") {
            env.entry("DD_DATA_PLANE_FORCE_ENABLE".to_string())
                .or_insert_with(|| "true".to_string());
        }
    }

    env
}

pub(crate) struct RunArgs {
    /// The number of tests to run in parallel.
    parallelism: NonZeroUsize,

    /// Whether to stop execution at the first failure.
    fail_fast: bool,

    /// Reports run progress and results to the collector.
    event_sender: EventSender,

    /// Requests run cancellation.
    ///
    /// The consume loop cancels this on an interrupt. Active tests get `GRACE_TIME` to return.
    cancel_all: CancellationToken,
}

impl RunArgs {
    pub(crate) fn new(cancel_all: CancellationToken, event_sender: EventSender) -> Self {
        Self {
            parallelism: NonZeroUsize::MIN,
            fail_fast: false,
            event_sender,
            cancel_all,
        }
    }

    pub(crate) fn with_parallelism(mut self, parallelism: NonZeroUsize) -> Self {
        self.parallelism = parallelism;
        self
    }

    pub(crate) fn with_fail_fast(mut self, fail_fast: bool) -> Self {
        self.fail_fast = fail_fast;
        self
    }
}

/// Owns the set of registered tests and orchestrates their execution.
///
/// Handles parallelism, timeout enforcement, cancellation propagation, log directory creation,
/// and event dispatch. Individual test logic lives in the `Test`
/// implementations.
pub(crate) struct Runner {
    tests: Vec<Box<dyn Test>>,
    log_base_dir: PathBuf,
    settings: RunnerSettings,
    kind_ready: Option<crate::test::KindReadyReceiver>,
}

impl Runner {
    pub(crate) fn new(log_base_dir: impl Into<PathBuf>, settings: RunnerSettings) -> Self {
        Self {
            tests: Vec::new(),
            log_base_dir: log_base_dir.into(),
            settings,
            kind_ready: None,
        }
    }

    pub(crate) fn with_kind_ready(mut self, rx: crate::test::KindReadyReceiver) -> Self {
        self.kind_ready = Some(rx);
        self
    }

    /// Register a test. Returns an error if the test name is a duplicate.
    pub(crate) fn register(&mut self, test: Box<dyn Test>) -> Result<(), GenericError> {
        // Inefficient but it probably does not matter unless we have thousands of tests.
        if self.tests.iter().any(|existing| existing.name() == test.name()) {
            return Err(generic_error!(
                "A test named '{}' already exists in the registry",
                test.name()
            ));
        }
        self.tests.push(test);
        Ok(())
    }

    /// Runs the registered tests, reporting progress and results as events.
    pub(crate) async fn run_tests(&self, args: RunArgs) {
        let RunArgs {
            parallelism,
            fail_fast,
            event_sender,
            cancel_all,
        } = args;

        let tests: Vec<&dyn Test> = self.tests.iter().map(|t| t.as_ref()).collect();

        let _ = event_sender.send(TestEvent::RunStarted {
            total_tests: tests.len(),
        });

        let parallelism = parallelism.get();
        let semaphore = Arc::new(Semaphore::new(parallelism));

        if fail_fast {
            self.run_fail_fast(&tests, &semaphore, &event_sender, &cancel_all).await;
        } else {
            self.run_parallel(&tests, parallelism, &semaphore, &event_sender, &cancel_all)
                .await;
        }
    }

    async fn run_fail_fast(
        &self, tests: &[&dyn Test], semaphore: &Semaphore, event_sender: &EventSender, cancel_all: &CancellationToken,
    ) {
        for test in tests {
            if cancel_all.is_cancelled() {
                break;
            }

            // Kind tests wait for cluster readiness before acquiring a concurrency slot.
            if test.runtime() == "kubernetes_in_docker" {
                if let Some(mut rx) = self.kind_ready.clone() {
                    loop {
                        if rx.borrow().is_some() {
                            break;
                        }
                        tokio::select! {
                            _ = cancel_all.cancelled() => break,
                            result = rx.changed() => { if result.is_err() { break; } }
                        }
                    }
                }
            }

            let _permit = semaphore.acquire().await.unwrap();
            let result = Self::run_one(
                *test,
                event_sender,
                cancel_all,
                self.log_base_dir.clone(),
                self.settings.clone(),
                self.kind_ready.clone(),
            )
            .await;

            if !result.outcome.is_passed() {
                break;
            }
        }
    }

    async fn run_parallel(
        &self, tests: &[&dyn Test], parallelism: usize, semaphore: &Arc<Semaphore>, event_sender: &EventSender,
        cancel_all: &CancellationToken,
    ) {
        let mut futures = stream::FuturesUnordered::new();

        for test in tests {
            let semaphore = semaphore.clone();
            let cancel = cancel_all.clone();
            let log_base_dir = self.log_base_dir.clone();
            let settings = self.settings.clone();
            let mut kind_ready = self.kind_ready.clone();
            futures.push(async move {
                if cancel.is_cancelled() {
                    return;
                }

                // Kind tests wait for cluster readiness before acquiring a concurrency slot
                // so they don't starve Docker tests while the cluster is being set up.
                if test.runtime() == "kubernetes_in_docker" {
                    if let Some(ref mut rx) = kind_ready {
                        loop {
                            if rx.borrow().is_some() {
                                break;
                            }
                            tokio::select! {
                                _ = cancel.cancelled() => break,
                                result = rx.changed() => { if result.is_err() { break; } }
                            }
                        }
                    }
                }

                if cancel.is_cancelled() {
                    return;
                }

                let _permit = semaphore.acquire().await.unwrap();

                if cancel.is_cancelled() {
                    return;
                }

                Self::run_one(*test, event_sender, &cancel, log_base_dir, settings, kind_ready).await;
            });

            while futures.len() >= parallelism {
                futures.next().await;
            }
        }

        while futures.next().await.is_some() {}
    }

    async fn run_one(
        test: &dyn Test, event_sender: &EventSender, cancel_all: &CancellationToken, log_base_dir: PathBuf,
        settings: RunnerSettings, kind_ready: Option<crate::test::KindReadyReceiver>,
    ) -> TestResult {
        let name = test.name();
        let suite = test.suite();
        let timeout = test.timeout();
        let started = Instant::now();

        // A test-specific token lets a timeout cancel this test without cancelling the run.
        let test_cancel = CancellationToken::new();

        // Create a directory for the test to write logs into and pass it into the test context.
        let log_dir = log_base_dir.join(format!("{:?}", suite).to_lowercase()).join(&name);
        if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
            let mut result = TestResult::setup_error(
                name.clone(),
                started.elapsed(),
                format!("Error creating log directory at {}: {e}", log_dir.display()),
            );
            result.case_path = Some(test.case_path());

            // Report setup failures through events so the collector accounts for the selected test.
            let _ = event_sender.send(TestEvent::TestStarted { name });
            let _ = event_sender.send(TestEvent::TestCompleted {
                result: Box::new(result.clone()),
                log_dir,
            });

            return result;
        }

        let case = Deadline::after(Budget::Case, timeout);
        let mut tctx = TestContext::new(test_cancel.clone(), case, log_dir.clone(), settings);
        if test.runtime() == "kubernetes_in_docker" {
            if let Some(rx) = kind_ready {
                tctx = tctx.with_kind_ready(rx);
            }
        }

        let _ = event_sender.send(TestEvent::TestStarted { name: name.clone() });

        let phases = tctx.phases.clone();
        let record = tctx.record.clone();
        let ledger = tctx.ledger.clone();
        let tasks = tctx.tasks.clone();

        let mut result = {
            let run_fut = test.run(tctx);
            pin!(run_fut);

            // On timeout or run cancellation, cancel the test and allow GRACE_TIME for it to return.
            //
            // The case timer wins ties with the body so an expired deadline is reported, even after a verdict.
            // The body precedes `cancel_all` to preserve a result ready when external cancellation arrives.
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(case.at) => {
                    let stop = Stop::snapshot(StoppedBy::CaseDeadline, &phases, &record);
                    test_cancel.cancel();
                    let returned = tokio::time::timeout(GRACE_TIME, &mut run_fut).await.ok();
                    stop.settle(returned, &name, timeout, started.elapsed(), &phases, &record)
                }
                r = &mut run_fut => r,
                _ = cancel_all.cancelled() => {
                    let stop = Stop::snapshot(StoppedBy::Signal, &phases, &record);
                    test_cancel.cancel();
                    let returned = tokio::time::timeout(GRACE_TIME, &mut run_fut).await.ok();
                    stop.settle(returned, &name, timeout, started.elapsed(), &phases, &record)
                }
            }
            // Drop the body before cleanup; its tracked background tasks are handled below.
        };

        // Cleanup has its own budget because the test token may already be cancelled. Give tracked tasks time to stop
        // and mark interrupted creations before reclaim reads the ledger.
        let reclaim_started = Instant::now();
        let deadline = tokio::time::Instant::now() + RECLAIM_TIME;
        let running = tasks.stop_all(tokio::time::Instant::now() + TASK_STOP_TIME).await;
        if running > 0 {
            warn!(
                test = %name,
                running,
                "Background tasks were still running in synchronous code after {:?}.",
                TASK_STOP_TIME
            );
        }
        let (reclaim, issues) = ledger.reclaim(deadline).await;
        if !reclaim.is_empty() {
            result.phase_timings.push(PhaseTiming {
                phase: "reclaim".to_string(),
                duration: reclaim_started.elapsed(),
            });
            result.duration = started.elapsed();
        }
        result.reclaim = reclaim;
        result.issues.extend(issues);

        // The runner is the only place that knows both the test and where its artifacts landed, so
        // it stamps that provenance onto the result before anything reports on it.
        result.case_path = Some(test.case_path());
        result.log_dir = Some(log_dir.clone());

        write_result_log(&result, &log_dir);
        // Traffic retention runs before the report, so `result.json` lists the artifacts that are on disk.
        crate::traffic::finalize(&log_dir, result.outcome);
        crate::machine_output::write_test_report(&result, &log_dir);

        let _ = event_sender.send(TestEvent::TestCompleted {
            result: Box::new(result.clone()),
            log_dir,
        });

        result
    }
}

/// Test state captured before cancellation can change step statuses or advance the phase.
struct Stop {
    by: StoppedBy,
    /// The phase the test was in.
    active_phase: Option<String>,
    /// Whether the test had reached its verdict.
    decided: bool,
    /// The procedure steps that were running.
    active: Vec<ActiveStep>,
}

impl Stop {
    fn snapshot(by: StoppedBy, phases: &PhaseTracker, record: &CaseRecord) -> Self {
        Self {
            by,
            active_phase: phases.active(),
            decided: record.decided(),
            active: record.running(),
        }
    }

    /// Uses the result returned during grace, falling back to the case record when grace expires.
    ///
    /// Preserves a verdict reached before cancellation, recording any expired deadline or cleanup grace as an issue.
    /// Otherwise, the stop determines the outcome: a timeout for the case deadline, an internal error for a signal.
    fn settle(
        self, returned: Option<TestResult>, name: &str, timeout: Duration, elapsed: Duration, phases: &PhaseTracker,
        record: &CaseRecord,
    ) -> TestResult {
        let grace_phase = returned
            .is_none()
            .then(|| phases.active().unwrap_or_else(|| UNKNOWN_PHASE.to_string()));
        let mut result = returned.unwrap_or_else(|| record.result(name, elapsed, phases.completed()));
        let grace_expired = grace_phase.as_ref().map(|phase| Issue {
            kind: IssueKind::CleanupGraceExpired,
            message: format!("cleanup grace {:?} expired in {}", GRACE_TIME, phase),
        });

        if self.decided {
            if self.by == StoppedBy::CaseDeadline {
                result.issues.push(Issue {
                    kind: IssueKind::PostVerdictDeadline,
                    message: format!(
                        "case budget {:?} expired in {} after the verdict",
                        timeout,
                        self.active_phase.as_deref().unwrap_or(UNKNOWN_PHASE)
                    ),
                });
            }
            result.issues.extend(grace_expired);
            return result;
        }

        result.cause = Some(StopCause {
            by: self.by,
            active: self.active,
        });
        match (self.by, grace_phase) {
            (StoppedBy::CaseDeadline, None) => result
                .with_harness_error(ErrorKind::Timeout, format!("Test timed out after {:?}.", timeout))
                .with_timeout_attribution(TimeoutAttribution::test_deadline(timeout, self.active_phase)),
            (StoppedBy::CaseDeadline, Some(grace_phase)) => result
                .with_harness_error(
                    ErrorKind::Timeout,
                    format!(
                        "Test timed out after {:?} and did not return within {:?}.",
                        timeout, GRACE_TIME
                    ),
                )
                .with_timeout_attribution(TimeoutAttribution::cleanup_grace(
                    GRACE_TIME,
                    timeout,
                    self.active_phase,
                    Some(grace_phase),
                )),
            (StoppedBy::Signal, _) => {
                result.issues.extend(grace_expired);
                result.with_harness_error(ErrorKind::Internal, "Test was cancelled.")
            }
        }
    }
}

// ----------------------------------------------------------------------------
// IntegrationRunner: single integration test case execution
// ----------------------------------------------------------------------------

/// Runner for a single integration test case.
pub(crate) struct IntegrationRunner {
    test_case: IntegrationTestCase,
    isolation_group_id: String,
    tctx: TestContext,
    log_buffer: Arc<RwLock<LogBuffer>>,
}

impl IntegrationRunner {
    /// Create a new test runner for the given test case.
    pub(crate) fn new(test_case: IntegrationTestCase, tctx: TestContext) -> Self {
        Self {
            test_case,
            isolation_group_id: tctx.ledger.isolation_group(),
            tctx,
            log_buffer: Arc::new(RwLock::new(LogBuffer::default())),
        }
    }

    /// Run the test case and return the result.
    pub(crate) async fn run(&mut self) -> TestResult {
        let started = Instant::now();
        let test_name = self.test_case.config.name.clone();
        let mut phase_timings = Vec::new();

        info!(
            test = %test_name,
            isolation_group = %self.isolation_group_id,
            "Starting test case."
        );

        // The intake sidecar is a Linux container, so it creates the isolation group's network with
        // Docker's `bridge` driver. A Windows target needs a `nat` network and cannot join that one,
        // so reject the combination up front instead of failing later inside Docker.
        if self.test_case.config.intake.enabled && self.test_case.active_runtime == crate::config::WINDOWS_RUNTIME {
            return TestResult::errored(
                test_name,
                ErrorKind::Setup,
                format!(
                    "Intake sidecar is not supported on the '{}' runtime.",
                    crate::config::WINDOWS_RUNTIME
                ),
                started.elapsed(),
                phase_timings,
            );
        }

        // Build the driver configuration.
        debug!(test = %test_name, "Building driver configuration...");
        let phase = self.tctx.phases.enter("driver_config_build");
        let driver_config = match self.build_driver_config().await {
            Ok(config) => config,
            Err(e) => {
                error!(test = %test_name, error = %e, "Failed to build driver configuration.");
                phase_timings.push(phase.finish());
                return TestResult::errored(
                    test_name,
                    ErrorKind::Setup,
                    format!("Failed to build driver configuration: {}", e),
                    started.elapsed(),
                    phase_timings,
                );
            }
        };
        phase_timings.push(phase.finish());

        // Start the intake sidecar first, so the target can flush to it from the moment it runs.
        let mut intake_host_port = None;
        let mut _intake_driver = None;
        if self.test_case.config.intake.enabled {
            let phase = self.tctx.phases.enter("intake_start");
            info!(test = %test_name, "Starting intake sidecar...");
            let outcome = self.start_intake().await;
            phase_timings.push(phase.finish());

            match outcome {
                Ok((driver, host_port)) => {
                    intake_host_port = Some(host_port);
                    _intake_driver = Some(driver);
                }
                Err(e) => {
                    error!(test = %test_name, error = %e, "Failed to start intake sidecar.");
                    return self.fail_setup(format!("Failed to start intake sidecar: {}", e), started, phase_timings);
                }
            }
        }

        // Create and start the driver.
        let phase = self.tctx.phases.enter("container_start");
        debug!(test = %test_name, "Creating container driver...");
        let driver = match Driver::from_config(self.isolation_group_id.clone(), driver_config) {
            Ok(driver) => driver,
            Err(e) => {
                error!(test = %test_name, error = %e, "Failed to create container driver.");
                phase_timings.push(phase.finish());
                return self.fail_setup(format!("Failed to create driver: {}", e), started, phase_timings);
            }
        };

        info!(test = %test_name, "Starting container...");
        let details = match self
            .tctx
            .ledger
            .start_container(&driver, &self.isolation_group_id, &self.tctx.test_cancel_token())
            .await
        {
            Ok(details) => details,
            Err(e) => {
                error!(test = %test_name, error = %e, "Failed to start container.");
                phase_timings.push(phase.finish());
                return self.fail_setup(format!("Failed to start container: {}", e), started, phase_timings);
            }
        };
        phase_timings.push(phase.finish());

        info!(
            test = %test_name,
            container = %details.container_name(),
            "Container started successfully."
        );

        // Start log capture in the background.
        debug!(test = %test_name, "Starting log capture...");
        if let Err(e) = self.start_log_capture(details.container_name()).await {
            warn!(test = %test_name, error = %e, "Failed to start log capture.");
        }

        // Monitor container exit in the background. Uses its own token so that a normal
        // container exit does not trigger the external cancel path.
        let exit_token = CancellationToken::new();
        let exit_signal = exit_token.clone();
        let docker_exit_code: crate::assertions::DockerExitCodeCell = Arc::new(std::sync::OnceLock::new());
        let docker_exit_code_for_exit = docker_exit_code.clone();
        let container_name = details.container_name().to_string();
        let container_name_for_exit = container_name.clone();
        let exit_handle = tokio::spawn(async move {
            let docker = match airlock::docker::connect() {
                Ok(d) => d,
                Err(_) => return,
            };

            let mut wait_stream = docker.wait_container(
                &container_name_for_exit,
                None::<bollard::query_parameters::WaitContainerOptions>,
            );
            if let Some(result) = wait_stream.next().await {
                let exit_code = match result {
                    Ok(response) => Some(response.status_code),
                    Err(DockerError::DockerContainerWaitError { code, .. }) => Some(code),
                    Err(_) => None,
                };
                if let Some(code) = exit_code {
                    let _ = docker_exit_code_for_exit.set(code);
                }
                exit_signal.cancel();
            }
        });

        // Build port mappings for assertions.
        let port_mappings = self.build_port_mappings(&details);

        // Resolve dynamic variables if any PANORAMIC_DYNAMIC_* env vars are defined.
        if crate::dynamic_vars::has_dynamic_vars(&self.test_case.config) {
            let phase = self.tctx.phases.enter("dynamic_vars");
            debug!(test = %test_name, "Resolving dynamic variables...");

            let resolved_vars = if self.test_case.active_runtime == crate::config::WINDOWS_RUNTIME {
                crate::dynamic_vars::resolve_windows_vars(&self.test_case.config, details.container_ip()).await
            } else {
                crate::dynamic_vars::read_resolved_vars(&driver, DYNAMIC_VARS_TIMEOUT, &self.tctx.test_cancel_token())
                    .await
            };

            match resolved_vars {
                Ok(vars) => {
                    // Fail on empty values — indicates the init script command failed.
                    for (key, value) in &vars {
                        if value.is_empty() {
                            error!(test = %test_name, key = key, "Dynamic variable resolved to empty string.");
                            phase_timings.push(phase.finish());
                            let message = format!(
                                "Dynamic variable PANORAMIC_DYNAMIC_{} resolved to an empty string. \
                                 The shell command in the test config likely failed.",
                                key
                            );
                            return self.fail_setup(message, started, phase_timings);
                        }
                    }

                    info!(
                        test = %test_name,
                        variable_count = vars.len(),
                        "Resolved dynamic variables."
                    );
                    self.test_case.resolve_dynamic_vars(&vars);

                    // Fail if any placeholders remain unresolved after substitution.
                    let unresolved = self.test_case.unresolved_placeholders();
                    if !unresolved.is_empty() {
                        error!(test = %test_name, unresolved = ?unresolved, "Unresolved dynamic variable placeholders.");
                        phase_timings.push(phase.finish());
                        let message = format!(
                            "Unresolved dynamic variable placeholders in assertions: {}. \
                             Check that matching PANORAMIC_DYNAMIC_* env vars are defined.",
                            unresolved.join(", ")
                        );
                        return self.fail_setup(message, started, phase_timings);
                    }
                }
                Err(e) => {
                    error!(test = %test_name, error = %e, "Failed to resolve dynamic variables.");
                    phase_timings.push(phase.finish());
                    return self.fail_setup(
                        format!("Failed to resolve dynamic variables: {}", e),
                        started,
                        phase_timings,
                    );
                }
            }

            phase_timings.push(phase.finish());
        }

        // The step driver bounds each step and stops when the runner cancels the test.
        info!(
            test = %test_name,
            assertion_count = self.test_case.total_assertion_count(),
            "Running assertions..."
        );

        let phase = self.tctx.phases.enter("assertions");
        let test_cancel = self.tctx.test_cancel_token();

        self.run_assertions(
            &port_mappings,
            details.container_ip(),
            &container_name,
            &exit_token,
            docker_exit_code,
            intake_host_port,
        )
        .await;
        phase_timings.push(phase.finish());

        // Cancel the exit monitor.
        exit_handle.abort();

        // Keep what the intake holds now as diagnostic evidence; retention is decided once the verdict is final.
        if let Some(intake_host_port) = intake_host_port {
            let phase = self.tctx.phases.enter("intake_capture");
            crate::traffic::capture_intake(
                &crate::traffic::traffic_dir(self.tctx.log_dir()),
                &IntakeClient::new(intake_host_port),
                &test_cancel,
                INTAKE_CAPTURE_BUDGET,
            )
            .await;
            phase_timings.push(phase.finish());
        }

        // Write logs to disk if configured.
        let phase = self.tctx.phases.enter("write_logs");
        if let Err(e) = self.write_logs(&test_name).await {
            warn!(test = %test_name, error = %e, "Failed to write container logs to disk.");
        }
        debug!(test = %test_name, "Wrote container logs to disk.");
        phase_timings.push(phase.finish());

        let result = self.tctx.record.result(&test_name, started.elapsed(), phase_timings);
        info!(
            test = %test_name,
            outcome = %result.outcome,
            duration = ?started.elapsed(),
            "Test case completed."
        );
        result
    }

    /// Records and returns a setup failure.
    fn fail_setup(&self, message: String, started: Instant, phase_timings: Vec<PhaseTiming>) -> TestResult {
        let result = TestResult::errored(
            self.test_case.config.name.clone(),
            ErrorKind::Setup,
            message,
            started.elapsed(),
            phase_timings,
        );
        self.tctx.record.decide(Verdict::SetupFailed(Box::new(result.clone())));
        result
    }

    async fn build_driver_config(&self) -> Result<DriverConfig, GenericError> {
        let container = &self.test_case.config.container;

        // Merge framework-level port-isolation env vars with the test's own env. Framework
        // defaults are applied first so the test's `env` block takes precedence. Keeps the test
        // surface consistent across the linux and `mac` runtimes — both see the same shifted
        // port table.
        let mut merged_env = crate::test_env::port_isolation_env();
        for (k, v) in &self.test_case.config.env {
            merged_env.insert(k.clone(), v.clone());
        }
        let merged_env = normalize_env_for_runtime(merged_env, &self.test_case.active_runtime);
        let env_vars: Vec<String> = merged_env.iter().map(|(k, v)| format!("{}={}", k, v)).collect();

        // Build the target config.
        let container_os = if self.test_case.active_runtime == crate::config::WINDOWS_RUNTIME {
            ContainerOs::Windows
        } else {
            ContainerOs::Linux
        };

        let image = self
            .test_case
            .image(crate::config::TARGET_IMAGE_NAME)
            .ok_or_else(|| {
                generic_error!(
                    "Runtime '{}' has no associated container image; integration tests are unsupported.",
                    self.test_case.active_runtime
                )
            })?
            .to_string();

        let target_config = airlock::config::TargetConfig {
            image,
            entrypoint: container.entrypoint.clone(),
            command: container.command.clone(),
            additional_env_vars: env_vars,
            container_os,
            host_cgroup_namespace: container.host_cgroup_namespace,
        };

        let mut config = DriverConfig::target("target", target_config)
            .await?
            .with_alpine_image(&self.tctx.settings.alpine_image);

        // Apply panoramic's read-only file overlays before any test-specific bind mounts. The
        // overlays target Linux paths (such as /var/log/datadog) and don't apply to Windows
        // containers, which use their own platform paths.
        if self.test_case.active_runtime != crate::config::WINDOWS_RUNTIME {
            config = crate::mounts::apply_target_mounts(config, self.tctx.mounts_dir())?;
        }

        // Add file mounts.
        for file_spec in &container.files {
            let (host_path, container_path) = parse_file_spec(file_spec)?;
            let absolute_host_path = crate::test::resolve_case_path(&self.test_case.config.loaded_from, host_path);

            // Verify the host path exists.
            if !absolute_host_path.exists() {
                return Err(generic_error!(
                    "Host path does not exist: {}",
                    absolute_host_path.display()
                ));
            }

            config = config.with_bind_mount(absolute_host_path, Path::new(container_path));
        }

        // Add exposed ports.
        for port_spec in &container.exposed_ports {
            let (port, protocol) = parse_port_spec(port_spec)?;
            // Protocol must be a static string for airlock API.
            let protocol: &'static str = match protocol {
                "tcp" => "tcp",
                "udp" => "udp",
                _ => return Err(generic_error!("Invalid protocol: {}.", protocol)),
            };
            config = config.with_exposed_port(protocol, port);
        }

        Ok(config)
    }

    /// Starts the Datadog intake sidecar in this test's isolation group.
    ///
    /// Sharing the isolation group puts the sidecar on the same Docker network as the target, which
    /// reaches it by network alias. Returns the sidecar's driver and the host port that its HTTP
    /// endpoint is published on.
    async fn start_intake(&self) -> Result<(Driver, u16), GenericError> {
        let intake_image = self
            .test_case
            .image(crate::config::INTAKE_IMAGE_NAME)
            .ok_or_else(|| generic_error!("The intake sidecar is not enabled for this test case."))?
            .to_string();

        let config = DriverConfig::datadog_intake(airlock::config::DatadogIntakeConfig {
            image: intake_image,
            binary_path: None,
        })
        .await?
        .with_network_alias(crate::config::INTAKE_NETWORK_ALIAS)
        .with_alpine_image(&self.tctx.settings.alpine_image);

        let mut driver = Driver::from_config(self.isolation_group_id.clone(), config)?
            .with_logging(self.tctx.log_dir().to_path_buf());
        let details = self
            .tctx
            .ledger
            .start_container(&driver, &self.isolation_group_id, &self.tctx.test_cancel_token())
            .await?;

        // Bound the health wait and watch cancellation so an unhealthy sidecar releases the test promptly.
        probe(
            "the intake sidecar to report healthy",
            INTAKE_HEALTH_TIMEOUT,
            &self.tctx.test_cancel_token(),
            driver.wait_for_container_healthy(),
        )
        .await?
        .error_context("Intake sidecar failed its health check.")?;

        let host_port = details
            .try_get_exposed_port("tcp", crate::config::INTAKE_HTTP_PORT)
            .ok_or_else(|| generic_error!("Intake container did not publish its HTTP port."))?;

        Ok((driver, host_port))
    }

    fn build_port_mappings(&self, details: &DriverDetails) -> HashMap<String, u16> {
        build_port_mappings_for_runtime(
            &self.test_case.active_runtime,
            &self.test_case.config.container.exposed_ports,
            details,
        )
    }

    async fn start_log_capture(&self, container_name: &str) -> Result<(), GenericError> {
        let docker = airlock::docker::connect()?;
        let log_buffer = self.log_buffer.clone();
        let container_name = container_name.to_string();

        let logs_options = bollard::query_parameters::LogsOptions {
            follow: true,
            stdout: true,
            stderr: true,
            ..Default::default()
        };

        let mut log_stream = docker.logs(&container_name, Some(logs_options));

        tokio::spawn(async move {
            while let Some(log_result) = log_stream.next().await {
                match log_result {
                    Ok(log) => {
                        let mut buffer = log_buffer.write().unwrap();
                        match log {
                            LogOutput::StdOut { message } => {
                                if let Ok(line) = String::from_utf8(message.to_vec()) {
                                    for l in line.lines() {
                                        buffer.stdout.push(l.to_string());
                                    }
                                }
                            }
                            LogOutput::StdErr { message } => {
                                if let Ok(line) = String::from_utf8(message.to_vec()) {
                                    for l in line.lines() {
                                        buffer.stderr.push(l.to_string());
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "Log stream ended or encountered an error.");
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    async fn run_assertions(
        &self, port_mappings: &HashMap<String, u16>, container_ip: Option<&str>, container_name: &str,
        exit_token: &CancellationToken, docker_exit_code: crate::assertions::DockerExitCodeCell,
        intake_host_port: Option<u16>,
    ) {
        let target_os = if self.test_case.active_runtime == crate::config::WINDOWS_RUNTIME {
            ContainerOs::Windows
        } else {
            ContainerOs::Linux
        };
        let ctx = AssertionContext {
            log_buffer: self.log_buffer.clone(),
            container_exit_token: exit_token.clone(),
            cancel_token: self.tctx.test_cancel_token(),
            port_mappings: port_mappings.clone(),
            container_ip: container_ip.map(str::to_string),
            target_os: Some(target_os),
            container_name: container_name.to_string(),
            is_host_process: false,
            host_process_exit_code: None,
            docker_container_exit_code: Some(docker_exit_code),
            intake_host_port,
            core_agent_auth_token_path: None,
            adp_cli_command: container_adp_cli_command(target_os),
            core_agent_cli_command: container_core_agent_cli_command(target_os),
        };
        crate::assertions::run_assertion_steps(&self.test_case, &ctx, self.tctx.deadline, &self.tctx.record).await
    }

    async fn write_logs(&self, test_name: &str) -> Result<(), GenericError> {
        let log_dir = self.tctx.log_dir();

        // Get the log buffer contents.
        let buffer = self.log_buffer.read().unwrap();

        // Write stdout.
        let stdout_path = log_dir.join("stdout.log");
        let mut stdout_file = std::fs::File::create(&stdout_path)
            .error_context(format!("Failed to create stdout log file: {}", stdout_path.display()))?;
        for line in &buffer.stdout {
            writeln!(stdout_file, "{}", line).error_context("Failed to write to stdout log")?;
        }

        // Write stderr.
        let stderr_path = log_dir.join("stderr.log");
        let mut stderr_file = std::fs::File::create(&stderr_path)
            .error_context(format!("Failed to create stderr log file: {}", stderr_path.display()))?;
        for line in &buffer.stderr {
            writeln!(stderr_file, "{}", line).error_context("Failed to write to stderr log")?;
        }

        debug!(
            test = %test_name,
            path = %log_dir.display(),
            "Container logs written to disk."
        );

        Ok(())
    }
}

fn write_result_log(result: &TestResult, dir: impl AsRef<Path>) {
    let dir = dir.as_ref();

    let path = dir.join("result.log");
    let mut f = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to create result log file.");
            return;
        }
    };

    let _ = writeln!(f, "{} {} ({:.2?})", result.label(), result.name, result.duration);
    for line in result.cause_lines() {
        let _ = writeln!(f, "{}", line);
    }

    let ran: Vec<_> = result
        .assertion_results
        .iter()
        .enumerate()
        .filter(|(_, step)| step.status != StepStatus::NotRun)
        .collect();
    if !ran.is_empty() {
        let _ = writeln!(f);
        let _ = writeln!(f, "Assertions:");
        for (i, assertion) in ran {
            let indicator = if assertion.status == StepStatus::Passed {
                "+"
            } else {
                "-"
            };
            let _ = writeln!(f, "  {} {} ({:.2?})", indicator, assertion.name, assertion.duration);
            let full_details = result.assertion_details.get(i).map(|d| d.as_slice()).unwrap_or(&[]);
            if !full_details.is_empty() {
                for line in full_details {
                    let _ = writeln!(f, "    {}", line);
                }
            } else {
                for line in assertion.message.lines() {
                    let _ = writeln!(f, "    {}", line);
                }
            }
        }
    }

    if !result.phase_timings.is_empty() {
        let _ = writeln!(f);
        let _ = writeln!(f, "Phase timings:");
        for phase in &result.phase_timings {
            let _ = writeln!(f, "  {} ({:.2?})", phase.phase, phase.duration);
        }
    }
}

// These TestResult constructors just declutter the execution code a little bit.
impl TestResult {
    fn setup_error(name: impl Into<String>, total_duration: Duration, e: impl AsRef<str>) -> Self {
        Self::errored(
            name,
            ErrorKind::Setup,
            format!("Test failed to start due to an error during setup. {}", e.as_ref()),
            total_duration,
            vec![],
        )
    }
}

/// Scripted tests and settings for exercising [`Runner`].
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::BTreeMap;

    use tokio::sync::Notify;

    use super::*;
    use crate::assertions::test_support::{host_context, procedure};
    use crate::assertions::{run_assertion_steps, AssertionResult, AssertionStatus};
    use crate::test::TestSuite;

    /// What a [`ScriptedTest`] does once it has entered its phase.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Behavior {
        /// Passes one assertion.
        Pass,
        /// Fails one assertion.
        Fail,
        /// Panics, taking the runner task down with it.
        Panic,
        /// Waits for its cancellation token, then returns only if `finish_on_cancel` is set.
        WaitForCancel { finish_on_cancel: bool },
        /// Sleeps until the case deadline, then passes.
        ReturnAtDeadline,
        /// Runs `procedure:` entries on the host against a `ready` log line and reports the step results.
        Procedure(&'static str),
        /// Runs [`Behavior::Procedure`], finishes its phase, then waits for cancellation in `then_phase`.
        ///
        /// Returns the recorded result if `returns_on_cancel` is set; otherwise stays pending.
        ProcedureThenWait {
            steps: &'static str,
            then_phase: &'static str,
            returns_on_cancel: bool,
        },
        /// Returns a setup error on cancellation. With `recorded`, records the verdict before waiting;
        /// otherwise simulates a setup wait that fails due to cancellation.
        SetupError { recorded: bool },
    }

    /// A test that enters a phase and then behaves as scripted.
    pub(crate) struct ScriptedTest {
        pub(crate) name: String,
        pub(crate) timeout: Duration,
        pub(crate) phase: &'static str,
        pub(crate) behavior: Behavior,
        /// Notified once the test has entered its phase.
        pub(crate) entered: Option<Arc<Notify>>,
        /// Published port of the intake sidecar that procedure steps poll.
        pub(crate) intake: Option<u16>,
        /// A directory the test creates and registers before its behavior runs.
        pub(crate) registers: Option<PathBuf>,
        /// Spawns a pending task and records its drop.
        pub(crate) spawns: Option<TaskProbe>,
    }

    /// Observes task shutdown for a [`ScriptedTest`].
    #[derive(Debug, Default, Clone)]
    pub(crate) struct TaskProbe(Arc<std::sync::Mutex<Option<Dropped>>>);

    /// What a [`TaskProbe`] saw when its task was dropped.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) struct Dropped {
        /// Elapsed time since the task's drop guard was created.
        pub(crate) elapsed: Duration,
        /// Whether the test's registered directory still existed.
        pub(crate) directory_existed: bool,
    }

    impl TaskProbe {
        pub(crate) fn dropped(&self) -> Option<Dropped> {
            *self.0.lock().unwrap()
        }
    }

    struct ProbeGuard {
        probe: TaskProbe,
        started: tokio::time::Instant,
        directory: Option<PathBuf>,
    }

    impl Drop for ProbeGuard {
        fn drop(&mut self) {
            *self.probe.0.lock().unwrap() = Some(Dropped {
                elapsed: self.started.elapsed(),
                directory_existed: self.directory.as_ref().is_some_and(|dir| dir.exists()),
            });
        }
    }

    impl ScriptedTest {
        /// Creates a test with a timeout long enough for cancellation tests to interrupt it first.
        pub(crate) fn new(name: &str, behavior: Behavior) -> Self {
            Self {
                name: name.to_string(),
                timeout: Duration::from_secs(60),
                phase: "assertions",
                behavior,
                entered: None,
                intake: None,
                registers: None,
                spawns: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl Test for ScriptedTest {
        fn name(&self) -> String {
            self.name.clone()
        }

        fn suite(&self) -> TestSuite {
            TestSuite::Integration
        }

        fn description(&self) -> Option<String> {
            None
        }

        fn case_path(&self) -> PathBuf {
            PathBuf::from("/cases").join(&self.name)
        }

        fn timeout(&self) -> Duration {
            self.timeout
        }

        fn images(&self) -> BTreeMap<&str, String> {
            BTreeMap::new()
        }

        async fn run(&self, tctx: TestContext) -> TestResult {
            if let Some(dir) = &self.registers {
                std::fs::create_dir_all(dir).expect("the registered directory should be creatable");
                tctx.ledger.directory(dir.clone());
            }
            if let Some(probe) = &self.spawns {
                let guard = ProbeGuard {
                    probe: probe.clone(),
                    started: tokio::time::Instant::now(),
                    directory: self.registers.clone(),
                };
                tctx.tasks.spawn(async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                });
            }

            // Keep the phase active to model an interrupted phase; `ProcedureThenWait` finishes it before switching.
            let phase = tctx.phases.enter(self.phase);
            if let Some(entered) = &self.entered {
                entered.notify_one();
            }

            let assertion = |status| AssertionResult {
                name: "scripted".to_string(),
                status,
                message: String::new(),
                duration: Duration::from_millis(1),
            };
            match self.behavior {
                Behavior::Pass => TestResult::from_assertions(
                    &self.name,
                    Duration::from_millis(1),
                    vec![assertion(AssertionStatus::Passed)],
                    Vec::new(),
                ),
                Behavior::Fail => TestResult::from_assertions(
                    &self.name,
                    Duration::from_millis(1),
                    vec![assertion(AssertionStatus::Failed)],
                    Vec::new(),
                ),
                Behavior::Panic => panic!("scripted test '{}' panicked", self.name),
                Behavior::WaitForCancel { finish_on_cancel } => {
                    tctx.test_cancel_token().cancelled().await;
                    if !finish_on_cancel {
                        std::future::pending::<()>().await;
                    }
                    TestResult::from_assertions(&self.name, Duration::from_secs(1), Vec::new(), Vec::new())
                }
                Behavior::ReturnAtDeadline => {
                    tokio::time::sleep_until(tctx.deadline.at).await;
                    TestResult::from_assertions(
                        &self.name,
                        Duration::from_millis(1),
                        vec![assertion(AssertionStatus::Passed)],
                        Vec::new(),
                    )
                }
                Behavior::Procedure(steps) => {
                    let started = Instant::now();
                    self.run_procedure(steps, &tctx).await;
                    tctx.record.result(&self.name, started.elapsed(), Vec::new())
                }
                Behavior::ProcedureThenWait {
                    steps,
                    then_phase,
                    returns_on_cancel,
                } => {
                    let started = Instant::now();
                    self.run_procedure(steps, &tctx).await;
                    phase.finish();
                    let _then = tctx.phases.enter(then_phase);
                    tctx.test_cancel_token().cancelled().await;
                    if !returns_on_cancel {
                        std::future::pending::<()>().await;
                    }
                    tctx.record
                        .result(&self.name, started.elapsed(), tctx.phases.completed())
                }
                Behavior::SetupError { recorded } => {
                    let failed = |message| {
                        TestResult::errored(
                            &self.name,
                            ErrorKind::Setup,
                            message,
                            Duration::from_millis(1),
                            Vec::new(),
                        )
                    };
                    if recorded {
                        tctx.record.decide(Verdict::SetupFailed(Box::new(failed("boom"))));
                    }
                    tctx.test_cancel_token().cancelled().await;
                    failed(if recorded { "boom" } else { "Canceled while waiting." })
                }
            }
        }
    }

    impl ScriptedTest {
        /// Runs `steps` against a host context containing a `ready` log line and records their results.
        async fn run_procedure(&self, steps: &str, tctx: &TestContext) {
            let mut ctx = host_context(tctx.test_cancel_token());
            ctx.log_buffer.write().unwrap().stdout.push("ready".to_string());
            ctx.intake_host_port = self.intake;
            run_assertion_steps(&procedure(steps), &ctx, tctx.deadline, &tctx.record).await;
        }
    }

    pub(crate) fn test_runner_settings(mounts_dir: &Path) -> RunnerSettings {
        RunnerSettings {
            mounts_dir: mounts_dir.to_path_buf(),
            adp_binary_path: PathBuf::new(),
            core_agent_binary_path: PathBuf::new(),
            alpine_image: airlock::driver::DEFAULT_ALPINE_IMAGE.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{test_runner_settings, Behavior, ScriptedTest, TaskProbe};
    use super::*;
    use crate::record::StepStatus;
    use crate::reporter::test_support::{each_outcome, NOT_RUN_KIND};
    use crate::reporter::{ReclaimResult, TestOutcome, TimeoutDeadline};

    /// A bound for real-time tests.
    const BOUND: Duration = Duration::from_secs(5);

    /// Bounds paused-time tests while allowing the runner's [`GRACE_TIME`] to expire.
    const PAUSED_BOUND: Duration = Duration::from_secs(120);

    /// An assertion that matches the log line [`Behavior::Procedure`] provides.
    const LOG_MATCHES: &str = "- assertion: log_contains\n  pattern: ready\n  timeout: 5s\n";

    /// Runs a test within `bound`, using `log_base_dir` and run cancellation `cancel_all`. Discards events.
    async fn run_scripted_test(
        test: &dyn Test, log_base_dir: &Path, cancel_all: &CancellationToken, bound: Duration,
    ) -> TestResult {
        let mounts_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let settings = test_runner_settings(mounts_dir.path());
        run_with_settings(test, settings, log_base_dir, cancel_all, bound).await
    }

    /// Runs a test with explicit settings and a timeout to catch hangs.
    async fn run_with_settings(
        test: &dyn Test, settings: RunnerSettings, log_base_dir: &Path, cancel_all: &CancellationToken, bound: Duration,
    ) -> TestResult {
        let (tx, _rx) = mpsc::unbounded_channel();

        tokio::time::timeout(
            bound,
            Runner::run_one(test, &tx, cancel_all, log_base_dir.to_path_buf(), settings, None),
        )
        .await
        .expect("the runner should finish the test within the bound")
    }

    /// Returns a token that cancels the run after `after`.
    fn cancel_all_after(after: Duration) -> CancellationToken {
        let cancel_all = CancellationToken::new();
        let cancel = cancel_all.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            cancel.cancel();
        });
        cancel_all
    }

    /// Each recorded step's kind, parallel member, and status.
    fn statuses(result: &TestResult) -> Vec<(&str, Option<usize>, StepStatus)> {
        result
            .assertion_results
            .iter()
            .map(|step| (step.name.as_str(), step.member, step.status))
            .collect()
    }

    fn phase_names(result: &TestResult) -> Vec<&str> {
        result.phase_timings.iter().map(|t| t.phase.as_str()).collect()
    }

    /// Returns registered resource labels and their cleanup results.
    fn reclaimed(result: &TestResult) -> Vec<(&str, ReclaimResult)> {
        result
            .reclaim
            .iter()
            .map(|entry| (entry.resource.as_str(), entry.result))
            .collect()
    }

    /// Reads the test's saved `result.json`.
    fn written_report(result: &TestResult) -> serde_json::Value {
        let log_dir = result
            .log_dir
            .as_ref()
            .expect("the runner should record the log directory");
        let written = std::fs::read_to_string(log_dir.join("result.json")).expect("result.json should exist");
        serde_json::from_str(&written).expect("result.json should parse")
    }

    fn issue(kind: IssueKind, message: &str) -> Issue {
        Issue {
            kind,
            message: message.to_string(),
        }
    }

    /// Creates a test that times out in `phase` and returns on cancellation.
    fn timing_out_in(phase: &'static str) -> ScriptedTest {
        ScriptedTest {
            timeout: Duration::from_millis(50),
            phase,
            ..ScriptedTest::new("stuck", Behavior::WaitForCancel { finish_on_cancel: true })
        }
    }

    #[tokio::test]
    async fn setup_failure_before_the_test_starts_is_reported_as_an_error() {
        // A regular file where the log base directory belongs, so the test's log directory cannot be
        // created. Consumers that build the suite result from events have to see this failure, or
        // the run reports no test at all and exits zero.
        let occupied_base_dir = tempfile::NamedTempFile::new().expect("temp file should be creatable");
        let mounts_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let (tx, mut rx) = mpsc::unbounded_channel();

        let result = Runner::run_one(
            &ScriptedTest::new("stuck", Behavior::WaitForCancel { finish_on_cancel: true }),
            &tx,
            &CancellationToken::new(),
            occupied_base_dir.path().to_path_buf(),
            test_runner_settings(mounts_dir.path()),
            None,
        )
        .await;

        assert_eq!(result.outcome, TestOutcome::Errored);
        assert_eq!(result.error.as_ref().map(|e| e.kind), Some(ErrorKind::Setup));
        assert_eq!(result.case_path, Some(PathBuf::from("/cases/stuck")));

        let mut reported = None;
        while let Ok(event) = rx.try_recv() {
            if let TestEvent::TestCompleted { result, .. } = event {
                reported = Some(result);
            }
        }

        let reported = reported.expect("a setup failure should be reported as a completed test");
        assert_eq!(reported.name, "stuck");
        assert_eq!(reported.outcome, TestOutcome::Errored);
        assert_eq!(reported.error.as_ref().map(|e| e.kind), Some(ErrorKind::Setup));
    }

    #[tokio::test]
    async fn test_deadline_timeout_names_the_phase_that_was_active() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let result = run_scripted_test(
            &timing_out_in("container_start"),
            log_base_dir.path(),
            &CancellationToken::new(),
            BOUND,
        )
        .await;

        let timeout = result.timeout.expect("a timed-out test should carry timeout detail");
        assert_eq!(result.outcome, TestOutcome::TimedOut);
        assert_eq!(timeout.deadline, TimeoutDeadline::TestDeadline);
        assert_eq!(timeout.active_phase, "container_start");
        assert_eq!(timeout.configured_ms, 50);
    }

    #[tokio::test]
    async fn result_log_labels_a_timeout_and_gives_its_cause() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let result = run_scripted_test(
            &timing_out_in("container_start"),
            log_base_dir.path(),
            &CancellationToken::new(),
            BOUND,
        )
        .await;

        let log_dir = result.log_dir.expect("the runner should record the log directory");
        let result_log = std::fs::read_to_string(log_dir.join("result.log")).expect("result.log should exist");
        let mut lines = result_log.lines();
        let first = lines.next().expect("result.log should not be empty");
        assert!(first.starts_with("TIMEOUT stuck "), "first line was: {}", first);
        assert_eq!(lines.next(), Some("case budget 50ms expired in container_start"));
    }

    #[test]
    fn result_log_counts_the_steps_that_never_ran_instead_of_listing_them() {
        let log_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let [_, (failed, _), ..] = each_outcome();

        write_result_log(&failed, log_dir.path());

        let result_log = std::fs::read_to_string(log_dir.path().join("result.log")).expect("result.log should exist");
        let assertions: Vec<&str> = result_log
            .lines()
            .skip_while(|line| *line != "Assertions:")
            .skip(1)
            .take_while(|line| !line.is_empty())
            .filter(|line| !line.starts_with("    "))
            .collect();
        assert_eq!(assertions, ["  - log_contains (1.00ms)"], "{result_log}");
        assert!(!result_log.contains(NOT_RUN_KIND), "{result_log}");
        assert!(result_log.contains("\n1 step not run\n"), "{result_log}");
    }

    #[tokio::test]
    async fn a_case_deadline_interrupts_the_running_step_and_keeps_the_finished_ones() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let test = ScriptedTest {
            timeout: Duration::from_millis(300),
            ..ScriptedTest::new(
                "procedure",
                Behavior::Procedure(
                    "- assertion: log_contains\n  pattern: ready\n  timeout: 5s\n\
                     - assertion: process_stable_for\n  duration: 30s\n",
                ),
            )
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), BOUND).await;

        assert_eq!(result.outcome, TestOutcome::TimedOut);
        let timeout = result
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::TestDeadline);
        assert_eq!(timeout.active_phase, "assertions");
        assert_eq!(
            statuses(&result),
            [
                ("log_contains", None, StepStatus::Passed),
                ("process_stable_for", None, StepStatus::Interrupted),
            ]
        );
        let interrupted = &result.assertion_results[1];
        assert!(
            interrupted.message.contains("of its 30s step budget"),
            "{}",
            interrupted.message
        );
        let cause = result.cause.as_ref().expect("a stopped test should carry its cause");
        assert_eq!(cause.by, StoppedBy::CaseDeadline);
        let active: Vec<_> = cause
            .active
            .iter()
            .map(|step| (step.step, step.member, step.kind.as_str(), step.budget_ms))
            .collect();
        assert_eq!(active, [(1, None, "process_stable_for", 30_000)]);
    }

    #[tokio::test]
    async fn a_step_that_exhausts_its_own_budget_fails_the_test() {
        // The retry sleep outlasts the step budget, proving the slack preserves the assertion's own timeout message.
        // The last attempt is a request error or a probe timeout, depending on how busy the machine is.
        crate::intake::test_server::install_crypto_provider();
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let test = ScriptedTest::new(
            "procedure",
            Behavior::Procedure(
                "- assertion: http_check\n  endpoint: http://127.0.0.1:0/\n  status:\n    equal: 200\n  timeout: 200ms\n",
            ),
        );

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), BOUND).await;

        assert_eq!(result.outcome, TestOutcome::Failed);
        assert!(result.timeout.is_none());
        assert_eq!(result.assertion_results.len(), 1);
        let message = &result.assertion_results[0].message;
        assert!(
            message.starts_with(
                "HTTP endpoint 'http://127.0.0.1:0/' returns status 200. did not happen within 200ms.\nLast attempt: "
            ),
            "{message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_request_cut_short_by_the_case_deadline_is_interrupted() {
        let intake = crate::intake::test_server::MockIntake::unresponsive().await;
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let test = ScriptedTest {
            timeout: Duration::from_secs(1),
            intake: Some(intake.port()),
            ..ScriptedTest::new(
                "stalled",
                Behavior::Procedure("- assertion: intake_has_metric\n  name: some.metric\n  timeout: 30s\n"),
            )
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;

        assert_eq!(result.outcome, TestOutcome::TimedOut, "{:?}", result.error);
        let timeout = result
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::TestDeadline);
        let cause = result.cause.as_ref().expect("a stopped test should carry its cause");
        let active: Vec<_> = cause
            .active
            .iter()
            .map(|step| (step.step, step.member, step.kind.as_str()))
            .collect();
        assert_eq!(active, [(0, None, "intake_has_metric")]);
        assert_eq!(
            statuses(&result),
            [("intake_has_metric", None, StepStatus::Interrupted)]
        );
        let message = &result.assertion_results[0].message;
        assert!(
            message.starts_with(
                "Interrupted after 1.0s. The case left this step 1.0s of its 30s step budget.\n\
                 Cancelled after 1.0s waiting for the intake sidecar's metrics dump.\n\
                 No metric matching name=some.metric, tags_match=subset had arrived at the intake.\n"
            ),
            "{message}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_the_core_agent_ipc_wait_returns_at_once() {
        // Both binaries exit at once, so the Core Agent never writes its IPC credentials. The case timeout leaves
        // room to start both processes on a busy machine before the wait begins.
        let case = MacCase::new(
            &format!("name: ipc-wait\ntimeout: 3s\nprocedure:\n{LOG_MATCHES}"),
            "exit 0",
            "exit 0",
        );
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");

        let result = case.run(log_base_dir.path(), 3 * BOUND).await;

        assert_eq!(result.outcome, TestOutcome::TimedOut, "{:?}", result.error);
        let timeout = result
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::TestDeadline);
        assert_eq!(timeout.active_phase, "core_agent_ipc_ready");
        let reclaimed = reclaimed(&result);
        assert_eq!(reclaimed.len(), 2, "{reclaimed:?}");
        let state_dir = state_dir(reclaimed[1].0);
        assert!(!state_dir.exists(), "{} should be removed", state_dir.display());
        assert_eq!(
            reclaimed,
            [
                ("process:ipc-wait-core-agent", ReclaimResult::Reclaimed),
                (reclaimed[1].0, ReclaimResult::Reclaimed),
            ]
        );
        assert_eq!(
            phase_names(&result),
            ["core_agent_spawn", "core_agent_ipc_ready", "reclaim"]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_mac_case_stops_adp_then_the_core_agent_and_then_removes_its_state_directory() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let marker = log_base_dir.path().join("marker");
        let case = MacCase::new(
            &marking_case("mac-reclaim", &marker),
            &marking_script("agent", CORE_AGENT_IPC_FILES),
            &marking_script("adp", "echo ready"),
        );

        let result = case.run(log_base_dir.path(), Duration::from_secs(10)).await;

        assert_eq!(result.outcome, TestOutcome::Passed, "{:?}", result.error);
        let marked = std::fs::read_to_string(&marker).expect("the processes should have written the marker");
        assert_eq!(marked, "adp dir\nagent dir\n");
        let reclaimed = reclaimed(&result);
        assert_eq!(reclaimed.len(), 3, "{reclaimed:?}");
        let state_dir = state_dir(reclaimed[2].0);
        assert!(!state_dir.exists(), "{} should be removed", state_dir.display());
        assert_eq!(
            reclaimed,
            [
                ("process:mac-reclaim", ReclaimResult::Reclaimed),
                ("process:mac-reclaim-core-agent", ReclaimResult::Reclaimed),
                (reclaimed[2].0, ReclaimResult::Reclaimed),
            ]
        );
        assert_eq!(
            written_report(&result)["reclaim"],
            serde_json::json!([
                { "resource": "process:mac-reclaim", "result": "reclaimed" },
                { "resource": "process:mac-reclaim-core-agent", "result": "reclaimed" },
                { "resource": reclaimed[2].0, "result": "reclaimed" },
            ])
        );
        assert_eq!(result.issues, []);
        assert_eq!(
            phase_names(&result),
            [
                "core_agent_spawn",
                "core_agent_ipc_ready",
                "spawn",
                "assertions",
                "write_logs",
                "reclaim"
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_process_that_does_not_confirm_its_exit_leaks_and_keeps_the_state_directory() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let marker = log_base_dir.path().join("marker");
        // Printing `ready` after leaving ADP's process group ensures escape before cleanup starts. The subsequent
        // `sleep` holds ADP's output pipes open until it exits on its own.
        let case = MacCase::new(
            &marking_case("mac-leak", &marker),
            &marking_script("agent", CORE_AGENT_IPC_FILES),
            &marking_script(
                "adp",
                r#"perl -e '$| = 1; setpgrp(0, 0); print "ready\n"; exec @ARGV' sleep 10 &"#,
            ),
        );

        let result = case.run(log_base_dir.path(), Duration::from_secs(15)).await;

        assert_eq!(result.outcome, TestOutcome::Passed, "{:?}", result.error);
        let reclaimed = reclaimed(&result);
        assert_eq!(reclaimed.len(), 3, "{reclaimed:?}");
        let state_dir = state_dir(reclaimed[2].0);
        let kept = state_dir.exists();
        let _ = std::fs::remove_dir_all(state_dir);
        assert!(kept, "{} should be kept", state_dir.display());
        assert_eq!(
            reclaimed,
            [
                ("process:mac-leak", ReclaimResult::Leaked),
                ("process:mac-leak-core-agent", ReclaimResult::Reclaimed),
                (reclaimed[2].0, ReclaimResult::Leaked),
            ]
        );
        assert_eq!(
            result.issues,
            [
                issue(
                    IssueKind::ResourceLeaked,
                    "process:mac-leak: 'mac-leak' exited, but a descendant outside its process group still holds its \
                     output pipes"
                ),
                issue(
                    IssueKind::ResourceLeaked,
                    &format!("{}: kept because process:mac-leak was not reclaimed", reclaimed[2].0)
                ),
            ]
        );
    }

    #[tokio::test]
    async fn the_case_deadline_wins_when_the_test_returns_at_the_same_instant() {
        // Unbiased polling lets the body win roughly a third of ties. Repetition makes a false pass unlikely.
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        for run in 0..50 {
            let test = ScriptedTest {
                timeout: Duration::from_millis(10),
                ..ScriptedTest::new("tie", Behavior::ReturnAtDeadline)
            };

            let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), BOUND).await;

            assert_eq!(
                result.outcome,
                TestOutcome::TimedOut,
                "run {run} reported the body's result"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_dropped_after_its_procedure_keeps_its_steps_and_where_grace_ran_out() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let steps = "- assertion: log_contains\n  pattern: ready\n  timeout: 5s\n\
                     - assertion: log_contains\n  pattern: ready\n  timeout: 5s\n\
                     - parallel:\n  - assertion: process_stable_for\n    duration: 600s\n  \
                     - assertion: process_stable_for\n    duration: 600s\n\
                     - assertion: log_contains\n  pattern: ready\n  timeout: 5s\n";
        let test = ScriptedTest {
            timeout: Duration::from_secs(1),
            ..ScriptedTest::new(
                "dropped",
                Behavior::ProcedureThenWait {
                    steps,
                    then_phase: "cleanup",
                    returns_on_cancel: false,
                },
            )
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;

        assert_eq!(result.outcome, TestOutcome::TimedOut);
        let timeout = result
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::CleanupGrace);
        assert_eq!(timeout.active_phase, "assertions");
        assert_eq!(timeout.grace_phase.as_deref(), Some("cleanup"));
        let cause = result.cause.as_ref().expect("a stopped test should carry its cause");
        assert_eq!(cause.by, StoppedBy::CaseDeadline);
        let member = |member| ActiveStep {
            step: 2,
            member: Some(member),
            kind: "process_stable_for".to_string(),
            elapsed_ms: 1_000,
            budget_ms: 600_000,
        };
        assert_eq!(cause.active, [member(0), member(1)]);
        let expected_statuses = [
            ("log_contains", None, StepStatus::Passed),
            ("log_contains", None, StepStatus::Passed),
            ("process_stable_for", Some(0), StepStatus::Interrupted),
            ("process_stable_for", Some(1), StepStatus::Interrupted),
            ("log_contains", None, StepStatus::NotRun),
        ];
        assert_eq!(statuses(&result), expected_statuses);
        assert_eq!(phase_names(&result), ["assertions"]);

        let json = written_report(&result);
        assert_eq!(json["schema_version"], 2);
        let written_statuses: Vec<&str> = json["assertions"]
            .as_array()
            .expect("assertions is an array")
            .iter()
            .map(|step| step["status"].as_str().expect("status is a string"))
            .collect();
        assert_eq!(
            written_statuses,
            ["passed", "passed", "interrupted", "interrupted", "not_run"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn resources_are_reclaimed_after_the_runner_drops_a_body_stuck_past_grace() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let registered = tempfile::tempdir().expect("temp dir should be creatable");
        let stuck = |name: &str, behavior| ScriptedTest {
            timeout: Duration::from_secs(1),
            registers: Some(registered.path().join(name)),
            ..ScriptedTest::new(name, behavior)
        };
        let decided = stuck(
            "decided",
            Behavior::ProcedureThenWait {
                steps: LOG_MATCHES,
                then_phase: "intake_capture",
                returns_on_cancel: false,
            },
        );
        let undecided = stuck(
            "undecided",
            Behavior::WaitForCancel {
                finish_on_cancel: false,
            },
        );

        let decided = run_scripted_test(&decided, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;
        let undecided =
            run_scripted_test(&undecided, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;

        assert_eq!(decided.outcome, TestOutcome::Passed, "{:?}", decided.error);
        let issue_kinds: Vec<_> = decided.issues.iter().map(|issue| issue.kind).collect();
        assert_eq!(
            issue_kinds,
            [IssueKind::PostVerdictDeadline, IssueKind::CleanupGraceExpired]
        );
        assert_eq!(undecided.outcome, TestOutcome::TimedOut);
        let timeout = undecided
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::CleanupGrace);
        let error = undecided
            .error
            .as_ref()
            .expect("a timed-out test should carry its error");
        assert_eq!(error.message, "Test timed out after 1s and did not return within 30s.");
        for result in [&decided, &undecided] {
            let dir = registered.path().join(&result.name);
            let resource = format!("directory:{}", dir.display());
            assert_eq!(reclaimed(result), [(resource.as_str(), ReclaimResult::Reclaimed)]);
            assert!(!dir.exists(), "{} should be removed", dir.display());
            assert_eq!(phase_names(result).last(), Some(&"reclaim"), "{}", result.name);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_passing_case_keeps_its_verdict_and_stops_its_tasks_before_reclaim() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let registered = tempfile::tempdir().expect("temp dir should be creatable");
        let dir = registered.path().join("passing");
        let probe = TaskProbe::default();
        let test = ScriptedTest {
            registers: Some(dir.clone()),
            spawns: Some(probe.clone()),
            ..ScriptedTest::new("passing", Behavior::Pass)
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;

        assert_eq!(result.outcome, TestOutcome::Passed, "{:?}", result.error);
        assert_eq!(result.issues, []);
        let resource = format!("directory:{}", dir.display());
        assert_eq!(reclaimed(&result), [(resource.as_str(), ReclaimResult::Reclaimed)]);
        assert!(!dir.exists(), "{} should be removed", dir.display());
        let dropped = probe.dropped().expect("the runner should stop the test's task");
        assert!(dropped.directory_existed, "the task should stop before reclaim");
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_case_stops_its_tasks_at_the_deadline() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let registered = tempfile::tempdir().expect("temp dir should be creatable");
        let dir = registered.path().join("stuck");
        let probe = TaskProbe::default();
        let test = ScriptedTest {
            timeout: Duration::from_secs(1),
            registers: Some(dir.clone()),
            spawns: Some(probe.clone()),
            ..ScriptedTest::new(
                "stuck",
                Behavior::WaitForCancel {
                    finish_on_cancel: false,
                },
            )
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;

        assert_eq!(result.outcome, TestOutcome::TimedOut);
        let timeout = result
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::CleanupGrace);
        let dropped = probe.dropped().expect("the case deadline should stop the test's task");
        assert_eq!(dropped.elapsed, Duration::from_secs(1));
        let resource = format!("directory:{}", dir.display());
        assert_eq!(reclaimed(&result), [(resource.as_str(), ReclaimResult::Reclaimed)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_verdict_reached_before_the_runner_stops_the_test_is_kept() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let passes_then_waits = |timeout, returns_on_cancel| ScriptedTest {
            timeout,
            ..ScriptedTest::new(
                "decided",
                Behavior::ProcedureThenWait {
                    steps: LOG_MATCHES,
                    then_phase: "intake_capture",
                    returns_on_cancel,
                },
            )
        };
        let deadline_issue = issue(
            IssueKind::PostVerdictDeadline,
            "case budget 1s expired in intake_capture after the verdict",
        );
        let grace_issue = issue(
            IssueKind::CleanupGraceExpired,
            "cleanup grace 30s expired in intake_capture",
        );
        let cases = [
            (
                "timer, returns",
                passes_then_waits(Duration::from_secs(1), true),
                CancellationToken::new(),
                vec![deadline_issue.clone()],
            ),
            (
                "timer, never returns",
                passes_then_waits(Duration::from_secs(1), false),
                CancellationToken::new(),
                vec![deadline_issue, grace_issue],
            ),
            (
                "signal, returns",
                passes_then_waits(Duration::from_secs(60), true),
                cancel_all_after(Duration::from_secs(1)),
                vec![],
            ),
        ];

        for (case, test, cancel_all, issues) in cases {
            let result = run_scripted_test(&test, log_base_dir.path(), &cancel_all, PAUSED_BOUND).await;

            assert_eq!(result.outcome, TestOutcome::Passed, "{case}: {:?}", result.error);
            assert!(result.cause.is_none(), "{case}: {:?}", result.cause);
            assert!(result.timeout.is_none(), "{case}: {:?}", result.timeout);
            assert_eq!(result.issues, issues, "{case}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_setup_error_is_kept_only_if_it_was_reached_before_the_deadline() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let setup_error = |phase, recorded| ScriptedTest {
            timeout: Duration::from_secs(1),
            phase,
            ..ScriptedTest::new("setup", Behavior::SetupError { recorded })
        };

        let recorded = run_scripted_test(
            &setup_error("cleanup", true),
            log_base_dir.path(),
            &CancellationToken::new(),
            PAUSED_BOUND,
        )
        .await;

        assert_eq!(recorded.outcome, TestOutcome::Errored);
        let error = recorded.error.as_ref().expect("a setup failure should carry its error");
        assert_eq!((error.kind, error.message.as_str()), (ErrorKind::Setup, "boom"));
        assert_eq!(
            recorded.issues,
            [issue(
                IssueKind::PostVerdictDeadline,
                "case budget 1s expired in cleanup after the verdict"
            )]
        );

        let unrecorded = run_scripted_test(
            &setup_error("intake_start", false),
            log_base_dir.path(),
            &CancellationToken::new(),
            PAUSED_BOUND,
        )
        .await;

        assert_eq!(unrecorded.outcome, TestOutcome::TimedOut, "{:?}", unrecorded.error);
        let timeout = unrecorded
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::TestDeadline);
        assert_eq!(timeout.active_phase, "intake_start");
    }

    #[tokio::test(start_paused = true)]
    async fn a_signal_whose_grace_expires_stays_interrupted() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let test = ScriptedTest {
            timeout: Duration::from_secs(60),
            ..ScriptedTest::new(
                "interrupted",
                Behavior::ProcedureThenWait {
                    steps: "- assertion: process_stable_for\n  duration: 600s\n",
                    then_phase: "cleanup",
                    returns_on_cancel: false,
                },
            )
        };

        let result = run_scripted_test(
            &test,
            log_base_dir.path(),
            &cancel_all_after(Duration::from_secs(1)),
            PAUSED_BOUND,
        )
        .await;

        assert_eq!(result.outcome, TestOutcome::Errored);
        assert_eq!(result.error.as_ref().map(|e| e.kind), Some(ErrorKind::Internal));
        assert_eq!(result.label(), "INTERRUPTED");
        let cause = result.cause.as_ref().expect("a stopped test should carry its cause");
        assert_eq!(cause.by, StoppedBy::Signal);
        let active: Vec<_> = cause
            .active
            .iter()
            .map(|step| (step.step, step.kind.as_str()))
            .collect();
        assert_eq!(active, [(0, "process_stable_for")]);
        assert_eq!(
            result.issues,
            [issue(
                IssueKind::CleanupGraceExpired,
                "cleanup grace 30s expired in cleanup"
            )]
        );
        assert!(result.timeout.is_none(), "{:?}", result.timeout);
        assert_eq!(phase_names(&result), ["assertions"]);
        assert_eq!(
            statuses(&result),
            [("process_stable_for", None, StepStatus::Interrupted)]
        );
    }

    /// A test case for the `mac` runtime using shell scripts as ADP and Core Agent binaries.
    #[cfg(unix)]
    struct MacCase {
        test: IntegrationTestCase,
        settings: RunnerSettings,
        /// Holds the scripts until the case is dropped.
        _scripts: tempfile::TempDir,
    }

    #[cfg(unix)]
    impl MacCase {
        /// Parses a `mac` case with the given Core Agent and ADP script bodies.
        fn new(case: &str, core_agent: &str, adp: &str) -> Self {
            use std::os::unix::fs::PermissionsExt as _;

            use crate::config::MAC_RUNTIME;
            use crate::image_override::ImageOverrides;
            use crate::integration::IntegrationSettings;

            let scripts = tempfile::tempdir().expect("temp dir should be creatable");
            let script = |name: &str, body: &str| {
                let path = scripts.path().join(name);
                std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("the script should be writable");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("the script should be made executable");
                path
            };
            let settings = RunnerSettings {
                adp_binary_path: script("adp", adp),
                core_agent_binary_path: script("core-agent", core_agent),
                ..test_runner_settings(scripts.path())
            };
            let config = serde_saphyr::from_str(case).expect("the case should parse");
            let test = IntegrationTestCase::new(
                config,
                &IntegrationSettings::new(MAC_RUNTIME, &ImageOverrides::new(&[])),
            );
            Self {
                test,
                settings,
                _scripts: scripts,
            }
        }

        /// Runs the case through the runner within `bound`.
        async fn run(&self, log_base_dir: &Path, bound: Duration) -> TestResult {
            run_with_settings(
                &self.test,
                self.settings.clone(),
                log_base_dir,
                &CancellationToken::new(),
                bound,
            )
            .await
        }
    }

    /// Creates empty IPC credential files so the runner's readiness check succeeds.
    #[cfg(unix)]
    const CORE_AGENT_IPC_FILES: &str = r#": > "$dir/auth_token"; : > "$dir/ipc_cert.pem""#;

    /// A passing `mac` case whose processes record their stop order in `marker`.
    #[cfg(unix)]
    fn marking_case(name: &str, marker: &Path) -> String {
        format!(
            "name: {name}\ntimeout: 30s\nenv:\n  MARKER: \"{}\"\nprocedure:\n{LOG_MATCHES}",
            marker.display()
        )
    }

    /// A process script that runs `setup` with `$dir` set to the state directory, then runs until SIGTERM.
    ///
    /// On SIGTERM, appends `role` to `$MARKER`, plus ` dir` if the state directory exists. The trap uses built-in shell
    /// commands to finish within the grace period before SIGKILL.
    #[cfg(unix)]
    fn marking_script(role: &str, setup: &str) -> String {
        format!(
            r#"dir=${{DD_AUTH_TOKEN_FILE_PATH%/*}}
trap 'if [ -d "$dir" ]; then echo "{role} dir" >> "$MARKER"; else echo {role} >> "$MARKER"; fi; exit 0' TERM
{setup}
while :; do sleep 1; done"#
        )
    }

    /// Validates and extracts a `mac` state directory from a cleanup resource label.
    #[cfg(unix)]
    #[track_caller]
    fn state_dir(resource: &str) -> &Path {
        let temp_dir = std::env::temp_dir();
        match resource.strip_prefix("directory:").map(Path::new) {
            Some(path)
                if path.parent() == Some(temp_dir.as_path())
                    && path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("panoramic-unix-")) =>
            {
                path
            }
            _ => panic!("{resource} should name a mac state directory"),
        }
    }

    #[test]
    fn windows_runtime_adds_adp_native_env_aliases() {
        let env = HashMap::from([
            ("DD_DATA_PLANE_ENABLED".to_string(), "true".to_string()),
            ("DD_DATA_PLANE_STANDALONE_MODE".to_string(), "true".to_string()),
            ("DD_DATA_PLANE_DOGSTATSD_ENABLED".to_string(), "false".to_string()),
            ("DD_DATA_PLANE_OTLP_ENABLED".to_string(), "true".to_string()),
            ("DD_DATA_PLANE_OTLP_PROXY_ENABLED".to_string(), "false".to_string()),
            (
                "DD_DATA_PLANE_OTLP_PROXY_TRACES_ENABLED".to_string(),
                "false".to_string(),
            ),
        ]);

        let normalized = normalize_env_for_runtime(env, crate::config::WINDOWS_RUNTIME);

        assert_eq!(normalized.get("DD_DATA_PLANE__ENABLED"), Some(&"true".to_string()));
        assert_eq!(
            normalized.get("DD_DATA_PLANE__STANDALONE_MODE"),
            Some(&"true".to_string())
        );
        assert_eq!(
            normalized.get("DD_DATA_PLANE__DOGSTATSD__ENABLED"),
            Some(&"false".to_string())
        );
        assert_eq!(
            normalized.get("DD_DATA_PLANE__OTLP__ENABLED"),
            Some(&"true".to_string())
        );
        assert_eq!(
            normalized.get("DD_DATA_PLANE__OTLP__PROXY__ENABLED"),
            Some(&"false".to_string())
        );
        assert_eq!(
            normalized.get("DD_DATA_PLANE__OTLP__PROXY__TRACES__ENABLED"),
            Some(&"false".to_string())
        );
        // Without an explicit DD_DATA_PLANE_REMOTE_AGENT_ENABLED in the input, the normalized
        // env should not contain a synthesized nested key either.
        assert!(!normalized.contains_key("DD_DATA_PLANE__REMOTE_AGENT_ENABLED"));
    }

    #[test]
    fn windows_runtime_preserves_explicit_adp_native_env_values() {
        let env = HashMap::from([
            ("DD_DATA_PLANE_ENABLED".to_string(), "true".to_string()),
            ("DD_DATA_PLANE__ENABLED".to_string(), "false".to_string()),
        ]);

        let normalized = normalize_env_for_runtime(env, crate::config::WINDOWS_RUNTIME);

        assert_eq!(normalized.get("DD_DATA_PLANE__ENABLED"), Some(&"false".to_string()));
    }

    #[test]
    fn linux_runtime_does_not_add_adp_native_env_aliases() {
        let env = HashMap::from([("DD_DATA_PLANE_ENABLED".to_string(), "true".to_string())]);

        let normalized = normalize_env_for_runtime(env, crate::config::LINUX_RUNTIME);

        assert!(!normalized.contains_key("DD_DATA_PLANE__ENABLED"));
    }

    #[test]
    fn windows_runtime_uses_identity_port_mappings_for_exposed_ports() {
        let exposed_ports = vec!["55100/tcp".to_string(), "58125/udp".to_string()];

        let mappings = build_port_mappings_for_runtime(
            crate::config::WINDOWS_RUNTIME,
            &exposed_ports,
            &DriverDetails::default(),
        );

        assert_eq!(mappings.get("55100/tcp"), Some(&55100));
        assert_eq!(mappings.get("58125/udp"), Some(&58125));
    }

    #[test]
    fn linux_runtime_uses_docker_host_port_mappings_for_exposed_ports() {
        let exposed_ports = vec!["55100/tcp".to_string()];

        let mappings =
            build_port_mappings_for_runtime(crate::config::LINUX_RUNTIME, &exposed_ports, &DriverDetails::default());

        assert!(mappings.is_empty());
    }

    #[test]
    fn windows_runtime_force_enables_adp_when_enabled() {
        let env = HashMap::from([("DD_DATA_PLANE_ENABLED".to_string(), "true".to_string())]);

        let normalized = normalize_env_for_runtime(env, crate::config::WINDOWS_RUNTIME);

        // The Core Agent in the Windows target container needs DD_DATA_PLANE_FORCE_ENABLE on
        // non-Linux platforms (Agent 7.80+ `sanitizeDataPlaneConfig`), otherwise it streams
        // data_plane.enabled=false to ADP and ADP exits.
        assert_eq!(normalized.get("DD_DATA_PLANE_FORCE_ENABLE"), Some(&"true".to_string()));
    }

    #[test]
    fn windows_runtime_does_not_force_enable_adp_when_disabled() {
        let env = HashMap::from([("DD_DATA_PLANE_ENABLED".to_string(), "false".to_string())]);

        let normalized = normalize_env_for_runtime(env, crate::config::WINDOWS_RUNTIME);

        assert!(!normalized.contains_key("DD_DATA_PLANE_FORCE_ENABLE"));
    }

    #[test]
    fn windows_runtime_preserves_explicit_force_enable_value() {
        let env = HashMap::from([
            ("DD_DATA_PLANE_ENABLED".to_string(), "true".to_string()),
            ("DD_DATA_PLANE_FORCE_ENABLE".to_string(), "false".to_string()),
        ]);

        let normalized = normalize_env_for_runtime(env, crate::config::WINDOWS_RUNTIME);

        assert_eq!(normalized.get("DD_DATA_PLANE_FORCE_ENABLE"), Some(&"false".to_string()));
    }

    #[test]
    fn linux_runtime_does_not_force_enable_adp() {
        let env = HashMap::from([("DD_DATA_PLANE_ENABLED".to_string(), "true".to_string())]);

        let normalized = normalize_env_for_runtime(env, crate::config::LINUX_RUNTIME);

        assert!(!normalized.contains_key("DD_DATA_PLANE_FORCE_ENABLE"));
    }
}
