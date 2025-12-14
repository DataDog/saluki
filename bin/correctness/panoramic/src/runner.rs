use std::{
    collections::HashMap,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use airlock::driver::{Driver, DriverConfig, DriverDetails};
use bollard::{container::LogOutput, Docker};
use futures::StreamExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    assertions::{create_assertion, AssertionContext, AssertionResult, LogBuffer},
    config::{parse_file_spec, parse_port_spec, TestCase},
    reporter::TestResult,
};

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
pub struct TestRunner {
    test_case: TestCase,
    isolation_group_id: String,
    cancel_token: CancellationToken,
    log_buffer: Arc<RwLock<LogBuffer>>,
    log_dir: Option<PathBuf>,
}

impl TestRunner {
    /// Create a new test runner for the given test case.
    pub fn new(test_case: TestCase) -> Self {
        Self {
            test_case,
            isolation_group_id: generate_isolation_group_id(),
            cancel_token: CancellationToken::new(),
            log_buffer: Arc::new(RwLock::new(LogBuffer::default())),
            log_dir: None,
        }
    }

    /// Set the directory where container logs should be written.
    pub fn with_log_dir(mut self, log_dir: PathBuf) -> Self {
        self.log_dir = Some(log_dir);
        self
    }

    /// Run the test case and return the result.
    pub async fn run(&mut self) -> TestResult {
        let started = Instant::now();
        let test_name = self.test_case.name.clone();

        info!(
            test = %test_name,
            isolation_group = %self.isolation_group_id,
            "Starting test case."
        );

        // Build the driver configuration.
        debug!(test = %test_name, "Building driver configuration...");
        let driver_config = match self.build_driver_config().await {
            Ok(config) => config,
            Err(e) => {
                error!(test = %test_name, error = %e, "Failed to build driver configuration.");
                return TestResult {
                    name: test_name,
                    passed: false,
                    duration: started.elapsed(),
                    assertion_results: vec![],
                    error: Some(format!("Failed to build driver configuration: {}", e)),
                };
            }
        };

        // Create and start the driver.
        debug!(test = %test_name, "Creating container driver...");
        let mut driver = match Driver::from_config(self.isolation_group_id.clone(), driver_config) {
            Ok(driver) => driver,
            Err(e) => {
                error!(test = %test_name, error = %e, "Failed to create container driver.");
                return TestResult {
                    name: test_name,
                    passed: false,
                    duration: started.elapsed(),
                    assertion_results: vec![],
                    error: Some(format!("Failed to create driver: {}", e)),
                };
            }
        };

        info!(test = %test_name, "Starting container...");
        let details = match driver.start().await {
            Ok(details) => details,
            Err(e) => {
                error!(test = %test_name, error = %e, "Failed to start container.");
                let _ = self.cleanup(&driver).await;
                return TestResult {
                    name: test_name,
                    passed: false,
                    duration: started.elapsed(),
                    assertion_results: vec![],
                    error: Some(format!("Failed to start container: {}", e)),
                };
            }
        };

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
        let exit_handle = tokio::spawn(async move {
            let docker = match Docker::connect_with_defaults() {
                Ok(d) => d,
                Err(_) => return,
            };

            let mut wait_stream = docker.wait_container::<String>(&container_name, None);
            if wait_stream.next().await.is_some() {
                exit_cancel.cancel();
            }
        });

        // Build port mappings for assertions.
        let port_mappings = self.build_port_mappings(&details);

        // Run assertions with overall timeout.
        info!(
            test = %test_name,
            assertion_count = self.test_case.assertions.len(),
            "Running assertions..."
        );

        let assertion_results = tokio::select! {
            results = self.run_assertions(&port_mappings) => results,
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
        if let Err(e) = self.write_logs(&test_name).await {
            warn!(test = %test_name, error = %e, "Failed to write container logs to disk.");
        }

        // Cleanup.
        debug!(test = %test_name, "Cleaning up container and resources...");
        if let Err(e) = self.cleanup(&driver).await {
            warn!(test = %test_name, error = %e, "Failed to clean up resources.");
        }
        debug!(test = %test_name, "Cleanup complete.");

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
        let docker = Docker::connect_with_defaults()?;
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

    async fn run_assertions(&self, port_mappings: &HashMap<String, u16>) -> Vec<AssertionResult> {
        let mut results = Vec::new();

        let ctx = AssertionContext {
            log_buffer: self.log_buffer.clone(),
            cancel_token: self.cancel_token.clone(),
            port_mappings: port_mappings.clone(),
        };

        for (index, assertion_config) in self.test_case.assertions.iter().enumerate() {
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
                assertion_index = index + 1,
                assertion_total = self.test_case.assertions.len(),
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

            // Fail fast on first failure.
            if failed {
                debug!("Stopping assertion execution due to failure (fail-fast).");
                break;
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
