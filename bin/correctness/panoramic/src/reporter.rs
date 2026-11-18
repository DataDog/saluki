use std::path::{Path, PathBuf};
use std::time::Duration;

use colored::Colorize as _;

use crate::assertions::AssertionResult;
use crate::core::record::{StepResult, StepStatus};
use crate::core::result::{PhaseTiming, TestResult};
use crate::machine_output::{InterruptReason, RunReport, RunState, RUN_REPORT_FILE_NAME};

impl TestResult {
    /// Creates a result from sequential assertions, assigning step indices in order.
    pub fn from_assertions(
        name: impl Into<String>, duration: Duration, assertion_results: Vec<AssertionResult>,
        phase_timings: Vec<PhaseTiming>,
    ) -> Self {
        let steps = assertion_results
            .into_iter()
            .enumerate()
            .map(|(step, result)| StepResult {
                step,
                member: None,
                name: result.name,
                status: result.status.into(),
                message: result.message,
                duration: result.duration,
            })
            .collect();
        Self::from_steps(name, duration, steps, phase_timings)
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
            let ran = result
                .assertion_results
                .iter()
                .filter(|step| step.status != StepStatus::NotRun);
            for assertion in ran {
                let indicator = if assertion.status == StepStatus::Passed {
                    "+".green()
                } else {
                    "-".red()
                };

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

/// Results shared by the renderers' tests.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::core::result::{ErrorKind, Issue, IssueKind, StopCause, StoppedBy, TimeoutAttribution};

    /// Expected cause line for the timeout result in [`each_outcome`].
    pub(crate) const TIMEOUT_CAUSE: &str = "case budget 50ms expired in container_start";

    /// Expected cause line for the issue on the passing result in [`each_outcome`].
    pub(crate) const PASS_ISSUE: &str =
        "Issue: post_verdict_deadline: case budget 50ms expired in cleanup after the verdict";

    /// Kind of the step that never ran in the failing result in [`each_outcome`].
    pub(crate) const NOT_RUN_KIND: &str = "log_not_contains";

    /// A step that runs alone, with a placeholder message.
    pub(crate) fn step(step: usize, name: &str, status: StepStatus) -> StepResult {
        StepResult {
            step,
            member: None,
            name: name.to_string(),
            status,
            message: "message".to_string(),
            duration: Duration::from_millis(1),
        }
    }

    /// One result per label, in `PASS`, `FAIL`, `ERROR`, `TIMEOUT`, `INTERRUPTED` order.
    ///
    /// The passing result includes an issue; the failing result includes a later `not_run` step.
    pub(crate) fn each_outcome() -> [(TestResult, &'static str); 5] {
        let mut passed = TestResult::from_steps(
            "p",
            Duration::from_secs(1),
            vec![step(0, "log_contains", StepStatus::Passed)],
            Vec::new(),
        );
        passed.issues.push(Issue {
            kind: IssueKind::PostVerdictDeadline,
            message: "case budget 50ms expired in cleanup after the verdict".to_string(),
        });
        let failed = TestResult::from_steps(
            "f",
            Duration::from_secs(1),
            vec![
                step(0, "log_contains", StepStatus::Failed),
                step(1, NOT_RUN_KIND, StepStatus::NotRun),
            ],
            Vec::new(),
        );
        let timed_out = TestResult::errored("t", ErrorKind::Timeout, "slow", Duration::from_secs(1), Vec::new())
            .with_timeout_attribution(TimeoutAttribution::test_deadline(
                Duration::from_millis(50),
                Some("container_start".to_string()),
            ));
        let mut interrupted = TestResult::errored(
            "i",
            ErrorKind::Internal,
            "Test was cancelled.",
            Duration::from_secs(1),
            Vec::new(),
        );
        interrupted.cause = Some(StopCause {
            by: StoppedBy::Signal,
            active: Vec::new(),
        });

        [
            (passed, "PASS"),
            (failed, "FAIL"),
            (
                TestResult::errored("e", ErrorKind::Setup, "boom", Duration::from_secs(1), Vec::new()),
                "ERROR",
            ),
            (timed_out, "TIMEOUT"),
            (interrupted, "INTERRUPTED"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{each_outcome, step, NOT_RUN_KIND, PASS_ISSUE, TIMEOUT_CAUSE};
    use super::*;
    use crate::assertions::AssertionStatus;
    use crate::collector::RunCollector;
    use crate::core::record::ActiveStep;
    use crate::core::result::{ErrorKind, Issue, IssueKind, StopCause, StoppedBy, TimeoutAttribution};
    use crate::events::TestEvent;

    const LABELS: [&str; 5] = ["PASS", "FAIL", "ERROR", "TIMEOUT", "INTERRUPTED"];

    #[test]
    fn every_outcome_has_its_own_label() {
        let labels: Vec<&str> = each_outcome().iter().map(|(result, _)| result.label()).collect();
        assert_eq!(labels, LABELS);
    }

    /// A timed-out result with two running parallel members, an issue, and two steps that never ran.
    fn stopped_during_a_parallel_block() -> TestResult {
        let active = |member, kind: &str| ActiveStep {
            step: 2,
            member: Some(member),
            kind: kind.to_string(),
            elapsed_ms: 4_900,
            budget_ms: 30_000,
        };
        let steps = vec![
            step(0, "log_contains", StepStatus::Passed),
            step(1, "log_contains", StepStatus::Passed),
            step(3, "log_contains", StepStatus::NotRun),
            step(4, "log_contains", StepStatus::NotRun),
        ];
        let mut result = TestResult::from_steps("t", Duration::from_secs(121), steps, Vec::new())
            .with_harness_error(ErrorKind::Timeout, "slow")
            .with_timeout_attribution(TimeoutAttribution::test_deadline(
                Duration::from_secs(120),
                Some("assertions".to_string()),
            ));
        result.cause = Some(StopCause {
            by: StoppedBy::CaseDeadline,
            active: vec![active(0, "intake_has_metric"), active(1, "log_contains")],
        });
        result.issues.push(Issue {
            kind: IssueKind::CleanupGraceExpired,
            message: "cleanup grace 30s expired in cleanup".to_string(),
        });
        result
    }

    #[test]
    fn a_timeout_cause_names_the_budget_and_the_phase_it_expired_in() {
        let timed_out = |attribution| {
            TestResult::errored("t", ErrorKind::Timeout, "slow", Duration::from_secs(121), Vec::new())
                .with_timeout_attribution(attribution)
        };
        let assertions = |status| {
            vec![AssertionResult {
                name: "log_contains".to_string(),
                status,
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
                    Some("cleanup".to_string()),
                )),
                vec!["cleanup grace 30s expired in cleanup after the case budget (120s) expired in assertions"],
            ),
            (stopped_during_a_parallel_block(), {
                vec![
                    "case budget 120s expired in assertions",
                    "Running: step 3.1 intake_has_metric (4.9s of 30s), step 3.2 log_contains (4.9s of 30s)",
                    "Issue: cleanup_grace_expired: cleanup grace 30s expired in cleanup",
                    "2 steps not run",
                ]
            }),
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
                TestResult::from_assertions(
                    "f",
                    Duration::from_secs(1),
                    assertions(AssertionStatus::Failed),
                    Vec::new(),
                ),
                vec![],
            ),
            (
                TestResult::from_assertions(
                    "p",
                    Duration::from_secs(1),
                    assertions(AssertionStatus::Passed),
                    Vec::new(),
                ),
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
            match label {
                "PASS" => assert_eq!(lines[1..], [format!("  {}", PASS_ISSUE)]),
                "FAIL" => {
                    assert_eq!(lines[1], "  1 step not run");
                    assert!(
                        lines.iter().all(|line| !line.contains(NOT_RUN_KIND)),
                        "a step that never ran should not be listed: {lines:?}"
                    );
                }
                "TIMEOUT" => assert_eq!(lines[1], format!("  {}", TIMEOUT_CAUSE)),
                _ => {}
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
        let [(passed, _), (failed, _), _, (timed_out, _), _] = each_outcome();

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
}
