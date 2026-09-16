use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use colored::Colorize as _;
use serde::Serialize;

use crate::assertions::AssertionResult;
use crate::machine_output::RUN_REPORT_FILE_NAME;

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
}

/// Result of running all test cases.
#[derive(Clone, Debug, Serialize)]
pub struct TestSuiteResult {
    /// Total number of tests.
    pub total: usize,
    /// Number of passed tests.
    pub passed: usize,
    /// Number of tests whose assertions failed.
    pub failed: usize,
    /// Number of tests the harness could not run to a verdict.
    pub errored: usize,
    /// Total duration of the test suite.
    #[serde(with = "duration_millis")]
    pub duration: Duration,
    /// Individual test results.
    pub results: Vec<TestResult>,
}

impl TestSuiteResult {
    /// Create a test suite result from individual test results.
    pub fn from_results(results: Vec<TestResult>, duration: Duration) -> Self {
        let total = results.len();
        let passed = results.iter().filter(|r| r.outcome == TestOutcome::Passed).count();
        let errored = results.iter().filter(|r| r.outcome.is_error()).count();
        let failed = total - passed - errored;

        Self {
            total,
            passed,
            failed,
            errored,
            duration,
            results,
        }
    }

    /// Returns true if all tests passed.
    pub fn all_passed(&self) -> bool {
        self.failed == 0 && self.errored == 0
    }

    /// Returns true if the harness failed to run at least one test to a verdict.
    pub fn any_errored(&self) -> bool {
        self.errored > 0
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
            let status = match result.outcome {
                TestOutcome::Passed => "PASS".green().bold(),
                TestOutcome::Failed => "FAIL".red().bold(),
                TestOutcome::Errored | TestOutcome::TimedOut => "ERROR".red().bold(),
            };

            println!("{} {} ({:.2?})", status, result.name, result.duration);

            // Show error if present
            if let Some(ref error) = result.error {
                let mut lines = error.message.lines();
                if let Some(first) = lines.next() {
                    println!("  {} {}", "Error:".red(), first);
                    for line in lines {
                        println!("  {}", line);
                    }
                }
            }

            // Show assertion results on failure or in verbose mode
            if !result.outcome.is_passed() || self.verbose {
                for assertion in &result.assertion_results {
                    let indicator = if assertion.passed { "+".green() } else { "-".red() };

                    println!("  {} {} ({:.2?})", indicator, assertion.name, assertion.duration);
                    println!("    {}", assertion.message);
                }

                if !result.phase_timings.is_empty() {
                    println!("  {}", "Phase timings:".dimmed());
                    for phase in &result.phase_timings {
                        println!("    {} ({:.2?})", phase.phase, phase.duration);
                    }
                }

                println!("  {} {}", "Logs:".dimmed(), log_dir.as_ref().display());
            }
        }
    }

    /// Report the final test suite result.
    pub fn report_suite_result(&self, suite: &TestSuiteResult) {
        match self.format {
            OutputFormat::Text => {
                println!();
                println!("{}", "=".repeat(60));

                let status = if suite.all_passed() {
                    "PASSED".green().bold()
                } else {
                    "FAILED".red().bold()
                };

                println!(
                    "{}: {} passed, {} failed, {} errored, {} total ({:.2?})",
                    status, suite.passed, suite.failed, suite.errored, suite.total, suite.duration
                );

                if !suite.all_passed() {
                    println!();
                    println!("{}", "Failed tests:".red().bold());
                    for result in &suite.results {
                        if !result.outcome.is_passed() {
                            println!("  - {} (outcome={})", result.name, result.outcome);
                        }
                    }
                }

                if let Some(log_dir) = self.log_dir.as_deref() {
                    println!();
                    println!("Artifacts: {}", log_dir.display());
                    println!("Machine-readable: {}", log_dir.join(RUN_REPORT_FILE_NAME).display());
                }
            }
            OutputFormat::Json => {
                // Nothing here: the machine-readable document is emitted by `report_run`, which
                // owns the versioned contract. Keeping it in one place stops the two shapes from
                // drifting apart.
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
