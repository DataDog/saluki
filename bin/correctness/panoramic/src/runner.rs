//! Test execution.
//!
//! This module provides the single entry point for running tests. The same code path
//! is used regardless of output mode (TUI or plain). Events are emitted to a channel
//! and consumed by either a TUI renderer or logging consumer.

use std::{
    collections::HashMap,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use airlock::driver::{Driver, DriverConfig, DriverDetails};
use bollard::container::LogOutput;
use futures::{
    future,
    stream::{self, StreamExt as _},
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::sync::{mpsc, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::test::{Test, TestContext};
use crate::{
    assertions::{create_assertion, AssertionContext, AssertionResult, LogBuffer},
    config::{parse_file_spec, parse_port_spec, AssertionStep, IntegrationConfig},
    events::TestEvent,
    reporter::{PhaseTiming, TestResult},
};

/// A function that tells us whether to run a test. Used to filter for the desired test or tests.
pub(crate) type TestFilter = Box<dyn Fn(&dyn Test) -> bool + Send>;

/// A sender that the `Runner` can use to send `TestEvent`s.
pub(crate) type EventSender = mpsc::UnboundedSender<TestEvent>;

/// The amount of time a test has to clean up after cancellation or timing out.
const GRACE_TIME: Duration = Duration::from_secs(5);

pub(crate) struct RunArgs {
    /// The number of tests to run in parallel.
    parallelism: usize,

    /// Whether to stop execution at the first failure.
    fail_fast: bool,

    /// A function to filter tests.
    filter: Option<TestFilter>,

    /// A channel that the runner can use to send events out to the caller.
    event_sender: Option<EventSender>,

    /// The token to be used for graceful shutdown.
    cancel_token: CancellationToken,
}

impl RunArgs {
    pub(crate) fn new(cancel_token: CancellationToken) -> Self {
        Self {
            parallelism: 1,
            fail_fast: false,
            filter: None,
            event_sender: None,
            cancel_token,
        }
    }

    pub(crate) fn with_parallelism(mut self, parallelism: usize) -> Self {
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

    pub(crate) fn with_event_sender(mut self, sender: EventSender) -> Self {
        self.event_sender = Some(sender);
        self
    }
}

/// A registry and runner of tests.
pub(crate) struct Runner {
    tests: Vec<Box<dyn Test>>,
    log_base_dir: PathBuf,
    mounts_dir: PathBuf,
}

impl Runner {
    pub(crate) fn new(log_base_dir: impl Into<PathBuf>, mounts_dir: impl Into<PathBuf>) -> Self {
        Self {
            tests: Vec::new(),
            log_base_dir: log_base_dir.into(),
            mounts_dir: mounts_dir.into(),
        }
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

    /// Run all registered tests, emitting events and returning results.
    pub(crate) async fn run_tests(&self, args: RunArgs) -> Vec<TestResult> {
        let RunArgs {
            parallelism,
            fail_fast,
            filter,
            event_sender,
            cancel_token,
        } = args;

        let tests: Vec<&dyn Test> = self
            .tests
            .iter()
            .filter(|t| filter.as_ref().is_none_or(|f| f(t.as_ref())))
            .map(|t| t.as_ref())
            .collect();

        if let Some(ref tx) = event_sender {
            let _ = tx.send(TestEvent::RunStarted {
                total_tests: tests.len(),
            });
        }

        let parallelism = parallelism.max(1);
        let semaphore = Arc::new(Semaphore::new(parallelism));

        let results = if fail_fast {
            self.run_fail_fast(&tests, &semaphore, &event_sender, &cancel_token)
                .await
        } else {
            self.run_parallel(&tests, parallelism, &semaphore, &event_sender, &cancel_token)
                .await
        };

        if let Some(ref tx) = event_sender {
            let _ = tx.send(TestEvent::AllDone);
        }
        results
    }

    async fn run_fail_fast(
        &self, tests: &[&dyn Test], semaphore: &Semaphore, event_sender: &Option<EventSender>,
        cancel_token: &CancellationToken,
    ) -> Vec<TestResult> {
        let mut results = Vec::new();

        for test in tests {
            if cancel_token.is_cancelled() {
                break;
            }

            let _permit = semaphore.acquire().await.unwrap();
            let result = Self::run_one(
                *test,
                event_sender,
                cancel_token,
                self.log_base_dir.clone(),
                self.mounts_dir.clone(),
            )
            .await;
            let failed = !result.passed;
            results.push(result);

            if failed {
                break;
            }
        }

        results
    }

    async fn run_parallel(
        &self, tests: &[&dyn Test], parallelism: usize, semaphore: &Arc<Semaphore>, event_sender: &Option<EventSender>,
        cancel_token: &CancellationToken,
    ) -> Vec<TestResult> {
        let mut futures = stream::FuturesUnordered::new();
        let mut results = Vec::new();

        for test in tests {
            let semaphore = semaphore.clone();
            let cancel = cancel_token.clone();
            let log_base_dir = self.log_base_dir.clone();
            let mounts_dir = self.mounts_dir.clone();
            futures.push(async move {
                if cancel.is_cancelled() {
                    return None;
                }

                let _permit = semaphore.acquire().await.unwrap();

                if cancel.is_cancelled() {
                    return None;
                }

                Some(Self::run_one(*test, event_sender, &cancel, log_base_dir, mounts_dir).await)
            });

            while futures.len() >= parallelism {
                if let Some(Some(result)) = futures.next().await {
                    results.push(result);
                }
            }
        }

        while let Some(r) = futures.next().await {
            if let Some(result) = r {
                results.push(result);
            }
        }
        results
    }

    async fn run_one(
        test: &dyn Test, event_sender: &Option<EventSender>, program_cancel: &CancellationToken, log_base_dir: PathBuf,
        mounts_dir: PathBuf,
    ) -> TestResult {
        let name = test.name();
        let suite = test.suite();
        let timeout = test.timeout();
        let started = Instant::now();

        // This is the cancellation token that we will use to tell this individual test to stop and cleanup. Not to be
        // confused with program_cancel which is how we receive a signal from the top level to stop (ctrl-c).
        let test_cancel = CancellationToken::new();

        // Create a directory for the test to write logs into and pass it into the test context.
        let log_dir = log_base_dir.join(format!("{:?}", suite).to_lowercase()).join(&name);
        if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
            return TestResult::setup_error(
                name,
                started.elapsed(),
                format!("Error creating log directory at {}: {e}", log_dir.display()),
            );
        }

        let tctx = TestContext::new(test_cancel.clone(), log_dir.clone(), mounts_dir);

        if let Some(ref tx) = event_sender {
            let _ = tx.send(TestEvent::TestStarted { name: name.clone() });
        }

        let run_fut = test.run(tctx);
        tokio::pin!(run_fut);

        let result = tokio::select! {
            r = &mut run_fut => r,
            _ = tokio::time::sleep(timeout) => {
                test_cancel.cancel();
                match tokio::time::timeout(GRACE_TIME, run_fut).await {
                    Ok(r) => r,
                    Err(_) => TestResult::hard_timeout(name, timeout, started.elapsed())
                }
            }
            _ = program_cancel.cancelled() => {
                test_cancel.cancel();
                match tokio::time::timeout(GRACE_TIME, run_fut).await {
                    Ok(r) => r,
                    Err(_) => TestResult::cancellation_failure(name, GRACE_TIME, started.elapsed())
                }
            }
        };

        write_result_log(&result, &log_dir);

        if let Some(ref tx) = event_sender {
            let _ = tx.send(TestEvent::TestCompleted {
                result: result.clone(),
                log_dir,
            });
        }

        result
    }
}

// ----------------------------------------------------------------------------
// IntegrationRunner: single integration test case execution
// ----------------------------------------------------------------------------

/// Generates a random isolation group ID.
fn generate_isolation_group_id() -> String {
    use rand::Rng as _;
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
    test_case: IntegrationConfig,
    isolation_group_id: String,
    tctx: TestContext,
    log_buffer: Arc<RwLock<LogBuffer>>,
}

impl IntegrationRunner {
    /// Create a new test runner for the given test case.
    pub(crate) fn new(test_case: IntegrationConfig, tctx: TestContext) -> Self {
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
        let test_name = self.test_case.name.clone();
        let mut phase_timings = Vec::new();

        info!(
            test = %test_name,
            isolation_group = %self.isolation_group_id,
            "Starting test case."
        );

        // Build the driver configuration.
        debug!(test = %test_name, "Building driver configuration...");
        let phase_start = Instant::now();
        let driver_config = match self.build_driver_config().await {
            Ok(config) => config,
            Err(e) => {
                error!(test = %test_name, error = %e, "Failed to build driver configuration.");
                phase_timings.push(PhaseTiming {
                    phase: "driver_config_build".to_string(),
                    duration: phase_start.elapsed(),
                });
                return TestResult {
                    name: test_name,
                    passed: false,
                    duration: started.elapsed(),
                    assertion_results: vec![],
                    error: Some(format!("Failed to build driver configuration: {}", e)),
                    phase_timings,
                    assertion_details: vec![],
                };
            }
        };
        phase_timings.push(PhaseTiming {
            phase: "driver_config_build".to_string(),
            duration: phase_start.elapsed(),
        });

        // Create and start the driver.
        let phase_start = Instant::now();
        debug!(test = %test_name, "Creating container driver...");
        let mut driver = match Driver::from_config(self.isolation_group_id.clone(), driver_config) {
            Ok(driver) => driver,
            Err(e) => {
                error!(test = %test_name, error = %e, "Failed to create container driver.");
                phase_timings.push(PhaseTiming {
                    phase: "container_start".to_string(),
                    duration: phase_start.elapsed(),
                });
                return TestResult {
                    name: test_name,
                    passed: false,
                    duration: started.elapsed(),
                    assertion_results: vec![],
                    error: Some(format!("Failed to create driver: {}", e)),
                    phase_timings,
                    assertion_details: vec![],
                };
            }
        };

        info!(test = %test_name, "Starting container...");
        let details = match driver.start().await {
            Ok(details) => details,
            Err(e) => {
                error!(test = %test_name, error = %e, "Failed to start container.");
                phase_timings.push(PhaseTiming {
                    phase: "container_start".to_string(),
                    duration: phase_start.elapsed(),
                });
                let _ = self.cleanup(&driver).await;
                return TestResult {
                    name: test_name,
                    passed: false,
                    duration: started.elapsed(),
                    assertion_results: vec![],
                    error: Some(format!("Failed to start container: {}", e)),
                    phase_timings,
                    assertion_details: vec![],
                };
            }
        };
        phase_timings.push(PhaseTiming {
            phase: "container_start".to_string(),
            duration: phase_start.elapsed(),
        });

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
        let container_name = details.container_name().to_string();
        let container_name_for_exit = container_name.clone();
        let exit_handle = tokio::spawn(async move {
            let docker = match airlock::docker::connect() {
                Ok(d) => d,
                Err(_) => return,
            };

            let mut wait_stream = docker.wait_container::<String>(&container_name_for_exit, None);
            if wait_stream.next().await.is_some() {
                exit_signal.cancel();
            }
        });

        // Build port mappings for assertions.
        let port_mappings = self.build_port_mappings(&details);

        // Resolve dynamic variables if any PANORAMIC_DYNAMIC_* env vars are defined.
        if crate::dynamic_vars::has_dynamic_vars(&self.test_case) {
            let phase_start = Instant::now();
            debug!(test = %test_name, "Resolving dynamic variables...");

            match crate::dynamic_vars::read_resolved_vars(&driver).await {
                Ok(vars) => {
                    // Fail on empty values — indicates the init script command failed.
                    for (key, value) in &vars {
                        if value.is_empty() {
                            error!(test = %test_name, key = key, "Dynamic variable resolved to empty string.");
                            phase_timings.push(PhaseTiming {
                                phase: "dynamic_vars".to_string(),
                                duration: phase_start.elapsed(),
                            });
                            let _ = self.cleanup(&driver).await;
                            return TestResult {
                                name: test_name,
                                passed: false,
                                duration: started.elapsed(),
                                assertion_results: vec![],
                                error: Some(format!(
                                    "Dynamic variable PANORAMIC_DYNAMIC_{} resolved to an empty string. \
                                     The shell command in the test config likely failed.",
                                    key
                                )),
                                phase_timings,
                                assertion_details: vec![],
                            };
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
                        phase_timings.push(PhaseTiming {
                            phase: "dynamic_vars".to_string(),
                            duration: phase_start.elapsed(),
                        });
                        let _ = self.cleanup(&driver).await;
                        return TestResult {
                            name: test_name,
                            passed: false,
                            duration: started.elapsed(),
                            assertion_results: vec![],
                            error: Some(format!(
                                "Unresolved dynamic variable placeholders in assertions: {}. \
                                 Check that matching PANORAMIC_DYNAMIC_* env vars are defined.",
                                unresolved.join(", ")
                            )),
                            phase_timings,
                            assertion_details: vec![],
                        };
                    }
                }
                Err(e) => {
                    error!(test = %test_name, error = %e, "Failed to resolve dynamic variables.");
                    phase_timings.push(PhaseTiming {
                        phase: "dynamic_vars".to_string(),
                        duration: phase_start.elapsed(),
                    });
                    let _ = self.cleanup(&driver).await;
                    return TestResult {
                        name: test_name,
                        passed: false,
                        duration: started.elapsed(),
                        assertion_results: vec![],
                        error: Some(format!("Failed to resolve dynamic variables: {}", e)),
                        phase_timings,
                        assertion_details: vec![],
                    };
                }
            }

            phase_timings.push(PhaseTiming {
                phase: "dynamic_vars".to_string(),
                duration: phase_start.elapsed(),
            });
        }

        // Run assertions. Timeout is handled by the Runner, which calls cancel() on this test
        // if the deadline is exceeded.
        info!(
            test = %test_name,
            assertion_count = self.test_case.total_assertion_count(),
            "Running assertions..."
        );

        let phase_start = Instant::now();
        let cancel_token = self.tctx.cancel();
        let assertion_results = tokio::select! {
            results = self.run_assertions(&port_mappings, &container_name, &exit_token) => results,
            _ = cancel_token.cancelled() => vec![AssertionResult {
                name: "cancelled".to_string(),
                passed: false,
                message: "Test was cancelled.".to_string(),
                duration: phase_start.elapsed(),
            }],
        };
        phase_timings.push(PhaseTiming {
            phase: "assertions".to_string(),
            duration: phase_start.elapsed(),
        });

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

        // Write logs to disk if configured.
        let phase_start = Instant::now();
        if let Err(e) = self.write_logs(&test_name).await {
            warn!(test = %test_name, error = %e, "Failed to write container logs to disk.");
        }
        debug!(test = %test_name, "Wrote container logs to disk.");
        phase_timings.push(PhaseTiming {
            phase: "write_logs".to_string(),
            duration: phase_start.elapsed(),
        });

        // Cleanup.
        let phase_start = Instant::now();
        debug!(test = %test_name, "Cleaning up container and resources...");
        if let Err(e) = self.cleanup(&driver).await {
            warn!(test = %test_name, error = %e, "Failed to clean up resources.");
        }
        debug!(test = %test_name, "Cleanup complete.");
        phase_timings.push(PhaseTiming {
            phase: "cleanup".to_string(),
            duration: phase_start.elapsed(),
        });

        info!(
            test = %test_name,
            passed = passed,
            duration = ?started.elapsed(),
            "Test case completed."
        );

        TestResult {
            name: test_name,
            passed,
            duration: started.elapsed(),
            assertion_results,
            error: None,
            phase_timings,
            assertion_details: vec![],
        }
    }

    async fn build_driver_config(&self) -> Result<DriverConfig, GenericError> {
        let container = &self.test_case.container;

        // Convert env vars to the format expected by airlock.
        let env_vars: Vec<String> = container.env.iter().map(|(k, v)| format!("{}={}", k, v)).collect();

        // Build the target config.
        let target_config = airlock::config::TargetConfig {
            image: container.image.clone(),
            entrypoint: container.entrypoint.clone(),
            command: container.command.clone(),
            additional_env_vars: env_vars,
        };

        let mut config = DriverConfig::target("target", target_config).await?;

        // Apply panoramic's read-only file overlays before any test-specific bind mounts.
        config = crate::mounts::apply_target_mounts(config, self.tctx.mounts_dir())?;

        // Add file mounts.
        for file_spec in &container.files {
            let (host_path, container_path) = parse_file_spec(file_spec)?;
            let absolute_host_path = self.test_case.resolve_path(host_path);

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

    fn build_port_mappings(&self, details: &DriverDetails) -> HashMap<String, u16> {
        let mut mappings = HashMap::new();

        for port_spec in &self.test_case.container.exposed_ports {
            if let Ok((port, protocol)) = parse_port_spec(port_spec) {
                if let Some(host_port) = details.try_get_exposed_port(protocol, port) {
                    mappings.insert(format!("{}/{}", port, protocol), host_port);
                }
            }
        }

        mappings
    }

    async fn start_log_capture(&self, container_name: &str) -> Result<(), GenericError> {
        let docker = airlock::docker::connect()?;
        let log_buffer = self.log_buffer.clone();
        let container_name = container_name.to_string();

        let logs_options = bollard::container::LogsOptions::<String> {
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
                        let mut buffer = log_buffer.write().await;
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
        &self, port_mappings: &HashMap<String, u16>, container_name: &str, exit_token: &CancellationToken,
    ) -> Vec<AssertionResult> {
        let mut results = Vec::new();
        let total_steps = self.test_case.assertions.len();

        let ctx = AssertionContext {
            log_buffer: self.log_buffer.clone(),
            container_exit_token: exit_token.clone(),
            cancel_token: self.tctx.cancel(),
            port_mappings: port_mappings.clone(),
            container_name: container_name.to_string(),
        };

        for (step_index, step) in self.test_case.assertions.iter().enumerate() {
            match step {
                AssertionStep::Single(assertion_config) => {
                    let assertion = match create_assertion(assertion_config) {
                        Ok(a) => a,
                        Err(e) => {
                            error!(error = %e, "Failed to create assertion from configuration.");
                            results.push(AssertionResult {
                                name: "config_error".to_string(),
                                passed: false,
                                message: format!("Failed to create assertion: {}.", e),
                                duration: Duration::ZERO,
                            });
                            break;
                        }
                    };

                    debug!(
                        step = step_index + 1,
                        step_total = total_steps,
                        assertion_type = assertion.name(),
                        description = %assertion.description(),
                        "Running assertion..."
                    );

                    let result = assertion.check(&ctx).await;

                    if result.passed {
                        debug!(
                            assertion_type = assertion.name(),
                            duration = ?result.duration,
                            "Assertion passed."
                        );
                    } else {
                        debug!(
                            assertion_type = assertion.name(),
                            duration = ?result.duration,
                            message = %result.message,
                            "Assertion failed."
                        );
                    }

                    let failed = !result.passed;
                    results.push(result);

                    if failed {
                        debug!("Stopping assertion execution due to failure (fail-fast).");
                        break;
                    }
                }

                AssertionStep::Parallel { parallel } => {
                    let mut assertions = Vec::new();
                    let mut config_error = false;

                    for assertion_config in parallel {
                        match create_assertion(assertion_config) {
                            Ok(a) => assertions.push(a),
                            Err(e) => {
                                error!(error = %e, "Failed to create assertion from configuration.");
                                results.push(AssertionResult {
                                    name: "config_error".to_string(),
                                    passed: false,
                                    message: format!("Failed to create assertion: {}.", e),
                                    duration: Duration::ZERO,
                                });
                                config_error = true;
                                break;
                            }
                        }
                    }

                    if config_error {
                        break;
                    }

                    debug!(
                        step = step_index + 1,
                        step_total = total_steps,
                        assertion_count = assertions.len(),
                        "Running parallel assertion block..."
                    );

                    let futures: Vec<_> = assertions.iter().map(|a| a.check(&ctx)).collect();
                    let parallel_results = future::join_all(futures).await;

                    let any_failed = parallel_results.iter().any(|r| !r.passed);

                    for result in parallel_results {
                        debug!(
                            assertion_type = %result.name,
                            passed = result.passed,
                            duration = ?result.duration,
                            "Parallel assertion completed."
                        );
                        results.push(result);
                    }

                    if any_failed {
                        debug!("Stopping assertion execution due to failure in parallel block (fail-fast).");
                        break;
                    }
                }
            }
        }

        results
    }

    async fn cleanup(&self, _driver: &Driver) -> Result<(), GenericError> {
        // Cancel any running operations.
        self.tctx.cancel().cancel();

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
        let buffer = self.log_buffer.read().await;

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

    let status = if result.passed { "PASS" } else { "FAIL" };
    let _ = writeln!(f, "{} {} ({:.2?})", status, result.name, result.duration);

    if let Some(ref error) = result.error {
        let _ = writeln!(f, "Error: {}", error);
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

impl TestResult {
    fn hard_timeout(name: impl Into<String>, timeout: Duration, total_duration: Duration) -> Self {
        Self {
            name: name.into(),
            passed: false,
            duration: total_duration,
            assertion_results: vec![],
            error: Some(format!(
                "Test timed out after {:?} and failed to clean up resources in time.",
                timeout
            )),
            phase_timings: vec![],
            assertion_details: vec![],
        }
    }

    fn cancellation_failure(name: impl Into<String>, grace: Duration, total_duration: Duration) -> Self {
        Self {
            name: name.into(),
            passed: false,
            duration: total_duration,
            assertion_results: vec![],
            error: Some(format!(
                "Test was cancelled and failed to clean up its resources with a grace period of {:?}.",
                grace
            )),
            phase_timings: vec![],
            assertion_details: vec![],
        }
    }

    fn setup_error(name: impl Into<String>, total_duration: Duration, e: impl AsRef<str>) -> Self {
        Self {
            name: name.into(),
            passed: false,
            duration: total_duration,
            assertion_results: vec![],
            error: Some(format!(
                "Test failed to start due to an error during setup. {}",
                e.as_ref()
            )),
            phase_timings: vec![],
            assertion_details: vec![],
        }
    }
}
