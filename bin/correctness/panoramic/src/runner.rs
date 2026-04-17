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

use crate::{
    assertions::{create_assertion, AssertionContext, AssertionResult, LogBuffer},
    config::{parse_file_spec, parse_port_spec, AssertionStep, DiscoveredTest, TestCase},
    events::TestEvent,
    reporter::{PhaseTiming, TestResult},
};

// ----------------------------------------------------------------------------
// Public API: run_tests
// ----------------------------------------------------------------------------

/// Run all tests, emitting events to the provided channel.
///
/// This is the single entry point for test execution, used identically by both
/// TUI and plain output modes. The caller is responsible for spawning an appropriate
/// consumer to handle the emitted events.
pub async fn run_tests(
    test_cases: Vec<DiscoveredTest>, parallelism: usize, fail_fast: bool, log_dir: Option<PathBuf>,
    mounts_dir: PathBuf, event_tx: mpsc::UnboundedSender<TestEvent>, cancel_token: CancellationToken,
) -> Vec<TestResult> {
    // Emit run started event.
    let _ = event_tx.send(TestEvent::RunStarted {
        total_tests: test_cases.len(),
    });

    let parallelism = parallelism.max(1);
    let semaphore = Arc::new(Semaphore::new(parallelism));
    let event_tx = Arc::new(event_tx);
    let log_dir = Arc::new(log_dir);
    let mounts_dir = Arc::new(mounts_dir);

    let results = if fail_fast {
        run_fail_fast(
            test_cases,
            semaphore,
            event_tx.clone(),
            log_dir,
            mounts_dir,
            cancel_token,
        )
        .await
    } else {
        run_parallel(
            test_cases,
            parallelism,
            semaphore,
            event_tx.clone(),
            log_dir,
            mounts_dir,
            cancel_token,
        )
        .await
    };

    let _ = event_tx.send(TestEvent::AllDone);
    results
}

/// Run tests sequentially, stopping at the first failure.
async fn run_fail_fast(
    test_cases: Vec<DiscoveredTest>, semaphore: Arc<Semaphore>, event_tx: Arc<mpsc::UnboundedSender<TestEvent>>,
    log_dir: Arc<Option<PathBuf>>, mounts_dir: Arc<PathBuf>, cancel_token: CancellationToken,
) -> Vec<TestResult> {
    let mut results = Vec::new();

    for test_case in test_cases {
        // Check for cancellation before starting each test.
        if cancel_token.is_cancelled() {
            break;
        }

        let _permit = semaphore.acquire().await.unwrap();
        let name = test_case.name().to_string();

        let _ = event_tx.send(TestEvent::TestStarted { name: name.clone() });

        let result = run_with_timeout(test_case, name, &log_dir, &mounts_dir).await;

        let failed = !result.passed;
        let _ = event_tx.send(TestEvent::TestCompleted { result: result.clone() });
        results.push(result);

        if failed {
            break;
        }
    }

    results
}

/// Run tests in parallel up to the parallelism limit.
async fn run_parallel(
    test_cases: Vec<DiscoveredTest>, parallelism: usize, semaphore: Arc<Semaphore>,
    event_tx: Arc<mpsc::UnboundedSender<TestEvent>>, log_dir: Arc<Option<PathBuf>>, mounts_dir: Arc<PathBuf>,
    cancel_token: CancellationToken,
) -> Vec<TestResult> {
    let cancel = cancel_token.clone();

    stream::iter(test_cases)
        .take_while(|_| {
            let cancelled = cancel.is_cancelled();
            async move { !cancelled }
        })
        .map(|test_case| {
            let semaphore = semaphore.clone();
            let event_tx = event_tx.clone();
            let cancel = cancel_token.clone();
            let log_dir = log_dir.clone();
            let mounts_dir = mounts_dir.clone();

            async move {
                // Check for cancellation before acquiring permit.
                if cancel.is_cancelled() {
                    return None;
                }

                let _permit = semaphore.acquire().await.unwrap();

                // Check again after acquiring permit.
                if cancel.is_cancelled() {
                    return None;
                }

                let name = test_case.name().to_string();

                let _ = event_tx.send(TestEvent::TestStarted { name: name.clone() });

                let result = run_with_timeout(test_case, name, &log_dir, &mounts_dir).await;

                let _ = event_tx.send(TestEvent::TestCompleted { result: result.clone() });

                Some(result)
            }
        })
        .buffer_unordered(parallelism)
        .filter_map(|r| async { r })
        .collect()
        .await
}

/// Run a single test with an appropriate timeout.
///
/// Integration tests handle their own timeout internally, so no outer timeout is applied.
/// Correctness tests have no built-in timeout, so a 20-minute outer timeout is applied.
async fn run_with_timeout(
    test_case: DiscoveredTest, name: String, log_dir: &Arc<Option<PathBuf>>, mounts_dir: &Arc<PathBuf>,
) -> TestResult {
    match &test_case {
        DiscoveredTest::Integration(_) => run_single_test(test_case, log_dir, mounts_dir).await,
        DiscoveredTest::Correctness { .. } => {
            let timeout = test_case.timeout();
            let correctness_log_dir = (**log_dir).as_ref().map(|d| d.join("correctness").join(&name));
            tokio::select! {
                r = run_single_test(test_case, log_dir, mounts_dir) => r,
                _ = tokio::time::sleep(timeout) => {
                    TestResult {
                        name,
                        passed: false,
                        duration: timeout,
                        assertion_results: vec![],
                        error: Some(format!("Test timed out after {:?}.", timeout)),
                        phase_timings: vec![],
                        log_dir: correctness_log_dir,
                        assertion_details: vec![],
                    }
                }
            }
        }
    }
}

/// Run a single test, dispatching to the appropriate runner based on test type.
async fn run_single_test(
    test_case: DiscoveredTest, log_dir: &Arc<Option<PathBuf>>, mounts_dir: &Arc<PathBuf>,
) -> TestResult {
    match test_case {
        DiscoveredTest::Integration(tc) => {
            let test_log_dir = (**log_dir).as_ref().map(|d| d.join("integration").join(&tc.name));
            let mut runner = TestRunner::new(tc, (**mounts_dir).clone());
            if let Some(ref dir) = **log_dir {
                runner = runner.with_log_dir(dir.join("integration"));
            }
            let mut result = runner.run().await;
            result.log_dir = test_log_dir;
            write_result_log(&result);
            result
        }
        DiscoveredTest::Correctness { name, config } => {
            let correctness_log_dir = (**log_dir).as_ref().map(|d| d.join("correctness").join(&name));
            let result = crate::correctness::runner::run_correctness_test(
                name,
                config,
                correctness_log_dir,
                (**mounts_dir).clone(),
            )
            .await;
            write_result_log(&result);
            result
        }
    }
}

// ----------------------------------------------------------------------------
// TestRunner: single test case execution
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

/// Runner for a single test case.
struct TestRunner {
    test_case: TestCase,
    isolation_group_id: String,
    cancel_token: CancellationToken,
    log_buffer: Arc<RwLock<LogBuffer>>,
    log_dir: Option<PathBuf>,
    mounts_dir: PathBuf,
}

impl TestRunner {
    /// Create a new test runner for the given test case.
    fn new(test_case: TestCase, mounts_dir: PathBuf) -> Self {
        Self {
            test_case,
            isolation_group_id: generate_isolation_group_id(),
            cancel_token: CancellationToken::new(),
            log_buffer: Arc::new(RwLock::new(LogBuffer::default())),
            log_dir: None,
            mounts_dir,
        }
    }

    /// Set the directory where container logs should be written.
    fn with_log_dir(mut self, log_dir: PathBuf) -> Self {
        self.log_dir = Some(log_dir);
        self
    }

    /// Run the test case and return the result.
    async fn run(&mut self) -> TestResult {
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
                    log_dir: None,
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
                    log_dir: None,
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
                    log_dir: None,
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

        // Monitor container exit in the background.
        let exit_cancel = self.cancel_token.clone();
        let container_name = details.container_name().to_string();
        let container_name_for_exit = container_name.clone();
        let exit_handle = tokio::spawn(async move {
            let docker = match airlock::docker::connect() {
                Ok(d) => d,
                Err(_) => return,
            };

            let mut wait_stream = docker.wait_container::<String>(&container_name_for_exit, None);
            if wait_stream.next().await.is_some() {
                exit_cancel.cancel();
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
                                log_dir: None,
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
                            log_dir: None,
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
                        log_dir: None,
                        assertion_details: vec![],
                    };
                }
            }

            phase_timings.push(PhaseTiming {
                phase: "dynamic_vars".to_string(),
                duration: phase_start.elapsed(),
            });
        }

        // Run assertions with overall timeout.
        info!(
            test = %test_name,
            assertion_count = self.test_case.total_assertion_count(),
            "Running assertions..."
        );

        let phase_start = Instant::now();
        let assertion_results = tokio::select! {
            results = self.run_assertions(&port_mappings, &container_name) => results,
            _ = tokio::time::sleep(self.test_case.timeout.0) => {
                error!(test = %test_name, timeout = ?self.test_case.timeout.0, "Test timed out.");
                vec![AssertionResult {
                    name: "timeout".to_string(),
                    passed: false,
                    message: format!("Test timed out after {:?}.", self.test_case.timeout.0),
                    duration: self.test_case.timeout.0,
                }]
            }
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
            log_dir: None,
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
        config = crate::mounts::apply_target_mounts(config, &self.mounts_dir)?;

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

    async fn run_assertions(&self, port_mappings: &HashMap<String, u16>, container_name: &str) -> Vec<AssertionResult> {
        let mut results = Vec::new();
        let total_steps = self.test_case.assertions.len();

        let ctx = AssertionContext {
            log_buffer: self.log_buffer.clone(),
            cancel_token: self.cancel_token.clone(),
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
        self.cancel_token.cancel();

        // Give background tasks a moment to notice cancellation.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Clean up all resources in the isolation group.
        Driver::clean_related_resources(self.isolation_group_id.clone())
            .await
            .error_context("Failed to clean up isolation group resources.")?;

        Ok(())
    }

    async fn write_logs(&self, test_name: &str) -> Result<(), GenericError> {
        let log_dir = match &self.log_dir {
            Some(dir) => dir,
            None => return Ok(()), // Log writing not enabled.
        };

        // Create the test-specific log directory.
        let test_log_dir = log_dir.join(test_name);
        std::fs::create_dir_all(&test_log_dir)
            .error_context(format!("Failed to create log directory: {}", test_log_dir.display()))?;

        // Get the log buffer contents.
        let buffer = self.log_buffer.read().await;

        // Write stdout.
        let stdout_path = test_log_dir.join("stdout.log");
        let mut stdout_file = std::fs::File::create(&stdout_path)
            .error_context(format!("Failed to create stdout log file: {}", stdout_path.display()))?;
        for line in &buffer.stdout {
            writeln!(stdout_file, "{}", line).error_context("Failed to write to stdout log")?;
        }

        // Write stderr.
        let stderr_path = test_log_dir.join("stderr.log");
        let mut stderr_file = std::fs::File::create(&stderr_path)
            .error_context(format!("Failed to create stderr log file: {}", stderr_path.display()))?;
        for line in &buffer.stderr {
            writeln!(stderr_file, "{}", line).error_context("Failed to write to stderr log")?;
        }

        debug!(
            test = %test_name,
            path = %test_log_dir.display(),
            "Container logs written to disk."
        );

        Ok(())
    }
}

fn write_result_log(result: &crate::reporter::TestResult) {
    let dir = match &result.log_dir {
        Some(d) => d,
        None => return,
    };

    if let Err(e) = std::fs::create_dir_all(dir) {
        warn!(path = %dir.display(), error = %e, "Failed to create log directory for result log.");
        return;
    }

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
