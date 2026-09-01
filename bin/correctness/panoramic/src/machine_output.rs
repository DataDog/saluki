//! Run reports and their durable artifacts.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tracing::warn;

use crate::reporter::{DurationMs, PhaseTiming, TestError, TestOutcome, TestResult, TimeoutAttribution};

/// Reported when the build could not read a repository to get a revision from.
const UNKNOWN_REVISION: &str = "unknown";

/// File name of the run-level report, written directly under the run's log directory.
pub const RUN_REPORT_FILE_NAME: &str = "run.json";

/// File name of the per-test report, written into each test's log directory.
pub const TEST_REPORT_FILE_NAME: &str = "result.json";

/// Directory, under a test's log directory, holding uncapped assertion detail files.
const DETAILS_DIR_NAME: &str = "details";

/// Number of detail lines carried inline per assertion. The full set always lands on disk.
const DETAIL_LINE_CAP: usize = 100;

/// Number of diagnostic lines carried per test. The artifacts they came from hold the rest.
const DIAGNOSTIC_LINE_CAP: usize = 25;

/// Length a reported diagnostic line is cut to, so one long line cannot bloat a report.
const DIAGNOSTIC_LINE_LENGTH_CAP: usize = 500;

/// Severity markers that qualify a captured stdout line.
///
/// Matched case-sensitively, so a level token is distinguished from prose mentioning an error.
/// Warning levels do not qualify because startup emits them routinely.
const SEVERITY_MARKERS: &[&str] = &[
    "ERROR",
    "FATAL",
    "CRITICAL",
    "PANIC",
    "panicked at",
    "level=error",
    "\"level\":\"error\"",
];

/// A whole run, as consumed by a program.
#[derive(Debug, Serialize)]
pub struct RunReport {
    /// Revision of the repository this binary was built from: a commit SHA, `-dirty` suffixed when
    /// the checkout had uncommitted changes, or `unknown` when the build could not read a repository.
    pub build_revision: String,
    /// Test-case directories the run discovered from.
    pub suite_dirs: Vec<PathBuf>,
    /// Integration-test runtime the run was scoped to.
    pub runtime: String,
    /// How many tests ran concurrently.
    pub parallelism: usize,
    /// When the run started, as an RFC 3339 timestamp.
    pub started_at: String,
    /// How long the run took.
    pub duration_ms: u64,
    /// Absolute path of the run's log directory.
    pub log_dir: PathBuf,
    /// Counts by outcome.
    pub totals: RunTotals,
    /// One entry per test that ran.
    pub tests: Vec<TestReport>,
}

/// Counts of tests by outcome. `errored` covers both harness errors and timeouts.
#[derive(Debug, Serialize)]
pub struct RunTotals {
    /// Tests whose assertions all passed.
    pub passed: usize,
    /// Tests whose assertions failed.
    pub failed: usize,
    /// Tests the harness could not run to a verdict.
    pub errored: usize,
}

/// A single test, as consumed by a program.
#[derive(Debug, Serialize)]
pub struct TestReport {
    /// Name of the test case.
    pub name: String,
    /// Directory the test case was loaded from, when the runner knows it.
    pub case_path: Option<PathBuf>,
    /// How the test finished.
    pub outcome: TestOutcome,
    /// How long the test took.
    pub duration_ms: u64,
    /// Absolute path of this test's log directory, when the runner knows it.
    pub log_dir: Option<PathBuf>,
    /// Absolute paths of the files in this test's log directory.
    pub artifacts: Vec<PathBuf>,
    /// Timing breakdown of the test's phases.
    pub phase_timings: Vec<PhaseTimingReport>,
    /// Harness-side error, when the test never reached a verdict.
    pub error: Option<TestError>,
    /// Which deadline fired and what the test was doing, when `outcome` is `timed_out`.
    pub timeout: Option<TimeoutAttribution>,
    /// Captured output lines worth reading first. Empty for a passing test.
    pub diagnostic_lines: Vec<DiagnosticLine>,
    /// Whether `diagnostic_lines` was capped.
    pub diagnostic_lines_truncated: bool,
    /// One entry per assertion the test ran.
    pub assertions: Vec<AssertionReport>,
}

/// A captured output line the harness offers as a starting point for diagnosis.
///
/// Treat a selected line as a lead and read it in context in the artifact it names: benign lines land
/// on stderr, and a failing test can produce no diagnostic lines at all.
#[derive(Debug, Serialize)]
pub struct DiagnosticLine {
    /// Absolute path of the artifact the line came from.
    pub source: PathBuf,
    /// Position of the line in that artifact, starting at one.
    pub line_number: usize,
    /// Why the line was selected.
    pub reason: DiagnosticReason,
    /// The line, cut to a bounded length.
    pub text: String,
}

/// Why a captured output line was selected as a diagnostic lead.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticReason {
    /// The line was written to the component's stderr, where anything at all is worth reading.
    CapturedStderr,
    /// The stdout line carries one of the severity markers the harness looks for.
    SeverityMarker,
}

/// Streams a test's captured output is split across, distinguished because they are read differently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapturedStream {
    Stdout,
    Stderr,
}

impl CapturedStream {
    /// Classifies an artifact by the captured stream it holds, or `None` if it holds neither.
    ///
    /// Only these two are read, so the harness's own files (`result.log`, the JSON reports, and
    /// assertion detail) never feed back into the reported lines.
    fn from_artifact(path: &Path) -> Option<Self> {
        let name = path.file_name().and_then(|name| name.to_str())?;

        if name.ends_with("stderr.log") {
            Some(Self::Stderr)
        } else if name.ends_with("stdout.log") {
            Some(Self::Stdout)
        } else {
            None
        }
    }

    /// Why `line` is worth reporting from this stream, or `None` to skip it.
    fn reason_for(&self, line: &str) -> Option<DiagnosticReason> {
        match self {
            Self::Stderr if !line.trim().is_empty() => Some(DiagnosticReason::CapturedStderr),
            Self::Stdout if SEVERITY_MARKERS.iter().any(|marker| line.contains(marker)) => {
                Some(DiagnosticReason::SeverityMarker)
            }
            _ => None,
        }
    }
}

/// One phase of a test's execution.
#[derive(Debug, Serialize)]
pub struct PhaseTimingReport {
    /// Name of the phase.
    pub phase: String,
    /// How long the phase took.
    pub duration_ms: u64,
}

/// One assertion within a test.
#[derive(Debug, Serialize)]
pub struct AssertionReport {
    /// Position of the assertion within the test, starting at zero.
    pub index: usize,
    /// Kind of assertion, as named by the harness.
    pub kind: String,
    /// Whether the assertion passed.
    pub passed: bool,
    /// Human-readable summary.
    pub message: String,
    /// How long the assertion took.
    pub duration_ms: u64,
    /// Mismatch detail, capped at 100 lines.
    pub details: Vec<String>,
    /// Whether `details` was capped.
    pub details_truncated: bool,
    /// Absolute path of the file holding the uncapped detail, when there is one.
    pub details_path: Option<PathBuf>,
}

impl RunReport {
    /// Builds the run-level document from the results of a finished run.
    pub fn new(
        suite_dirs: Vec<PathBuf>, runtime: String, parallelism: usize, started_at: String, duration: Duration,
        log_dir: PathBuf, results: &[TestResult],
    ) -> Self {
        let suite = crate::reporter::TestSuiteResult::from_results(results.to_vec(), duration);

        Self {
            build_revision: build_revision(),
            suite_dirs,
            runtime,
            parallelism,
            started_at,
            duration_ms: DurationMs(duration).as_millis(),
            log_dir,
            totals: RunTotals {
                passed: suite.passed,
                failed: suite.failed,
                errored: suite.errored,
            },
            tests: results.iter().map(TestReport::new).collect(),
        }
    }
}

impl TestReport {
    /// Builds the per-test document. Assumes the test's detail files are already on disk, and so
    /// points at them by path rather than writing them.
    pub fn new(result: &TestResult) -> Self {
        let details_dir = result.log_dir.as_ref().map(|dir| dir.join(DETAILS_DIR_NAME));

        let assertions = result
            .assertion_results
            .iter()
            .enumerate()
            .map(|(index, assertion)| {
                let details = result.assertion_details.get(index).cloned().unwrap_or_default();
                let details_path = details_dir
                    .as_ref()
                    .filter(|_| !details.is_empty())
                    .map(|dir| dir.join(detail_file_name(index, &assertion.name)));

                AssertionReport {
                    index,
                    kind: assertion.name.clone(),
                    passed: assertion.passed,
                    message: assertion.message.clone(),
                    duration_ms: DurationMs(assertion.duration).as_millis(),
                    details_truncated: details.len() > DETAIL_LINE_CAP,
                    details: details.into_iter().take(DETAIL_LINE_CAP).collect(),
                    details_path,
                }
            })
            .collect();

        let artifacts = result.log_dir.as_deref().map(list_artifacts).unwrap_or_default();

        // Only non-passing tests get their output scanned. Error-looking lines are common in output
        // that every assertion accepted.
        let (diagnostic_lines, diagnostic_lines_truncated) = if result.outcome.is_passed() {
            (Vec::new(), false)
        } else {
            collect_diagnostic_lines(&artifacts)
        };

        Self {
            name: result.name.clone(),
            case_path: result.case_path.clone(),
            outcome: result.outcome,
            duration_ms: DurationMs(result.duration).as_millis(),
            log_dir: result.log_dir.clone(),
            artifacts,
            phase_timings: result.phase_timings.iter().map(PhaseTimingReport::new).collect(),
            error: result.error.clone(),
            timeout: result.timeout.clone(),
            diagnostic_lines,
            diagnostic_lines_truncated,
            assertions,
        }
    }
}

/// Revision of the repository this binary was built from, as recorded by the build script.
fn build_revision() -> String {
    revision(env!("VERGEN_GIT_SHA"), env!("VERGEN_GIT_DIRTY"))
}

/// Formats the build script's SHA and dirty flag as a revision string.
///
/// A value that is not a SHA collapses to `unknown` on its own, with no `-dirty` suffix to read as
/// provenance the build never had.
fn revision(sha: &str, dirty: &str) -> String {
    if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return UNKNOWN_REVISION.to_string();
    }

    if dirty == "true" {
        format!("{}-dirty", sha)
    } else {
        sha.to_string()
    }
}

/// Scans a test's captured output for lines worth reading first, capped and in artifact order.
///
/// Returns the lines and whether the cap cut the set short.
fn collect_diagnostic_lines(artifacts: &[PathBuf]) -> (Vec<DiagnosticLine>, bool) {
    let mut lines = Vec::new();
    let mut truncated = false;

    for artifact in artifacts {
        let Some(stream) = CapturedStream::from_artifact(artifact) else {
            continue;
        };

        let Ok(file) = std::fs::File::open(artifact) else {
            continue;
        };

        for (offset, line) in BufReader::new(file).lines().enumerate() {
            let Ok(line) = line else {
                break;
            };

            let Some(reason) = stream.reason_for(&line) else {
                continue;
            };

            if lines.len() == DIAGNOSTIC_LINE_CAP {
                truncated = true;
                break;
            }

            lines.push(DiagnosticLine {
                source: artifact.clone(),
                line_number: offset + 1,
                reason,
                text: cut_to_length(&line),
            });
        }
    }

    (lines, truncated)
}

/// Cuts `line` to [`DIAGNOSTIC_LINE_LENGTH_CAP`] characters, respecting character boundaries.
fn cut_to_length(line: &str) -> String {
    match line.char_indices().nth(DIAGNOSTIC_LINE_LENGTH_CAP) {
        Some((byte_offset, _)) => line[..byte_offset].to_string(),
        None => line.to_string(),
    }
}

impl PhaseTimingReport {
    fn new(timing: &PhaseTiming) -> Self {
        Self {
            phase: timing.phase.clone(),
            duration_ms: DurationMs(timing.duration).as_millis(),
        }
    }
}

/// Writes a test's `result.json` and its uncapped assertion detail files.
///
/// Called for every test regardless of output format, so that a run killed mid-flight still leaves
/// machine-readable evidence behind. Failures are logged, never fatal: losing an artifact should not
/// change a verdict.
pub fn write_test_report(result: &TestResult, log_dir: &Path) {
    write_detail_files(result, log_dir);

    let report = TestReport::new(result);
    write_json(&log_dir.join(TEST_REPORT_FILE_NAME), &report);
}

/// Writes the run-level `run.json`.
pub fn write_run_report(report: &RunReport) {
    write_json(&report.log_dir.join(RUN_REPORT_FILE_NAME), report);
}

fn write_detail_files(result: &TestResult, log_dir: &Path) {
    let has_details = result.assertion_details.iter().any(|details| !details.is_empty());
    if !has_details {
        return;
    }

    let details_dir = log_dir.join(DETAILS_DIR_NAME);
    if let Err(e) = std::fs::create_dir_all(&details_dir) {
        warn!(path = %details_dir.display(), error = %e, "Failed to create assertion details directory.");
        return;
    }

    for (index, details) in result.assertion_details.iter().enumerate() {
        if details.is_empty() {
            continue;
        }

        let name = result
            .assertion_results
            .get(index)
            .map(|assertion| assertion.name.as_str())
            .unwrap_or("assertion");
        let path = details_dir.join(detail_file_name(index, name));
        match std::fs::File::create(&path) {
            Ok(mut file) => {
                for line in details {
                    if let Err(e) = writeln!(file, "{}", line) {
                        warn!(path = %path.display(), error = %e, "Failed to write assertion details.");
                        break;
                    }
                }
            }
            Err(e) => warn!(path = %path.display(), error = %e, "Failed to create assertion details file."),
        }
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, format!("{}\n", json)) {
                warn!(path = %path.display(), error = %e, "Failed to write machine-readable report.");
            }
        }
        Err(e) => warn!(path = %path.display(), error = %e, "Failed to serialize machine-readable report."),
    }
}

/// Names a detail file after its assertion's position and kind, so the file is identifiable without
/// opening it and stays stable across reruns.
fn detail_file_name(index: usize, kind: &str) -> String {
    let sanitized: String = kind
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{}-{}.txt", index, sanitized)
}

fn list_artifacts(log_dir: &Path) -> Vec<PathBuf> {
    let mut artifacts = Vec::new();
    collect_artifacts(log_dir, &mut artifacts);
    artifacts.sort();
    artifacts
}

fn collect_artifacts(dir: &Path, artifacts: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_file() => artifacts.push(path),
            Ok(kind) if kind.is_dir() => collect_artifacts(&path, artifacts),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::assertions::AssertionResult;
    use crate::reporter::{ErrorKind, TestOutcome, TimeoutAttribution};

    fn assertion(name: &str, passed: bool) -> AssertionResult {
        AssertionResult {
            name: name.to_string(),
            passed,
            message: "message".to_string(),
            duration: Duration::from_millis(5),
        }
    }

    #[test]
    fn test_report_pins_the_contract_field_names() {
        let mut result = TestResult::from_assertions(
            "dsd-plain",
            Duration::from_secs(3),
            vec![assertion("telemetry matches", false)],
            vec![PhaseTiming {
                phase: "analysis".to_string(),
                duration: Duration::from_millis(20),
            }],
        )
        .with_assertion_details(vec![vec!["one".to_string(), "two".to_string()]]);
        result.case_path = Some(PathBuf::from("/cases/dsd-plain"));
        result.log_dir = Some(PathBuf::from("/logs/correctness/dsd-plain"));

        let json = serde_json::to_value(TestReport::new(&result)).expect("test report should serialize");

        assert_eq!(json["name"], "dsd-plain");
        assert_eq!(json["outcome"], "failed");
        assert_eq!(json["duration_ms"], 3000);
        assert_eq!(json["case_path"], "/cases/dsd-plain");
        assert_eq!(json["log_dir"], "/logs/correctness/dsd-plain");
        assert_eq!(json["phase_timings"][0]["phase"], "analysis");
        assert_eq!(json["phase_timings"][0]["duration_ms"], 20);
        assert_eq!(json["assertions"][0]["index"], 0);
        assert_eq!(json["assertions"][0]["kind"], "telemetry matches");
        assert_eq!(json["assertions"][0]["details"][1], "two");
        assert_eq!(json["assertions"][0]["details_truncated"], false);
        let expected_details_path = PathBuf::from("/logs/correctness/dsd-plain")
            .join("details")
            .join("0-telemetry-matches.txt");
        assert_eq!(
            json["assertions"][0]["details_path"],
            serde_json::to_value(expected_details_path).expect("details path should serialize")
        );
        assert!(json["error"].is_null());
    }

    #[test]
    fn errored_test_report_carries_a_structured_error() {
        let result = TestResult::errored(
            "dsd-plain",
            ErrorKind::Setup,
            "Failed to spawn shared millstone container.",
            Duration::from_secs(1),
            Vec::new(),
        );

        let json = serde_json::to_value(TestReport::new(&result)).expect("test report should serialize");

        assert_eq!(json["outcome"], "errored");
        assert_eq!(json["error"]["kind"], "setup");
        assert_eq!(json["error"]["message"], "Failed to spawn shared millstone container.");
        assert!(json["assertions"]
            .as_array()
            .expect("assertions is an array")
            .is_empty());
    }

    #[test]
    fn assertion_details_are_capped_inline_and_complete_on_disk() {
        let details: Vec<String> = (0..DETAIL_LINE_CAP + 25).map(|i| format!("line {}", i)).collect();
        let log_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let mut result = TestResult::from_assertions(
            "dsd-plain",
            Duration::from_secs(1),
            vec![assertion("telemetry matches", false)],
            Vec::new(),
        )
        .with_assertion_details(vec![details]);
        result.log_dir = Some(log_dir.path().to_path_buf());

        let nested_log_dir = log_dir.path().join("comparison");
        std::fs::create_dir(&nested_log_dir).expect("nested log directory should be creatable");
        let nested_log = nested_log_dir.join("target.stdout.log");
        std::fs::write(&nested_log, "log line\n").expect("nested log should be writable");

        write_test_report(&result, log_dir.path());

        let report = TestReport::new(&result);
        assert!(report.artifacts.contains(&nested_log));
        assert_eq!(report.assertions[0].details.len(), DETAIL_LINE_CAP);
        assert!(report.assertions[0].details_truncated);

        let detail_path = report.assertions[0]
            .details_path
            .as_ref()
            .expect("a detail path should be reported");
        let written = std::fs::read_to_string(detail_path).expect("detail file should exist");
        assert_eq!(written.lines().count(), DETAIL_LINE_CAP + 25);

        let result_json =
            std::fs::read_to_string(log_dir.path().join(TEST_REPORT_FILE_NAME)).expect("result.json should exist");
        let parsed: serde_json::Value = serde_json::from_str(&result_json).expect("result.json should parse");
        assert_eq!(parsed["outcome"], "failed");
    }

    #[test]
    fn timed_out_test_report_separates_the_deadline_from_the_active_phase() {
        let deadline = TestResult::errored(
            "slow",
            ErrorKind::Timeout,
            "too slow",
            Duration::from_secs(61),
            Vec::new(),
        )
        .with_timeout_attribution(TimeoutAttribution::test_deadline(
            Duration::from_secs(60),
            Some("container_start".to_string()),
        ));
        let grace = TestResult::errored(
            "stuck",
            ErrorKind::Timeout,
            "stuck",
            Duration::from_secs(91),
            Vec::new(),
        )
        .with_timeout_attribution(TimeoutAttribution::cleanup_grace(
            Duration::from_secs(30),
            Duration::from_secs(60),
            Some("assertions".to_string()),
        ));

        let deadline = serde_json::to_value(TestReport::new(&deadline)).expect("test report should serialize");
        assert_eq!(deadline["outcome"], "timed_out");
        assert_eq!(deadline["timeout"]["deadline"], "test_deadline");
        assert_eq!(deadline["timeout"]["active_phase"], "container_start");
        assert_eq!(deadline["timeout"]["configured_ms"], 60_000);
        assert_eq!(deadline["timeout"]["test_deadline_ms"], 60_000);

        let grace = serde_json::to_value(TestReport::new(&grace)).expect("test report should serialize");
        assert_eq!(grace["timeout"]["deadline"], "cleanup_grace");
        assert_eq!(grace["timeout"]["active_phase"], "assertions");
        assert_eq!(grace["timeout"]["configured_ms"], 30_000);
        assert_eq!(grace["timeout"]["test_deadline_ms"], 60_000);
    }

    #[test]
    fn timed_out_test_report_says_unknown_when_no_phase_was_active() {
        let result = TestResult::errored(
            "slow",
            ErrorKind::Timeout,
            "too slow",
            Duration::from_secs(61),
            Vec::new(),
        )
        .with_timeout_attribution(TimeoutAttribution::test_deadline(Duration::from_secs(60), None));

        let json = serde_json::to_value(TestReport::new(&result)).expect("test report should serialize");

        assert_eq!(json["timeout"]["active_phase"], "unknown");
    }

    #[test]
    fn cancelled_test_report_carries_no_timeout_attribution() {
        let result = TestResult::from_assertions(
            "cancelled",
            Duration::from_secs(1),
            vec![assertion("cancelled", false)],
            Vec::new(),
        )
        .with_harness_error(ErrorKind::Internal, "Test was cancelled.");

        let json = serde_json::to_value(TestReport::new(&result)).expect("test report should serialize");

        assert_eq!(json["outcome"], "errored");
        assert_eq!(json["error"]["kind"], "internal");
        assert!(json["timeout"].is_null());
    }

    #[test]
    fn failed_test_report_surfaces_diagnostic_lines_from_captured_output() {
        let log_dir = tempfile::tempdir().expect("temp dir should be creatable");
        std::fs::write(
            log_dir.path().join("stdout.log"),
            "starting up\nERROR failed to bind socket\nno error here in this ordinary line\n",
        )
        .expect("stdout log should be writable");
        // A stderr line qualifies on its own, with no severity marker.
        std::fs::write(
            log_dir.path().join("stderr.log"),
            "\n/bin/sh: 1: exec: /usr/bin/adp: not found\n",
        )
        .expect("stderr log should be writable");
        // The harness's own files are not scanned, so its summaries never feed back as leads.
        std::fs::write(log_dir.path().join("result.log"), "FAIL error in the summary line\n")
            .expect("result log should be writable");

        let mut result = TestResult::from_assertions(
            "dsd-plain",
            Duration::from_secs(1),
            vec![assertion("log_contains", false)],
            Vec::new(),
        );
        result.log_dir = Some(log_dir.path().to_path_buf());

        let report = TestReport::new(&result);

        let selected: Vec<(&str, DiagnosticReason)> = report
            .diagnostic_lines
            .iter()
            .map(|line| (line.text.as_str(), line.reason))
            .collect();
        // Artifacts are scanned in the sorted order they are reported in, so `stderr.log` first.
        assert_eq!(
            selected,
            vec![
                (
                    "/bin/sh: 1: exec: /usr/bin/adp: not found",
                    DiagnosticReason::CapturedStderr
                ),
                ("ERROR failed to bind socket", DiagnosticReason::SeverityMarker),
            ]
        );
        assert!(!report.diagnostic_lines_truncated);
        assert_eq!(report.diagnostic_lines[0].source, log_dir.path().join("stderr.log"));
        // The blank first line of `stderr.log` is skipped, and the line numbers stay absolute.
        assert_eq!(report.diagnostic_lines[0].line_number, 2);
        assert_eq!(report.diagnostic_lines[1].source, log_dir.path().join("stdout.log"));
        assert_eq!(report.diagnostic_lines[1].line_number, 2);
    }

    #[test]
    fn passed_test_report_surfaces_no_diagnostic_lines() {
        let log_dir = tempfile::tempdir().expect("temp dir should be creatable");
        std::fs::write(log_dir.path().join("stdout.log"), "ERROR failed to bind socket\n")
            .expect("stdout log should be writable");

        let mut result = TestResult::from_assertions(
            "dsd-plain",
            Duration::from_secs(1),
            vec![assertion("log_contains", true)],
            Vec::new(),
        );
        result.log_dir = Some(log_dir.path().to_path_buf());

        let report = TestReport::new(&result);

        assert!(report.diagnostic_lines.is_empty());
        assert!(!report.diagnostic_lines_truncated);
    }

    #[test]
    fn diagnostic_lines_are_capped_in_count_and_length() {
        let log_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let long_line = format!("ERROR {}", "x".repeat(DIAGNOSTIC_LINE_LENGTH_CAP * 2));
        let lines: Vec<String> = (0..DIAGNOSTIC_LINE_CAP + 10).map(|_| long_line.clone()).collect();
        std::fs::write(log_dir.path().join("stdout.log"), lines.join("\n")).expect("stdout log should be writable");

        let mut result = TestResult::from_assertions(
            "dsd-plain",
            Duration::from_secs(1),
            vec![assertion("log_contains", false)],
            Vec::new(),
        );
        result.log_dir = Some(log_dir.path().to_path_buf());

        let report = TestReport::new(&result);

        assert_eq!(report.diagnostic_lines.len(), DIAGNOSTIC_LINE_CAP);
        assert!(report.diagnostic_lines_truncated);
        assert_eq!(
            report.diagnostic_lines[0].text.chars().count(),
            DIAGNOSTIC_LINE_LENGTH_CAP
        );
    }

    #[test]
    fn run_report_records_the_revision_it_was_built_from() {
        let report = RunReport::new(
            vec![PathBuf::from("/cases")],
            "linux".to_string(),
            1,
            "2026-09-01T12:00:00Z".to_string(),
            Duration::from_secs(1),
            PathBuf::from("/logs"),
            &[],
        );

        // A build outside a checkout reports `unknown`; inside one it is a SHA, optionally dirty.
        let revision = report.build_revision.trim_end_matches("-dirty");
        assert!(
            revision == UNKNOWN_REVISION || revision.chars().all(|c| c.is_ascii_hexdigit()),
            "revision was: {}",
            report.build_revision
        );
        assert!(!revision.is_empty());
    }

    #[test]
    fn revision_collapses_to_unknown_without_a_sha() {
        assert_eq!(revision("", "true"), UNKNOWN_REVISION);
        assert_eq!(revision("VERGEN_IDEMPOTENT_OUTPUT", "true"), UNKNOWN_REVISION);
        assert_eq!(revision("abc123", "true"), "abc123-dirty");
        assert_eq!(revision("abc123", "false"), "abc123");
    }

    #[test]
    fn run_report_totals_separate_failures_from_errors() {
        let results = vec![
            TestResult::from_assertions("a", Duration::from_secs(1), vec![assertion("x", true)], Vec::new()),
            TestResult::from_assertions("b", Duration::from_secs(1), vec![assertion("x", false)], Vec::new()),
            TestResult::errored("c", ErrorKind::Setup, "boom", Duration::from_secs(1), Vec::new()),
            TestResult::errored("d", ErrorKind::Timeout, "too slow", Duration::from_secs(1), Vec::new()),
        ];

        let report = RunReport::new(
            vec![PathBuf::from("/cases")],
            "linux".to_string(),
            4,
            "2026-09-01T12:00:00Z".to_string(),
            Duration::from_secs(10),
            PathBuf::from("/logs"),
            &results,
        );

        assert_eq!(report.totals.passed, 1);
        assert_eq!(report.totals.failed, 1);
        assert_eq!(report.totals.errored, 2);
        assert_eq!(results[3].outcome, TestOutcome::TimedOut);
        assert_eq!(report.tests[3].outcome, TestOutcome::TimedOut);
    }
}
