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

use crate::test::{RunnerSettings, Test, TestContext};
use crate::{
    assertions::{AssertionContext, AssertionResult, LogBuffer, TargetCommand},
    config::{parse_file_spec, parse_port_spec},
    events::TestEvent,
    intake::IntakeClient,
    integration::IntegrationTestCase,
    reporter::{ErrorKind, PhaseTiming, TestResult, TimeoutAttribution},
};

/// A function that tells us whether to run a test. Used to filter for the desired test or tests.
pub(crate) type TestFilter = Box<dyn Fn(&dyn Test) -> bool + Send>;

/// A sender that the `Runner` can use to send `TestEvent`s.
pub(crate) type EventSender = mpsc::UnboundedSender<TestEvent>;

/// The amount of time a test has to clean up after cancellation or timing out.
const GRACE_TIME: Duration = Duration::from_secs(30);

/// The amount of time the intake sidecar has to report healthy before the test fails.
const INTAKE_HEALTH_TIMEOUT: Duration = Duration::from_secs(60);

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

    /// A function to filter tests.
    filter: Option<TestFilter>,

    /// Reports run progress and results to the collector.
    event_sender: EventSender,

    /// Requests run cancellation.
    ///
    /// The consume loop cancels this on an interrupt. Active tests get `GRACE_TIME` to clean up.
    cancel_all: CancellationToken,
}

impl RunArgs {
    pub(crate) fn new(cancel_all: CancellationToken, event_sender: EventSender) -> Self {
        Self {
            parallelism: NonZeroUsize::MIN,
            fail_fast: false,
            filter: None,
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

    pub(crate) fn with_filter(mut self, filter: TestFilter) -> Self {
        self.filter = Some(filter);
        self
    }
}

/// Owns the set of registered tests and orchestrates their execution.
///
/// Handles test filtering, parallelism, timeout enforcement, cancellation propagation,
/// log directory creation, and event dispatch. Individual test logic lives in the `Test`
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

    /// Runs selected tests, reporting progress and results as events.
    pub(crate) async fn run_tests(&self, args: RunArgs) {
        let RunArgs {
            parallelism,
            fail_fast,
            filter,
            event_sender,
            cancel_all,
        } = args;

        let tests: Vec<&dyn Test> = self
            .tests
            .iter()
            .filter(|t| filter.as_ref().is_none_or(|f| f(t.as_ref())))
            .map(|t| t.as_ref())
            .collect();

        let _ = event_sender.send(TestEvent::RunStarted {
            tests: tests.iter().map(|t| t.name()).collect(),
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

        let mut tctx = TestContext::new(test_cancel.clone(), log_dir.clone(), settings);
        if test.runtime() == "kubernetes_in_docker" {
            if let Some(rx) = kind_ready {
                tctx = tctx.with_kind_ready(rx);
            }
        }

        let _ = event_sender.send(TestEvent::TestStarted { name: name.clone() });

        let phases = tctx.phases.clone();

        let run_fut = test.run(tctx);
        pin!(run_fut);

        // Run the test for the duration of 'timeout', then send a cancel request if it times out and wait GRACE_TIME
        // for teardown.
        let mut result = tokio::select! {
            r = &mut run_fut => r,
            _ = tokio::time::sleep(timeout) => {
                // Read the active phase before cancelling, since teardown moves the test on from the
                // phase the deadline interrupted.
                let active_phase = phases.active();
                test_cancel.cancel();
                match tokio::time::timeout(GRACE_TIME, run_fut).await {
                    // The test tore down in time, so it brings back its own phase timings and
                    // whatever assertions it finished.
                    Ok(r) => r
                        .with_harness_error(ErrorKind::Timeout, format!("Test timed out after {:?}.", timeout))
                        .with_timeout_attribution(TimeoutAttribution::test_deadline(timeout, active_phase)),
                    Err(_) => TestResult::hard_timeout(name, timeout, started.elapsed(), active_phase, phases.completed())
                }
            }
            // We received a request from above to kill all tests, so send the cancellation and wait GRACE_TIME.
            _ = cancel_all.cancelled() => {
                test_cancel.cancel();
                match tokio::time::timeout(GRACE_TIME, run_fut).await {
                    Ok(r) => r.with_harness_error(ErrorKind::Internal, "Test was cancelled."),
                    Err(_) => TestResult::cancellation_failure(name, GRACE_TIME, started.elapsed())
                }
            }
        };

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

// ----------------------------------------------------------------------------
// IntegrationRunner: single integration test case execution
// ----------------------------------------------------------------------------

/// Generates a random isolation group ID.
fn generate_isolation_group_id() -> String {
    use rand::RngExt as _;
    let mut rng = rand::rng();
    let chars: String = (0..8)
        .map(|_| {
            let idx = rng.random_range(0..36);
            if idx < 10 {
                (b'0' + idx) as char
            } else {
                (b'a' + idx - 10) as char
            }
        })
        .collect();
    chars
}

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
            isolation_group_id: generate_isolation_group_id(),
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
                    let _ = self.cleanup().await;
                    return TestResult::errored(
                        test_name,
                        ErrorKind::Setup,
                        format!("Failed to start intake sidecar: {}", e),
                        started.elapsed(),
                        phase_timings,
                    );
                }
            }
        }

        // Create and start the driver.
        let phase = self.tctx.phases.enter("container_start");
        debug!(test = %test_name, "Creating container driver...");
        let mut driver = match Driver::from_config(self.isolation_group_id.clone(), driver_config) {
            Ok(driver) => driver,
            Err(e) => {
                error!(test = %test_name, error = %e, "Failed to create container driver.");
                phase_timings.push(phase.finish());
                // An intake sidecar started above already owns a container and the isolation group's
                // network, so tear the group down before giving up on the test.
                let _ = self.cleanup().await;
                return TestResult::errored(
                    test_name,
                    ErrorKind::Setup,
                    format!("Failed to create driver: {}", e),
                    started.elapsed(),
                    phase_timings,
                );
            }
        };

        info!(test = %test_name, "Starting container...");
        let details = match driver.start().await {
            Ok(details) => details,
            Err(e) => {
                error!(test = %test_name, error = %e, "Failed to start container.");
                phase_timings.push(phase.finish());
                let _ = self.cleanup().await;
                return TestResult::errored(
                    test_name,
                    ErrorKind::Setup,
                    format!("Failed to start container: {}", e),
                    started.elapsed(),
                    phase_timings,
                );
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
                crate::dynamic_vars::read_resolved_vars(&driver).await
            };

            match resolved_vars {
                Ok(vars) => {
                    // Fail on empty values — indicates the init script command failed.
                    for (key, value) in &vars {
                        if value.is_empty() {
                            error!(test = %test_name, key = key, "Dynamic variable resolved to empty string.");
                            phase_timings.push(phase.finish());
                            let _ = self.cleanup().await;
                            return TestResult::errored(
                                test_name,
                                ErrorKind::Setup,
                                format!(
                                    "Dynamic variable PANORAMIC_DYNAMIC_{} resolved to an empty string. \
                                     The shell command in the test config likely failed.",
                                    key
                                ),
                                started.elapsed(),
                                phase_timings,
                            );
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
                        let _ = self.cleanup().await;
                        return TestResult::errored(
                            test_name,
                            ErrorKind::Setup,
                            format!(
                                "Unresolved dynamic variable placeholders in assertions: {}. \
                                 Check that matching PANORAMIC_DYNAMIC_* env vars are defined.",
                                unresolved.join(", ")
                            ),
                            started.elapsed(),
                            phase_timings,
                        );
                    }
                }
                Err(e) => {
                    error!(test = %test_name, error = %e, "Failed to resolve dynamic variables.");
                    phase_timings.push(phase.finish());
                    let _ = self.cleanup().await;
                    return TestResult::errored(
                        test_name,
                        ErrorKind::Setup,
                        format!("Failed to resolve dynamic variables: {}", e),
                        started.elapsed(),
                        phase_timings,
                    );
                }
            }

            phase_timings.push(phase.finish());
        }

        // Run assertions. Timeout is handled by the Runner, which calls cancel() on this test
        // if the deadline is exceeded.
        info!(
            test = %test_name,
            assertion_count = self.test_case.total_assertion_count(),
            "Running assertions..."
        );

        let phase = self.tctx.phases.enter("assertions");
        let test_cancel = self.tctx.test_cancel_token();

        // If we are canceled while running our assertions, we return early to respect cancellation.
        let assertion_results = tokio::select! {
            results = self.run_assertions(&port_mappings, details.container_ip(), &container_name, &exit_token, docker_exit_code, intake_host_port) => results,
            _ = test_cancel.cancelled() => vec![AssertionResult {
                name: "cancelled".to_string(),
                passed: false,
                message: "Test was cancelled.".to_string(),
                duration: phase.elapsed(),
            }],
        };
        phase_timings.push(phase.finish());

        // Cancel the exit monitor.
        exit_handle.abort();

        // Determine if the test passed.
        let passed = assertion_results.iter().all(|r| r.passed);
        let passed_count = assertion_results.iter().filter(|r| r.passed).count();
        let failed_count = assertion_results.len() - passed_count;

        if passed {
            info!(
                test = %test_name,
                assertions_passed = passed_count,
                "All assertions passed."
            );
        } else {
            error!(
                test = %test_name,
                assertions_passed = passed_count,
                assertions_failed = failed_count,
                "Some assertions failed."
            );
        }

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

        // Cleanup.
        let phase = self.tctx.phases.enter("cleanup");
        debug!(test = %test_name, "Cleaning up container and resources...");
        if let Err(e) = self.cleanup().await {
            warn!(test = %test_name, error = %e, "Failed to clean up resources.");
        }
        debug!(test = %test_name, "Cleanup complete.");
        phase_timings.push(phase.finish());

        info!(
            test = %test_name,
            passed = passed,
            duration = ?started.elapsed(),
            "Test case completed."
        );

        TestResult::from_assertions(test_name, started.elapsed(), assertion_results, phase_timings)
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
        let details = driver.start().await?;

        // The health wait polls until it gets an answer, so bound it and watch for cancellation. A sidecar that never
        // reports healthy would otherwise hold the test past its teardown grace period, leaking the isolation group.
        let test_cancel = self.tctx.test_cancel_token();
        tokio::select! {
            result = driver.wait_for_container_healthy() => {
                result.error_context("Intake sidecar failed its health check.")?
            }
            _ = tokio::time::sleep(INTAKE_HEALTH_TIMEOUT) => return Err(generic_error!(
                "Intake sidecar did not report healthy within {:?}.",
                INTAKE_HEALTH_TIMEOUT
            )),
            _ = test_cancel.cancelled() => return Err(generic_error!(
                "Canceled while waiting for the intake sidecar to report healthy."
            )),
        }

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
    ) -> Vec<AssertionResult> {
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
        crate::assertions::run_assertion_steps(&self.test_case, &ctx).await
    }

    async fn cleanup(&self) -> Result<(), GenericError> {
        // Cancel any running operations holding children of cancel token.
        self.tctx.test_cancel_token().cancel();

        // Give background tasks a moment to notice cancellation.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Clean up all resources in the isolation group.
        Driver::clean_related_resources(self.isolation_group_id.clone())
            .await
            .error_context("Failed to clean up isolation group resources.")?;

        Ok(())
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

    if !result.assertion_results.is_empty() {
        let _ = writeln!(f);
        let _ = writeln!(f, "Assertions:");
        for (i, assertion) in result.assertion_results.iter().enumerate() {
            let indicator = if assertion.passed { "+" } else { "-" };
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
    fn hard_timeout(
        name: impl Into<String>, timeout: Duration, total_duration: Duration, active_phase: Option<String>,
        phase_timings: Vec<PhaseTiming>,
    ) -> Self {
        Self::errored(
            name,
            ErrorKind::Timeout,
            format!(
                "Test timed out after {:?} and did not clean up its resources within {:?}.",
                timeout, GRACE_TIME
            ),
            total_duration,
            phase_timings,
        )
        .with_timeout_attribution(TimeoutAttribution::cleanup_grace(GRACE_TIME, timeout, active_phase))
    }

    fn cancellation_failure(name: impl Into<String>, grace: Duration, total_duration: Duration) -> Self {
        Self::errored(
            name,
            ErrorKind::Internal,
            format!(
                "Test was cancelled and failed to clean up its resources with a grace period of {:?}.",
                grace
            ),
            total_duration,
            vec![],
        )
    }

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
    }

    /// A test that enters a phase and then behaves as scripted.
    pub(crate) struct ScriptedTest {
        pub(crate) name: String,
        pub(crate) timeout: Duration,
        pub(crate) phase: &'static str,
        pub(crate) behavior: Behavior,
        /// Notified once the test has entered its phase.
        pub(crate) entered: Option<Arc<Notify>>,
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
            // The phase is deliberately left active: a test that overruns its deadline does not get
            // to finish the phase it was in.
            let _phase = tctx.phases.enter(self.phase);
            if let Some(entered) = &self.entered {
                entered.notify_one();
            }

            let assertion = |passed| AssertionResult {
                name: "scripted".to_string(),
                passed,
                message: String::new(),
                duration: Duration::from_millis(1),
            };
            match self.behavior {
                Behavior::Pass => {
                    TestResult::from_assertions(&self.name, Duration::from_millis(1), vec![assertion(true)], Vec::new())
                }
                Behavior::Fail => TestResult::from_assertions(
                    &self.name,
                    Duration::from_millis(1),
                    vec![assertion(false)],
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
            }
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
    use super::test_support::{test_runner_settings, Behavior, ScriptedTest};
    use super::*;
    use crate::reporter::{DurationMs, PhaseTracker, TestOutcome, TimeoutDeadline};

    /// Runs one test to completion in `log_base_dir`, discarding its events.
    async fn run_scripted_test(test: ScriptedTest, log_base_dir: &Path) -> TestResult {
        let mounts_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let (tx, _rx) = mpsc::unbounded_channel();

        Runner::run_one(
            &test,
            &tx,
            &CancellationToken::new(),
            log_base_dir.to_path_buf(),
            test_runner_settings(mounts_dir.path()),
            None,
        )
        .await
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
        let result = run_scripted_test(timing_out_in("container_start"), log_base_dir.path()).await;

        let timeout = result.timeout.expect("a timed-out test should carry timeout detail");
        assert_eq!(result.outcome, TestOutcome::TimedOut);
        assert_eq!(timeout.deadline, TimeoutDeadline::TestDeadline);
        assert_eq!(timeout.active_phase, "container_start");
        assert_eq!(timeout.configured_ms, 50);
    }

    #[tokio::test]
    async fn result_log_labels_a_timeout_and_gives_its_cause() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let result = run_scripted_test(timing_out_in("container_start"), log_base_dir.path()).await;

        let log_dir = result.log_dir.expect("the runner should record the log directory");
        let result_log = std::fs::read_to_string(log_dir.join("result.log")).expect("result.log should exist");
        let mut lines = result_log.lines();
        let first = lines.next().expect("result.log should not be empty");
        assert!(first.starts_with("TIMEOUT stuck "), "first line was: {}", first);
        assert_eq!(lines.next(), Some("case budget 50ms expired in container_start"));
    }

    #[test]
    fn cleanup_grace_timeout_keeps_the_phase_and_the_timings_the_test_left_behind() {
        // Stands in for a test whose future never came back: the runner has only what the shared
        // tracker holds.
        let phases = PhaseTracker::default();
        phases.enter("container_start").finish();
        let _stuck_in = phases.enter("assertions");

        let result = TestResult::hard_timeout(
            "stuck",
            Duration::from_secs(60),
            Duration::from_secs(90),
            phases.active(),
            phases.completed(),
        );

        let timeout = result.timeout.expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::CleanupGrace);
        assert_eq!(timeout.active_phase, "assertions");
        assert_eq!(timeout.configured_ms, DurationMs(GRACE_TIME).as_millis());
        assert_eq!(timeout.test_deadline_ms, 60_000);
        assert_eq!(
            result
                .phase_timings
                .iter()
                .map(|t| t.phase.as_str())
                .collect::<Vec<_>>(),
            vec!["container_start"]
        );
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
