use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

/// Shared one-shot cell holding the exit code of a top-level Docker target process.
///
/// The container-wait task populates the cell when the container exits; assertions read it
/// to make decisions that depend on the exit code. Mirrors [`airlock::unix::ExitCodeCell`] for
/// the host-process path.
pub type DockerExitCodeCell = Arc<OnceLock<i64>>;

use panoramic_core::case::ProcedureNode;
use panoramic_core::deadline::Deadline;
use panoramic_core::probe::PROBE_TIMEOUT;
use panoramic_core::procedure::{Member, Procedure};
use panoramic_core::record::{AssertionResult, AssertionStatus, CaseRecord, StepReport, Verdict};
use panoramic_core::result::{ErrorKind, TestResult};
use saluki_error::GenericError;
use tokio_util::sync::CancellationToken;
use tracing::error;

use crate::actions::Action;
use crate::config::{AssertionConfig, AssertionStep, LogStream};
use crate::integration::IntegrationTestCase;

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

/// Tracks product observations and harness errors across polling attempts.
///
/// Any product observation makes an unsuccessful step `failed`; otherwise it is `errored`.
#[derive(Debug, Default)]
pub(crate) struct Attempts {
    reached_product: bool,
    last: Option<String>,
}

impl Attempts {
    /// Records a product observation: a response, connection failure, or exit code.
    pub(crate) fn observed(&mut self, observation: impl fmt::Display) {
        self.reached_product = true;
        self.last = Some(observation.to_string());
    }

    /// Records an attempt that failed in the harness before it reached the product.
    pub(crate) fn errored(&mut self, error: impl fmt::Display) {
        self.last = Some(error.to_string());
    }

    /// Returns `Failed` if any attempt observed the product, and `Errored` otherwise.
    pub(crate) fn status(&self) -> AssertionStatus {
        if self.reached_product {
            AssertionStatus::Failed
        } else {
            AssertionStatus::Errored
        }
    }

    /// Explains a step timeout.
    ///
    /// An `errored` step leads with its last harness error so it appears in the test's error line.
    /// A `failed` step leads with `summary`.
    pub(crate) fn explain_expired(&self, summary: &str) -> String {
        match &self.last {
            Some(last) if self.reached_product => format!("{summary}\nLast attempt: {last}"),
            Some(last) => format!("{last}\n{summary}"),
            None => summary.to_string(),
        }
    }

    /// Combines `cancellation` with the last attempt's diagnostic, if any.
    pub(crate) fn explain_cancelled(&self, cancellation: impl fmt::Display) -> String {
        match &self.last {
            Some(last) => format!("{cancellation}\nLast attempt: {last}"),
            None => cancellation.to_string(),
        }
    }
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

    /// Returns the limit for one attempt of a target command that is retried until `remaining` runs out.
    ///
    /// A timed-out host command is killed, so its attempts are capped at [`PROBE_TIMEOUT`] to leave time for retries. A
    /// timed-out Docker exec keeps running, so a container attempt gets all of `remaining` and is never retried.
    pub(crate) fn command_attempt_limit(&self, remaining: Duration) -> Duration {
        if self.is_host_process {
            remaining.min(PROBE_TIMEOUT)
        } else {
            remaining
        }
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
    /// Classify failures using [`AssertionStatus`]. Release owned resources on drop: the step driver may drop this
    /// future on cancellation or a budget overrun.
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
        )?)),
    }
}

/// Runs the procedure from `test_case` against `ctx` through core's procedure driver.
///
/// A build error records a setup failure. Core records each step, runs parallel members together, and enforces
/// cancellation, budgets and fail-fast at block boundaries.
///
/// Used by both the docker and `mac` integration runners; the only thing that differs
/// between runtimes is how `ctx` is constructed (port mappings come from a Docker driver vs.
/// identity-mapped from the test config; `is_host_process` and `host_process_exit_code` flip).
/// `case` limits each step's deadline; the runner enforces it by cancelling `ctx.cancel_token`.
pub(crate) async fn run_assertion_steps(
    test_case: &IntegrationTestCase, ctx: &AssertionContext, case: Deadline, record: &CaseRecord,
) {
    let procedure = match build_procedure(&test_case.procedure, ctx) {
        Ok(procedure) => procedure,
        Err(message) => {
            error!(error = %message, "Failed to create procedure step from configuration.");
            let result = TestResult::errored(
                &test_case.config.name,
                ErrorKind::Setup,
                message,
                Duration::ZERO,
                Vec::new(),
            );
            record.decide(Verdict::SetupFailed(Box::new(result)));
            return;
        }
    };
    procedure.run(case, &ctx.cancel_token, record).await;
}

/// Builds every step of `procedure` against `ctx`, naming the first that fails.
fn build_procedure<'a>(procedure: &[AssertionStep], ctx: &'a AssertionContext) -> Result<Procedure<'a>, String> {
    let bind = |executable: Result<Executable, GenericError>, budget| {
        executable.map(|executable| {
            Box::new(BoundStep {
                executable,
                budget,
                ctx,
            }) as Box<dyn Member + 'a>
        })
    };
    Procedure::build(procedure.iter().map(|config| {
        match config {
            AssertionStep::Action(action) => ProcedureNode::Step(bind(
                crate::actions::create_action(action).map(Executable::Action),
                action.budget(),
            )),
            AssertionStep::Single(assertion) => ProcedureNode::Step(bind(
                create_assertion(assertion).map(Executable::Assertion),
                assertion.budget(),
            )),
            AssertionStep::Parallel { parallel } => ProcedureNode::Parallel(
                parallel
                    .iter()
                    .map(|assertion| {
                        bind(
                            create_assertion(assertion).map(Executable::Assertion),
                            assertion.budget(),
                        )
                    })
                    .collect(),
            ),
        }
    }))
}

/// An action or assertion bound to the context it runs against.
struct BoundStep<'a> {
    executable: Executable,
    budget: Duration,
    ctx: &'a AssertionContext,
}

/// The action or assertion a [`BoundStep`] runs.
enum Executable {
    Action(Box<dyn Action>),
    Assertion(Box<dyn Assertion>),
}

#[async_trait::async_trait]
impl Member for BoundStep<'_> {
    fn name(&self) -> &'static str {
        match &self.executable {
            Executable::Action(action) => action.name(),
            Executable::Assertion(assertion) => assertion.name(),
        }
    }

    fn description(&self) -> String {
        match &self.executable {
            Executable::Action(action) => action.description(),
            Executable::Assertion(assertion) => assertion.description(),
        }
    }

    fn budget(&self) -> Duration {
        self.budget
    }

    async fn run(&self) -> StepReport {
        match &self.executable {
            Executable::Action(action) => action.execute(self.ctx).await,
            Executable::Assertion(assertion) => assertion.check(self.ctx).await.into(),
        }
    }
}

/// Fixtures for running a procedure through [`run_assertion_steps`] on the host.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::config::runner_artifacts::RunnerArtifacts;
    use crate::config::LINUX_RUNTIME;

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
            &RunnerArtifacts::Overrides(&[]).integration_settings(LINUX_RUNTIME),
        )
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use futures::future;
    use panoramic_core::deadline::Budget;
    use panoramic_core::record::{StepResult, StepStatus};
    use panoramic_core::result::TestOutcome;
    use tokio::time::Instant;

    use super::test_support::{host_context, procedure};
    use super::*;
    use crate::intake::test_server::MockIntake;

    /// Shorter than the long-running test steps, so missed cancellation fails promptly.
    const BOUND: Duration = Duration::from_secs(120);

    /// An assertion that matches the log line in [`context`].
    const LOG_MATCHES: &str = "- assertion: log_contains\n  pattern: ready\n  timeout: 5s\n";

    /// Returns a host context whose logs hold `ready`, cancelled by `cancel`.
    fn context(cancel: &CancellationToken, target: Option<&MockIntake>) -> AssertionContext {
        let mut ctx = host_context(cancel.clone());
        ctx.log_buffer.write().unwrap().stdout.push("ready".to_string());
        if let Some(target) = target {
            ctx.port_mappings.insert("1234/tcp".to_string(), target.port());
        }
        ctx
    }

    fn cancel_after(cancel: &CancellationToken, after: Duration) {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            cancel.cancel();
        });
    }

    /// Runs `steps` with a case deadline longer than the test bound, returning the record and elapsed time.
    async fn run(steps: &str, ctx: &AssertionContext) -> (CaseRecord, Duration) {
        run_within(steps, ctx, Duration::from_secs(3600)).await
    }

    /// Runs `steps` with a case deadline `case` from now, returning the record and elapsed time.
    async fn run_within(steps: &str, ctx: &AssertionContext, case: Duration) -> (CaseRecord, Duration) {
        let case = Deadline::after(Budget::Case, case);
        let record = CaseRecord::default();
        let started = Instant::now();
        tokio::time::timeout(BOUND, run_assertion_steps(&procedure(steps), ctx, case, &record))
            .await
            .expect("the step driver should stop the procedure within the bound");
        (record, started.elapsed())
    }

    /// Each recorded step's kind, parallel member, and status.
    fn statuses(steps: &[StepResult]) -> Vec<(&str, Option<usize>, StepStatus)> {
        steps
            .iter()
            .map(|step| (step.name.as_str(), step.member, step.status))
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_a_step_that_ignores_it_and_keeps_finished_steps() {
        let cancel = CancellationToken::new();
        let ctx = context(&cancel, None);
        cancel_after(&cancel, Duration::from_secs(1));
        let steps = format!("{LOG_MATCHES}- assertion: process_stable_for\n  duration: 600s\n{LOG_MATCHES}");

        let (record, _) = run(&steps, &ctx).await;

        let steps = record.steps();
        assert_eq!(
            statuses(&steps),
            [
                ("log_contains", None, StepStatus::Passed),
                ("process_stable_for", None, StepStatus::Interrupted),
                ("log_contains", None, StepStatus::NotRun),
            ]
        );
        assert_eq!(steps[1].message, "Interrupted after 1.0s.");
        assert!(
            !record.decided(),
            "a cancelled procedure should leave the outcome to the runner"
        );
    }

    #[tokio::test]
    async fn a_failed_step_ends_the_procedure_and_leaves_later_steps_not_run() {
        let ctx = context(&CancellationToken::new(), None);
        // An exited process makes a missing log line fail at once.
        ctx.container_exit_token.cancel();
        let steps = format!(
            "{LOG_MATCHES}- assertion: log_contains\n  pattern: missing\n  timeout: 5s\n- parallel:\n  \
             - assertion: log_contains\n    pattern: ready\n    timeout: 5s\n  \
             - assertion: log_contains\n    pattern: ready\n    timeout: 5s\n"
        );

        let (record, _) = run(&steps, &ctx).await;

        let steps = record.steps();
        assert_eq!(
            statuses(&steps),
            [
                ("log_contains", None, StepStatus::Passed),
                ("log_contains", None, StepStatus::Failed),
                ("log_contains", Some(0), StepStatus::NotRun),
                ("log_contains", Some(1), StepStatus::NotRun),
            ]
        );
        assert_eq!(steps[3].step, 2);
        assert_eq!(steps[3].message, "Not run.");
        assert!(record.decided(), "a failed step should decide the procedure");
        let result = record.result("t", Duration::ZERO, Vec::new());
        assert_eq!(result.outcome, TestOutcome::Failed);
    }

    #[tokio::test]
    async fn a_procedure_with_an_invalid_step_runs_none_of_it() {
        let ctx = context(&CancellationToken::new(), None);
        let steps = format!(
            "{LOG_MATCHES}- assertion: adp_config_key_equals\n  key: some.key\n  value: true\n  \
             endpoint: https://localhost:55101/unsupported\n  timeout: 1s\n"
        );

        let (record, _) = run(&steps, &ctx).await;

        assert!(record.steps().is_empty(), "no step should run: {:?}", record.steps());
        let result = record.result("t", Duration::ZERO, Vec::new());
        assert_eq!(result.outcome, TestOutcome::Errored);
        let error = result.error.expect("an invalid step should be a setup error");
        assert_eq!(error.kind, ErrorKind::Setup);
        assert!(
            error
                .message
                .starts_with("Failed to create procedure step 2: Unsupported ADP configuration endpoint."),
            "{}",
            error.message
        );
    }

    const HTTP_POLL: &str =
        "- assertion: http_check\n  endpoint: http://127.0.0.1:1234/\n  status:\n    equal: 200\n  timeout: 30s\n";

    #[tokio::test]
    async fn an_evaluation_error_reports_the_first_line_of_the_step_message() {
        let ctx = context(&CancellationToken::new(), None);
        let invalid_regex = "- assertion: log_contains\n  pattern: '('\n  regex: true\n  timeout: 5s\n";

        let (record, _) = run(invalid_regex, &ctx).await;

        let steps = record.steps();
        assert_eq!(statuses(&steps), [("log_contains", None, StepStatus::Errored)]);
        let first_line = "Invalid regex pattern '(': regex parse error:";
        assert_eq!(steps[0].message.lines().next(), Some(first_line));
        assert!(steps[0].message.lines().count() > 1, "{}", steps[0].message);
        let result = record.result("t", Duration::ZERO, Vec::new());
        assert_eq!(result.outcome, TestOutcome::Errored);
        let error = result.error.expect("an invalid regex should be an evaluation error");
        assert_eq!(error.kind, ErrorKind::Evaluation);
        assert_eq!(
            error.message,
            format!("Step 1 (log_contains) could not be evaluated: {first_line}")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_target_request_is_retried_until_the_check_fails() {
        let target = MockIntake::unresponsive().await;
        let ctx = context(&CancellationToken::new(), None);
        let endpoint = format!("http://127.0.0.1:{}/", target.port());
        let steps =
            format!("- assertion: http_check\n  endpoint: {endpoint}\n  status:\n    equal: 200\n  timeout: 30s\n");

        let (record, elapsed) = run(&steps, &ctx).await;

        let steps = record.steps();
        assert_eq!(statuses(&steps), [("http_check", None, StepStatus::Failed)]);
        assert_eq!(elapsed, Duration::from_millis(30_500));
        assert_eq!(
            steps[0].message,
            format!(
                "HTTP endpoint '{endpoint}' returns status 200. did not happen within 30s.\nLast attempt: Timed out \
                 after 9s waiting for GET {endpoint}."
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_times_only_its_own_budget_until_the_runner_cancels() {
        // Delay runner cancellation past the case deadline to prove the step uses only its own budget.
        let target = MockIntake::unresponsive().await;
        let cancel = CancellationToken::new();
        let ctx = context(&cancel, Some(&target));
        cancel_after(&cancel, Duration::from_millis(1500));

        let (record, _) = run_within(HTTP_POLL, &ctx, Duration::from_secs(1)).await;

        let steps = record.steps();
        assert_eq!(statuses(&steps), [("http_check", None, StepStatus::Interrupted)]);
        assert_eq!(
            steps[0].message,
            format!(
                "Interrupted after 1.5s. The case left this step 1.0s of its 30s step budget.\n\
                 Cancelled after 1.5s waiting for GET http://127.0.0.1:{}/.",
                target.port()
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_parallel_block_lasts_as_long_as_its_slowest_member() {
        let target = MockIntake::unresponsive().await;
        let ctx = context(&CancellationToken::new(), Some(&target));
        let steps = "- parallel:\n  - assertion: process_stable_for\n    duration: 2s\n  \
                     - assertion: http_check\n    endpoint: http://127.0.0.1:1234/\n    status:\n      \
                     equal: 200\n    timeout: 5s\n";

        let (record, elapsed) = run(steps, &ctx).await;

        let steps = record.steps();
        assert_eq!(
            statuses(&steps),
            [
                ("process_stable_for", Some(0), StepStatus::Passed),
                ("http_check", Some(1), StepStatus::Failed),
            ]
        );
        assert!(
            steps[1].message.ends_with(&format!(
                "Last attempt: Timed out after 5s waiting for GET http://127.0.0.1:{}/.",
                target.port()
            )),
            "{}",
            steps[1].message
        );
        assert_eq!(elapsed, Duration::from_millis(5500));
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_every_unfinished_parallel_member() {
        let target = MockIntake::unresponsive().await;
        let cancel = CancellationToken::new();
        let ctx = context(&cancel, Some(&target));
        cancel_after(&cancel, Duration::from_secs(1));
        let steps = "- parallel:\n  - assertion: log_contains\n    pattern: ready\n    timeout: 5s\n  \
                     - assertion: process_stable_for\n    duration: 600s\n  \
                     - assertion: http_check\n    endpoint: http://127.0.0.1:1234/\n    status:\n      \
                     equal: 200\n    timeout: 600s\n";

        let (record, _) = run(steps, &ctx).await;

        let steps = record.steps();
        assert_eq!(
            statuses(&steps),
            [
                ("log_contains", Some(0), StepStatus::Passed),
                ("process_stable_for", Some(1), StepStatus::Interrupted),
                ("http_check", Some(2), StepStatus::Interrupted),
            ]
        );
        assert_eq!(steps[1].message, "Interrupted after 1.0s.");
        assert_eq!(
            steps[2].message,
            format!(
                "Interrupted after 1.0s.\nCancelled after 1.0s waiting for GET http://127.0.0.1:{}/.",
                target.port()
            )
        );
    }

    #[tokio::test]
    async fn polling_steps_cancelled_between_polls_report_their_last_attempt() {
        use crate::intake::test_server::install_crypto_provider;

        install_crypto_provider();
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let auth_token = dir.path().join("auth_token");
        std::fs::write(&auth_token, "token").expect("the auth token should be writable");
        let missing = dir.path().join("missing");
        // Port zero keeps every probe unsuccessful, independent of other tests' listeners.
        let closed = 0;
        let cancel = CancellationToken::new();
        let mut ctx = context(&cancel, None);
        ctx.port_mappings.insert("1234/tcp".to_string(), closed);
        ctx.core_agent_auth_token_path = Some(auth_token);
        // Every step must finish an attempt before the cancel, even on a busy machine or on Windows, which retries a
        // refused connection for about two seconds.
        cancel_after(&cancel, Duration::from_secs(5));
        let assertions = format!(
            "- parallel:\n  - assertion: http_check\n    endpoint: http://127.0.0.1:{closed}/\n    status:\n      \
             equal: 200\n    timeout: 30s\n  - assertion: port_listening\n    port: 1234\n    protocol: tcp\n    \
             timeout: 30s\n  - assertion: file_contains\n    path: {}\n    timeout: 30s\n",
            missing.display()
        );
        let action = format!(
            "- action: core_agent_config_set\n  key: some_key\n  value: some-value\n  endpoint: \
             http://127.0.0.1:{closed}/{{key}}\n  timeout: 30s\n"
        );

        let ((assertions, _), (action, _)) = tokio::join!(run(&assertions, &ctx), run(&action, &ctx));

        let steps: Vec<_> = assertions.steps().into_iter().chain(action.steps()).collect();
        let names: Vec<_> = steps.iter().map(|step| step.name.as_str()).collect();
        assert_eq!(
            names,
            ["http_check", "port_listening", "file_contains", "core_agent_config_set"]
        );
        for step in &steps {
            assert_eq!(step.status, StepStatus::Interrupted, "{}: {}", step.name, step.message);
            // A sleep that ignored cancellation would leave only the driver's interruption line.
            let mut lines = step.message.lines().skip(1);
            assert!(
                lines
                    .next()
                    .is_some_and(|line| line.to_lowercase().contains("cancelled"))
                    && lines.next().is_some_and(|line| line.starts_with("Last attempt: ")),
                "{}: {}",
                step.name,
                step.message
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn each_step_kind_separates_harness_failures_from_product_failures() {
        use crate::intake::test_server::install_crypto_provider;

        install_crypto_provider();
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let directory = dir.path().display().to_string();
        let missing = dir.path().join("missing");
        // Port zero never accepts, so another test's listener can't make these probes succeed.
        let closed = 0;
        let context_with = |configure: &dyn Fn(&mut AssertionContext)| {
            let mut ctx = context(&CancellationToken::new(), None);
            configure(&mut ctx);
            ctx
        };
        let cli = |command: &[&str]| TargetCommand::new(command.iter().map(ToString::to_string).collect());
        // Rows share one thread, so budgets leave room for every row's setup and first attempt on a busy machine.
        let port_listening = "- assertion: port_listening\n  port: 1234\n  protocol: tcp\n  timeout: 5s\n";
        let file_contains = |path: &str| format!("- assertion: file_contains\n  path: {path}\n  timeout: 5s\n");
        let config_key = "- assertion: adp_config_key_equals\n  key: some.key\n  value: true\n  timeout: 5s\n";
        let rows = [
            (
                "http_check, closed port".to_string(),
                format!(
                    "- assertion: http_check\n  endpoint: http://127.0.0.1:{closed}/\n  status:\n    equal: 200\n  \
                     timeout: 5s\n"
                ),
                context_with(&|_| {}),
                StepStatus::Failed,
            ),
            (
                "port_listening, closed port".to_string(),
                port_listening.to_string(),
                context_with(&|ctx| {
                    ctx.port_mappings.insert("1234/tcp".to_string(), closed);
                }),
                StepStatus::Failed,
            ),
            (
                "port_listening, unexposed port".to_string(),
                port_listening.to_string(),
                context_with(&|_| {}),
                StepStatus::Errored,
            ),
            (
                "file_contains, directory".to_string(),
                file_contains(&directory),
                context_with(&|_| {}),
                StepStatus::Errored,
            ),
            (
                "file_contains, missing path".to_string(),
                file_contains(&missing.display().to_string()),
                context_with(&|_| {}),
                StepStatus::Failed,
            ),
            (
                "log_contains, invalid regex".to_string(),
                "- assertion: log_contains\n  pattern: \"(\"\n  regex: true\n  timeout: 5s\n".to_string(),
                context_with(&|_| {}),
                StepStatus::Errored,
            ),
            (
                "target_exec, missing program".to_string(),
                "- action: target_exec\n  command: [/nonexistent/panoramic-missing]\n  timeout: 5s\n".to_string(),
                context_with(&|_| {}),
                StepStatus::Errored,
            ),
            (
                "target_exec, nonzero exit".to_string(),
                "- action: target_exec\n  command: [sh, -c, \"exit 3\"]\n  timeout: 5s\n".to_string(),
                context_with(&|_| {}),
                StepStatus::Failed,
            ),
            (
                "adp_config_key_equals, nonzero exit".to_string(),
                config_key.to_string(),
                context_with(&|ctx| ctx.adp_cli_command = cli(&["sh", "-c", "exit 1"])),
                StepStatus::Failed,
            ),
            (
                "adp_config_key_equals, missing program".to_string(),
                config_key.to_string(),
                context_with(&|ctx| ctx.adp_cli_command = cli(&["/nonexistent/panoramic-missing"])),
                StepStatus::Errored,
            ),
            (
                "core_agent_config_set, missing auth token".to_string(),
                "- action: core_agent_config_set\n  key: some_key\n  value: some-value\n  timeout: 5s\n".to_string(),
                context_with(&|ctx| ctx.core_agent_auth_token_path = Some(missing.clone())),
                StepStatus::Errored,
            ),
            (
                "dogstatsd_send, unpublished port".to_string(),
                "- action: dogstatsd_send\n  payload: \"some.metric:1|c\"\n  port: 8125\n  timeout: 5s\n".to_string(),
                context_with(&|_| {}),
                StepStatus::Errored,
            ),
        ];

        let observed = tokio::time::timeout(
            Duration::from_secs(30),
            future::join_all(rows.iter().map(|(_, steps, ctx, _)| async move {
                let (record, _) = run(steps, ctx).await;
                record.steps().remove(0)
            })),
        )
        .await
        .expect("every row should finish within 30 seconds");

        let misclassified: Vec<_> = rows
            .iter()
            .zip(&observed)
            .filter(|((_, _, _, expected), step)| step.status != *expected)
            .map(|((row, _, _, expected), step)| format!("{row}: expected {expected:?}, got {step:?}"))
            .collect();
        assert!(misclassified.is_empty(), "{misclassified:#?}");
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_step_that_returns_on_cancellation_keeps_its_message_under_the_interruption() {
        let cancel = CancellationToken::new();
        let ctx = context(&cancel, None);
        cancel_after(&cancel, Duration::from_secs(1));
        // `exec` replaces the shell, so dropping the action kills the sleeping process itself.
        let steps = "- action: target_exec\n  command: [sh, -c, exec sleep 600]\n  timeout: 600s\n";

        let (record, _) = run(steps, &ctx).await;

        let steps = record.steps();
        assert_eq!(statuses(&steps), [("target_exec", None, StepStatus::Interrupted)]);
        assert_eq!(steps[0].message, "Interrupted after 1.0s.\nAction cancelled.");
    }

    #[tokio::test]
    async fn a_cancelled_procedure_starts_no_step() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = context(&cancel, None);

        let (record, _) = run(LOG_MATCHES, &ctx).await;

        assert_eq!(statuses(&record.steps()), [("log_contains", None, StepStatus::NotRun)]);
        assert!(!record.decided());
        let result = record.result("t", Duration::ZERO, Vec::new());
        assert_ne!(
            result.outcome,
            TestOutcome::Passed,
            "an unfinished procedure must not pass"
        );
    }

    #[test]
    fn only_killable_commands_get_attempts_short_enough_to_retry() {
        // Capping a container attempt let a timed-out ADP config CLI keep running while the next attempt started
        // another, so slow commands piled up in the container.
        let mut ctx = host_context(CancellationToken::new());
        assert_eq!(ctx.command_attempt_limit(Duration::from_secs(60)), PROBE_TIMEOUT);
        assert_eq!(
            ctx.command_attempt_limit(Duration::from_secs(3)),
            Duration::from_secs(3)
        );

        ctx.is_host_process = false;
        assert_eq!(
            ctx.command_attempt_limit(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
        assert_eq!(
            ctx.command_attempt_limit(Duration::from_secs(3)),
            Duration::from_secs(3)
        );
    }
}
