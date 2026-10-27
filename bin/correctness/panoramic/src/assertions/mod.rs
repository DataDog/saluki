use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

/// Shared one-shot cell holding the exit code of a top-level Docker target process.
///
/// The container-wait task populates the cell when the container exits; assertions read it
/// to make decisions that depend on the exit code. Mirrors [`airlock::unix::ExitCodeCell`] for
/// the host-process path.
pub type DockerExitCodeCell = Arc<OnceLock<i64>>;

use std::future::Future;

use futures::future;
use saluki_error::GenericError;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use crate::config::{AssertionConfig, AssertionStep, LogStream};
use crate::deadline::{Budget, Deadline};
use crate::integration::IntegrationTestCase;

mod adp_config_key_equals;
mod adp_exits;
mod file_contains;
mod http_check;
mod intake_has_metric;
mod log_contains;
mod port_listening;
mod process_stable;

pub use adp_config_key_equals::{default_adp_config_endpoint, AdpConfigKeyEqualsAssertion};
pub use adp_exits::AdpExitsWithAssertion;
pub use file_contains::FileContainsAssertion;
pub use http_check::HttpCheckAssertion;
pub use intake_has_metric::IntakeHasMetricAssertion;
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

/// Runtime-specific command prefix for invoking the tested target's CLI.
#[derive(Clone)]
pub struct TargetCommand {
    command_prefix: Vec<String>,
    host_env: Option<HashMap<String, String>>,
}

impl TargetCommand {
    /// Creates a target command with no explicit host-process environment.
    pub fn new(command_prefix: Vec<String>) -> Self {
        Self {
            command_prefix,
            host_env: None,
        }
    }

    /// Supplies environment entries for host-process execution.
    pub fn with_host_env(mut self, host_env: HashMap<String, String>) -> Self {
        self.host_env = Some(host_env);
        self
    }

    /// Returns the program and global arguments that precede action-specific arguments.
    pub fn command_prefix(&self) -> &[String] {
        &self.command_prefix
    }

    /// Returns the environment entries supplied for host-process execution.
    pub fn host_env(&self) -> Option<&HashMap<String, String>> {
        self.host_env.as_ref()
    }

    /// Appends action-specific arguments to the command prefix without shell interpretation.
    pub fn with_args(&self, args: &[String]) -> Vec<String> {
        let mut command = Vec::with_capacity(self.command_prefix().len() + args.len());
        command.extend(self.command_prefix().iter().cloned());
        command.extend(args.iter().cloned());
        command
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
    /// Published port of the Datadog intake sidecar's HTTP endpoint, when the test runs one.
    ///
    /// `None` when the test has no intake sidecar, including on host-process runtimes.
    pub intake_host_port: Option<u16>,
    /// Core Agent auth token path for host-process runtimes.
    pub core_agent_auth_token_path: Option<PathBuf>,
    /// Runtime-specific prefix and host environment for invoking the tested ADP binary as a CLI.
    pub adp_cli_command: TargetCommand,
    /// Runtime-specific prefix and host environment for invoking the Core Agent binary as a CLI.
    pub core_agent_cli_command: TargetCommand,
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
    ///
    /// The step driver may drop this future on cancellation or a budget overrun. Release owned resources on drop.
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
        AssertionConfig::IntakeHasMetric {
            name,
            metric_type,
            value,
            tags,
            tags_match,
            timeout,
        } => Ok(Box::new(IntakeHasMetricAssertion::new(
            name.clone(),
            *metric_type,
            *value,
            tags.clone(),
            *tags_match,
            timeout.0,
        )?)),
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
        )?)),
    }
}

/// Grace period for a step to report its own result after its deadline.
const STEP_SLACK: Duration = Duration::from_secs(10);

/// Runs the assertion steps from `test_case` against `ctx`, returning the per-assertion results.
///
/// Iterates through the test case's assertion list, executing single steps sequentially and
/// parallel blocks concurrently. Stops at the first failure (fail-fast), so the returned vector
/// is truncated past the failing step.
///
/// Each step, including parallel members, runs through [`run_step`]. Cancellation stops new steps and
/// preserves completed results.
///
/// Used by both the docker and `mac` integration runners; the only thing that differs
/// between runtimes is how `ctx` is constructed (port mappings come from a Docker driver vs.
/// identity-mapped from the test config; `is_host_process` and `host_process_exit_code` flip).
/// `case` limits each step's deadline; the runner enforces it by cancelling `ctx.cancel_token`.
pub(crate) async fn run_assertion_steps(
    test_case: &IntegrationTestCase, ctx: &AssertionContext, case: Deadline,
) -> Vec<AssertionResult> {
    let mut results = Vec::new();
    let total_steps = test_case.procedure.len();

    for (step_index, step) in test_case.procedure.iter().enumerate() {
        if ctx.cancel_token.is_cancelled() {
            break;
        }

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

                let result = run_step(
                    action.name(),
                    action_config.budget(),
                    case,
                    &ctx.cancel_token,
                    action.execute(ctx),
                )
                .await;
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

                let result = run_step(
                    assertion.name(),
                    assertion_config.budget(),
                    case,
                    &ctx.cancel_token,
                    assertion.check(ctx),
                )
                .await;

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
                        Ok(a) => assertions.push((assertion_config.budget(), a)),
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

                let members = assertions
                    .iter()
                    .map(|(budget, a)| run_step(a.name(), *budget, case, &ctx.cancel_token, a.check(ctx)));
                let parallel_results = future::join_all(members).await;

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

/// Runs a step until completion, cancellation, or its deadline plus [`STEP_SLACK`].
///
/// The earlier of the case and step deadlines applies. The step is polled first so a failure returned on
/// cancellation keeps its message below the interruption message.
async fn run_step<F>(
    name: &'static str, budget: Duration, case: Deadline, cancel: &CancellationToken, step: F,
) -> AssertionResult
where
    F: Future<Output = AssertionResult>,
{
    let started = Instant::now();
    let deadline = case.narrow(Budget::Step, budget);
    let stopped = StoppedStep {
        name,
        started,
        budget,
        case,
        deadline,
    };

    tokio::select! {
        biased;
        result = step => if !result.passed && cancel.is_cancelled() {
            stopped.interrupted(Some(result.message))
        } else {
            result
        },
        _ = cancel.cancelled() => stopped.interrupted(None),
        _ = tokio::time::sleep_until(deadline.at + STEP_SLACK) => match deadline.budget {
            Budget::Step => stopped.overran(),
            Budget::Case => stopped.interrupted(None),
        },
    }
}

/// State used to report a step interrupted or stopped by the driver.
struct StoppedStep {
    name: &'static str,
    started: Instant,
    /// The step's own budget.
    budget: Duration,
    case: Deadline,
    /// `case` narrowed to `budget`.
    deadline: Deadline,
}

impl StoppedStep {
    /// Returns an interruption failure, appending the step's own message if available.
    fn interrupted(&self, step_message: Option<String>) -> AssertionResult {
        let elapsed = self.started.elapsed();
        let mut message = if self.case.remaining().is_zero() {
            let mut message = format!(
                "Interrupted after {elapsed:.1?}: the case budget of {:?} expired.",
                self.case.configured
            );
            if self.deadline.budget == Budget::Case {
                let left = self.deadline.at.saturating_duration_since(self.started);
                message.push_str(&format!(
                    " The case left this step {left:.1?} of its {:?} step budget.",
                    self.budget
                ));
            }
            message
        } else {
            format!("Interrupted after {elapsed:.1?}: the test was cancelled.")
        };
        if let Some(step_message) = step_message {
            message.push('\n');
            message.push_str(&step_message);
        }
        self.failure(message, elapsed)
    }

    /// Returns a failure for a step that exceeded its budget and [`STEP_SLACK`].
    fn overran(&self) -> AssertionResult {
        let elapsed = self.started.elapsed();
        self.failure(
            format!(
                "Did not finish within its {:?} step budget; stopped after {elapsed:.1?}.",
                self.budget
            ),
            elapsed,
        )
    }

    fn failure(&self, message: String, duration: Duration) -> AssertionResult {
        AssertionResult {
            name: self.name.to_string(),
            passed: false,
            message,
            duration,
        }
    }
}

/// Fixtures for running a procedure through [`run_assertion_steps`] on the host.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::config::LINUX_RUNTIME;
    use crate::image_override::ImageOverrides;
    use crate::integration::IntegrationSettings;

    /// Returns a host-process context using `cancel`, with empty logs and target commands.
    pub(crate) fn host_context(cancel: CancellationToken) -> AssertionContext {
        AssertionContext {
            log_buffer: Arc::new(RwLock::new(LogBuffer::default())),
            container_exit_token: CancellationToken::new(),
            cancel_token: cancel,
            port_mappings: HashMap::new(),
            container_ip: None,
            target_os: None,
            container_name: String::new(),
            is_host_process: true,
            host_process_exit_code: None,
            docker_container_exit_code: None,
            intake_host_port: None,
            core_agent_auth_token_path: None,
            adp_cli_command: TargetCommand::new(Vec::new()),
            core_agent_cli_command: TargetCommand::new(Vec::new()),
        }
    }

    /// Parses YAML `procedure:` entries into a test case.
    pub(crate) fn procedure(steps: &str) -> IntegrationTestCase {
        let config = serde_saphyr::from_str(&format!("name: procedure\ntimeout: 1s\nprocedure:\n{steps}"))
            .expect("procedure should parse");
        IntegrationTestCase::new(
            config,
            &IntegrationSettings::new(LINUX_RUNTIME, &ImageOverrides::new(&[])),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{host_context, procedure};
    use super::*;
    use crate::intake::test_server::MockIntake;

    /// Shorter than the long-running test steps, so missed cancellation fails promptly.
    const BOUND: Duration = Duration::from_secs(120);

    /// An assertion that matches the log line in [`context`].
    const LOG_MATCHES: &str = "- assertion: log_contains\n  pattern: ready\n  timeout: 5s\n";

    /// Returns a host context whose logs hold `ready`, cancelled by `cancel`.
    fn context(cancel: &CancellationToken, intake: Option<&MockIntake>) -> AssertionContext {
        let mut ctx = host_context(cancel.clone());
        ctx.log_buffer.write().unwrap().stdout.push("ready".to_string());
        ctx.intake_host_port = intake.map(MockIntake::port);
        ctx
    }

    fn cancel_after(cancel: &CancellationToken, after: Duration) {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            cancel.cancel();
        });
    }

    /// Runs `steps` with a case deadline longer than the test bound, returning results and elapsed time.
    async fn run(steps: &str, ctx: &AssertionContext) -> (Vec<AssertionResult>, Duration) {
        let case = Deadline::after(Budget::Case, Duration::from_secs(3600));
        let started = Instant::now();
        let results = tokio::time::timeout(BOUND, run_assertion_steps(&procedure(steps), ctx, case))
            .await
            .expect("the step driver should stop the procedure within the bound");
        (results, started.elapsed())
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_a_step_that_ignores_it_and_keeps_finished_steps() {
        let cancel = CancellationToken::new();
        let ctx = context(&cancel, None);
        cancel_after(&cancel, Duration::from_secs(1));
        let steps = format!("{LOG_MATCHES}- assertion: process_stable_for\n  duration: 600s\n{LOG_MATCHES}");

        let (results, _) = run(&steps, &ctx).await;

        assert_eq!(results.len(), 2, "the third step should not start: {results:?}");
        assert!(results[0].passed, "{}", results[0].message);
        assert_eq!(results[1].name, "process_stable_for");
        assert!(!results[1].passed);
        assert_eq!(results[1].message, "Interrupted after 1.0s: the test was cancelled.");
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_stalled_past_its_own_budget_is_stopped_after_the_slack() {
        // The intake holds requests open, so the step stays pending while Tokio's paused clock advances to the timeout.
        let intake = MockIntake::unresponsive().await;
        let ctx = context(&CancellationToken::new(), Some(&intake));
        let steps = "- assertion: intake_has_metric\n  name: some.metric\n  timeout: 1s\n";

        let (results, elapsed) = run(steps, &ctx).await;

        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
        assert_eq!(
            results[0].message,
            "Did not finish within its 1s step budget; stopped after 11.0s."
        );
        assert_eq!(elapsed, Duration::from_secs(11));
    }

    #[tokio::test(start_paused = true)]
    async fn a_parallel_block_lasts_as_long_as_its_slowest_member() {
        let intake = MockIntake::unresponsive().await;
        let ctx = context(&CancellationToken::new(), Some(&intake));
        let steps = "- parallel:\n  - assertion: process_stable_for\n    duration: 2s\n  \
                     - assertion: intake_has_metric\n    name: some.metric\n    timeout: 1s\n";

        let (results, elapsed) = run(steps, &ctx).await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].name, "process_stable_for");
        assert!(results[0].passed, "{}", results[0].message);
        assert_eq!(results[1].name, "intake_has_metric");
        assert!(!results[1].passed);
        assert!(results[1]
            .message
            .starts_with("Did not finish within its 1s step budget"));
        assert_eq!(elapsed, Duration::from_secs(11));
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_every_unfinished_parallel_member() {
        let intake = MockIntake::unresponsive().await;
        let cancel = CancellationToken::new();
        let ctx = context(&cancel, Some(&intake));
        cancel_after(&cancel, Duration::from_secs(1));
        let steps = "- parallel:\n  - assertion: log_contains\n    pattern: ready\n    timeout: 5s\n  \
                     - assertion: process_stable_for\n    duration: 600s\n  \
                     - assertion: intake_has_metric\n    name: some.metric\n    timeout: 600s\n";

        let (results, _) = run(steps, &ctx).await;

        assert_eq!(results.len(), 3);
        assert!(results[0].passed, "{}", results[0].message);
        for interrupted in &results[1..] {
            assert!(!interrupted.passed);
            assert_eq!(interrupted.message, "Interrupted after 1.0s: the test was cancelled.");
        }
        assert_eq!(results[1].name, "process_stable_for");
        assert_eq!(results[2].name, "intake_has_metric");
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_step_that_returns_on_cancellation_keeps_its_message_under_the_interruption() {
        let cancel = CancellationToken::new();
        let ctx = context(&cancel, None);
        cancel_after(&cancel, Duration::from_secs(1));
        // `exec` replaces the shell, so dropping the action kills the sleeping process itself.
        let steps = "- action: target_exec\n  command: [sh, -c, exec sleep 600]\n  timeout: 600s\n";

        let (results, _) = run(steps, &ctx).await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "target_exec");
        assert!(!results[0].passed);
        assert_eq!(
            results[0].message,
            "Interrupted after 1.0s: the test was cancelled.\nAction cancelled."
        );
    }

    #[tokio::test]
    async fn a_cancelled_procedure_starts_no_step() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = context(&cancel, None);

        let (results, _) = run(LOG_MATCHES, &ctx).await;

        assert!(results.is_empty(), "no step should start: {results:?}");
    }
}
