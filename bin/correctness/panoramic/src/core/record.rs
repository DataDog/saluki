//! Shared test progress and verdicts that survive cancellation.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tokio::time::Instant;

use crate::core::deadline::{Budget, Deadline};
use crate::core::result::{DurationMs, PhaseTiming, TestResult};

/// Shared step states and verdict, retained by the runner when it drops the test future.
///
/// The step driver records each step's start and result. The first [`Verdict`] determines the outcome, which the
/// runner preserves if a deadline or signal arrives later. Running steps identify where the test was interrupted.
#[derive(Clone, Debug, Default)]
pub(crate) struct CaseRecord {
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
pub(crate) enum Verdict {
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
    pub(crate) fn plan(&self, steps: Vec<StepResult>) {
        self.lock().slots = steps.into_iter().map(Slot::Waiting).collect();
    }

    /// Marks the step in `slot` as running.
    pub(crate) fn start(&self, slot: usize, step: RunningStep) {
        if let Some(entry) = self.lock().slots.get_mut(slot) {
            *entry = Slot::Running(step);
        }
    }

    /// Records the result of the step in `slot`.
    pub(crate) fn finish(&self, slot: usize, result: StepResult) {
        if let Some(entry) = self.lock().slots.get_mut(slot) {
            *entry = Slot::Finished(result);
        }
    }

    /// Sets the verdict if the case is undecided.
    pub(crate) fn decide(&self, verdict: Verdict) {
        self.lock().verdict.get_or_insert(verdict);
    }

    /// Whether a verdict has been reached.
    pub(crate) fn decided(&self) -> bool {
        self.lock().verdict.is_some()
    }

    /// Returns the running steps in procedure order.
    pub(crate) fn running(&self) -> Vec<ActiveStep> {
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
    pub(crate) fn steps(&self) -> Vec<StepResult> {
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
    pub(crate) fn result(&self, name: &str, duration: Duration, phase_timings: Vec<PhaseTiming>) -> TestResult {
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
pub(crate) fn step_label(step: usize, member: Option<usize>) -> String {
    match member {
        Some(member) => format!("{}.{}", step + 1, member + 1),
        None => (step + 1).to_string(),
    }
}

/// Identity and timing of a running step, retained in the case record until it finishes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RunningStep {
    pub(crate) step: usize,
    pub(crate) member: Option<usize>,
    pub(crate) name: &'static str,
    pub(crate) started: Instant,
    /// The step's own budget.
    pub(crate) budget: Duration,
    /// The case deadline narrowed to `budget`.
    pub(crate) deadline: Deadline,
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

    /// Returns an `errored` result for a step that exceeded its budget and the slack the step driver allows.
    pub(crate) fn overran(&self) -> StepResult {
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

    /// Returns a result for this step with the given status, message and duration.
    pub(crate) fn result(&self, status: StepStatus, message: String, duration: Duration) -> StepResult {
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
