//! Runs a case's procedure: builds every step first, then runs steps in order, members of a parallel block
//! together, each bounded by its budget and the case deadline.

use std::{fmt, time::Duration};

use futures::future::join_all;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::{
    case::ProcedureNode,
    deadline::{Budget, Deadline},
    record::{step_label, AssertionStatus, CaseRecord, RunningStep, StepReport, StepResult, StepStatus, Verdict},
};

/// Grace period for a step to report its own result after its deadline.
const STEP_SLACK: Duration = Duration::from_secs(10);

/// A procedure step or parallel member, carrying the context it needs to run.
#[async_trait::async_trait]
pub trait Member: Send + Sync {
    /// The step kind, such as `log_contains`.
    fn name(&self) -> &'static str;

    /// A human-readable description for the execution log.
    fn description(&self) -> String;

    /// The step's own time budget.
    fn budget(&self) -> Duration;

    /// Runs the step and reports its status, message and duration.
    ///
    /// Release owned resources on drop: the driver may drop this future on cancellation or a budget overrun.
    async fn run(&self) -> StepReport;
}

/// A fully built procedure, with one record slot per step or parallel member.
pub struct Procedure<'a> {
    steps: Vec<Vec<Planned<'a>>>,
}

impl<'a> Procedure<'a> {
    /// Builds every member and assigns record slots in procedure order.
    ///
    /// # Errors
    ///
    /// Returns a message naming the first member that could not be built.
    pub fn build<E>(
        nodes: impl IntoIterator<Item = ProcedureNode<Result<Box<dyn Member + 'a>, E>>>,
    ) -> Result<Self, String>
    where
        E: fmt::Display,
    {
        let mut slot = 0;
        let mut steps = Vec::new();
        for (step, node) in nodes.into_iter().enumerate() {
            let members: Vec<_> = match node {
                ProcedureNode::Step(member) => vec![(None, member)],
                ProcedureNode::Parallel(members) => members
                    .into_iter()
                    .enumerate()
                    .map(|(index, member)| (Some(index), member))
                    .collect(),
            };
            let mut planned = Vec::with_capacity(members.len());
            for (member, runner) in members {
                let runner = runner.map_err(|error| {
                    format!("Failed to create procedure step {}: {error}.", step_label(step, member))
                })?;
                planned.push(Planned {
                    slot,
                    step,
                    member,
                    runner,
                });
                slot += 1;
            }
            steps.push(planned);
        }
        Ok(Self { steps })
    }

    /// Plans every record slot, then runs steps in order and parallel members together.
    ///
    /// A non-passing block ends the procedure after all its members finish. Cancellation or interruption leaves
    /// the case undecided; completion or fail-fast records [`Verdict::Procedure`]. Later steps remain `not_run`.
    pub async fn run(&self, case: Deadline, cancel: &CancellationToken, record: &CaseRecord) {
        record.plan(self.steps.iter().flatten().map(Planned::not_run).collect());

        let total_steps = self.steps.len();
        for (step_index, members) in self.steps.iter().enumerate() {
            if cancel.is_cancelled() {
                return;
            }

            debug!(
                step = step_index + 1,
                step_total = total_steps,
                member_count = members.len(),
                "Starting procedure step..."
            );
            let statuses = join_all(members.iter().map(|member| member.run(record, case, cancel))).await;

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
}

struct Planned<'a> {
    slot: usize,
    step: usize,
    member: Option<usize>,
    runner: Box<dyn Member + 'a>,
}

impl Planned<'_> {
    fn not_run(&self) -> StepResult {
        StepResult {
            step: self.step,
            member: self.member,
            name: self.runner.name().to_string(),
            status: StepStatus::NotRun,
            message: "Not run.".to_string(),
            duration: Duration::ZERO,
        }
    }

    async fn run(&self, record: &CaseRecord, case: Deadline, cancel: &CancellationToken) -> StepStatus {
        let budget = self.runner.budget();
        let running = RunningStep {
            step: self.step,
            member: self.member,
            name: self.runner.name(),
            started: Instant::now(),
            budget,
            deadline: case.narrow(Budget::Step, budget),
        };
        record.start(self.slot, running);
        debug!(step_type = running.name, description = %self.runner.description(), "Running procedure step...");

        // Poll the step first so a failure returned on cancellation keeps its own message.
        let result = tokio::select! {
            biased;
            report = self.runner.run() => if report.status != AssertionStatus::Passed && cancel.is_cancelled() {
                running.interrupted(Some(report.message))
            } else {
                running.returned(report)
            },
            _ = cancel.cancelled() => running.interrupted(None),
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
        record.finish(self.slot, result);
        status
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::time::{sleep, timeout};

    use super::*;
    use crate::result::{ErrorKind, TestOutcome};

    #[derive(Clone)]
    enum Behavior {
        Report {
            after: Duration,
            status: AssertionStatus,
        },
        Pending,
        OnCancel {
            cancel: CancellationToken,
            status: AssertionStatus,
        },
    }

    #[derive(Clone)]
    struct Fake {
        name: &'static str,
        budget: Duration,
        message: String,
        behavior: Behavior,
        starts: Arc<Mutex<Vec<Instant>>>,
    }

    impl Fake {
        fn new(name: &'static str, behavior: Behavior) -> Self {
            Self {
                name,
                budget: Duration::from_secs(30),
                message: format!("{name} report"),
                behavior,
                starts: Arc::default(),
            }
        }

        fn report(name: &'static str, seconds: u64, status: AssertionStatus) -> Self {
            Self::new(
                name,
                Behavior::Report {
                    after: Duration::from_secs(seconds),
                    status,
                },
            )
        }
    }

    #[async_trait::async_trait]
    impl Member for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> String {
            self.message.clone()
        }
        fn budget(&self) -> Duration {
            self.budget
        }
        async fn run(&self) -> StepReport {
            let started = Instant::now();
            self.starts.lock().unwrap().push(started);
            let status = match &self.behavior {
                Behavior::Report { after, status } => {
                    sleep(*after).await;
                    *status
                }
                Behavior::Pending => std::future::pending().await,
                Behavior::OnCancel { cancel, status } => {
                    cancel.cancelled().await;
                    *status
                }
            };
            StepReport::new(started, status, self.message.clone())
        }
    }

    fn build(nodes: Vec<ProcedureNode<Fake>>) -> Procedure<'static> {
        let boxed = |member: Fake| Ok::<Box<dyn Member>, String>(Box::new(member));
        Procedure::build(nodes.into_iter().map(|node| match node {
            ProcedureNode::Step(member) => ProcedureNode::Step(boxed(member)),
            ProcedureNode::Parallel(members) => ProcedureNode::Parallel(members.into_iter().map(boxed).collect()),
        }))
        .unwrap()
    }

    async fn run_within(
        procedure: &Procedure<'_>, cancel: &CancellationToken, case: Duration,
    ) -> (CaseRecord, Duration) {
        let record = CaseRecord::default();
        let started = Instant::now();
        timeout(
            Duration::from_secs(120),
            procedure.run(Deadline::after(Budget::Case, case), cancel, &record),
        )
        .await
        .expect("the procedure must finish within the test bound");
        (record, started.elapsed())
    }

    async fn run(procedure: &Procedure<'_>, cancel: &CancellationToken) -> (CaseRecord, Duration) {
        run_within(procedure, cancel, Duration::from_secs(3600)).await
    }

    async fn cancel_after_one_second(cancel: &CancellationToken) {
        sleep(Duration::from_secs(1)).await;
        cancel.cancel();
    }

    fn statuses(record: &CaseRecord) -> Vec<StepStatus> {
        record.steps().iter().map(|step| step.status).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn a_completed_procedure_decides_the_case_and_keeps_every_report() {
        for status in [AssertionStatus::Passed, AssertionStatus::Failed] {
            let first = Fake::report("first", 1, AssertionStatus::Passed);
            let second = Fake::report("second", 2, AssertionStatus::Passed);
            let third = Fake::report("third", 3, status);
            let procedure = build(vec![
                ProcedureNode::Step(first.clone()),
                ProcedureNode::Parallel(vec![]),
                ProcedureNode::Step(second.clone()),
                ProcedureNode::Step(third.clone()),
            ]);
            assert!(first.starts.lock().unwrap().is_empty());
            let (record, elapsed) = run(&procedure, &CancellationToken::new()).await;
            assert!(record.decided());
            assert_eq!(elapsed, Duration::from_secs(6));
            assert_eq!(
                statuses(&record),
                [StepStatus::Passed, StepStatus::Passed, status.into()]
            );
            let steps = record.steps();
            assert_eq!(steps[1].name, "second");
            assert_eq!(steps[1].message, "second report");
            assert_eq!(steps[1].duration, Duration::from_secs(2));
            assert_eq!(steps[2].message, "third report");
            for member in [first, second, third] {
                assert_eq!(member.starts.lock().unwrap().len(), 1);
            }
            assert_eq!(
                record.result("case", elapsed, vec![]).outcome,
                if status == AssertionStatus::Failed {
                    TestOutcome::Failed
                } else {
                    TestOutcome::Passed
                }
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn members_take_record_slots_in_procedure_order_with_parallel_members_counted_separately() {
        let procedure = build(vec![
            ProcedureNode::Step(Fake::report("first", 0, AssertionStatus::Passed)),
            ProcedureNode::Parallel(vec![
                Fake::report("left", 2, AssertionStatus::Passed),
                Fake::report("right", 3, AssertionStatus::Passed),
            ]),
            ProcedureNode::Parallel(vec![Fake::report("single", 0, AssertionStatus::Passed)]),
        ]);
        let record = CaseRecord::default();
        let cancel = CancellationToken::new();
        let case = Deadline::after(Budget::Case, Duration::from_secs(60));
        tokio::join!(procedure.run(case, &cancel, &record), async {
            sleep(Duration::from_secs(1)).await;
            let active = record.running();
            assert_eq!(
                active
                    .iter()
                    .map(|step| (step.step, step.member, step.kind.as_str(), step.budget_ms))
                    .collect::<Vec<_>>(),
                [(1, Some(0), "left", 30000), (1, Some(1), "right", 30000)]
            );
        });
        let steps = record.steps();
        assert_eq!(
            steps.iter().map(|step| (step.step, step.member)).collect::<Vec<_>>(),
            [(0, None), (1, Some(0)), (1, Some(1)), (2, Some(0))]
        );
        assert_eq!(statuses(&record), [StepStatus::Passed; 4]);
        assert!(record.running().is_empty());
    }

    #[test]
    fn building_stops_at_the_first_member_that_fails_and_names_it() {
        let first = Fake::report("first", 0, AssertionStatus::Passed);
        let boxed = || Ok::<Box<dyn Member>, &str>(Box::new(first.clone()));
        let nodes = vec![
            ProcedureNode::Step(boxed()),
            ProcedureNode::Parallel(vec![boxed(), Err("boom")]),
        ];
        let result = Procedure::build(nodes.into_iter().chain(std::iter::once_with(|| {
            panic!("must stop building after the first error")
        })));
        assert_eq!(result.err().unwrap(), "Failed to create procedure step 2.2: boom.");
        let result = Procedure::build([
            ProcedureNode::Step(boxed()),
            ProcedureNode::Step(boxed()),
            ProcedureNode::Step(Err("boom")),
        ]);
        assert_eq!(result.err().unwrap(), "Failed to create procedure step 3: boom.");
        assert!(first.starts.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_member_ends_the_procedure_after_its_block_and_leaves_later_steps_not_run() {
        for status in [AssertionStatus::Failed, AssertionStatus::Errored] {
            let later = Fake::report("later", 0, AssertionStatus::Passed);
            let procedure = build(vec![
                ProcedureNode::Parallel(vec![
                    Fake::report("fails", 1, status),
                    Fake::report("sibling", 3, AssertionStatus::Passed),
                ]),
                ProcedureNode::Step(later.clone()),
            ]);
            let (record, elapsed) = run(&procedure, &CancellationToken::new()).await;
            assert_eq!(elapsed, Duration::from_secs(3));
            assert_eq!(
                statuses(&record),
                [status.into(), StepStatus::Passed, StepStatus::NotRun]
            );
            assert_eq!(record.steps()[1].duration, Duration::from_secs(3));
            assert_eq!(record.steps()[2].message, "Not run.");
            assert_eq!(record.steps()[2].duration, Duration::ZERO);
            assert!(record.decided());
            assert!(later.starts.lock().unwrap().is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_parallel_block_lasts_as_long_as_its_slowest_member() {
        let first = Fake::report("first", 2, AssertionStatus::Passed);
        let second = Fake::report("second", 5, AssertionStatus::Passed);
        let procedure = build(vec![ProcedureNode::Parallel(vec![first.clone(), second.clone()])]);
        let started = Instant::now();
        let (record, elapsed) = run(&procedure, &CancellationToken::new()).await;
        assert_eq!(elapsed, Duration::from_secs(5));
        assert_eq!(statuses(&record), [StepStatus::Passed; 2]);
        assert_eq!(*first.starts.lock().unwrap(), [started]);
        assert_eq!(*second.starts.lock().unwrap(), [started]);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_a_member_that_ignores_it_and_keeps_finished_steps() {
        let cancel = CancellationToken::new();
        let later = Fake::report("later", 0, AssertionStatus::Passed);
        let procedure = build(vec![
            ProcedureNode::Step(Fake::report("first", 0, AssertionStatus::Passed)),
            ProcedureNode::Step(Fake::new("pending", Behavior::Pending)),
            ProcedureNode::Step(later.clone()),
        ]);
        let ((record, elapsed), ()) = tokio::join!(run(&procedure, &cancel), cancel_after_one_second(&cancel));
        assert_eq!(elapsed, Duration::from_secs(1));
        assert_eq!(
            statuses(&record),
            [StepStatus::Passed, StepStatus::Interrupted, StepStatus::NotRun]
        );
        assert_eq!(record.steps()[1].message, "Interrupted after 1.0s.");
        assert!(!record.decided());
        assert!(later.starts.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_every_unfinished_parallel_member() {
        let cancel = CancellationToken::new();
        let procedure = build(vec![ProcedureNode::Parallel(vec![
            Fake::report("first", 0, AssertionStatus::Passed),
            Fake::new("second", Behavior::Pending),
            Fake::new("third", Behavior::Pending),
        ])]);
        let ((record, elapsed), ()) = tokio::join!(run(&procedure, &cancel), cancel_after_one_second(&cancel));
        assert_eq!(elapsed, Duration::from_secs(1));
        assert_eq!(
            statuses(&record),
            [StepStatus::Passed, StepStatus::Interrupted, StepStatus::Interrupted]
        );
        for step in &record.steps()[1..] {
            assert_eq!(step.message, "Interrupted after 1.0s.");
        }
        assert!(!record.decided());
    }

    #[tokio::test(start_paused = true)]
    async fn a_member_that_fails_on_cancellation_keeps_its_message_under_the_interruption() {
        let cancel = CancellationToken::new();
        let member = Fake {
            message: "own message".into(),
            ..Fake::new(
                "cancellable",
                Behavior::OnCancel {
                    cancel: cancel.clone(),
                    status: AssertionStatus::Failed,
                },
            )
        };
        let procedure = build(vec![ProcedureNode::Step(member)]);
        let ((record, elapsed), ()) = tokio::join!(run(&procedure, &cancel), cancel_after_one_second(&cancel));
        assert_eq!(elapsed, Duration::from_secs(1));
        assert_eq!(statuses(&record), [StepStatus::Interrupted]);
        assert_eq!(record.steps()[0].message, "Interrupted after 1.0s.\nown message");
        assert!(!record.decided());
    }

    #[tokio::test(start_paused = true)]
    async fn a_member_that_passes_on_cancellation_keeps_its_pass() {
        let cancel = CancellationToken::new();
        let procedure = build(vec![
            ProcedureNode::Step(Fake::new(
                "cancellable",
                Behavior::OnCancel {
                    cancel: cancel.clone(),
                    status: AssertionStatus::Passed,
                },
            )),
            ProcedureNode::Step(Fake::report("later", 0, AssertionStatus::Passed)),
        ]);
        let ((record, elapsed), ()) = tokio::join!(run(&procedure, &cancel), cancel_after_one_second(&cancel));
        assert_eq!(elapsed, Duration::from_secs(1));
        assert_eq!(statuses(&record), [StepStatus::Passed, StepStatus::NotRun]);
        assert_eq!(record.steps()[0].message, "cancellable report");
        assert!(!record.decided());
    }

    #[tokio::test(start_paused = true)]
    async fn a_member_stalled_past_its_own_budget_errors_after_the_slack() {
        let member = Fake {
            budget: Duration::from_secs(1),
            ..Fake::new("stalled", Behavior::Pending)
        };
        let procedure = build(vec![ProcedureNode::Step(member)]);
        let (record, elapsed) = run(&procedure, &CancellationToken::new()).await;
        assert_eq!(elapsed, Duration::from_secs(11));
        assert_eq!(statuses(&record), [StepStatus::Errored]);
        assert_eq!(
            record.steps()[0].message,
            "Did not finish within its 1s step budget; stopped after 11.0s."
        );
        assert!(record.decided());
        let result = record.result("case", elapsed, vec![]);
        assert_eq!(result.outcome, TestOutcome::Errored);
        let error = result.error.unwrap();
        assert_eq!(error.kind, ErrorKind::Evaluation);
        assert_eq!(
            error.message,
            "Step 1 (stalled) could not be evaluated: Did not finish within its 1s step budget; stopped after 11.0s."
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_member_cut_short_by_the_case_deadline_is_interrupted_after_the_slack() {
        let procedure = build(vec![ProcedureNode::Step(Fake::new("pending", Behavior::Pending))]);
        let (record, elapsed) = run_within(&procedure, &CancellationToken::new(), Duration::from_secs(1)).await;
        assert_eq!(elapsed, Duration::from_secs(11));
        assert_eq!(statuses(&record), [StepStatus::Interrupted]);
        assert_eq!(
            record.steps()[0].message,
            "Interrupted after 11.0s. The case left this step 1.0s of its 30s step budget."
        );
        assert!(!record.decided());
    }

    #[tokio::test]
    async fn a_cancelled_procedure_starts_no_member() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let member = Fake::new("pending", Behavior::Pending);
        let procedure = build(vec![
            ProcedureNode::Step(member.clone()),
            ProcedureNode::Parallel(vec![member.clone(), member.clone()]),
        ]);
        let (record, _) = run(&procedure, &cancel).await;
        assert_eq!(statuses(&record), [StepStatus::NotRun; 3]);
        assert!(record
            .steps()
            .iter()
            .all(|step| step.message == "Not run." && step.duration == Duration::ZERO));
        assert!(member.starts.lock().unwrap().is_empty());
        assert!(!record.decided());
    }
}
