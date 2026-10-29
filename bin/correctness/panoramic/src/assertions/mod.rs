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

use futures::future;
use saluki_error::GenericError;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use crate::actions::Action;
use crate::config::{AssertionConfig, AssertionStep, LogStream};
use crate::deadline::{Budget, Deadline};
use crate::integration::IntegrationTestCase;
use crate::probe::PROBE_TIMEOUT;
use crate::record::{step_label, ActiveStep, CaseRecord, StepResult, StepStatus, Verdict};
use crate::reporter::{DurationMs, ErrorKind, TestResult};

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

/// What an action or assertion concluded. The step driver adds `interrupted` and `not_run`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssertionStatus {
    /// The step met its expectation.
    Passed,
    /// The observed product behavior did not meet the expectation.
    Failed,
    /// A harness or configuration error prevented evaluation of the product.
    Errored,
}

impl From<AssertionStatus> for StepStatus {
    fn from(status: AssertionStatus) -> Self {
        match status {
            AssertionStatus::Passed => Self::Passed,
            AssertionStatus::Failed => Self::Failed,
            AssertionStatus::Errored => Self::Errored,
        }
    }
}

/// Result of running an assertion.
#[derive(Clone, Debug)]
pub struct AssertionResult {
    /// Name of the assertion type.
    pub name: String,
    /// How the step ended.
    pub status: AssertionStatus,
    /// Human-readable message about the result.
    pub message: String,
    /// How long the assertion took to run.
    pub duration: Duration,
}

impl AssertionResult {
    /// Creates a step result with the elapsed time since `started`.
    pub(crate) fn new(name: &str, started: Instant, status: AssertionStatus, message: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status,
            message: message.into(),
            duration: started.elapsed(),
        }
    }
}

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

/// Runs the procedure from `test_case` against `ctx`, recording each step in `record`.
///
/// Builds all steps before execution; a build error records a setup failure. Steps run in order, with parallel
/// members running together. The driver stops on cancellation or after a non-passing step or parallel block, leaving
/// later steps `not_run`. Completion or fail-fast records [`Verdict::Procedure`]; cancellation or interruption leaves
/// the outcome to the runner.
///
/// Used by both the docker and `mac` integration runners; the only thing that differs
/// between runtimes is how `ctx` is constructed (port mappings come from a Docker driver vs.
/// identity-mapped from the test config; `is_host_process` and `host_process_exit_code` flip).
/// `case` limits each step's deadline; the runner enforces it by cancelling `ctx.cancel_token`.
pub(crate) async fn run_assertion_steps(
    test_case: &IntegrationTestCase, ctx: &AssertionContext, case: Deadline, record: &CaseRecord,
) {
    let procedure = match build_procedure(&test_case.procedure) {
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
    record.plan(procedure.iter().flatten().map(Member::not_run).collect());

    let total_steps = procedure.len();
    for (step_index, members) in procedure.iter().enumerate() {
        if ctx.cancel_token.is_cancelled() {
            return;
        }

        debug!(
            step = step_index + 1,
            step_total = total_steps,
            member_count = members.len(),
            "Starting procedure step..."
        );
        let statuses = future::join_all(members.iter().map(|member| run_step(record, member, case, ctx))).await;

        if statuses.contains(&StepStatus::Interrupted) {
            return;
        }
        if statuses.iter().any(|status| *status != StepStatus::Passed) {
            debug!("Stopping procedure after a step that did not pass (fail-fast).");
            break;
        }
    }

    record.decide(Verdict::Procedure);
}

/// Builds the procedure as one list of members per step.
///
/// # Errors
///
/// Returns a message naming the first step whose configuration can't be built.
fn build_procedure(procedure: &[AssertionStep]) -> Result<Vec<Vec<Member>>, String> {
    let mut slot = 0;
    let mut built = Vec::with_capacity(procedure.len());
    for (step, config) in procedure.iter().enumerate() {
        let configs: Vec<(Option<usize>, Result<Executable, GenericError>, Duration)> = match config {
            AssertionStep::Action(action) => vec![(
                None,
                crate::actions::create_action(action).map(Executable::Action),
                action.budget(),
            )],
            AssertionStep::Single(assertion) => vec![(
                None,
                create_assertion(assertion).map(Executable::Assertion),
                assertion.budget(),
            )],
            AssertionStep::Parallel { parallel } => parallel
                .iter()
                .enumerate()
                .map(|(member, assertion)| {
                    (
                        Some(member),
                        create_assertion(assertion).map(Executable::Assertion),
                        assertion.budget(),
                    )
                })
                .collect(),
        };

        let mut members = Vec::with_capacity(configs.len());
        for (member, executable, budget) in configs {
            let executable = executable
                .map_err(|e| format!("Failed to create procedure step {}: {}.", step_label(step, member), e))?;
            members.push(Member {
                slot,
                step,
                member,
                budget,
                executable,
            });
            slot += 1;
        }
        built.push(members);
    }
    Ok(built)
}

/// A procedure step, or one member of a parallel block, ready to run.
struct Member {
    /// Slot in the case record, counting parallel members separately.
    slot: usize,
    step: usize,
    member: Option<usize>,
    /// The step's own budget.
    budget: Duration,
    executable: Executable,
}

/// The action or assertion a [`Member`] runs.
enum Executable {
    Action(Box<dyn Action>),
    Assertion(Box<dyn Assertion>),
}

impl Member {
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

    /// The record entry for this member before it starts.
    fn not_run(&self) -> StepResult {
        StepResult {
            step: self.step,
            member: self.member,
            name: self.name().to_string(),
            status: StepStatus::NotRun,
            message: "Not run.".to_string(),
            duration: Duration::ZERO,
        }
    }
}

/// Runs a step until completion, cancellation, or its deadline plus [`STEP_SLACK`], recording it in `record`.
///
/// The earlier of the case and step deadlines applies. The step is polled first so a failure returned on
/// cancellation keeps its message below the interruption line.
async fn run_step(record: &CaseRecord, member: &Member, case: Deadline, ctx: &AssertionContext) -> StepStatus {
    let running = RunningStep {
        step: member.step,
        member: member.member,
        name: member.name(),
        started: Instant::now(),
        budget: member.budget,
        deadline: case.narrow(Budget::Step, member.budget),
    };
    record.start(member.slot, running);
    debug!(step_type = running.name, description = %member.description(), "Running procedure step...");

    let step = match &member.executable {
        Executable::Action(action) => action.execute(ctx),
        Executable::Assertion(assertion) => assertion.check(ctx),
    };
    let result = tokio::select! {
        biased;
        result = step => if result.status != AssertionStatus::Passed && ctx.cancel_token.is_cancelled() {
            running.interrupted(Some(result.message))
        } else {
            running.returned(result)
        },
        _ = ctx.cancel_token.cancelled() => running.interrupted(None),
        _ = tokio::time::sleep_until(running.deadline.at + STEP_SLACK) => match running.deadline.budget {
            Budget::Step => running.overran(),
            Budget::Case => running.interrupted(None),
        },
    };

    debug!(
        step_type = %result.name,
        status = ?result.status,
        duration = ?result.duration,
        message = %result.message,
        "Procedure step finished."
    );
    let status = result.status;
    record.finish(member.slot, result);
    status
}

/// Identity and timing of a running step, retained in the case record until it finishes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RunningStep {
    step: usize,
    member: Option<usize>,
    name: &'static str,
    started: Instant,
    /// The step's own budget.
    budget: Duration,
    /// The case deadline narrowed to `budget`.
    deadline: Deadline,
}

impl RunningStep {
    /// Captures the step's identity and timing for interruption reports.
    pub(crate) fn active(&self) -> ActiveStep {
        ActiveStep {
            step: self.step,
            member: self.member,
            kind: self.name.to_string(),
            elapsed_ms: DurationMs(self.started.elapsed()).as_millis(),
            budget_ms: DurationMs(self.budget).as_millis(),
        }
    }

    /// Returns an `interrupted` result, appending the step's own message if available.
    ///
    /// Reports elapsed time and any budget reduction imposed by the case deadline. The test result carries the
    /// interruption cause.
    pub(crate) fn interrupted(&self, step_message: Option<String>) -> StepResult {
        let elapsed = self.started.elapsed();
        let mut message = format!("Interrupted after {elapsed:.1?}.");
        if self.deadline.budget == Budget::Case {
            let left = self.deadline.at.saturating_duration_since(self.started);
            message.push_str(&format!(
                " The case left this step {left:.1?} of its {:?} step budget.",
                self.budget
            ));
        }
        if let Some(step_message) = step_message {
            message.push('\n');
            message.push_str(&step_message);
        }
        self.result(StepStatus::Interrupted, message, elapsed)
    }

    /// Returns an `errored` result for a step that exceeded its budget and [`STEP_SLACK`].
    fn overran(&self) -> StepResult {
        let elapsed = self.started.elapsed();
        self.result(
            StepStatus::Errored,
            format!(
                "Did not finish within its {:?} step budget; stopped after {elapsed:.1?}.",
                self.budget
            ),
            elapsed,
        )
    }

    /// Returns the result the step reported itself.
    fn returned(&self, result: AssertionResult) -> StepResult {
        self.result(result.status.into(), result.message, result.duration)
    }

    fn result(&self, status: StepStatus, message: String, duration: Duration) -> StepResult {
        StepResult {
            step: self.step,
            member: self.member,
            name: self.name.to_string(),
            status,
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
    use crate::reporter::TestOutcome;

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
            "{LOG_MATCHES}- assertion: intake_has_metric\n  name: some.sketch\n  metric_type: sketch\n  value: 1.0\n  timeout: 1s\n"
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
                .starts_with("Failed to create procedure step 2: intake_has_metric `some.sketch` can't combine"),
            "{}",
            error.message
        );
    }

    /// An assertion whose check never returns.
    struct Stalled;

    #[async_trait::async_trait]
    impl Assertion for Stalled {
        fn name(&self) -> &'static str {
            "stalled"
        }

        fn description(&self) -> String {
            "Never returns.".to_string()
        }

        async fn check(&self, _ctx: &AssertionContext) -> AssertionResult {
            std::future::pending().await
        }
    }

    /// An intake assertion with enough time for multiple probes.
    const INTAKE_POLL: &str = "- assertion: intake_has_metric\n  name: some.metric\n  timeout: 30s\n";

    #[tokio::test(start_paused = true)]
    async fn a_step_stalled_past_its_own_budget_is_stopped_after_the_slack() {
        let ctx = context(&CancellationToken::new(), None);
        let member = Member {
            slot: 0,
            step: 0,
            member: None,
            budget: Duration::from_secs(1),
            executable: Executable::Assertion(Box::new(Stalled)),
        };
        let record = CaseRecord::default();
        record.plan(vec![member.not_run()]);
        let case = Deadline::after(Budget::Case, Duration::from_secs(3600));
        let started = Instant::now();

        let status = tokio::time::timeout(BOUND, run_step(&record, &member, case, &ctx))
            .await
            .expect("the step driver should stop the step within the bound");

        assert_eq!(status, StepStatus::Errored);
        assert_eq!(started.elapsed(), Duration::from_secs(11));
        let steps = record.steps();
        assert_eq!(statuses(&steps), [("stalled", None, StepStatus::Errored)]);
        assert_eq!(
            steps[0].message,
            "Did not finish within its 1s step budget; stopped after 11.0s."
        );
        let result = record.result("t", Duration::ZERO, Vec::new());
        assert_eq!(result.outcome, TestOutcome::Errored);
        let error = result.error.expect("an overrun should be a harness error");
        assert_eq!(error.kind, ErrorKind::Evaluation);
        assert_eq!(
            error.message,
            "Step 1 (stalled) could not be evaluated: Did not finish within its 1s step budget; stopped after 11.0s."
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_intake_poll_is_retried_until_the_step_errors() {
        // Holding requests open lets Tokio's paused clock advance each probe to its timeout.
        let intake = MockIntake::unresponsive().await;
        let ctx = context(&CancellationToken::new(), Some(&intake));

        let (record, elapsed) = run(INTAKE_POLL, &ctx).await;

        let steps = record.steps();
        assert_eq!(statuses(&steps), [("intake_has_metric", None, StepStatus::Errored)]);
        // The final probe uses the step's remaining budget rather than a full probe timeout.
        assert_eq!(elapsed, Duration::from_secs(30));
        let first_line = "Timed out after 9s waiting for the intake sidecar's metrics dump.: deadline has elapsed";
        assert_eq!(
            steps[0].message.lines().next(),
            Some(first_line),
            "{}",
            steps[0].message
        );
        let result = record.result("t", Duration::ZERO, Vec::new());
        assert_eq!(result.outcome, TestOutcome::Errored);
        let error = result.error.expect("a stalled intake should be a harness error");
        assert_eq!(error.kind, ErrorKind::Evaluation);
        assert_eq!(
            error.message,
            format!("Step 1 (intake_has_metric) could not be evaluated: {first_line}")
        );
    }

    #[tokio::test]
    async fn errors_after_a_successful_poll_keep_the_step_failed() {
        let intake = MockIntake::serving_once(serde_json::json!([{
            "context": {"name": "some.metric", "tags": []},
            "values": [[0, {"mtype": "Count", "value": 1.0}]],
        }]))
        .await;
        let mut ctx = context(&CancellationToken::new(), Some(&intake));
        let exit = CancellationToken::new();
        ctx.container_exit_token = exit.clone();
        let steps = "- assertion: intake_has_metric\n  name: some.metric\n  value: 2\n  timeout: 30s\n";
        // Ending the target once a poll has failed after the snapshot keeps the test independent of request speed.
        let failed_poll_seen = async {
            while intake.connections() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            exit.cancel();
        };

        let ((record, _), ()) = tokio::join!(run(steps, &ctx), failed_poll_seen);

        let steps = record.steps();
        assert_eq!(statuses(&steps), [("intake_has_metric", None, StepStatus::Failed)]);
        let message = &steps[0].message;
        assert!(
            message.contains("\nLast collection error, after the last successful snapshot: "),
            "{message}"
        );
        assert!(
            message.contains("\n  - some.metric (count): count(1) from 1 point(s); fails value: expected 2"),
            "{message}"
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
        let intake = MockIntake::unresponsive().await;
        let cancel = CancellationToken::new();
        let ctx = context(&cancel, Some(&intake));
        cancel_after(&cancel, Duration::from_millis(1500));

        let (record, _) = run_within(INTAKE_POLL, &ctx, Duration::from_secs(1)).await;

        let steps = record.steps();
        assert_eq!(statuses(&steps), [("intake_has_metric", None, StepStatus::Interrupted)]);
        let message = &steps[0].message;
        assert!(
            message.starts_with(
                "Interrupted after 1.5s. The case left this step 1.0s of its 30s step budget.\nCancelled after 1.5s \
                 waiting for the intake sidecar's metrics dump.\n"
            ),
            "{message}"
        );
        assert!(!message.contains("Timed out"), "{message}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_parallel_block_lasts_as_long_as_its_slowest_member() {
        let intake = MockIntake::unresponsive().await;
        let ctx = context(&CancellationToken::new(), Some(&intake));
        let steps = "- parallel:\n  - assertion: process_stable_for\n    duration: 2s\n  \
                     - assertion: intake_has_metric\n    name: some.metric\n    timeout: 5s\n";

        let (record, elapsed) = run(steps, &ctx).await;

        let steps = record.steps();
        assert_eq!(
            statuses(&steps),
            [
                ("process_stable_for", Some(0), StepStatus::Passed),
                ("intake_has_metric", Some(1), StepStatus::Errored),
            ]
        );
        assert!(
            steps[1].message.starts_with(
                "Timed out after 5s waiting for the intake sidecar's metrics dump.: deadline has elapsed\n"
            ),
            "{}",
            steps[1].message
        );
        assert_eq!(elapsed, Duration::from_secs(5));
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

        let (record, _) = run(steps, &ctx).await;

        let steps = record.steps();
        assert_eq!(
            statuses(&steps),
            [
                ("log_contains", Some(0), StepStatus::Passed),
                ("process_stable_for", Some(1), StepStatus::Interrupted),
                ("intake_has_metric", Some(2), StepStatus::Interrupted),
            ]
        );
        assert_eq!(steps[1].message, "Interrupted after 1.0s.");
        assert!(
            steps[2].message.starts_with(
                "Interrupted after 1.0s.\nCancelled after 1.0s waiting for the intake sidecar's metrics dump.\n"
            ),
            "{}",
            steps[2].message
        );
    }

    #[tokio::test]
    async fn polling_steps_cancelled_between_polls_report_their_last_attempt() {
        use crate::intake::test_server::{closed_port, install_crypto_provider};

        install_crypto_provider();
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let auth_token = dir.path().join("auth_token");
        std::fs::write(&auth_token, "token").expect("the auth token should be writable");
        let missing = dir.path().join("missing");
        let closed = closed_port();
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
        use crate::intake::test_server::{closed_port, install_crypto_provider};

        install_crypto_provider();
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let directory = dir.path().display().to_string();
        let missing = dir.path().join("missing");
        let closed = closed_port();
        let context_with = |configure: &dyn Fn(&mut AssertionContext)| {
            let mut ctx = context(&CancellationToken::new(), None);
            configure(&mut ctx);
            ctx
        };
        let cli = |command: &[&str]| TargetCommand::new(command.iter().map(ToString::to_string).collect());
        // Rows share one thread, so budgets leave room for every row's setup and first attempt on a busy machine.
        let intake_poll = "- assertion: intake_has_metric\n  name: some.metric\n  timeout: 5s\n";
        let port_listening = "- assertion: port_listening\n  port: 1234\n  protocol: tcp\n  timeout: 5s\n";
        let file_contains = |path: &str| format!("- assertion: file_contains\n  path: {path}\n  timeout: 5s\n");
        let config_key = "- assertion: adp_config_key_equals\n  key: some.key\n  value: true\n  timeout: 5s\n";
        let rows = [
            (
                "intake_has_metric, closed intake port".to_string(),
                intake_poll.to_string(),
                context_with(&|ctx| ctx.intake_host_port = Some(closed)),
                StepStatus::Errored,
            ),
            (
                "intake_has_metric, no intake".to_string(),
                intake_poll.to_string(),
                context_with(&|_| {}),
                StepStatus::Errored,
            ),
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
