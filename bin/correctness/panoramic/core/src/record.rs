//! Step results, steps in progress and the shared case record that keeps them through cancellation.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tokio::time::Instant;

use crate::deadline::{Budget, Deadline};
use crate::result::{DurationMs, PhaseTiming, TestResult};

/// Shared step states and verdict, retained by the runner when it drops the test future.
///
/// The step driver records each step's start and result. The first [`Verdict`] determines the outcome, which the
/// runner preserves if a deadline or signal arrives later. Running steps identify where the test was interrupted.
#[derive(Clone, Debug, Default)]
pub struct CaseRecord {
    state: Arc<Mutex<RecordState>>,
}

#[derive(Debug, Default)]
struct RecordState {
    /// One slot per procedure step or parallel member, in procedure order.
    slots: Vec<Slot>,
    /// The first decisive event.
    verdict: Option<Verdict>,
}

#[derive(Debug)]
enum Slot {
    /// A planned step with its `not_run` result.
    Waiting(StepResult),
    Running(RunningStep),
    Finished(StepResult),
}

/// A completed procedure or setup failure that fixes the case's outcome.
#[derive(Clone, Debug)]
pub enum Verdict {
    /// The procedure finished. The step statuses decide the outcome.
    Procedure,
    /// Setup failed with this result.
    SetupFailed(Box<TestResult>),
}

impl CaseRecord {
    fn lock(&self) -> MutexGuard<'_, RecordState> {
        // The state stays consistent if a holder panics: every update is a single assignment.
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Registers the procedure's steps as `not_run`.
    pub fn plan(&self, steps: Vec<StepResult>) {
        self.lock().slots = steps.into_iter().map(Slot::Waiting).collect();
    }

    /// Marks the step in `slot` as running.
    pub fn start(&self, slot: usize, step: RunningStep) {
        if let Some(entry) = self.lock().slots.get_mut(slot) {
            *entry = Slot::Running(step);
        }
    }

    /// Records the result of the step in `slot`.
    pub fn finish(&self, slot: usize, result: StepResult) {
        if let Some(entry) = self.lock().slots.get_mut(slot) {
            *entry = Slot::Finished(result);
        }
    }

    /// Sets the verdict if the case is undecided.
    pub fn decide(&self, verdict: Verdict) {
        self.lock().verdict.get_or_insert(verdict);
    }

    /// Whether a verdict has been reached.
    pub fn decided(&self) -> bool {
        self.lock().verdict.is_some()
    }

    /// Returns the running steps in procedure order.
    pub fn running(&self) -> Vec<ActiveStep> {
        self.lock()
            .slots
            .iter()
            .filter_map(|slot| match slot {
                Slot::Running(step) => Some(step.active()),
                Slot::Waiting(_) | Slot::Finished(_) => None,
            })
            .collect()
    }

    /// Returns all planned steps in order, reporting running steps as `interrupted` and waiting steps as `not_run`.
    pub fn steps(&self) -> Vec<StepResult> {
        self.lock()
            .slots
            .iter()
            .map(|slot| match slot {
                Slot::Waiting(result) | Slot::Finished(result) => result.clone(),
                Slot::Running(step) => step.interrupted(None),
            })
            .collect()
    }

    /// Builds a result from the recorded setup failure or step statuses, using `duration` and `phase_timings`.
    pub fn result(&self, name: &str, duration: Duration, phase_timings: Vec<PhaseTiming>) -> TestResult {
        let verdict = self.lock().verdict.clone();
        match verdict {
            Some(Verdict::SetupFailed(result)) => TestResult {
                duration,
                phase_timings,
                ..*result
            },
            Some(Verdict::Procedure) | None => TestResult::from_steps(name, duration, self.steps(), phase_timings),
        }
    }
}

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
    pub fn new(name: &str, started: Instant, status: AssertionStatus, message: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status,
            message: message.into(),
            duration: started.elapsed(),
        }
    }
}

/// Identity and timing of a running step, retained in the case record until it finishes.
#[derive(Clone, Copy, Debug)]
pub struct RunningStep {
    /// Position of the step in the procedure, from zero.
    pub step: usize,
    /// Zero-based position within a parallel block, or `None` for a step that runs alone.
    pub member: Option<usize>,
    /// Step kind, such as `log_contains`.
    pub name: &'static str,
    /// When the step started running.
    pub started: Instant,
    /// The step's own budget.
    pub budget: Duration,
    /// The case deadline narrowed to `budget`.
    pub deadline: Deadline,
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
    pub fn interrupted(&self, step_message: Option<String>) -> StepResult {
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

    /// Returns an `errored` result for a step that ran past its budget and the driver's grace period.
    pub fn overran(&self) -> StepResult {
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
    pub fn returned(&self, result: AssertionResult) -> StepResult {
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

/// The recorded result of a procedure step or parallel member.
#[derive(Clone, Debug)]
pub struct StepResult {
    /// Position of the step in the procedure, from zero.
    pub step: usize,
    /// Zero-based position within a parallel block, or `None` for a step that runs alone.
    pub member: Option<usize>,
    /// Step kind, such as `log_contains`.
    pub name: String,
    /// How the step ended, or `not_run`.
    pub status: StepStatus,
    /// Human-readable detail about the result.
    pub message: String,
    /// How long the step ran.
    pub duration: Duration,
}

/// Execution status of a procedure step.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// The step met its expectation.
    Passed,
    /// The step observed behavior that did not meet its expectation.
    Failed,
    /// A budget overrun, harness failure, or invalid configuration prevented evaluation of the step.
    Errored,
    /// Cancellation or the case budget stopped the step.
    Interrupted,
    /// The step never started.
    NotRun,
}

/// A step that was running when the runner stopped the test.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ActiveStep {
    /// Position of the step in the procedure, from zero.
    pub step: usize,
    /// Zero-based position within a parallel block, or `None` for a step that runs alone.
    pub member: Option<usize>,
    /// Step kind, such as `log_contains`.
    pub kind: String,
    /// How long the step had been running, in milliseconds.
    pub elapsed_ms: u64,
    /// The step's own budget, in milliseconds.
    pub budget_ms: u64,
}

/// Formats a one-based step label: `3` for a standalone step, `3.1` for a parallel member.
pub fn step_label(step: usize, member: Option<usize>) -> String {
    match member {
        Some(member) => format!("{}.{}", step + 1, member + 1),
        None => (step + 1).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::advance;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_running_step_is_reported_as_interrupted_and_a_waiting_step_as_not_run() {
        let record = CaseRecord::default();
        record.plan(
            ["running", "waiting"]
                .into_iter()
                .enumerate()
                .map(|(step, name)| StepResult {
                    step,
                    member: None,
                    name: name.to_string(),
                    status: StepStatus::NotRun,
                    message: "Not run.".to_string(),
                    duration: Duration::ZERO,
                })
                .collect(),
        );
        record.start(
            0,
            RunningStep {
                step: 0,
                member: None,
                name: "running",
                started: Instant::now(),
                budget: Duration::from_secs(30),
                deadline: Deadline::after(Budget::Step, Duration::from_secs(30)),
            },
        );

        advance(Duration::from_secs(1)).await;

        let steps = record.steps();
        let results: Vec<_> = steps
            .iter()
            .map(|step| {
                (
                    step.step,
                    step.member,
                    step.name.as_str(),
                    step.status,
                    step.message.as_str(),
                    step.duration,
                )
            })
            .collect();
        assert_eq!(
            results,
            [
                (
                    0,
                    None,
                    "running",
                    StepStatus::Interrupted,
                    "Interrupted after 1.0s.",
                    Duration::from_secs(1),
                ),
                (1, None, "waiting", StepStatus::NotRun, "Not run.", Duration::ZERO),
            ]
        );
        assert_eq!(
            record.running(),
            [ActiveStep {
                step: 0,
                member: None,
                kind: "running".to_string(),
                elapsed_ms: 1000,
                budget_ms: 30_000,
            }]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_cut_short_by_the_case_deadline_reports_what_the_case_left_it() {
        let running = RunningStep {
            step: 0,
            member: None,
            name: "running",
            started: Instant::now(),
            budget: Duration::from_secs(30),
            deadline: Deadline::after(Budget::Case, Duration::from_secs(2)),
        };

        advance(Duration::from_millis(1500)).await;
        let result = running.interrupted(Some("own message".to_string()));

        assert_eq!(result.status, StepStatus::Interrupted);
        assert_eq!(result.duration, Duration::from_millis(1500));
        assert_eq!(
            result.message,
            "Interrupted after 1.5s. The case left this step 2.0s of its 30s step budget.\nown message"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_that_overruns_its_budget_is_errored() {
        let running = RunningStep {
            step: 0,
            member: None,
            name: "running",
            started: Instant::now(),
            budget: Duration::from_secs(1),
            deadline: Deadline::after(Budget::Step, Duration::from_secs(1)),
        };

        advance(Duration::from_secs(11)).await;
        let result = running.overran();

        assert_eq!(result.status, StepStatus::Errored);
        assert_eq!(result.duration, Duration::from_secs(11));
        assert_eq!(
            result.message,
            "Did not finish within its 1s step budget; stopped after 11.0s."
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_returned_result_keeps_its_status_message_and_duration() {
        let running = RunningStep {
            step: 2,
            member: Some(1),
            name: "running",
            started: Instant::now(),
            budget: Duration::from_secs(30),
            deadline: Deadline::after(Budget::Step, Duration::from_secs(30)),
        };

        advance(Duration::from_secs(2)).await;
        let result = running.returned(AssertionResult {
            name: "reported name".to_string(),
            status: AssertionStatus::Failed,
            message: "own message".to_string(),
            duration: Duration::from_secs(1),
        });

        assert_eq!(result.status, StepStatus::Failed);
        assert_eq!(result.message, "own message");
        assert_eq!(result.duration, Duration::from_secs(1));
        assert_eq!(result.step, 2);
        assert_eq!(result.member, Some(1));
        assert_eq!(result.name, "running");
    }
}
