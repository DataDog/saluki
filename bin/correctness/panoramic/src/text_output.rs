//! Human-readable run output: per-test results, run summaries and the `-o` output format.

use std::path::{Path, PathBuf};
use std::time::Duration;

use colored::Colorize as _;
use panoramic_core::machine_output::{InterruptReason, RunReport, RunState, RUN_REPORT_FILE_NAME};
use panoramic_core::record::StepStatus;
use panoramic_core::result::TestResult;

use crate::collector::every_test_skipped;

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

                if report.tests.iter().any(|test| test.outcome.is_unsuccessful()) {
                    println!();
                    println!("{}", "Failed tests:".red().bold());
                    for test in &report.tests {
                        if test.outcome.is_unsuccessful() {
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

/// The run's one-word status: `PASSED`, `FAILED`, `SKIPPED`, or `INCOMPLETE` for an interrupted run.
pub fn run_status_label(report: &RunReport) -> &'static str {
    let successful = !report.tests.iter().any(|test| test.outcome.is_unsuccessful())
        && report.running.is_empty()
        && report.not_run.is_empty();

    if report.state == RunState::Interrupted {
        "INCOMPLETE"
    } else if every_test_skipped(report) {
        "SKIPPED"
    } else if successful {
        RUN_PASSED_LABEL
    } else {
        "FAILED"
    }
}

/// Formats outcome counts, the selected-test total, and elapsed time.
///
/// Timed-out, skipped, running, and not-run counts appear only when nonzero.
pub fn run_counts(report: &RunReport) -> String {
    let totals = &report.totals;
    let mut counts = format!(
        "{} passed, {} failed, {} errored",
        totals.passed, totals.failed, totals.errored
    );
    if totals.timed_out > 0 {
        counts.push_str(&format!(" ({} timed out)", totals.timed_out));
    }
    if totals.skipped > 0 {
        counts.push_str(&format!(", {} skipped", totals.skipped));
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

#[cfg(test)]
mod tests {
    use panoramic_core::layout::Os;
    use panoramic_core::machine_output::{RunTotals, TestReport};
    use panoramic_core::result::test_support::{each_outcome, NOT_RUN_KIND, PASS_ISSUE, TIMEOUT_CAUSE};

    use super::*;

    const LABELS: [&str; 5] = ["PASS", "FAIL", "ERROR", "TIMEOUT", "INTERRUPTED"];

    #[test]
    fn run_status_is_skipped_only_when_every_test_is_skipped_and_counts_include_skips() {
        let mut report = RunReport::new(
            "unknown".into(),
            Vec::new(),
            Vec::new(),
            Os::MacOs,
            1,
            PathBuf::new(),
            Vec::new(),
            vec![TestReport::skipped("skipped".into(), PathBuf::new(), Os::MacOs)],
        );
        report.state = RunState::Complete;
        assert_eq!(run_status_label(&report), "SKIPPED");
        assert!(run_counts(&report).starts_with("0 passed, 0 failed, 0 errored, 1 skipped, 1 total ("));
        report.not_run.push("pending".into());
        assert_eq!(run_status_label(&report), "FAILED");
        report.not_run.clear();
        report.running.push("running".into());
        assert_eq!(run_status_label(&report), "FAILED");
        report.running.clear();
        report.state = RunState::Interrupted;
        assert_eq!(run_status_label(&report), "INCOMPLETE");
        report.state = RunState::Complete;
        for (result, _) in each_outcome() {
            let expected = if result.outcome.is_passed() { "PASSED" } else { "FAILED" };
            report.tests.push(TestReport::new(&result));
            report.totals = RunTotals::new(&report.tests, 0);
            assert_eq!(run_status_label(&report), expected);
            assert!(run_counts(&report).contains(", 1 skipped, 2 total ("));
            report.tests.pop();
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

    #[test]
    fn run_summary_reads_incomplete_for_an_interrupted_run() {
        struct Unfinished<'a> {
            running: &'a [&'a str],
            not_run: &'a [&'a str],
        }

        fn run_report(
            result: &TestResult, unfinished: Unfinished<'_>, state: RunState,
            interrupted_reason: Option<InterruptReason>,
        ) -> RunReport {
            let mut report = RunReport::new(
                "unknown".to_string(),
                Vec::new(),
                Vec::new(),
                Os::Linux,
                1,
                PathBuf::new(),
                Vec::new(),
                Vec::new(),
            );
            report.tests = vec![TestReport::new(result)];
            report.running = unfinished.running.iter().map(|name| name.to_string()).collect();
            report.not_run = unfinished.not_run.iter().map(|name| name.to_string()).collect();
            report.state = state;
            report.interrupted_reason = interrupted_reason;
            report.totals = RunTotals::new(&report.tests, report.not_run.len());
            report
        }

        let [(passed, _), (failed, _), _, (timed_out, _), _] = each_outcome();

        let all_pass = run_report(
            &passed,
            Unfinished {
                running: &[],
                not_run: &[],
            },
            RunState::Complete,
            None,
        );
        let report = &all_pass;
        assert_eq!(run_status_label(report), "PASSED");
        assert!(run_counts(report).starts_with("1 passed, 0 failed, 0 errored, 1 total ("));
        assert!(run_unfinished_lines(report).is_empty());

        let fail_fast = run_report(
            &failed,
            Unfinished {
                running: &[],
                not_run: &["b"],
            },
            RunState::Complete,
            None,
        );
        let report = &fail_fast;
        assert_eq!(run_status_label(report), "FAILED");
        assert!(run_counts(report).starts_with("0 passed, 1 failed, 0 errored, 1 not run, 2 total ("));
        assert_eq!(run_unfinished_lines(report), vec!["Not run: b"]);

        let interrupted = run_report(
            &timed_out,
            Unfinished {
                running: &["a"],
                not_run: &["b"],
            },
            RunState::Interrupted,
            Some(InterruptReason::Signal { source: "SIGTERM" }),
        );
        let report = &interrupted;
        assert_eq!(run_status_label(report), "INCOMPLETE");
        assert!(run_counts(report)
            .starts_with("0 passed, 0 failed, 1 errored (1 timed out), 1 running, 1 not run, 3 total ("));
        assert_eq!(
            run_unfinished_lines(report),
            vec!["Interrupted: SIGTERM", "Still running: a", "Not run: b"]
        );

        let mut long_run = RunReport::new(
            "unknown".to_string(),
            Vec::new(),
            Vec::new(),
            Os::Linux,
            1,
            PathBuf::new(),
            Vec::new(),
            Vec::new(),
        );
        long_run.duration_ms = 63_000;
        assert_eq!(run_counts(&long_run), "0 passed, 0 failed, 0 errored, 0 total (1m 3s)");
    }
}
