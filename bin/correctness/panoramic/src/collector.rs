//! Collects run results, writes `run.json`, and drives the display.
//!
//! [`RunCollector`] owns the report. [`consume_run`] handles events and interrupts for both TUI and plain output.

use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::events::TestEvent;
use crate::interrupts::Interrupts;
use crate::machine_output::{self, InterruptReason, RunReport, RunState, RunTotals, TestReport};
use crate::reporter::{run_counts, DurationMs, Reporter};
use crate::tui::Tui;

/// Exit code for an assertion failure.
pub(crate) const EXIT_ASSERTION_FAILURE: u8 = 1;

/// Exit code for a harness error, timeout, or interrupted run.
pub(crate) const EXIT_HARNESS_ERROR: u8 = 2;

/// Exit code for an empty selection or an unmatched test name.
pub(crate) const EXIT_NO_TESTS_SELECTED: u8 = 3;

/// How often the TUI reads keys and redraws.
const FRAME_INTERVAL: Duration = Duration::from_millis(50);

/// Builds a run's results from the runner's events and keeps `run.json` current.
///
/// Updates are synchronous so cancellation cannot interrupt a report update.
pub(crate) struct RunCollector {
    report: RunReport,
    clock: Instant,
    fail_fast: bool,
}

/// Whether an interrupt was the run's first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Interruption {
    First,
    Repeated,
}

impl RunCollector {
    /// Starts collecting with `report` and writes the initial `run.json`.
    pub(crate) fn start(report: RunReport, fail_fast: bool) -> Self {
        let mut collector = Self {
            report,
            clock: Instant::now(),
            fail_fast,
        };
        collector.write();
        collector
    }

    /// Applies a runner event and rewrites `run.json`, except for status messages.
    pub(crate) fn observe(&mut self, event: &TestEvent) {
        let report = &mut self.report;
        match event {
            TestEvent::RunStarted { tests } => report.not_run = tests.clone(),
            TestEvent::TestStarted { name } => {
                report.not_run.retain(|n| n != name);
                if !report.running.contains(name) {
                    report.running.push(name.clone());
                }
            }
            TestEvent::TestCompleted { result, .. } => {
                report.running.retain(|n| *n != result.name);
                report.not_run.retain(|n| *n != result.name);
                // Cache the report to avoid rescanning the test's logs on each update.
                report.tests.push(TestReport::new(result));
            }
            TestEvent::StatusLine { .. } => return,
        }
        self.write();
    }

    /// Marks the run interrupted, keeping the first reason, and rewrites `run.json`.
    ///
    /// Writes the interrupted state before cleanup so it survives a forced exit.
    pub(crate) fn interrupt(&mut self, reason: InterruptReason) -> Interruption {
        if self.report.state == RunState::Interrupted {
            return Interruption::Repeated;
        }

        self.report.state = RunState::Interrupted;
        self.report.interrupted_reason = Some(reason);
        self.write();
        Interruption::First
    }

    /// Finalizes the report when the runner task ends.
    ///
    /// A task failure or unfinished test marks the run interrupted; `--fail-fast` may skip tests.
    pub(crate) fn finish(&mut self, runner: Result<(), JoinError>) {
        match runner {
            Err(e) => {
                self.interrupt(InterruptReason::RunnerFailed {
                    message: format!("Runner task failed: {}", e),
                });
            }
            Ok(()) => {
                let report = &self.report;
                let stopped_by_fail_fast = self.fail_fast && report.tests.iter().any(|t| !t.outcome.is_passed());
                let unexplained = report.running.len() + if stopped_by_fail_fast { 0 } else { report.not_run.len() };
                if unexplained > 0 {
                    self.interrupt(InterruptReason::RunnerFailed {
                        message: format!("Runner returned without running {} selected test(s).", unexplained),
                    });
                }
            }
        }

        if self.report.state == RunState::Running {
            self.report.state = RunState::Complete;
        }
        self.write();
    }

    /// Returns the current in-memory report.
    pub(crate) fn report(&self) -> &RunReport {
        &self.report
    }

    fn write(&mut self) {
        let report = &mut self.report;
        report.duration_ms = DurationMs(self.clock.elapsed()).as_millis();
        report.totals = RunTotals::new(&report.tests, report.not_run.len());
        machine_output::write_run_report(report);
    }
}

/// Chooses the exit code, giving interruptions and harness errors precedence over assertion failures.
pub(crate) fn exit_code(report: &RunReport) -> ExitCode {
    if report.state == RunState::Interrupted || report.tests.iter().any(|t| t.outcome.is_error()) {
        ExitCode::from(EXIT_HARNESS_ERROR)
    } else if report.tests.iter().any(|t| !t.outcome.is_passed()) {
        ExitCode::from(EXIT_ASSERTION_FAILURE)
    } else {
        ExitCode::SUCCESS
    }
}

/// Renders a run through the TUI or plain-text reporter.
pub(crate) enum RunDisplay {
    Tui(Tui),
    Plain(Reporter),
}

impl RunDisplay {
    fn show_event(&mut self, event: &TestEvent) {
        match self {
            Self::Tui(tui) => tui.show_event(event),
            Self::Plain(reporter) => match event {
                TestEvent::RunStarted { tests } => info!("Running {} test(s)...", tests.len()),
                TestEvent::TestStarted { name } => info!("Starting test '{}'...", name),
                TestEvent::TestCompleted { result, log_dir } => reporter.report_test_result(result, log_dir),
                TestEvent::StatusLine { message } => info!("{}", message),
            },
        }
    }

    fn show_notice(&mut self, notice: &str) {
        match self {
            Self::Tui(tui) => tui.show_notice(notice),
            Self::Plain(_) => warn!("{}", notice),
        }
    }

    /// Shows the run's summary. The TUI also restores the terminal.
    fn finish(self, report: &RunReport) -> io::Result<()> {
        match self {
            Self::Tui(tui) => tui.finish(report),
            Self::Plain(reporter) => {
                info!("Test run {}. {}", report.state, run_counts(report));
                reporter.report_suite_result(report);
                Ok(())
            }
        }
    }
}

/// How [`consume_run`] ended.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RunEnd {
    /// The runner task ended; the report is `complete` or `interrupted`.
    Finished,
    /// A second interrupt arrived; in-flight tests are abandoned.
    Abandoned,
}

pub(crate) struct ConsumedRun {
    pub(crate) end: RunEnd,
    /// Display error, if any, recorded independently of test results.
    pub(crate) display_error: Option<io::Error>,
}

/// Collects and displays events until the runner task ends or a second interrupt arrives.
///
/// The first interrupt cancels `cancel_all`, allowing tests their cleanup grace period. A second interrupt returns
/// [`RunEnd::Abandoned`]. Queued events are recorded before either exit. TUI errors leave collection running.
///
/// The loop's waits are cancel-safe; event handling and display calls are synchronous.
pub(crate) async fn consume_run(
    mut events: mpsc::UnboundedReceiver<TestEvent>, mut runner: JoinHandle<()>, collector: &mut RunCollector,
    mut display: Option<RunDisplay>, interrupts: &mut Interrupts, cancel_all: &CancellationToken,
) -> ConsumedRun {
    let mut display_error = None;
    let mut frames = tokio::time::interval(FRAME_INTERVAL);
    frames.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let end = loop {
        let is_tui = matches!(display, Some(RunDisplay::Tui(_)));
        // Prioritize interrupts and runner completion over queued events, which are drained before exit.
        let interrupt = tokio::select! {
            biased;

            Some(source) = interrupts.recv() => Some(source),
            // Watching the task also detects runner panics.
            joined = &mut runner => {
                while let Ok(event) = events.try_recv() {
                    apply_event(collector, display.as_mut(), &event);
                }
                collector.finish(joined);
                break RunEnd::Finished;
            }
            _ = frames.tick(), if is_tui => {
                let Some(RunDisplay::Tui(tui)) = &mut display else {
                    continue;
                };
                let drawn = tui
                    .poll_interrupt_key()
                    .and_then(|key| tui.render(collector.report()).map(|()| key));
                match drawn {
                    Ok(key) => key,
                    Err(e) => {
                        // Dropping the TUI restores the terminal.
                        display = None;
                        display_error = Some(e);
                        None
                    }
                }
            }
            Some(event) = events.recv() => {
                apply_event(collector, display.as_mut(), &event);
                None
            }
        };

        let Some(source) = interrupt else {
            continue;
        };
        match collector.interrupt(InterruptReason::Signal { source }) {
            Interruption::First => {
                cancel_all.cancel();
                if let Some(display) = display.as_mut() {
                    display.show_notice(&format!(
                        "Received {}; cancelling the run. Interrupt again to stop now without waiting for cleanup.",
                        source
                    ));
                }
            }
            Interruption::Repeated => {
                // Preserve queued progress before abandoning the runner.
                while let Ok(event) = events.try_recv() {
                    apply_event(collector, display.as_mut(), &event);
                }
                if let Some(display) = display.as_mut() {
                    display.show_notice(&format!(
                        "Stopping without waiting for {} running test(s) to clean up; their containers may remain.",
                        collector.report().running.len()
                    ));
                }
                break RunEnd::Abandoned;
            }
        }
    };

    if let Some(display) = display {
        if let Err(e) = display.finish(collector.report()) {
            display_error.get_or_insert(e);
        }
    }

    ConsumedRun { end, display_error }
}

fn apply_event(collector: &mut RunCollector, display: Option<&mut RunDisplay>, event: &TestEvent) {
    collector.observe(event);
    if let Some(display) = display {
        display.show_event(event);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};
    use tokio::sync::Notify;

    use super::*;
    use crate::assertions::AssertionResult;
    use crate::events::create_event_channel;
    use crate::machine_output::RUN_REPORT_FILE_NAME;
    use crate::reporter::{ErrorKind, OutputFormat, TestResult};
    use crate::runner::test_support::{test_runner_settings, Behavior, ScriptedTest};
    use crate::runner::{RunArgs, Runner};

    /// Bounds test waits below the runner's cleanup grace period so hangs fail promptly.
    const WAIT_BOUND: Duration = Duration::from_secs(5);

    /// A spawned `Runner` with scripted tests, a collector, and a shared log directory.
    struct ScriptedRun {
        log_dir: tempfile::TempDir,
        _mounts_dir: tempfile::TempDir,
        collector: RunCollector,
        cancel_all: CancellationToken,
        events: mpsc::UnboundedReceiver<TestEvent>,
        runner: JoinHandle<()>,
    }

    impl ScriptedRun {
        /// Spawns a sequential runner. On a current-thread runtime, it starts when the test yields.
        fn spawn(tests: Vec<ScriptedTest>, fail_fast: bool) -> Self {
            let log_dir = tempfile::tempdir().expect("temp dir should be creatable");
            let mounts_dir = tempfile::tempdir().expect("temp dir should be creatable");
            let report = RunReport::new(Vec::new(), "linux".to_string(), 1, log_dir.path().to_path_buf());
            let collector = RunCollector::start(report, fail_fast);

            let mut runner = Runner::new(log_dir.path(), test_runner_settings(mounts_dir.path()));
            for test in tests {
                runner.register(Box::new(test)).expect("scripted test names are unique");
            }
            let cancel_all = CancellationToken::new();
            let (tx, events) = create_event_channel();
            let args = RunArgs::new(cancel_all.clone(), tx).with_fail_fast(fail_fast);
            let runner = tokio::spawn(async move { runner.run_tests(args).await });

            Self {
                log_dir,
                _mounts_dir: mounts_dir,
                collector,
                cancel_all,
                events,
                runner,
            }
        }

        /// Collects with a silent reporter, failing if the loop exceeds `WAIT_BOUND`.
        async fn consume(self, interrupts: &mut Interrupts) -> (RunEnd, ConsumedReport) {
            let Self {
                log_dir,
                _mounts_dir,
                mut collector,
                cancel_all,
                events,
                runner,
            } = self;
            let display = RunDisplay::Plain(Reporter::new(OutputFormat::Json, false, None));

            let consumed = tokio::time::timeout(
                WAIT_BOUND,
                consume_run(events, runner, &mut collector, Some(display), interrupts, &cancel_all),
            )
            .await
            .expect("consume_run should return within the wait bound");
            assert!(consumed.display_error.is_none());

            let report = ConsumedReport {
                on_disk: read_run_json(log_dir.path()),
                in_memory: serde_json::to_value(collector.report()).expect("run report should serialize"),
                exit_code: exit_code(collector.report()),
                cancelled: cancel_all.is_cancelled(),
                _log_dir: log_dir,
            };
            (consumed.end, report)
        }
    }

    struct ConsumedReport {
        on_disk: Value,
        in_memory: Value,
        exit_code: ExitCode,
        cancelled: bool,
        _log_dir: tempfile::TempDir,
    }

    fn read_run_json(log_dir: &std::path::Path) -> Value {
        let written = std::fs::read_to_string(log_dir.join(RUN_REPORT_FILE_NAME)).expect("run.json should exist");
        serde_json::from_str(&written).expect("run.json should parse")
    }

    fn outcomes(report: &Value) -> Vec<(String, String)> {
        report["tests"]
            .as_array()
            .expect("tests is an array")
            .iter()
            .map(|t| {
                (
                    t["name"].as_str().unwrap().to_string(),
                    t["outcome"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// A test that notifies `entered` once it is running, then waits to be cancelled.
    fn waiting_for_cancel(name: &str, finish_on_cancel: bool, entered: &Arc<Notify>) -> ScriptedTest {
        ScriptedTest {
            entered: Some(entered.clone()),
            ..ScriptedTest::new(name, Behavior::WaitForCancel { finish_on_cancel })
        }
    }

    fn interrupt_once_entered(entered: Arc<Notify>, source: &'static str) -> Interrupts {
        let (tx, interrupts) = Interrupts::manual();
        tokio::spawn(async move {
            entered.notified().await;
            tx.send(source).expect("the interrupt receiver should be alive");
        });
        interrupts
    }

    fn result(name: &str, passed: bool) -> TestResult {
        let assertion = AssertionResult {
            name: "log_contains".to_string(),
            passed,
            message: String::new(),
            duration: Duration::from_millis(1),
        };
        TestResult::from_assertions(name, Duration::from_secs(1), vec![assertion], Vec::new())
    }

    fn completed(result: TestResult) -> TestEvent {
        TestEvent::TestCompleted {
            result: Box::new(result),
            log_dir: std::path::PathBuf::new(),
        }
    }

    #[tokio::test]
    async fn run_json_moves_each_test_from_not_run_to_running_to_completed() {
        let mut run = ScriptedRun::spawn(
            vec![
                ScriptedTest::new("a", Behavior::Pass),
                ScriptedTest::new("b", Behavior::Fail),
            ],
            false,
        );
        let joined = tokio::time::timeout(WAIT_BOUND, &mut run.runner)
            .await
            .expect("the runner should finish within the wait bound");
        let mut events = Vec::new();
        while let Ok(event) = run.events.try_recv() {
            events.push(event);
        }

        let on_disk = read_run_json(run.log_dir.path());
        assert_eq!(on_disk["state"], "running");
        assert_eq!(on_disk["not_run"], json!([]));

        // Expected (running, not_run, completed names) in `run.json` after each event.
        let expected = [
            (json!([]), json!(["a", "b"]), vec![]),
            (json!(["a"]), json!(["b"]), vec![]),
            (json!([]), json!(["b"]), vec!["a"]),
            (json!(["b"]), json!([]), vec!["a"]),
            (json!([]), json!([]), vec!["a", "b"]),
        ];
        assert_eq!(events.len(), expected.len(), "events were: {:?}", events);
        for (event, (running, not_run, tests)) in events.iter().zip(expected) {
            run.collector.observe(event);
            let on_disk = read_run_json(run.log_dir.path());
            assert_eq!(on_disk["running"], running, "after {:?}", event);
            assert_eq!(on_disk["not_run"], not_run, "after {:?}", event);
            let names: Vec<String> = outcomes(&on_disk).into_iter().map(|(name, _)| name).collect();
            assert_eq!(names, tests, "after {:?}", event);
        }

        run.collector.finish(joined);
        let on_disk = read_run_json(run.log_dir.path());
        assert_eq!(on_disk["state"], "complete");
        assert_eq!(
            on_disk["totals"],
            json!({"passed": 1, "failed": 1, "errored": 0, "timed_out": 0, "not_run": 0})
        );
        assert_eq!(
            exit_code(run.collector.report()),
            ExitCode::from(EXIT_ASSERTION_FAILURE)
        );
    }

    #[tokio::test]
    async fn an_interrupt_before_any_test_starts_reports_every_selected_test_as_not_run() {
        let (tx, mut interrupts) = Interrupts::manual();
        tx.send("SIGTERM").expect("the interrupt receiver should be alive");
        // The current-thread runtime processes the queued interrupt before polling the spawned runner.
        // Asserting empty results below verifies that cancellation precedes test execution.
        let run = ScriptedRun::spawn(
            vec![
                ScriptedTest::new("a", Behavior::Pass),
                ScriptedTest::new("b", Behavior::Pass),
                ScriptedTest::new("c", Behavior::Pass),
            ],
            false,
        );

        let (end, report) = run.consume(&mut interrupts).await;

        assert_eq!(end, RunEnd::Finished);
        assert_eq!(report.on_disk["state"], "interrupted");
        assert_eq!(
            report.on_disk["interrupted_reason"],
            json!({"kind": "signal", "source": "SIGTERM"})
        );
        assert_eq!(report.on_disk["not_run"], json!(["a", "b", "c"]));
        assert_eq!(report.on_disk["tests"], json!([]));
        assert_eq!(report.exit_code, ExitCode::from(EXIT_HARNESS_ERROR));
        assert!(report.cancelled);
    }

    #[tokio::test]
    async fn an_interrupt_during_a_run_cancels_the_running_test_and_reports_the_rest_as_not_run() {
        let entered = Arc::new(Notify::new());
        let mut interrupts = interrupt_once_entered(entered.clone(), "SIGINT");
        let run = ScriptedRun::spawn(
            vec![
                waiting_for_cancel("a", true, &entered),
                ScriptedTest::new("b", Behavior::Pass),
                ScriptedTest::new("c", Behavior::Pass),
            ],
            false,
        );

        let (end, report) = run.consume(&mut interrupts).await;

        assert_eq!(end, RunEnd::Finished);
        assert_eq!(
            outcomes(&report.on_disk),
            vec![("a".to_string(), "errored".to_string())]
        );
        assert_eq!(report.on_disk["tests"][0]["error"]["message"], "Test was cancelled.");
        assert_eq!(report.on_disk["running"], json!([]));
        assert_eq!(report.on_disk["not_run"], json!(["b", "c"]));
        assert_eq!(report.on_disk["state"], "interrupted");
        assert_eq!(report.exit_code, ExitCode::from(EXIT_HARNESS_ERROR));
        assert_eq!(report.on_disk, report.in_memory);
    }

    #[tokio::test]
    async fn a_second_interrupt_returns_without_waiting_for_cleanup() {
        let entered = Arc::new(Notify::new());
        // The test ignores cancellation, so only the second interrupt can end the run inside the wait bound.
        let run = ScriptedRun::spawn(vec![waiting_for_cancel("a", false, &entered)], false);
        tokio::time::timeout(WAIT_BOUND, entered.notified())
            .await
            .expect("the test should start within the wait bound");
        // Interrupts take priority over queued events, so draining on exit must record `a` as running.
        let (tx, mut interrupts) = Interrupts::manual();
        tx.send("SIGINT").expect("the interrupt receiver should be alive");
        tx.send("SIGTERM").expect("the interrupt receiver should be alive");

        let (end, report) = run.consume(&mut interrupts).await;

        assert_eq!(end, RunEnd::Abandoned);
        assert_eq!(report.on_disk["state"], "interrupted");
        assert_eq!(report.on_disk["interrupted_reason"]["source"], "SIGINT");
        assert_eq!(report.on_disk["running"], json!(["a"]));
        assert_eq!(report.exit_code, ExitCode::from(EXIT_HARNESS_ERROR));
    }

    #[tokio::test]
    async fn fail_fast_reports_unstarted_tests_as_not_run_and_keeps_the_failure_exit_code() {
        let run = ScriptedRun::spawn(
            vec![
                ScriptedTest::new("a", Behavior::Fail),
                ScriptedTest::new("b", Behavior::Pass),
            ],
            true,
        );
        let (_tx, mut interrupts) = Interrupts::manual();

        let (end, report) = run.consume(&mut interrupts).await;

        assert_eq!(end, RunEnd::Finished);
        assert_eq!(report.on_disk["state"], "complete");
        assert_eq!(report.on_disk["interrupted_reason"], Value::Null);
        assert_eq!(outcomes(&report.on_disk), vec![("a".to_string(), "failed".to_string())]);
        assert_eq!(report.on_disk["not_run"], json!(["b"]));
        assert_eq!(report.exit_code, ExitCode::from(EXIT_ASSERTION_FAILURE));
    }

    #[tokio::test]
    async fn a_failed_runner_task_keeps_the_results_already_completed() {
        // A runner panic must end collection while preserving completed results and the unfinished test.
        let run = ScriptedRun::spawn(
            vec![
                ScriptedTest::new("a", Behavior::Pass),
                ScriptedTest::new("b", Behavior::Panic),
            ],
            false,
        );
        let (_tx, mut interrupts) = Interrupts::manual();

        let (end, report) = run.consume(&mut interrupts).await;

        assert_eq!(end, RunEnd::Finished);
        assert_eq!(outcomes(&report.on_disk), vec![("a".to_string(), "passed".to_string())]);
        assert_eq!(report.on_disk["running"], json!(["b"]));
        assert_eq!(report.on_disk["state"], "interrupted");
        assert_eq!(report.on_disk["interrupted_reason"]["kind"], "runner_failed");
        let message = report.on_disk["interrupted_reason"]["message"].as_str().unwrap();
        assert!(message.contains("panicked"), "message was: {}", message);
        assert_eq!(report.exit_code, ExitCode::from(EXIT_HARNESS_ERROR));
    }

    #[test]
    fn a_run_that_ends_with_unexplained_unstarted_tests_is_interrupted() {
        let log_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let report = RunReport::new(Vec::new(), "linux".to_string(), 1, log_dir.path().to_path_buf());
        let mut collector = RunCollector::start(report, false);
        collector.observe(&TestEvent::RunStarted {
            tests: vec!["a".to_string(), "b".to_string()],
        });
        collector.observe(&TestEvent::TestStarted { name: "a".to_string() });
        collector.observe(&completed(result("a", true)));

        collector.finish(Ok(()));

        let on_disk = read_run_json(log_dir.path());
        assert_eq!(on_disk["state"], "interrupted");
        assert_eq!(on_disk["interrupted_reason"]["kind"], "runner_failed");
        assert_eq!(on_disk["not_run"], json!(["b"]));
        assert_eq!(exit_code(collector.report()), ExitCode::from(EXIT_HARNESS_ERROR));
    }

    #[test]
    fn exit_code_distinguishes_failures_errors_and_interruptions() {
        let timed_out = || TestResult::errored("t", ErrorKind::Timeout, "slow", Duration::from_secs(1), Vec::new());
        let setup_error = || TestResult::errored("e", ErrorKind::Setup, "boom", Duration::from_secs(1), Vec::new());
        let cases = [
            ("pass", vec![result("a", true)], false, ExitCode::SUCCESS),
            (
                "pass and fail",
                vec![result("a", true), result("b", false)],
                false,
                ExitCode::from(EXIT_ASSERTION_FAILURE),
            ),
            (
                "setup error",
                vec![setup_error()],
                false,
                ExitCode::from(EXIT_HARNESS_ERROR),
            ),
            // Harness errors take precedence over assertion failures.
            (
                "fail and timeout",
                vec![result("b", false), timed_out()],
                false,
                ExitCode::from(EXIT_HARNESS_ERROR),
            ),
            (
                "pass, then interrupt",
                vec![result("a", true)],
                true,
                ExitCode::from(EXIT_HARNESS_ERROR),
            ),
        ];

        for (case, results, interrupted, expected) in cases {
            let log_dir = tempfile::tempdir().expect("temp dir should be creatable");
            let report = RunReport::new(Vec::new(), "linux".to_string(), 1, log_dir.path().to_path_buf());
            let mut collector = RunCollector::start(report, false);
            for result in results {
                collector.observe(&completed(result));
            }
            if interrupted {
                collector.interrupt(InterruptReason::Signal { source: "SIGINT" });
            }
            collector.finish(Ok(()));

            assert_eq!(exit_code(collector.report()), expected, "case: {}", case);
        }
    }
}
