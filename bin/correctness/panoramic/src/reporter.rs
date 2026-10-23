use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use colored::Colorize as _;
use serde::Serialize;

use crate::assertions::AssertionResult;
use crate::machine_output::{InterruptReason, RunReport, RunState, RUN_REPORT_FILE_NAME};

/// Phase name reported when the harness cannot tell which phase a test was in.
pub const UNKNOWN_PHASE: &str = "unknown";

/// Timing information for a single phase of test execution.
#[derive(Clone, Debug, Serialize)]
pub struct PhaseTiming {
    /// Name of the phase.
    pub phase: String,
    /// Duration of the phase.
    #[serde(with = "duration_millis")]
    pub duration: Duration,
}

/// The phase a test is executing right now, shared between a test and the runner that drives it.
///
/// A test that overruns its deadline may never hand its result back, so the phase it was in lives
/// outside its future: a test marks each phase as it enters it, and the runner reads the marker when
/// a deadline fires. Finished timings accumulate here too, so they survive a test that never
/// returns.
#[derive(Clone, Debug, Default)]
pub struct PhaseTracker {
    state: Arc<Mutex<PhaseTrackerState>>,
}

#[derive(Debug, Default)]
struct PhaseTrackerState {
    active: Option<String>,
    completed: Vec<PhaseTiming>,
}

impl PhaseTrackerState {
    /// Clears the active marker if it names `phase`, leaving a later phase's marker alone.
    fn clear_marker(&mut self, phase: &str) {
        if self.active.as_deref() == Some(phase) {
            self.active = None;
        }
    }
}

impl PhaseTracker {
    /// Marks `phase` as the active phase and starts timing it.
    pub fn enter(&self, phase: impl Into<String>) -> ActivePhase {
        let phase = phase.into();
        if let Ok(mut state) = self.state.lock() {
            state.active = Some(phase.clone());
        }

        ActivePhase {
            tracker: self.clone(),
            phase,
            started: Instant::now(),
        }
    }

    /// The phase active right now, if the test is in one.
    pub fn active(&self) -> Option<String> {
        self.state.lock().ok().and_then(|state| state.active.clone())
    }

    /// Timings of the phases that finished, in the order they finished.
    pub fn completed(&self) -> Vec<PhaseTiming> {
        self.state
            .lock()
            .map(|state| state.completed.clone())
            .unwrap_or_default()
    }
}

/// A phase a test is in the middle of. Marked active until it is finished or dropped.
pub struct ActivePhase {
    tracker: PhaseTracker,
    phase: String,
    started: Instant,
}

impl ActivePhase {
    /// How long the phase has been running.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Records the phase as finished and returns its timing.
    ///
    /// Consuming the phase keeps one entry per phase in the tracker.
    pub fn finish(self) -> PhaseTiming {
        let timing = PhaseTiming {
            phase: self.phase.clone(),
            duration: self.started.elapsed(),
        };

        if let Ok(mut state) = self.tracker.state.lock() {
            state.clear_marker(&self.phase);
            state.completed.push(timing.clone());
        }

        timing
    }

    /// Records the phase as finished and returns the timings of every phase finished so far.
    ///
    /// For callers that report the whole set rather than accumulating timings themselves.
    pub fn finish_and_collect(self) -> Vec<PhaseTiming> {
        let tracker = self.tracker.clone();
        self.finish();

        tracker.completed()
    }
}

impl Drop for ActivePhase {
    /// Clears the active marker for a phase that ends without being finished, so an early return
    /// leaves no stale phase name behind.
    fn drop(&mut self) {
        if let Ok(mut state) = self.tracker.state.lock() {
            state.clear_marker(&self.phase);
        }
    }
}

/// How a test case finished.
///
/// `Failed` means an assertion decided against the code under test; `Errored` means the harness
/// never got far enough to decide. Consumers use this to tell "read the diff" from "fix the
/// environment".
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    /// Every assertion passed.
    Passed,
    /// At least one assertion failed.
    Failed,
    /// The harness could not run the test to a verdict.
    Errored,
    /// The test exceeded its timeout.
    TimedOut,
}

impl TestOutcome {
    /// Whether every assertion the test ran passed.
    pub fn is_passed(&self) -> bool {
        matches!(self, Self::Passed)
    }

    /// Whether this outcome represents a harness-side error rather than an assertion verdict.
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Errored | Self::TimedOut)
    }
}

impl fmt::Display for TestOutcome {
    /// Writes the same names the JSON contract uses, so human and machine output agree.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Errored => "errored",
            Self::TimedOut => "timed_out",
        };
        f.write_str(name)
    }
}

/// Classification of a harness-side error, for consumers deciding what to do next.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The test could not be set up (missing image, bad config, failed container start).
    Setup,
    /// The test exceeded its timeout.
    Timeout,
    /// The harness was interrupted or failed internally.
    Internal,
}

/// A harness-side error that prevented a verdict.
#[derive(Clone, Debug, Serialize)]
pub struct TestError {
    /// What class of error this is.
    pub kind: ErrorKind,
    /// Human-readable detail.
    pub message: String,
}

/// The deadline that fired on a test the harness stopped for taking too long.
///
/// Only the deadlines the runner itself enforces end a test as timed out. An assertion, action, or
/// setup deadline expiring fails that assertion, and the test still reaches a verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutDeadline {
    /// The test case's own deadline, from its `timeout` setting.
    TestDeadline,
    /// The grace period a test gets to tear down after its own deadline fired.
    CleanupGrace,
}

/// What the harness knows about a test it stopped for taking too long.
///
/// `deadline` names the configured deadline that fired; `active_phase` names what the test was doing
/// when it did.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TimeoutAttribution {
    /// The deadline that fired.
    pub deadline: TimeoutDeadline,
    /// Duration of the deadline that fired, in milliseconds.
    pub configured_ms: u64,
    /// Duration of the test case's own deadline, whichever deadline fired, in milliseconds.
    pub test_deadline_ms: u64,
    /// Phase the test was in when the deadline fired, or `unknown` when it had not entered one.
    pub active_phase: String,
}

impl TimeoutAttribution {
    /// Attributes a timeout to the test case's own deadline.
    pub fn test_deadline(test_deadline: Duration, active_phase: Option<String>) -> Self {
        Self {
            deadline: TimeoutDeadline::TestDeadline,
            configured_ms: DurationMs(test_deadline).as_millis(),
            test_deadline_ms: DurationMs(test_deadline).as_millis(),
            active_phase: active_phase.unwrap_or_else(|| UNKNOWN_PHASE.to_string()),
        }
    }

    /// Attributes a timeout to the teardown grace period that follows the test's own deadline.
    ///
    /// `active_phase` is the phase the test was in when its own deadline fired, since that is what
    /// teardown interrupted.
    pub fn cleanup_grace(grace: Duration, test_deadline: Duration, active_phase: Option<String>) -> Self {
        Self {
            deadline: TimeoutDeadline::CleanupGrace,
            configured_ms: DurationMs(grace).as_millis(),
            test_deadline_ms: DurationMs(test_deadline).as_millis(),
            active_phase: active_phase.unwrap_or_else(|| UNKNOWN_PHASE.to_string()),
        }
    }
}

impl fmt::Display for TimeoutAttribution {
    /// Formats the expired budget and phase, for example `case budget 120s expired in assertions`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let configured = Duration::from_millis(self.configured_ms);
        match self.deadline {
            TimeoutDeadline::TestDeadline => {
                write!(f, "case budget {:?} expired in {}", configured, self.active_phase)
            }
            TimeoutDeadline::CleanupGrace => write!(
                f,
                "cleanup grace {:?} expired after the case budget ({:?}) expired in {}",
                configured,
                Duration::from_millis(self.test_deadline_ms),
                self.active_phase
            ),
        }
    }
}

/// Result of a single test case.
#[derive(Clone, Debug, Serialize)]
pub struct TestResult {
    /// Name of the test case.
    pub name: String,
    /// How the test finished.
    pub outcome: TestOutcome,
    /// Total duration of the test.
    #[serde(with = "duration_millis")]
    pub duration: Duration,
    /// Results of individual assertions.
    pub assertion_results: Vec<AssertionResult>,
    /// Error if the harness could not reach a verdict.
    pub error: Option<TestError>,
    /// What the harness knows about the timeout, when the outcome is [`TestOutcome::TimedOut`].
    pub timeout: Option<TimeoutAttribution>,
    /// Directory the test case was loaded from. Set by the runner.
    pub case_path: Option<PathBuf>,
    /// Directory this test's artifacts were written to. Set by the runner.
    pub log_dir: Option<PathBuf>,
    /// Timing breakdown for each phase of test execution.
    pub phase_timings: Vec<PhaseTiming>,
    /// Full per-assertion mismatch details for log output (not shown in TUI/reporter).
    #[serde(skip)]
    pub assertion_details: Vec<Vec<String>>,
}

impl TestResult {
    /// Creates a result from the assertions a test actually ran.
    pub fn from_assertions(
        name: impl Into<String>, duration: Duration, assertion_results: Vec<AssertionResult>,
        phase_timings: Vec<PhaseTiming>,
    ) -> Self {
        let passed = assertion_results.iter().all(|r| r.passed);
        Self {
            name: name.into(),
            outcome: if passed {
                TestOutcome::Passed
            } else {
                TestOutcome::Failed
            },
            duration,
            assertion_results,
            error: None,
            timeout: None,
            case_path: None,
            log_dir: None,
            phase_timings,
            assertion_details: Vec::new(),
        }
    }

    /// Creates a result for a test the harness could not run to a verdict.
    pub fn errored(
        name: impl Into<String>, kind: ErrorKind, message: impl Into<String>, duration: Duration,
        phase_timings: Vec<PhaseTiming>,
    ) -> Self {
        Self {
            name: name.into(),
            outcome: TestOutcome::Errored,
            duration,
            assertion_results: Vec::new(),
            error: None,
            timeout: None,
            case_path: None,
            log_dir: None,
            phase_timings,
            assertion_details: Vec::new(),
        }
        .with_harness_error(kind, message)
    }

    /// Marks a completed result as a harness error while retaining its partial evidence.
    pub fn with_harness_error(mut self, kind: ErrorKind, message: impl Into<String>) -> Self {
        self.outcome = if matches!(kind, ErrorKind::Timeout) {
            TestOutcome::TimedOut
        } else {
            TestOutcome::Errored
        };
        self.error = Some(TestError {
            kind,
            message: message.into(),
        });
        self
    }

    /// Records the timeout detail on a result the harness stopped for taking too long.
    pub fn with_timeout_attribution(mut self, attribution: TimeoutAttribution) -> Self {
        self.timeout = Some(attribution);
        self
    }

    /// Attaches full per-assertion mismatch details, indexed alongside `assertion_results`.
    pub fn with_assertion_details(mut self, details: Vec<Vec<String>>) -> Self {
        self.assertion_details = details;
        self
    }

    /// The one-word label every renderer prints: `PASS`, `FAIL`, `ERROR`, or `TIMEOUT`.
    pub fn label(&self) -> &'static str {
        match self.outcome {
            TestOutcome::Passed => "PASS",
            TestOutcome::Failed => "FAIL",
            TestOutcome::Errored => "ERROR",
            TestOutcome::TimedOut => "TIMEOUT",
        }
    }

    /// Formats a timeout's attribution when available, otherwise the harness error (`Error: ` on the first line).
    ///
    /// Returns no lines when neither is present.
    pub fn cause_lines(&self) -> Vec<String> {
        if let (TestOutcome::TimedOut, Some(timeout)) = (self.outcome, &self.timeout) {
            return vec![timeout.to_string()];
        }

        let Some(error) = &self.error else {
            return Vec::new();
        };
        error
            .message
            .lines()
            .enumerate()
            .map(|(i, line)| {
                if i == 0 {
                    format!("Error: {}", line)
                } else {
                    line.to_string()
                }
            })
            .collect()
    }
}

/// Output format for test results.
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

/// Reporter for test results.
pub struct Reporter {
    format: OutputFormat,
    verbose: bool,
    log_dir: Option<PathBuf>,
}

impl Reporter {
    /// Create a new reporter with the given output format.
    pub fn new(format: OutputFormat, verbose: bool, log_dir: Option<PathBuf>) -> Self {
        Self {
            format,
            verbose,
            log_dir,
        }
    }

    /// Report the result of a single test.
    pub fn report_test_result(&self, result: &TestResult, log_dir: impl AsRef<Path>) {
        if matches!(self.format, OutputFormat::Text) {
            for line in self.test_result_lines(result, log_dir.as_ref()) {
                println!("{}", line);
            }
        }
    }

    /// The text lines [`Reporter::report_test_result`] prints for one test.
    pub fn test_result_lines(&self, result: &TestResult, log_dir: &Path) -> Vec<String> {
        let label = if result.outcome.is_passed() {
            result.label().green().bold()
        } else {
            result.label().red().bold()
        };

        let mut lines = vec![format!("{} {} ({:.2?})", label, result.name, result.duration)];
        lines.extend(result.cause_lines().into_iter().map(|line| format!("  {}", line)));

        if !result.outcome.is_passed() || self.verbose {
            for assertion in &result.assertion_results {
                let indicator = if assertion.passed { "+".green() } else { "-".red() };

                lines.push(format!(
                    "  {} {} ({:.2?})",
                    indicator, assertion.name, assertion.duration
                ));
                lines.push(format!("    {}", assertion.message));
            }

            if !result.phase_timings.is_empty() {
                lines.push(format!("  {}", "Phase timings:".dimmed()));
                for phase in &result.phase_timings {
                    lines.push(format!("    {} ({:.2?})", phase.phase, phase.duration));
                }
            }

            lines.push(format!("  {} {}", "Logs:".dimmed(), log_dir.display()));
        }

        lines
    }

    /// Report the final run result.
    pub fn report_suite_result(&self, report: &RunReport) {
        match self.format {
            OutputFormat::Text => {
                println!();
                println!("{}", "=".repeat(60));

                let status = run_status_label(report);
                let status = if status == RUN_PASSED_LABEL {
                    status.green().bold()
                } else {
                    status.red().bold()
                };
                println!("{}: {}", status, run_counts(report));

                if report.tests.iter().any(|test| !test.outcome.is_passed()) {
                    println!();
                    println!("{}", "Failed tests:".red().bold());
                    for test in &report.tests {
                        if !test.outcome.is_passed() {
                            println!("  - {} (outcome={})", test.name, test.outcome);
                        }
                    }
                }

                let unfinished = run_unfinished_lines(report);
                if !unfinished.is_empty() {
                    println!();
                    for line in unfinished {
                        println!("{}", line);
                    }
                }

                if let Some(log_dir) = self.log_dir.as_deref() {
                    println!();
                    println!("Artifacts: {}", log_dir.display());
                    println!("Machine-readable: {}", log_dir.join(RUN_REPORT_FILE_NAME).display());
                }
            }
            OutputFormat::Json => {
                // `main` prints the collected report, sharing its schema with `run.json`.
            }
        }
    }
}

/// Status label of a run whose tests all passed.
pub const RUN_PASSED_LABEL: &str = "PASSED";

/// The run's one-word status: `PASSED`, `FAILED`, or `INCOMPLETE` for an interrupted run.
pub fn run_status_label(report: &RunReport) -> &'static str {
    let all_passed = report.tests.iter().all(|test| test.outcome.is_passed())
        && report.running.is_empty()
        && report.not_run.is_empty();

    if report.state == RunState::Interrupted {
        "INCOMPLETE"
    } else if all_passed {
        RUN_PASSED_LABEL
    } else {
        "FAILED"
    }
}

/// Formats outcome counts, the selected-test total, and elapsed time.
///
/// Timed-out, running, and not-run counts appear only when nonzero.
pub fn run_counts(report: &RunReport) -> String {
    let totals = &report.totals;
    let mut counts = format!(
        "{} passed, {} failed, {} errored",
        totals.passed, totals.failed, totals.errored
    );
    if totals.timed_out > 0 {
        counts.push_str(&format!(" ({} timed out)", totals.timed_out));
    }
    if !report.running.is_empty() {
        counts.push_str(&format!(", {} running", report.running.len()));
    }
    if totals.not_run > 0 {
        counts.push_str(&format!(", {} not run", totals.not_run));
    }

    let total = report.tests.len() + report.running.len() + report.not_run.len();
    let duration = Duration::from_millis(report.duration_ms);
    let duration = if duration.as_secs() >= 60 {
        format!("{}m {}s", duration.as_secs() / 60, duration.as_secs() % 60)
    } else {
        format!("{:.2?}", duration)
    };
    format!("{}, {} total ({})", counts, total, duration)
}

/// Formats any interruption reason and lists tests still running or never started, including fail-fast skips.
pub fn run_unfinished_lines(report: &RunReport) -> Vec<String> {
    let mut lines = Vec::new();
    match &report.interrupted_reason {
        Some(InterruptReason::Signal { source }) => lines.push(format!("Interrupted: {}", source)),
        Some(InterruptReason::RunnerFailed { message }) => lines.push(format!("Interrupted: {}", message)),
        None => {}
    }
    if !report.running.is_empty() {
        lines.push(format!("Still running: {}", report.running.join(", ")));
    }
    if !report.not_run.is_empty() {
        lines.push(format!("Not run: {}", report.not_run.join(", ")));
    }
    lines
}

/// A duration reported as whole milliseconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurationMs(pub Duration);

impl DurationMs {
    /// The duration in whole milliseconds, saturating instead of overflowing.
    pub fn as_millis(&self) -> u64 {
        u64::try_from(self.0.as_millis()).unwrap_or(u64::MAX)
    }
}

impl Serialize for DurationMs {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.as_millis().serialize(serializer)
    }
}

/// Serde helper for serializing Duration as milliseconds.
mod duration_millis {
    use std::time::Duration;

    use serde::{Serialize as _, Serializer};

    use super::DurationMs;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        DurationMs(*duration).serialize(serializer)
    }
}

/// Serde implementation for AssertionResult.
impl Serialize for AssertionResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut state = serializer.serialize_struct("AssertionResult", 4)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("passed", &self.passed)?;
        state.serialize_field("message", &self.message)?;
        state.serialize_field("duration_ms", &DurationMs(self.duration).as_millis())?;
        state.end()
    }
}

/// Results shared by the renderers' tests.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Expected cause line for the timeout result in [`each_outcome`].
    pub(crate) const TIMEOUT_CAUSE: &str = "case budget 50ms expired in container_start";

    /// One result per outcome, in `Passed`, `Failed`, `Errored`, `TimedOut` order, with their labels.
    pub(crate) fn each_outcome() -> [(TestResult, &'static str); 4] {
        let assertion = |passed| AssertionResult {
            name: "log_contains".to_string(),
            passed,
            message: "message".to_string(),
            duration: Duration::from_millis(1),
        };
        let timed_out = TestResult::errored("t", ErrorKind::Timeout, "slow", Duration::from_secs(1), Vec::new())
            .with_timeout_attribution(TimeoutAttribution::test_deadline(
                Duration::from_millis(50),
                Some("container_start".to_string()),
            ));

        [
            (
                TestResult::from_assertions("p", Duration::from_secs(1), vec![assertion(true)], Vec::new()),
                "PASS",
            ),
            (
                TestResult::from_assertions("f", Duration::from_secs(1), vec![assertion(false)], Vec::new()),
                "FAIL",
            ),
            (
                TestResult::errored("e", ErrorKind::Setup, "boom", Duration::from_secs(1), Vec::new()),
                "ERROR",
            ),
            (timed_out, "TIMEOUT"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{each_outcome, TIMEOUT_CAUSE};
    use super::*;
    use crate::collector::RunCollector;
    use crate::events::TestEvent;

    const LABELS: [&str; 4] = ["PASS", "FAIL", "ERROR", "TIMEOUT"];

    #[test]
    fn every_outcome_has_its_own_label() {
        let labels: Vec<&str> = each_outcome().iter().map(|(result, _)| result.label()).collect();
        assert_eq!(labels, LABELS);
    }

    #[test]
    fn a_timeout_cause_names_the_budget_and_the_phase_it_expired_in() {
        let timed_out = |attribution| {
            TestResult::errored("t", ErrorKind::Timeout, "slow", Duration::from_secs(121), Vec::new())
                .with_timeout_attribution(attribution)
        };
        let assertions = |passed| {
            vec![AssertionResult {
                name: "log_contains".to_string(),
                passed,
                message: "message".to_string(),
                duration: Duration::from_millis(1),
            }]
        };
        let cases = [
            (
                timed_out(TimeoutAttribution::test_deadline(
                    Duration::from_secs(120),
                    Some("assertions".to_string()),
                )),
                vec!["case budget 120s expired in assertions"],
            ),
            (
                timed_out(TimeoutAttribution::cleanup_grace(
                    Duration::from_secs(30),
                    Duration::from_secs(120),
                    Some("assertions".to_string()),
                )),
                vec!["cleanup grace 30s expired after the case budget (120s) expired in assertions"],
            ),
            (
                timed_out(TimeoutAttribution::test_deadline(Duration::from_secs(120), None)),
                vec!["case budget 120s expired in unknown"],
            ),
            (
                TestResult::errored(
                    "e",
                    ErrorKind::Setup,
                    "Failed to start container.\nNo such image.",
                    Duration::from_secs(1),
                    Vec::new(),
                ),
                vec!["Error: Failed to start container.", "No such image."],
            ),
            (
                TestResult::from_assertions("f", Duration::from_secs(1), assertions(false), Vec::new()),
                vec![],
            ),
            (
                TestResult::from_assertions("p", Duration::from_secs(1), assertions(true), Vec::new()),
                vec![],
            ),
        ];

        for (result, expected) in cases {
            assert_eq!(result.cause_lines(), expected, "result: {:?}", result);
        }
    }

    #[test]
    fn text_report_labels_every_outcome_and_prints_a_timeout_cause() {
        let reporter = Reporter::new(OutputFormat::Text, false, None);

        for (result, label) in each_outcome() {
            let lines = reporter.test_result_lines(&result, Path::new("/logs"));
            let first = &lines[0];
            assert!(first.contains(label), "first line was: {}", first);
            let others = LABELS.iter().filter(|other| **other != label);
            assert!(
                others.into_iter().all(|other| !first.contains(other)),
                "first line was: {}",
                first
            );
            if label == "TIMEOUT" {
                assert_eq!(lines[1], format!("  {}", TIMEOUT_CAUSE));
            }
        }
    }

    /// Applies events, an optional interrupt, and optional runner completion to a collector of `selected` tests.
    fn collected(
        log_dir: &Path, fail_fast: bool, selected: &[&str], events: Vec<TestEvent>, interrupt: Option<&'static str>,
        finished: bool,
    ) -> RunCollector {
        let selected = selected.iter().map(|name| name.to_string()).collect();
        let report = RunReport::new(Vec::new(), "linux".to_string(), 1, log_dir.to_path_buf(), selected);
        let mut collector = RunCollector::start(report, fail_fast);
        for event in &events {
            collector.observe(event);
        }
        if let Some(source) = interrupt {
            collector.interrupt(InterruptReason::Signal { source });
        }
        if finished {
            collector.finish(Ok(()));
        }
        collector
    }

    fn started(name: &str) -> TestEvent {
        TestEvent::TestStarted { name: name.to_string() }
    }

    fn completed(result: &TestResult) -> TestEvent {
        TestEvent::TestCompleted {
            result: Box::new(result.clone()),
            log_dir: PathBuf::new(),
        }
    }

    #[test]
    fn run_summary_reads_incomplete_for_an_interrupted_run() {
        let log_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let [(passed, _), (failed, _), _, (timed_out, _)] = each_outcome();

        let all_pass = collected(
            log_dir.path(),
            false,
            &["p"],
            vec![started("p"), completed(&passed)],
            None,
            true,
        );
        let report = all_pass.report();
        assert_eq!(run_status_label(report), "PASSED");
        assert!(run_counts(report).starts_with("1 passed, 0 failed, 0 errored, 1 total ("));
        assert!(run_unfinished_lines(report).is_empty());

        let fail_fast = collected(
            log_dir.path(),
            true,
            &["f", "b"],
            vec![started("f"), completed(&failed)],
            None,
            true,
        );
        let report = fail_fast.report();
        assert_eq!(run_status_label(report), "FAILED");
        assert!(run_counts(report).starts_with("0 passed, 1 failed, 0 errored, 1 not run, 2 total ("));
        assert_eq!(run_unfinished_lines(report), vec!["Not run: b"]);

        let interrupted = collected(
            log_dir.path(),
            false,
            &["t", "a", "b"],
            vec![started("t"), completed(&timed_out), started("a")],
            Some("SIGTERM"),
            false,
        );
        let report = interrupted.report();
        assert_eq!(run_status_label(report), "INCOMPLETE");
        assert!(run_counts(report)
            .starts_with("0 passed, 0 failed, 1 errored (1 timed out), 1 running, 1 not run, 3 total ("));
        assert_eq!(
            run_unfinished_lines(report),
            vec!["Interrupted: SIGTERM", "Still running: a", "Not run: b"]
        );

        let mut long_run = RunReport::new(
            Vec::new(),
            "linux".to_string(),
            1,
            log_dir.path().to_path_buf(),
            Vec::new(),
        );
        long_run.duration_ms = 63_000;
        assert_eq!(run_counts(&long_run), "0 passed, 0 failed, 0 errored, 0 total (1m 3s)");
    }

    #[test]
    fn finished_phases_are_recorded_once_and_leave_no_phase_active() {
        let phases = PhaseTracker::default();

        phases.enter("container_start").finish();
        let timings = phases.enter("assertions").finish_and_collect();

        assert_eq!(
            timings.iter().map(|t| t.phase.as_str()).collect::<Vec<_>>(),
            vec!["container_start", "assertions"]
        );
        assert_eq!(phases.completed().len(), 2);
        assert_eq!(phases.active(), None);
    }

    #[test]
    fn an_unfinished_phase_stops_being_active_once_it_is_dropped() {
        let phases = PhaseTracker::default();

        let phase = phases.enter("cleanup");
        assert_eq!(phases.active().as_deref(), Some("cleanup"));

        drop(phase);

        assert_eq!(phases.active(), None);
        assert!(phases.completed().is_empty());
    }
}
