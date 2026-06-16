use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

/// Shared one-shot cell holding the exit code of a top-level Docker target process.
///
/// The container-wait task populates the cell when the container exits; assertions read it
/// to make decisions that depend on the exit code. Mirrors [`airlock::unix::ExitCodeCell`] for
/// the host-process path.
pub type DockerExitCodeCell = Arc<OnceLock<i64>>;

use futures::future;
use saluki_error::GenericError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use crate::config::{AssertionConfig, AssertionStep, IntegrationConfig, LogStream};

mod adp_config_key_equals;
mod adp_exits;
mod file_contains;
mod http_check;
mod log_contains;
mod port_listening;
mod process_stable;

pub use adp_config_key_equals::{default_adp_config_endpoint, AdpConfigKeyEqualsAssertion};
pub use adp_exits::AdpExitsWithAssertion;
pub use file_contains::FileContainsAssertion;
pub use http_check::HttpCheckAssertion;
pub use log_contains::{LogContainsAssertion, LogNotContainsAssertion};
pub use port_listening::PortListeningAssertion;
pub use process_stable::ProcessStableForAssertion;

/// Result of running an assertion.
#[derive(Clone, Debug)]
pub struct AssertionResult {
    /// Name of the assertion type.
    pub name: String,
    /// Whether the assertion passed.
    pub passed: bool,
    /// Human-readable message about the result.
    pub message: String,
    /// How long the assertion took to run.
    pub duration: Duration,
}

/// Buffer for captured container logs.
#[derive(Debug, Default)]
pub struct LogBuffer {
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
}

impl LogBuffer {
    /// Check if any line matches the given pattern.
    pub fn contains_match(&self, pattern: &str, is_regex: bool, stream: &LogStream) -> bool {
        let lines: Vec<&str> = match stream {
            LogStream::Stdout => self.stdout.iter().map(|s| s.as_str()).collect(),
            LogStream::Stderr => self.stderr.iter().map(|s| s.as_str()).collect(),
            LogStream::Both => self
                .stdout
                .iter()
                .chain(self.stderr.iter())
                .map(|s| s.as_str())
                .collect(),
        };

        if is_regex {
            if let Ok(re) = regex::Regex::new(pattern) {
                lines.iter().any(|line| re.is_match(line))
            } else {
                false
            }
        } else {
            lines.iter().any(|line| line.contains(pattern))
        }
    }

    /// Find the first line that matches the given pattern.
    pub fn find_match(&self, pattern: &str, is_regex: bool, stream: &LogStream) -> Option<String> {
        let lines: Vec<&str> = match stream {
            LogStream::Stdout => self.stdout.iter().map(|s| s.as_str()).collect(),
            LogStream::Stderr => self.stderr.iter().map(|s| s.as_str()).collect(),
            LogStream::Both => self
                .stdout
                .iter()
                .chain(self.stderr.iter())
                .map(|s| s.as_str())
                .collect(),
        };

        if is_regex {
            if let Ok(re) = regex::Regex::new(pattern) {
                lines.iter().find(|line| re.is_match(line)).map(|s| s.to_string())
            } else {
                None
            }
        } else {
            lines.iter().find(|line| line.contains(pattern)).map(|s| s.to_string())
        }
    }
}

/// Context provided to assertions during execution.
pub struct AssertionContext {
    /// Shared log buffer for reading container logs.
    pub log_buffer: Arc<RwLock<LogBuffer>>,
    /// Fired when the container exits. Used by process assertions.
    pub container_exit_token: CancellationToken,
    /// Fired for external cancellation (timeout or user cancel).
    pub cancel_token: CancellationToken,
    /// Port mappings from internal port to host port.
    pub port_mappings: std::collections::HashMap<String, u16>,
    /// Container IP address on its primary Docker network, if known.
    pub container_ip: Option<String>,
    /// Operating system of the target container, when one exists.
    ///
    /// `None` for host-process runtimes (such as `mac`). Otherwise the value identifies the
    /// container OS so assertion helpers can pick a compatible probing strategy: Linux
    /// containers are probed from the host using mapped ephemeral ports, Windows containers are
    /// probed via `docker exec` inside the container against the listener's internal port.
    pub target_os: Option<airlock::driver::ContainerOs>,
    /// Name of the container being tested.
    pub container_name: String,
    /// Whether the test is running natively (no container). When `true`, assertions that would
    /// otherwise reach into a container (for example, reading a file via `docker exec`) should operate
    /// against the host filesystem / local process instead.
    pub is_host_process: bool,
    /// Exit code of the host target process, populated once it exits. `None` on the docker
    /// path or while the process is still running; `Some(None)` if the process was killed by
    /// signal; `Some(Some(code))` if it exited normally.
    pub host_process_exit_code: Option<airlock::unix::ExitCodeCell>,
    /// Exit code of a top-level Docker target process, populated once when the container
    /// exits. `None` on host-process runtimes; otherwise an empty cell that becomes populated
    /// when the docker wait stream completes.
    pub docker_container_exit_code: Option<DockerExitCodeCell>,
    /// Core Agent auth token path for host-process runtimes.
    pub core_agent_auth_token_path: Option<PathBuf>,
}

impl AssertionContext {
    /// Returns `true` when the target is a Windows container.
    pub fn target_is_windows(&self) -> bool {
        matches!(self.target_os, Some(airlock::driver::ContainerOs::Windows))
    }
}

/// Trait for assertion implementations.
#[async_trait::async_trait]
pub trait Assertion: Send + Sync {
    /// Returns the name of this assertion type.
    fn name(&self) -> &'static str;

    /// Returns a human-readable description of what this assertion checks.
    fn description(&self) -> String;

    /// Execute the assertion and return the result.
    async fn check(&self, ctx: &AssertionContext) -> AssertionResult;
}

/// Create an assertion from its configuration.
pub fn create_assertion(config: &AssertionConfig) -> Result<Box<dyn Assertion>, GenericError> {
    match config {
        AssertionConfig::ProcessStableFor { duration } => Ok(Box::new(ProcessStableForAssertion::new(duration.0))),
        AssertionConfig::AdpExitsWith { expected_code, timeout } => {
            Ok(Box::new(AdpExitsWithAssertion::new(*expected_code, timeout.0)))
        }
        AssertionConfig::PortListening {
            port,
            protocol,
            timeout,
        } => Ok(Box::new(PortListeningAssertion::new(
            *port,
            protocol.clone(),
            timeout.0,
        ))),
        AssertionConfig::LogContains {
            pattern,
            regex,
            timeout,
            stream,
        } => Ok(Box::new(LogContainsAssertion::new(
            pattern.clone(),
            *regex,
            timeout.0,
            stream.clone(),
        ))),
        AssertionConfig::LogNotContains {
            pattern,
            regex,
            during,
            stream,
        } => Ok(Box::new(LogNotContainsAssertion::new(
            pattern.clone(),
            *regex,
            during.0,
            stream.clone(),
        ))),
        AssertionConfig::HttpCheck {
            endpoint,
            status,
            insecure_skip_verify,
            timeout,
        } => Ok(Box::new(HttpCheckAssertion::new(
            endpoint.clone(),
            status.clone(),
            *insecure_skip_verify,
            timeout.0,
        ))),
        AssertionConfig::FileContains {
            path,
            pattern,
            regex,
            timeout,
        } => Ok(Box::new(FileContainsAssertion::new(
            path.clone(),
            pattern.clone(),
            *regex,
            timeout.0,
        ))),
        AssertionConfig::AdpConfigKeyEquals {
            key,
            value,
            endpoint,
            timeout,
        } => Ok(Box::new(AdpConfigKeyEqualsAssertion::new(
            key.clone(),
            value.clone(),
            endpoint.clone(),
            timeout.0,
        ))),
    }
}

/// Runs the assertion steps from `test_case` against `ctx`, returning the per-assertion results.
///
/// Iterates through the test case's assertion list, executing single steps sequentially and
/// parallel blocks concurrently. Stops at the first failure (fail-fast), so the returned vector
/// is truncated past the failing step.
///
/// Used by both the docker and `mac` integration runners; the only thing that differs
/// between runtimes is how `ctx` is constructed (port mappings come from a Docker driver vs.
/// identity-mapped from the test config; `is_host_process` and `host_process_exit_code` flip).
pub(crate) async fn run_assertion_steps(test_case: &IntegrationConfig, ctx: &AssertionContext) -> Vec<AssertionResult> {
    let mut results = Vec::new();
    let total_steps = test_case.assertions.len();

    for (step_index, step) in test_case.assertions.iter().enumerate() {
        match step {
            AssertionStep::Action(action_config) => {
                let action = match crate::actions::create_action(action_config) {
                    Ok(a) => a,
                    Err(e) => {
                        error!(error = %e, "Failed to create action from configuration.");
                        results.push(AssertionResult {
                            name: "config_error".to_string(),
                            passed: false,
                            message: format!("Failed to create action: {}.", e),
                            duration: Duration::ZERO,
                        });
                        break;
                    }
                };

                debug!(
                    step = step_index + 1,
                    step_total = total_steps,
                    action_type = action.name(),
                    description = %action.description(),
                    "Running action..."
                );

                let result = action.execute(ctx).await;
                if result.passed {
                    debug!(action_type = action.name(), duration = ?result.duration, "Action passed.");
                } else {
                    debug!(
                        action_type = action.name(),
                        duration = ?result.duration,
                        message = %result.message,
                        "Action failed."
                    );
                }

                let failed = !result.passed;
                results.push(result);

                if failed {
                    debug!("Stopping assertion execution due to action failure (fail-fast).");
                    break;
                }
            }

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

                let result = assertion.check(ctx).await;

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

                let futures: Vec<_> = assertions.iter().map(|a| a.check(ctx)).collect();
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
