//! Test execution shared by TUI and plain output.
//!
//! The runner sends progress and results to the run collector as events.

use std::num::NonZeroUsize;
use std::{
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use futures::stream::{self, StreamExt as _};
use saluki_error::{generic_error, GenericError};
use tokio::pin;
use tokio::sync::{mpsc, Semaphore, SemaphorePermit};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::core::deadline::{Budget, Deadline};
use crate::core::ledger::RECLAIM_TIME;
use crate::core::plan::CasePlan;
use crate::core::record::{ActiveStep, CaseRecord, StepStatus};
use crate::core::result::{
    ErrorKind, Issue, IssueKind, PhaseTiming, PhaseTracker, StopCause, StoppedBy, TestResult, TimeoutAttribution,
    UNKNOWN_PHASE,
};
use crate::core::tasks::TASK_STOP_TIME;
use crate::events::TestEvent;
use crate::test::{RunnerSettings, Test, TestContext};

/// A sender that the `Runner` can use to send `TestEvent`s.
pub(crate) type EventSender = mpsc::UnboundedSender<TestEvent>;

/// How long a cancelled or timed-out test has to return its result before the runner drops it.
const GRACE_TIME: Duration = Duration::from_secs(30);

/// The file in a case's log directory that holds its plan when it does not pass.
const PLAN_FILE: &str = "plan.json";

pub(crate) struct RunArgs {
    /// The number of tests to run in parallel.
    parallelism: NonZeroUsize,

    /// Whether to stop execution at the first failure.
    fail_fast: bool,

    /// Reports run progress and results to the collector.
    event_sender: EventSender,

    /// Requests run cancellation.
    ///
    /// The consume loop cancels this on an interrupt. Active tests get `GRACE_TIME` to return.
    cancel_all: CancellationToken,
}

impl RunArgs {
    pub(crate) fn new(cancel_all: CancellationToken, event_sender: EventSender) -> Self {
        Self {
            parallelism: NonZeroUsize::MIN,
            fail_fast: false,
            event_sender,
            cancel_all,
        }
    }

    pub(crate) fn with_parallelism(mut self, parallelism: NonZeroUsize) -> Self {
        self.parallelism = parallelism;
        self
    }

    pub(crate) fn with_fail_fast(mut self, fail_fast: bool) -> Self {
        self.fail_fast = fail_fast;
        self
    }
}

/// Owns the set of registered tests and orchestrates their execution.
///
/// Handles parallelism, timeout enforcement, cancellation propagation, log directory creation,
/// and event dispatch. Individual test logic lives in the `Test`
/// implementations.
pub(crate) struct Runner {
    tests: Vec<Box<dyn Test>>,
    log_base_dir: PathBuf,
    settings: RunnerSettings,
}

/// What bounds how many tests run at once.
struct Permits {
    /// One for each test that runs.
    general: Semaphore,
    /// One for the test that runs host processes, whose fixed ports another such test would collide with.
    host: Semaphore,
}

/// A test's hold on the runner's permits, released when it is dropped.
struct Held<'a> {
    _general: SemaphorePermit<'a>,
    _host: Option<SemaphorePermit<'a>>,
}

impl Permits {
    fn new(parallelism: usize) -> Self {
        Self {
            general: Semaphore::new(parallelism),
            host: Semaphore::new(1),
        }
    }

    /// Waits for what `test` needs. A test that runs host processes takes the host permit first, so it does not hold a
    /// general permit while it waits for the host. The permits are held until the test has run and reclaimed.
    async fn acquire(&self, test: &dyn Test) -> Held<'_> {
        let host = if test.runs_host_processes() {
            Some(self.host.acquire().await.unwrap())
        } else {
            None
        };
        Held {
            _general: self.general.acquire().await.unwrap(),
            _host: host,
        }
    }
}

impl Runner {
    pub(crate) fn new(log_base_dir: impl Into<PathBuf>, settings: RunnerSettings) -> Self {
        Self {
            tests: Vec::new(),
            log_base_dir: log_base_dir.into(),
            settings,
        }
    }

    /// Register a test. Returns an error if the test name is a duplicate.
    pub(crate) fn register(&mut self, test: Box<dyn Test>) -> Result<(), GenericError> {
        // Inefficient but it probably does not matter unless we have thousands of tests.
        if self.tests.iter().any(|existing| existing.name() == test.name()) {
            return Err(generic_error!(
                "A test named '{}' already exists in the registry",
                test.name()
            ));
        }
        self.tests.push(test);
        Ok(())
    }

    /// Runs the registered tests, reporting progress and results as events.
    pub(crate) async fn run_tests(&self, args: RunArgs) {
        let RunArgs {
            parallelism,
            fail_fast,
            event_sender,
            cancel_all,
        } = args;

        let tests: Vec<&dyn Test> = self.tests.iter().map(|t| t.as_ref()).collect();

        let _ = event_sender.send(TestEvent::RunStarted {
            total_tests: tests.len(),
        });

        let parallelism = parallelism.get();
        let permits = Arc::new(Permits::new(parallelism));

        if fail_fast {
            self.run_fail_fast(&tests, &permits, &event_sender, &cancel_all).await;
        } else {
            self.run_parallel(&tests, parallelism, &permits, &event_sender, &cancel_all)
                .await;
        }
    }

    async fn run_fail_fast(
        &self, tests: &[&dyn Test], permits: &Permits, event_sender: &EventSender, cancel_all: &CancellationToken,
    ) {
        for test in tests {
            if cancel_all.is_cancelled() {
                break;
            }

            let _permits = permits.acquire(*test).await;
            let result = Self::run_one(
                *test,
                event_sender,
                cancel_all,
                self.log_base_dir.clone(),
                self.settings.clone(),
            )
            .await;

            if result.outcome.is_failure() {
                break;
            }
        }
    }

    async fn run_parallel(
        &self, tests: &[&dyn Test], parallelism: usize, permits: &Arc<Permits>, event_sender: &EventSender,
        cancel_all: &CancellationToken,
    ) {
        let mut futures = stream::FuturesUnordered::new();

        for test in tests {
            let permits = permits.clone();
            let cancel = cancel_all.clone();
            let log_base_dir = self.log_base_dir.clone();
            let settings = self.settings.clone();
            futures.push(async move {
                if cancel.is_cancelled() {
                    return;
                }

                let _permits = permits.acquire(*test).await;

                if cancel.is_cancelled() {
                    return;
                }

                Self::run_one(*test, event_sender, &cancel, log_base_dir, settings).await;
            });

            while futures.len() >= parallelism {
                futures.next().await;
            }
        }

        while futures.next().await.is_some() {}
    }

    async fn run_one(
        test: &dyn Test, event_sender: &EventSender, cancel_all: &CancellationToken, log_base_dir: PathBuf,
        settings: RunnerSettings,
    ) -> TestResult {
        let name = test.name();
        let suite = test.suite();
        let timeout = test.timeout();
        let started = Instant::now();

        // A test-specific token lets a timeout cancel this test without cancelling the run.
        let test_cancel = CancellationToken::new();

        // Create a directory for the test to write logs into and pass it into the test context.
        let log_dir = log_base_dir.join(suite).join(&name);
        if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
            let mut result = TestResult::setup_error(
                name.clone(),
                started.elapsed(),
                format!("Error creating log directory at {}: {e}", log_dir.display()),
            );
            result.case_path = Some(test.case_path());

            // Report setup failures through events so the collector accounts for the selected test.
            let _ = event_sender.send(TestEvent::TestStarted { name });
            let _ = event_sender.send(TestEvent::TestCompleted {
                result: Box::new(result.clone()),
                log_dir,
            });

            return result;
        }

        let case = Deadline::after(Budget::Case, timeout);
        let tctx = TestContext::new(test_cancel.clone(), case, log_dir.clone(), settings);

        let _ = event_sender.send(TestEvent::TestStarted { name: name.clone() });

        let phases = tctx.phases.clone();
        let record = tctx.record.clone();
        let ledger = tctx.ledger.clone();
        let tasks = tctx.tasks.clone();

        let mut result = {
            let run_fut = test.run(tctx);
            pin!(run_fut);

            // On timeout or run cancellation, cancel the test and allow GRACE_TIME for it to return.
            //
            // The case timer wins ties with the body so an expired deadline is reported, even after a verdict.
            // The body precedes `cancel_all` to preserve a result ready when external cancellation arrives.
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(case.at) => {
                    let stop = Stop::snapshot(StoppedBy::CaseDeadline, &phases, &record);
                    test_cancel.cancel();
                    let returned = tokio::time::timeout(GRACE_TIME, &mut run_fut).await.ok();
                    stop.settle(returned, &name, timeout, started.elapsed(), &phases, &record)
                }
                r = &mut run_fut => r,
                _ = cancel_all.cancelled() => {
                    let stop = Stop::snapshot(StoppedBy::Signal, &phases, &record);
                    test_cancel.cancel();
                    let returned = tokio::time::timeout(GRACE_TIME, &mut run_fut).await.ok();
                    stop.settle(returned, &name, timeout, started.elapsed(), &phases, &record)
                }
            }
            // Drop the body before cleanup; its tracked background tasks are handled below.
        };

        // Cleanup does not change the outcome, and a second signal can end the run during it, so the plan of a case that
        // did not pass is written first.
        if let Some(plan) = test.participant_case().and_then(|case| case.plan.as_ref().ok()) {
            if !result.outcome.is_passed() {
                write_plan(plan, &log_dir);
            }
        }

        // Cleanup has its own budget because the test token may already be cancelled. Give tracked tasks time to stop
        // and mark interrupted creations before reclaim reads the ledger.
        let reclaim_started = Instant::now();
        let deadline = tokio::time::Instant::now() + RECLAIM_TIME;
        let running = tasks.stop_all(tokio::time::Instant::now() + TASK_STOP_TIME).await;
        if running > 0 {
            warn!(
                test = %name,
                running,
                "Background tasks were still tracked after the {:?} stop budget expired.",
                TASK_STOP_TIME
            );
        }
        let (reclaim, issues) = ledger.reclaim(deadline).await;
        if !reclaim.is_empty() {
            result.phase_timings.push(PhaseTiming {
                phase: "reclaim".to_string(),
                duration: reclaim_started.elapsed(),
            });
            result.duration = started.elapsed();
        }
        result.reclaim = reclaim;
        result.issues.extend(issues);

        // The runner is the only place that knows both the test and where its artifacts landed, so
        // it stamps that provenance onto the result before anything reports on it.
        result.case_path = Some(test.case_path());
        result.log_dir = Some(log_dir.clone());

        write_result_log(&result, &log_dir);
        crate::machine_output::write_test_report(&result, &log_dir);

        let _ = event_sender.send(TestEvent::TestCompleted {
            result: Box::new(result.clone()),
            log_dir,
        });

        result
    }
}

/// Test state captured before cancellation can change step statuses or advance the phase.
struct Stop {
    by: StoppedBy,
    /// The phase the test was in.
    active_phase: Option<String>,
    /// Whether the test had reached its verdict.
    decided: bool,
    /// The procedure steps that were running.
    active: Vec<ActiveStep>,
}

impl Stop {
    fn snapshot(by: StoppedBy, phases: &PhaseTracker, record: &CaseRecord) -> Self {
        Self {
            by,
            active_phase: phases.active(),
            decided: record.decided(),
            active: record.running(),
        }
    }

    /// Uses the result returned during grace, falling back to the case record when grace expires.
    ///
    /// Preserves a verdict reached before cancellation, recording any expired deadline or cleanup grace as an issue.
    /// Otherwise, the stop determines the outcome: a timeout for the case deadline, an internal error for a signal.
    fn settle(
        self, returned: Option<TestResult>, name: &str, timeout: Duration, elapsed: Duration, phases: &PhaseTracker,
        record: &CaseRecord,
    ) -> TestResult {
        let grace_phase = returned
            .is_none()
            .then(|| phases.active().unwrap_or_else(|| UNKNOWN_PHASE.to_string()));
        let mut result = returned.unwrap_or_else(|| record.result(name, elapsed, phases.completed()));
        let grace_expired = grace_phase.as_ref().map(|phase| Issue {
            kind: IssueKind::CleanupGraceExpired,
            message: format!("cleanup grace {:?} expired in {}", GRACE_TIME, phase),
        });

        if self.decided {
            if self.by == StoppedBy::CaseDeadline {
                result.issues.push(Issue {
                    kind: IssueKind::PostVerdictDeadline,
                    message: format!(
                        "case budget {:?} expired in {} after the verdict",
                        timeout,
                        self.active_phase.as_deref().unwrap_or(UNKNOWN_PHASE)
                    ),
                });
            }
            result.issues.extend(grace_expired);
            return result;
        }

        result.cause = Some(StopCause {
            by: self.by,
            active: self.active,
        });
        match (self.by, grace_phase) {
            (StoppedBy::CaseDeadline, None) => result
                .with_harness_error(ErrorKind::Timeout, format!("Test timed out after {:?}.", timeout))
                .with_timeout_attribution(TimeoutAttribution::test_deadline(timeout, self.active_phase)),
            (StoppedBy::CaseDeadline, Some(grace_phase)) => result
                .with_harness_error(
                    ErrorKind::Timeout,
                    format!(
                        "Test timed out after {:?} and did not return within {:?}.",
                        timeout, GRACE_TIME
                    ),
                )
                .with_timeout_attribution(TimeoutAttribution::cleanup_grace(
                    GRACE_TIME,
                    timeout,
                    self.active_phase,
                    Some(grace_phase),
                )),
            (StoppedBy::Signal, _) => {
                result.issues.extend(grace_expired);
                result.with_harness_error(ErrorKind::Internal, "Test was cancelled.")
            }
        }
    }
}

/// Writes `plan` to the case's log directory `log_dir`, as `panoramic plan` prints it.
fn write_plan(plan: &CasePlan, log_dir: &Path) {
    let path = log_dir.join(PLAN_FILE);
    let written = serde_json::to_string_pretty(plan)
        .map_err(|e| e.to_string())
        .and_then(|json| std::fs::write(&path, json + "\n").map_err(|e| e.to_string()));
    if let Err(e) = written {
        warn!(path = %path.display(), error = %e, "Failed to write the plan of a case that did not pass.");
    }
}

fn write_result_log(result: &TestResult, dir: impl AsRef<Path>) {
    let dir = dir.as_ref();

    let path = dir.join("result.log");
    let mut f = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to create result log file.");
            return;
        }
    };

    let _ = writeln!(f, "{} {} ({:.2?})", result.label(), result.name, result.duration);
    for line in result.cause_lines() {
        let _ = writeln!(f, "{}", line);
    }

    let ran: Vec<_> = result
        .assertion_results
        .iter()
        .enumerate()
        .filter(|(_, step)| step.status != StepStatus::NotRun)
        .collect();
    if !ran.is_empty() {
        let _ = writeln!(f);
        let _ = writeln!(f, "Assertions:");
        for (i, assertion) in ran {
            let indicator = if assertion.status == StepStatus::Passed {
                "+"
            } else {
                "-"
            };
            let _ = writeln!(f, "  {} {} ({:.2?})", indicator, assertion.name, assertion.duration);
            let full_details = result.assertion_details.get(i).map(|d| d.as_slice()).unwrap_or(&[]);
            if !full_details.is_empty() {
                for line in full_details {
                    let _ = writeln!(f, "    {}", line);
                }
            } else {
                for line in assertion.message.lines() {
                    let _ = writeln!(f, "    {}", line);
                }
            }
        }
    }

    if !result.phase_timings.is_empty() {
        let _ = writeln!(f);
        let _ = writeln!(f, "Phase timings:");
        for phase in &result.phase_timings {
            let _ = writeln!(f, "  {} ({:.2?})", phase.phase, phase.duration);
        }
    }
}

// These TestResult constructors just declutter the execution code a little bit.
impl TestResult {
    fn setup_error(name: impl Into<String>, total_duration: Duration, e: impl AsRef<str>) -> Self {
        Self::errored(
            name,
            ErrorKind::Setup,
            format!("Test failed to start due to an error during setup. {}", e.as_ref()),
            total_duration,
            vec![],
        )
    }
}

/// Scripted tests and settings for exercising [`Runner`].
#[cfg(test)]
pub(crate) mod test_support {
    use futures::future::join_all;
    use tokio::sync::Notify;

    use super::*;
    use crate::core::record::{RunningStep, StepResult, Verdict};
    use crate::participant_case::ParticipantCase;
    use crate::reporter::test_support::step;

    /// What a [`ScriptedTest`] does once it has entered its phase.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Behavior {
        /// Passes one assertion.
        Pass,
        /// Fails one assertion.
        Fail,
        /// Panics, taking the runner task down with it.
        Panic,
        /// Waits for its cancellation token, then returns only if `finish_on_cancel` is set.
        WaitForCancel { finish_on_cancel: bool },
        /// Sleeps until the case deadline, then passes.
        ReturnAtDeadline,
        /// Records `steps` in the case record, as the step driver does, and reports the recorded result.
        Steps(&'static [&'static [ScriptedStep]]),
        /// Runs [`Behavior::Steps`], finishes its phase, then waits for cancellation in `then_phase`.
        ///
        /// Returns the recorded result if `returns_on_cancel` is set; otherwise stays pending.
        StepsThenWait {
            steps: &'static [&'static [ScriptedStep]],
            then_phase: &'static str,
            returns_on_cancel: bool,
        },
        /// Returns a setup error on cancellation. With `recorded`, records the verdict before waiting;
        /// otherwise simulates a setup wait that fails due to cancellation.
        SetupError { recorded: bool },
    }

    /// A step of a scripted procedure.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum ScriptedStep {
        /// Passes at once.
        Pass,
        /// Runs until the test is cancelled, then is interrupted.
        Hang,
    }

    impl ScriptedStep {
        /// The step's kind and its own budget.
        fn kind(self) -> (&'static str, Duration) {
            match self {
                Self::Pass => ("log_contains", Duration::from_secs(5)),
                Self::Hang => ("process_stable_for", Duration::from_secs(600)),
            }
        }
    }

    /// A test that enters a phase and then behaves as scripted.
    pub(crate) struct ScriptedTest {
        pub(crate) name: String,
        pub(crate) timeout: Duration,
        pub(crate) phase: &'static str,
        pub(crate) behavior: Behavior,
        /// Notified once the test has entered its phase.
        pub(crate) entered: Option<Arc<Notify>>,
        /// A directory the test creates and registers before its behavior runs.
        pub(crate) registers: Option<PathBuf>,
        /// Spawns a pending task and records its drop.
        pub(crate) spawns: Option<TaskProbe>,
        /// The participant case the test stands for, which the runner reads the plan of.
        pub(crate) case: Option<ParticipantCase>,
    }

    /// Observes task shutdown for a [`ScriptedTest`].
    #[derive(Debug, Default, Clone)]
    pub(crate) struct TaskProbe(Arc<std::sync::Mutex<Option<Dropped>>>);

    /// What a [`TaskProbe`] saw when its task was dropped.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) struct Dropped {
        /// Elapsed time since the task's drop guard was created.
        pub(crate) elapsed: Duration,
        /// Whether the test's registered directory still existed.
        pub(crate) directory_existed: bool,
    }

    impl TaskProbe {
        pub(crate) fn dropped(&self) -> Option<Dropped> {
            *self.0.lock().unwrap()
        }
    }

    struct ProbeGuard {
        probe: TaskProbe,
        started: tokio::time::Instant,
        directory: Option<PathBuf>,
    }

    impl Drop for ProbeGuard {
        fn drop(&mut self) {
            *self.probe.0.lock().unwrap() = Some(Dropped {
                elapsed: self.started.elapsed(),
                directory_existed: self.directory.as_ref().is_some_and(|dir| dir.exists()),
            });
        }
    }

    impl ScriptedTest {
        /// Creates a test with a timeout long enough for cancellation tests to interrupt it first.
        pub(crate) fn new(name: &str, behavior: Behavior) -> Self {
            Self {
                name: name.to_string(),
                timeout: Duration::from_secs(60),
                phase: "assertions",
                behavior,
                entered: None,
                registers: None,
                spawns: None,
                case: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl Test for ScriptedTest {
        fn name(&self) -> String {
            self.name.clone()
        }

        fn suite(&self) -> String {
            "integration".to_string()
        }

        fn description(&self) -> Option<String> {
            None
        }

        fn case_path(&self) -> PathBuf {
            PathBuf::from("/cases").join(&self.name)
        }

        fn timeout(&self) -> Duration {
            self.timeout
        }

        fn runtime(&self) -> String {
            "linux".to_string()
        }

        fn participant_case(&self) -> Option<&ParticipantCase> {
            self.case.as_ref()
        }

        async fn run(&self, tctx: TestContext) -> TestResult {
            if let Some(dir) = &self.registers {
                std::fs::create_dir_all(dir).expect("the registered directory should be creatable");
                tctx.ledger.directory(dir.clone());
            }
            if let Some(probe) = &self.spawns {
                let guard = ProbeGuard {
                    probe: probe.clone(),
                    started: tokio::time::Instant::now(),
                    directory: self.registers.clone(),
                };
                tctx.tasks.spawn(async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                });
            }

            // Keep the phase active to model an interrupted phase; `StepsThenWait` finishes it before switching.
            let phase = tctx.phases.enter(self.phase);
            if let Some(entered) = &self.entered {
                entered.notify_one();
            }

            let scripted = |status| vec![step(0, "scripted", status)];
            match self.behavior {
                Behavior::Pass => TestResult::from_steps(
                    &self.name,
                    Duration::from_millis(1),
                    scripted(StepStatus::Passed),
                    Vec::new(),
                ),
                Behavior::Fail => TestResult::from_steps(
                    &self.name,
                    Duration::from_millis(1),
                    scripted(StepStatus::Failed),
                    Vec::new(),
                ),
                Behavior::Panic => panic!("scripted test '{}' panicked", self.name),
                Behavior::WaitForCancel { finish_on_cancel } => {
                    tctx.test_cancel_token().cancelled().await;
                    if !finish_on_cancel {
                        std::future::pending::<()>().await;
                    }
                    TestResult::from_steps(&self.name, Duration::from_secs(1), Vec::new(), Vec::new())
                }
                Behavior::ReturnAtDeadline => {
                    tokio::time::sleep_until(tctx.deadline.at).await;
                    TestResult::from_steps(
                        &self.name,
                        Duration::from_millis(1),
                        scripted(StepStatus::Passed),
                        Vec::new(),
                    )
                }
                Behavior::Steps(steps) => {
                    let started = Instant::now();
                    Self::run_steps(steps, &tctx).await;
                    tctx.record.result(&self.name, started.elapsed(), Vec::new())
                }
                Behavior::StepsThenWait {
                    steps,
                    then_phase,
                    returns_on_cancel,
                } => {
                    let started = Instant::now();
                    Self::run_steps(steps, &tctx).await;
                    phase.finish();
                    let _then = tctx.phases.enter(then_phase);
                    tctx.test_cancel_token().cancelled().await;
                    if !returns_on_cancel {
                        std::future::pending::<()>().await;
                    }
                    tctx.record
                        .result(&self.name, started.elapsed(), tctx.phases.completed())
                }
                Behavior::SetupError { recorded } => {
                    let failed = |message| {
                        TestResult::errored(
                            &self.name,
                            ErrorKind::Setup,
                            message,
                            Duration::from_millis(1),
                            Vec::new(),
                        )
                    };
                    if recorded {
                        tctx.record.decide(Verdict::SetupFailed(Box::new(failed("boom"))));
                    }
                    tctx.test_cancel_token().cancelled().await;
                    failed(if recorded { "boom" } else { "Canceled while waiting." })
                }
            }
        }
    }

    impl ScriptedTest {
        /// Records `steps` as the step driver does: plans every step as not run, runs the members of an entry together,
        /// stops at the first entry that does not pass, and decides the verdict when every entry passes.
        async fn run_steps(steps: &[&[ScriptedStep]], tctx: &TestContext) {
            let mut slot = 0;
            let mut entries = Vec::new();
            for (index, members) in steps.iter().enumerate() {
                let parallel = members.len() > 1;
                let mut entry = Vec::new();
                for (member, scripted) in members.iter().enumerate() {
                    entry.push((slot, index, parallel.then_some(member), *scripted));
                    slot += 1;
                }
                entries.push(entry);
            }
            tctx.record.plan(
                entries
                    .iter()
                    .flatten()
                    .map(|&(_, index, member, scripted)| StepResult {
                        step: index,
                        member,
                        name: scripted.kind().0.to_string(),
                        status: StepStatus::NotRun,
                        message: "Not run.".to_string(),
                        duration: Duration::ZERO,
                    })
                    .collect(),
            );

            for entry in entries {
                if tctx.test_cancel_token().is_cancelled() {
                    return;
                }
                let statuses = join_all(
                    entry
                        .into_iter()
                        .map(|(slot, index, member, scripted)| Self::run_step(slot, index, member, scripted, tctx)),
                )
                .await;
                if statuses.iter().any(|status| *status != StepStatus::Passed) {
                    return;
                }
            }
            tctx.record.decide(Verdict::Procedure);
        }

        /// Starts one step in the case record and finishes it as `scripted` says.
        async fn run_step(
            slot: usize, index: usize, member: Option<usize>, scripted: ScriptedStep, tctx: &TestContext,
        ) -> StepStatus {
            let (name, budget) = scripted.kind();
            let remaining = tctx.deadline.at.saturating_duration_since(tokio::time::Instant::now());
            let (budget, deadline) = if budget <= remaining {
                (budget, Deadline::after(Budget::Step, budget))
            } else {
                (remaining, tctx.deadline)
            };
            let running = RunningStep {
                step: index,
                member,
                name,
                started: tokio::time::Instant::now(),
                budget,
                deadline,
            };
            tctx.record.start(slot, running);
            let result = match scripted {
                ScriptedStep::Pass => running.result(StepStatus::Passed, "Passed.".to_string(), Duration::ZERO),
                ScriptedStep::Hang => {
                    tctx.test_cancel_token().cancelled().await;
                    running.interrupted(None)
                }
            };
            let status = result.status;
            tctx.record.finish(slot, result);
            status
        }
    }

    pub(crate) fn test_runner_settings() -> RunnerSettings {
        RunnerSettings {
            backends: crate::core::backend::Backends::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::test_support::{test_runner_settings, Behavior, ScriptedStep, ScriptedTest, TaskProbe};
    use super::*;
    use crate::core::participant::Os;
    use crate::core::record::StepStatus;
    use crate::core::result::{ReclaimResult, TestOutcome, TimeoutDeadline};
    use crate::participant_case::ParticipantCase;
    use crate::reporter::test_support::{each_outcome, NOT_RUN_KIND};

    /// A bound for real-time tests.
    const BOUND: Duration = Duration::from_secs(5);

    /// Bounds paused-time tests while allowing the runner's [`GRACE_TIME`] to expire.
    const PAUSED_BOUND: Duration = Duration::from_secs(120);

    /// Runs a test within `bound`, using `log_base_dir` and run cancellation `cancel_all`. Discards events.
    async fn run_scripted_test(
        test: &dyn Test, log_base_dir: &Path, cancel_all: &CancellationToken, bound: Duration,
    ) -> TestResult {
        run_with_settings(test, test_runner_settings(), log_base_dir, cancel_all, bound).await
    }

    /// Runs a test with explicit settings and a timeout to catch hangs.
    async fn run_with_settings(
        test: &dyn Test, settings: RunnerSettings, log_base_dir: &Path, cancel_all: &CancellationToken, bound: Duration,
    ) -> TestResult {
        let (tx, _rx) = mpsc::unbounded_channel();

        tokio::time::timeout(
            bound,
            Runner::run_one(test, &tx, cancel_all, log_base_dir.to_path_buf(), settings),
        )
        .await
        .expect("the runner should finish the test within the bound")
    }

    /// Returns a token that cancels the run after `after`.
    fn cancel_all_after(after: Duration) -> CancellationToken {
        let cancel_all = CancellationToken::new();
        let cancel = cancel_all.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            cancel.cancel();
        });
        cancel_all
    }

    /// Each recorded step's kind, parallel member, and status.
    fn statuses(result: &TestResult) -> Vec<(&str, Option<usize>, StepStatus)> {
        result
            .assertion_results
            .iter()
            .map(|step| (step.name.as_str(), step.member, step.status))
            .collect()
    }

    fn phase_names(result: &TestResult) -> Vec<&str> {
        result.phase_timings.iter().map(|t| t.phase.as_str()).collect()
    }

    /// Returns registered resource labels and their cleanup results.
    fn reclaimed(result: &TestResult) -> Vec<(&str, ReclaimResult)> {
        result
            .reclaim
            .iter()
            .map(|entry| (entry.resource.as_str(), entry.result))
            .collect()
    }

    /// Reads the test's saved `result.json`.
    fn written_report(result: &TestResult) -> serde_json::Value {
        let log_dir = result
            .log_dir
            .as_ref()
            .expect("the runner should record the log directory");
        let written = std::fs::read_to_string(log_dir.join("result.json")).expect("result.json should exist");
        serde_json::from_str(&written).expect("result.json should parse")
    }

    fn issue(kind: IssueKind, message: &str) -> Issue {
        Issue {
            kind,
            message: message.to_string(),
        }
    }

    /// Creates a test that times out in `phase` and returns on cancellation.
    fn timing_out_in(phase: &'static str) -> ScriptedTest {
        ScriptedTest {
            timeout: Duration::from_millis(50),
            phase,
            ..ScriptedTest::new("stuck", Behavior::WaitForCancel { finish_on_cancel: true })
        }
    }

    #[tokio::test]
    async fn setup_failure_before_the_test_starts_is_reported_as_an_error() {
        // A regular file where the log base directory belongs, so the test's log directory cannot be
        // created. Consumers that build the suite result from events have to see this failure, or
        // the run reports no test at all and exits zero.
        let occupied_base_dir = tempfile::NamedTempFile::new().expect("temp file should be creatable");
        let (tx, mut rx) = mpsc::unbounded_channel();

        let result = Runner::run_one(
            &ScriptedTest::new("stuck", Behavior::WaitForCancel { finish_on_cancel: true }),
            &tx,
            &CancellationToken::new(),
            occupied_base_dir.path().to_path_buf(),
            test_runner_settings(),
        )
        .await;

        assert_eq!(result.outcome, TestOutcome::Errored);
        assert_eq!(result.error.as_ref().map(|e| e.kind), Some(ErrorKind::Setup));
        assert_eq!(result.case_path, Some(PathBuf::from("/cases/stuck")));

        let mut reported = None;
        while let Ok(event) = rx.try_recv() {
            if let TestEvent::TestCompleted { result, .. } = event {
                reported = Some(result);
            }
        }

        let reported = reported.expect("a setup failure should be reported as a completed test");
        assert_eq!(reported.name, "stuck");
        assert_eq!(reported.outcome, TestOutcome::Errored);
        assert_eq!(reported.error.as_ref().map(|e| e.kind), Some(ErrorKind::Setup));
    }

    #[tokio::test]
    async fn test_deadline_timeout_names_the_phase_that_was_active() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let result = run_scripted_test(
            &timing_out_in("container_start"),
            log_base_dir.path(),
            &CancellationToken::new(),
            BOUND,
        )
        .await;

        let timeout = result.timeout.expect("a timed-out test should carry timeout detail");
        assert_eq!(result.outcome, TestOutcome::TimedOut);
        assert_eq!(timeout.deadline, TimeoutDeadline::TestDeadline);
        assert_eq!(timeout.active_phase, "container_start");
        assert_eq!(timeout.configured_ms, 50);
    }

    #[tokio::test]
    async fn result_log_labels_a_timeout_and_gives_its_cause() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let result = run_scripted_test(
            &timing_out_in("container_start"),
            log_base_dir.path(),
            &CancellationToken::new(),
            BOUND,
        )
        .await;

        let log_dir = result.log_dir.expect("the runner should record the log directory");
        let result_log = std::fs::read_to_string(log_dir.join("result.log")).expect("result.log should exist");
        let mut lines = result_log.lines();
        let first = lines.next().expect("result.log should not be empty");
        assert!(first.starts_with("TIMEOUT stuck "), "first line was: {}", first);
        assert_eq!(lines.next(), Some("case budget 50ms expired in container_start"));
    }

    #[test]
    fn result_log_counts_the_steps_that_never_ran_instead_of_listing_them() {
        let log_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let [_, (failed, _), ..] = each_outcome();

        write_result_log(&failed, log_dir.path());

        let result_log = std::fs::read_to_string(log_dir.path().join("result.log")).expect("result.log should exist");
        let assertions: Vec<&str> = result_log
            .lines()
            .skip_while(|line| *line != "Assertions:")
            .skip(1)
            .take_while(|line| !line.is_empty())
            .filter(|line| !line.starts_with("    "))
            .collect();
        assert_eq!(assertions, ["  - log_contains (1.00ms)"], "{result_log}");
        assert!(!result_log.contains(NOT_RUN_KIND), "{result_log}");
        assert!(result_log.contains("\n1 step not run\n"), "{result_log}");
    }

    #[tokio::test]
    async fn a_case_deadline_interrupts_the_running_step_and_keeps_the_finished_ones() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let test = ScriptedTest {
            timeout: Duration::from_millis(300),
            ..ScriptedTest::new(
                "steps",
                Behavior::Steps(&[&[ScriptedStep::Pass], &[ScriptedStep::Hang]]),
            )
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), BOUND).await;

        assert_eq!(result.outcome, TestOutcome::TimedOut);
        let timeout = result
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::TestDeadline);
        assert_eq!(timeout.active_phase, "assertions");
        assert_eq!(
            statuses(&result),
            [
                ("log_contains", None, StepStatus::Passed),
                ("process_stable_for", None, StepStatus::Interrupted),
            ]
        );
        let interrupted = &result.assertion_results[1];
        assert!(interrupted.message.contains("step budget"), "{}", interrupted.message);
        let cause = result.cause.as_ref().expect("a stopped test should carry its cause");
        assert_eq!(cause.by, StoppedBy::CaseDeadline);
        let active: Vec<_> = cause
            .active
            .iter()
            .map(|step| (step.step, step.member, step.kind.as_str()))
            .collect();
        assert_eq!(active, [(1, None, "process_stable_for")]);
    }

    #[tokio::test]
    async fn the_case_deadline_wins_when_the_test_returns_at_the_same_instant() {
        // Unbiased polling lets the body win roughly a third of ties. Repetition makes a false pass unlikely.
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        for run in 0..50 {
            let test = ScriptedTest {
                timeout: Duration::from_millis(10),
                ..ScriptedTest::new("tie", Behavior::ReturnAtDeadline)
            };

            let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), BOUND).await;

            assert_eq!(
                result.outcome,
                TestOutcome::TimedOut,
                "run {run} reported the body's result"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_dropped_after_its_procedure_keeps_its_steps_and_where_grace_ran_out() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let steps: &[&[ScriptedStep]] = &[
            &[ScriptedStep::Pass],
            &[ScriptedStep::Pass],
            &[ScriptedStep::Hang, ScriptedStep::Hang],
            &[ScriptedStep::Pass],
        ];
        let test = ScriptedTest {
            timeout: Duration::from_secs(1),
            ..ScriptedTest::new(
                "dropped",
                Behavior::StepsThenWait {
                    steps,
                    then_phase: "cleanup",
                    returns_on_cancel: false,
                },
            )
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;

        assert_eq!(result.outcome, TestOutcome::TimedOut);
        let timeout = result
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::CleanupGrace);
        assert_eq!(timeout.active_phase, "assertions");
        assert_eq!(timeout.grace_phase.as_deref(), Some("cleanup"));
        let cause = result.cause.as_ref().expect("a stopped test should carry its cause");
        assert_eq!(cause.by, StoppedBy::CaseDeadline);
        let member = |member| ActiveStep {
            step: 2,
            member: Some(member),
            kind: "process_stable_for".to_string(),
            elapsed_ms: 1_000,
            budget_ms: 1_000,
        };
        assert_eq!(cause.active, [member(0), member(1)]);
        let expected_statuses = [
            ("log_contains", None, StepStatus::Passed),
            ("log_contains", None, StepStatus::Passed),
            ("process_stable_for", Some(0), StepStatus::Interrupted),
            ("process_stable_for", Some(1), StepStatus::Interrupted),
            ("log_contains", None, StepStatus::NotRun),
        ];
        assert_eq!(statuses(&result), expected_statuses);
        assert_eq!(phase_names(&result), ["assertions"]);

        let json = written_report(&result);
        assert_eq!(json["schema_version"], 2);
        let written_statuses: Vec<&str> = json["assertions"]
            .as_array()
            .expect("assertions is an array")
            .iter()
            .map(|step| step["status"].as_str().expect("status is a string"))
            .collect();
        assert_eq!(
            written_statuses,
            ["passed", "passed", "interrupted", "interrupted", "not_run"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn resources_are_reclaimed_after_the_runner_drops_a_body_stuck_past_grace() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let registered = tempfile::tempdir().expect("temp dir should be creatable");
        let stuck = |name: &str, behavior| ScriptedTest {
            timeout: Duration::from_secs(1),
            registers: Some(registered.path().join(name)),
            ..ScriptedTest::new(name, behavior)
        };
        let decided = stuck(
            "decided",
            Behavior::StepsThenWait {
                steps: &[&[ScriptedStep::Pass]],
                then_phase: "observe",
                returns_on_cancel: false,
            },
        );
        let undecided = stuck(
            "undecided",
            Behavior::WaitForCancel {
                finish_on_cancel: false,
            },
        );

        let decided = run_scripted_test(&decided, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;
        let undecided =
            run_scripted_test(&undecided, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;

        assert_eq!(decided.outcome, TestOutcome::Passed, "{:?}", decided.error);
        let issue_kinds: Vec<_> = decided.issues.iter().map(|issue| issue.kind).collect();
        assert_eq!(
            issue_kinds,
            [IssueKind::PostVerdictDeadline, IssueKind::CleanupGraceExpired]
        );
        assert_eq!(undecided.outcome, TestOutcome::TimedOut);
        let timeout = undecided
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::CleanupGrace);
        let error = undecided
            .error
            .as_ref()
            .expect("a timed-out test should carry its error");
        assert_eq!(error.message, "Test timed out after 1s and did not return within 30s.");
        for result in [&decided, &undecided] {
            let dir = registered.path().join(&result.name);
            let resource = format!("directory:{}", dir.display());
            assert_eq!(reclaimed(result), [(resource.as_str(), ReclaimResult::Reclaimed)]);
            assert!(!dir.exists(), "{} should be removed", dir.display());
            assert_eq!(phase_names(result).last(), Some(&"reclaim"), "{}", result.name);
        }
    }

    /// Loads a participant case that plans for Linux, which needs no Linux host, from a directory under `cases`.
    fn participant_case(cases: &Path) -> ParticipantCase {
        let dir = cases.join("planned");
        std::fs::create_dir_all(&dir).expect("the case directory should be creatable");
        std::fs::write(
            dir.join("config.yaml"),
            "timeout: 1m\nsubject_os: [linux]\nparticipants:\n  app:\n    subject: true\n    image: alpine:3.20\nprocedure: []\n",
        )
        .expect("the case file should be writable");
        ParticipantCase::load(&dir, Os::Linux, &BTreeMap::new(), &crate::registry())
            .unwrap_or_else(|errors| panic!("{errors:?}"))
            .0
    }

    #[tokio::test(start_paused = true)]
    async fn a_case_dropped_after_grace_keeps_its_plan() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let cases = tempfile::tempdir().expect("temp dir should be creatable");
        let test = ScriptedTest {
            case: Some(participant_case(cases.path())),
            ..ScriptedTest::new(
                "planned",
                Behavior::WaitForCancel {
                    finish_on_cancel: false,
                },
            )
        };

        let result = run_scripted_test(
            &test,
            log_base_dir.path(),
            &cancel_all_after(Duration::from_secs(1)),
            PAUSED_BOUND,
        )
        .await;

        assert_eq!(result.label(), "INTERRUPTED");
        let log_dir = result
            .log_dir
            .as_ref()
            .expect("the runner should record the log directory");
        let plan = log_dir.join("plan.json");
        assert!(plan.is_file(), "{} should exist", plan.display());
        let report = written_report(&result);
        let artifacts: Vec<&str> = report["artifacts"]
            .as_array()
            .expect("artifacts is an array")
            .iter()
            .map(|artifact| artifact.as_str().expect("an artifact is a path"))
            .collect();
        assert!(
            artifacts.iter().any(|artifact| artifact.ends_with("plan.json")),
            "{artifacts:?}"
        );
    }

    #[tokio::test]
    async fn a_passing_case_writes_no_plan() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let cases = tempfile::tempdir().expect("temp dir should be creatable");
        let test = ScriptedTest {
            case: Some(participant_case(cases.path())),
            ..ScriptedTest::new("planned", Behavior::Pass)
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), BOUND).await;

        assert_eq!(result.outcome, TestOutcome::Passed);
        let log_dir = result
            .log_dir
            .as_ref()
            .expect("the runner should record the log directory");
        assert!(!log_dir.join("plan.json").exists());
    }

    #[tokio::test(start_paused = true)]
    async fn a_passing_case_keeps_its_verdict_and_stops_its_tasks_before_reclaim() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let registered = tempfile::tempdir().expect("temp dir should be creatable");
        let dir = registered.path().join("passing");
        let probe = TaskProbe::default();
        let test = ScriptedTest {
            registers: Some(dir.clone()),
            spawns: Some(probe.clone()),
            ..ScriptedTest::new("passing", Behavior::Pass)
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;

        assert_eq!(result.outcome, TestOutcome::Passed, "{:?}", result.error);
        assert_eq!(result.issues, []);
        let resource = format!("directory:{}", dir.display());
        assert_eq!(reclaimed(&result), [(resource.as_str(), ReclaimResult::Reclaimed)]);
        assert!(!dir.exists(), "{} should be removed", dir.display());
        let dropped = probe.dropped().expect("the runner should stop the test's task");
        assert!(dropped.directory_existed, "the task should stop before reclaim");
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_case_stops_its_tasks_at_the_deadline() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let registered = tempfile::tempdir().expect("temp dir should be creatable");
        let dir = registered.path().join("stuck");
        let probe = TaskProbe::default();
        let test = ScriptedTest {
            timeout: Duration::from_secs(1),
            registers: Some(dir.clone()),
            spawns: Some(probe.clone()),
            ..ScriptedTest::new(
                "stuck",
                Behavior::WaitForCancel {
                    finish_on_cancel: false,
                },
            )
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;

        assert_eq!(result.outcome, TestOutcome::TimedOut);
        let timeout = result
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::CleanupGrace);
        let dropped = probe.dropped().expect("the case deadline should stop the test's task");
        assert_eq!(dropped.elapsed, Duration::from_secs(1));
        let resource = format!("directory:{}", dir.display());
        assert_eq!(reclaimed(&result), [(resource.as_str(), ReclaimResult::Reclaimed)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_verdict_reached_before_the_runner_stops_the_test_is_kept() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let passes_then_waits = |timeout, returns_on_cancel| ScriptedTest {
            timeout,
            ..ScriptedTest::new(
                "decided",
                Behavior::StepsThenWait {
                    steps: &[&[ScriptedStep::Pass]],
                    then_phase: "observe",
                    returns_on_cancel,
                },
            )
        };
        let deadline_issue = issue(
            IssueKind::PostVerdictDeadline,
            "case budget 1s expired in observe after the verdict",
        );
        let grace_issue = issue(IssueKind::CleanupGraceExpired, "cleanup grace 30s expired in observe");
        let cases = [
            (
                "timer, returns",
                passes_then_waits(Duration::from_secs(1), true),
                CancellationToken::new(),
                vec![deadline_issue.clone()],
            ),
            (
                "timer, never returns",
                passes_then_waits(Duration::from_secs(1), false),
                CancellationToken::new(),
                vec![deadline_issue, grace_issue],
            ),
            (
                "signal, returns",
                passes_then_waits(Duration::from_secs(60), true),
                cancel_all_after(Duration::from_secs(1)),
                vec![],
            ),
        ];

        for (case, test, cancel_all, issues) in cases {
            let result = run_scripted_test(&test, log_base_dir.path(), &cancel_all, PAUSED_BOUND).await;

            assert_eq!(result.outcome, TestOutcome::Passed, "{case}: {:?}", result.error);
            assert!(result.cause.is_none(), "{case}: {:?}", result.cause);
            assert!(result.timeout.is_none(), "{case}: {:?}", result.timeout);
            assert_eq!(result.issues, issues, "{case}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_setup_error_is_kept_only_if_it_was_reached_before_the_deadline() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let setup_error = |phase, recorded| ScriptedTest {
            timeout: Duration::from_secs(1),
            phase,
            ..ScriptedTest::new("setup", Behavior::SetupError { recorded })
        };

        let recorded = run_scripted_test(
            &setup_error("cleanup", true),
            log_base_dir.path(),
            &CancellationToken::new(),
            PAUSED_BOUND,
        )
        .await;

        assert_eq!(recorded.outcome, TestOutcome::Errored);
        let error = recorded.error.as_ref().expect("a setup failure should carry its error");
        assert_eq!((error.kind, error.message.as_str()), (ErrorKind::Setup, "boom"));
        assert_eq!(
            recorded.issues,
            [issue(
                IssueKind::PostVerdictDeadline,
                "case budget 1s expired in cleanup after the verdict"
            )]
        );

        let unrecorded = run_scripted_test(
            &setup_error("intake_start", false),
            log_base_dir.path(),
            &CancellationToken::new(),
            PAUSED_BOUND,
        )
        .await;

        assert_eq!(unrecorded.outcome, TestOutcome::TimedOut, "{:?}", unrecorded.error);
        let timeout = unrecorded
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::TestDeadline);
        assert_eq!(timeout.active_phase, "intake_start");
    }

    #[tokio::test(start_paused = true)]
    async fn a_signal_whose_grace_expires_stays_interrupted() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let test = ScriptedTest {
            timeout: Duration::from_secs(60),
            ..ScriptedTest::new(
                "interrupted",
                Behavior::StepsThenWait {
                    steps: &[&[ScriptedStep::Hang]],
                    then_phase: "cleanup",
                    returns_on_cancel: false,
                },
            )
        };

        let result = run_scripted_test(
            &test,
            log_base_dir.path(),
            &cancel_all_after(Duration::from_secs(1)),
            PAUSED_BOUND,
        )
        .await;

        assert_eq!(result.outcome, TestOutcome::Errored);
        assert_eq!(result.error.as_ref().map(|e| e.kind), Some(ErrorKind::Internal));
        assert_eq!(result.label(), "INTERRUPTED");
        let cause = result.cause.as_ref().expect("a stopped test should carry its cause");
        assert_eq!(cause.by, StoppedBy::Signal);
        let active: Vec<_> = cause
            .active
            .iter()
            .map(|step| (step.step, step.kind.as_str()))
            .collect();
        assert_eq!(active, [(0, "process_stable_for")]);
        assert_eq!(
            result.issues,
            [issue(
                IssueKind::CleanupGraceExpired,
                "cleanup grace 30s expired in cleanup"
            )]
        );
        assert!(result.timeout.is_none(), "{:?}", result.timeout);
        assert_eq!(phase_names(&result), ["assertions"]);
        assert_eq!(
            statuses(&result),
            [("process_stable_for", None, StepStatus::Interrupted)]
        );
    }
}
