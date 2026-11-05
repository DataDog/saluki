//! Run reports and their durable artifacts.

use std::fmt;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};

use chrono::Local;
use serde::Serialize;
use tracing::warn;

use crate::artifact::VerifiedArtifact;
use crate::layout::{LayoutEntry, Os};
use crate::record::StepStatus;
use crate::result::{
    DurationMs, Issue, ParticipantReport, PhaseTiming, ReclaimEntry, StopCause, TestError, TestOutcome, TestResult,
    TimeoutAttribution,
};

/// Reported when the build could not read a repository to get a revision from.
const UNKNOWN_REVISION: &str = "unknown";

/// File name of the run-level report, written directly under the run's log directory.
pub const RUN_REPORT_FILE_NAME: &str = "run.json";

/// File name of the per-test report, written into each test's log directory.
pub(crate) const TEST_REPORT_FILE_NAME: &str = "result.json";

/// File name of the human-readable result log, written into each test's log directory.
pub const RESULT_LOG_FILE_NAME: &str = "result.log";

/// Number of diagnostic lines carried per test. The artifacts they came from hold the rest.
const DIAGNOSTIC_LINE_CAP: usize = 25;

/// Length a reported diagnostic line is cut to, so one long line cannot bloat a report.
const DIAGNOSTIC_LINE_LENGTH_CAP: usize = 500;

/// Log files the harness writes itself, which are never scanned for diagnostic lines.
///
/// The harness's own summaries repeat assertion messages, so scanning them would report the
/// verdict back as if it were evidence.
const HARNESS_LOG_FILE_NAMES: &[&str] = &[RESULT_LOG_FILE_NAME];

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

/// Schema version of `run.json` and `result.json`. An absent version field denotes version 1.
pub(crate) const REPORT_SCHEMA_VERSION: u32 = 4;

/// A whole run, as consumed by a program.
///
/// The run collector rewrites `run.json` throughout the run, so a reader may see any [`RunState`].
#[derive(Debug, Serialize)]
pub struct RunReport {
    /// Report schema version, set to `REPORT_SCHEMA_VERSION`.
    pub(crate) schema_version: u32,
    /// Current run status.
    pub state: RunState,
    /// First interruption reason, set when `state` becomes `interrupted`.
    pub interrupted_reason: Option<InterruptReason>,
    /// Revision of the repository the runner was built from: a commit SHA, `-dirty` suffixed when
    /// the checkout had uncommitted changes, or `unknown` when the build could not read a repository.
    pub(crate) build_revision: String,
    /// Test-case directories the run discovered from.
    pub(crate) suite_dirs: Vec<PathBuf>,
    /// Subject operating system selected for the run.
    pub(crate) subject_os: Os,
    /// Artifacts verified before the run, in role-name order; empty when none are needed.
    pub(crate) artifacts: Vec<VerifiedArtifact>,
    /// How many tests ran concurrently.
    pub(crate) parallelism: usize,
    /// When the run started, as an RFC 3339 timestamp.
    pub(crate) started_at: String,
    /// Elapsed milliseconds at the last report update.
    pub duration_ms: u64,
    /// Absolute path of the run's log directory.
    pub(crate) log_dir: PathBuf,
    /// Counts by outcome.
    pub totals: RunTotals,
    /// Tests that started and haven't completed, in start order.
    pub running: Vec<String>,
    /// Runnable selections that never started, in selection order.
    pub not_run: Vec<String>,
    /// Skipped tests in name order, followed by completed tests in completion order.
    pub tests: Vec<TestReport>,
}

/// Completion status of a run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// The run is in progress. A stale report may retain this state after the process exits.
    Running,
    /// Every selected test finished, except those skipped by `--fail-fast`.
    Complete,
    /// An interruption was recorded; cleanup may still be running.
    Interrupted,
}

impl fmt::Display for RunState {
    /// Uses the JSON state names in human-readable output.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Running => "running",
            Self::Complete => "complete",
            Self::Interrupted => "interrupted",
        };
        f.write_str(name)
    }
}

/// Reason a run was marked interrupted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InterruptReason {
    /// A signal or TUI interrupt key.
    Signal {
        /// The signal or key that interrupted the run.
        source: &'static str,
    },
    /// The runner task failed or left unfinished tests unexplained by `--fail-fast`.
    RunnerFailed {
        /// What went wrong.
        message: String,
    },
}

/// Counts of tests by outcome. `errored` covers both harness errors and timeouts.
#[derive(Debug, Serialize)]
pub struct RunTotals {
    /// Tests whose assertions all passed.
    pub passed: usize,
    /// Tests whose assertions failed.
    pub failed: usize,
    /// Tests the harness could not run to a verdict, including those that timed out.
    pub errored: usize,
    /// Tests that exceeded their timeout.
    pub timed_out: usize,
    /// Tests whose cases exclude the selected subject OS.
    pub skipped: usize,
    /// Runnable selections that never started.
    pub not_run: usize,
}

/// A single test, as consumed by a program.
#[derive(Debug, Serialize)]
pub struct TestReport {
    /// Name of the test case.
    pub name: String,
    /// Directory the test case was loaded from, when the runner knows it.
    pub(crate) case_path: Option<PathBuf>,
    /// How the test finished.
    pub outcome: TestOutcome,
    /// Why the case was skipped, present only when the outcome is `skipped`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) skip_reason: Option<String>,
    /// Planned participant layout, including when setup failed; skipped cases have `null`.
    pub(crate) layout: Option<Vec<LayoutEntry>>,
    /// Native participants whose creation was attempted and their observed processes.
    pub(crate) participants: Vec<ParticipantReport>,
    /// Files copied from participants into the case artifacts.
    pub(crate) collected: Vec<crate::result::Collected>,
    /// How long the test took.
    pub(crate) duration_ms: u64,
    /// Absolute path of this test's log directory, when the runner knows it.
    pub(crate) log_dir: Option<PathBuf>,
    /// Absolute paths of the files in this test's log directory.
    pub(crate) artifacts: Vec<PathBuf>,
    /// Timing breakdown of the test's phases.
    pub(crate) phase_timings: Vec<PhaseTimingReport>,
    /// Harness-side error, when the test never reached a verdict.
    pub(crate) error: Option<TestError>,
    /// Which deadline fired and what the test was doing, when `outcome` is `timed_out`.
    pub(crate) timeout: Option<TimeoutAttribution>,
    /// Cause and running steps for a test interrupted before its verdict.
    pub(crate) cause: Option<StopCause>,
    /// Cleanup results for registered resources, in reverse registration order.
    pub(crate) reclaim: Vec<ReclaimEntry>,
    /// Problems after the verdict or during cleanup. They never change `outcome`.
    pub(crate) issues: Vec<Issue>,
    /// Captured output lines worth reading first. Empty for a passing test.
    pub(crate) diagnostic_lines: Vec<DiagnosticLine>,
    /// Whether `diagnostic_lines` was capped.
    pub(crate) diagnostic_lines_truncated: bool,
    /// All planned steps and parallel members in procedure order, including those that never ran.
    pub(crate) assertions: Vec<AssertionReport>,
}

/// The contents of `result.json`: a [`TestReport`] with the schema version.
#[derive(Serialize)]
struct TestReportFile<'a> {
    /// Report schema version, set to `REPORT_SCHEMA_VERSION`.
    schema_version: u32,
    #[serde(flatten)]
    report: &'a TestReport,
}

/// A captured output line the harness offers as a starting point for diagnosis.
///
/// Treat a selected line as a lead and read it in context in the artifact it names: benign lines land
/// on stderr, and a failing test can produce no diagnostic lines at all.
#[derive(Debug, Serialize)]
pub(crate) struct DiagnosticLine {
    /// Absolute path of the artifact the line came from.
    pub(crate) source: PathBuf,
    /// Position of the line in that artifact, starting at one.
    pub(crate) line_number: usize,
    /// Why the line was selected.
    pub(crate) reason: DiagnosticReason,
    /// The line, cut to a bounded length.
    pub(crate) text: String,
}

/// Why a captured output line was selected as a diagnostic lead.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiagnosticReason {
    /// The line was written to the component's stderr, where anything at all is worth reading.
    CapturedStderr,
    /// The line carries one of the severity markers the harness looks for.
    SeverityMarker,
}

/// Streams a test's captured output is split across, distinguished because they are read differently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapturedStream {
    Stdout,
    Stderr,
    /// Both streams interleaved in one file, which is how a Kubernetes container's log arrives.
    Combined,
}

impl CapturedStream {
    /// Classifies an artifact by the captured output it holds, or `None` if it holds none.
    ///
    /// Only component logs are read, so harness summaries never feed back into the reported lines.
    fn from_artifact(path: &Path) -> Option<Self> {
        let name = path.file_name().and_then(|name| name.to_str())?;

        if HARNESS_LOG_FILE_NAMES.contains(&name) {
            None
        } else if name.ends_with("stderr.log") {
            Some(Self::Stderr)
        } else if name.ends_with("stdout.log") {
            Some(Self::Stdout)
        } else if name.ends_with(".log") {
            Some(Self::Combined)
        } else {
            None
        }
    }

    /// Why `line` is worth reporting from this stream, or `None` to skip it.
    fn reason_for(&self, line: &str) -> Option<DiagnosticReason> {
        match self {
            Self::Stderr if !line.trim().is_empty() => Some(DiagnosticReason::CapturedStderr),
            // A combined log holds stderr lines the reader cannot tell apart from stdout, so both
            // are held to the marker test rather than reporting an entire container's output.
            Self::Stdout | Self::Combined if SEVERITY_MARKERS.iter().any(|marker| line.contains(marker)) => {
                Some(DiagnosticReason::SeverityMarker)
            }
            _ => None,
        }
    }
}

/// One phase of a test's execution.
#[derive(Debug, Serialize)]
pub(crate) struct PhaseTimingReport {
    /// Name of the phase.
    pub(crate) phase: String,
    /// How long the phase took.
    pub(crate) duration_ms: u64,
}

/// One procedure step, or one member of a parallel block, within a test.
#[derive(Debug, Serialize)]
pub(crate) struct AssertionReport {
    /// Zero-based position in this test's entries, counting parallel members separately.
    pub(crate) index: usize,
    /// Position of the procedure step, starting at zero.
    pub(crate) step: usize,
    /// Position within a parallel block, starting at zero, or `null` for a step that runs alone.
    pub(crate) member: Option<usize>,
    /// Kind of assertion or action, as named by the harness.
    pub(crate) kind: String,
    /// How the step ended, or `not_run`.
    pub(crate) status: StepStatus,
    /// Whether `status` is `passed`.
    pub(crate) passed: bool,
    /// Human-readable summary.
    pub(crate) message: String,
    /// How long the assertion took.
    pub(crate) duration_ms: u64,
}

impl RunReport {
    /// Creates a running report, seeding skipped results and listing runnable selections as not run.
    ///
    /// `build_revision` identifies the runner build, formatted by [`revision`].
    /// `artifacts` contains the bindings verified before the run.
    pub fn new(
        build_revision: String, suite_dirs: Vec<PathBuf>, artifacts: Vec<VerifiedArtifact>, subject_os: Os,
        parallelism: usize, log_dir: PathBuf, selected: Vec<String>, skipped: Vec<TestReport>,
    ) -> Self {
        Self {
            schema_version: REPORT_SCHEMA_VERSION,
            state: RunState::Running,
            interrupted_reason: None,
            build_revision,
            suite_dirs,
            subject_os,
            artifacts,
            parallelism,
            started_at: Local::now().to_rfc3339(),
            duration_ms: 0,
            log_dir,
            totals: RunTotals::new(&skipped, selected.len()),
            running: Vec::new(),
            not_run: selected,
            tests: skipped,
        }
    }
}

impl RunTotals {
    /// Counts reported tests by outcome and records how many runnable selections never started.
    pub fn new(tests: &[TestReport], not_run: usize) -> Self {
        let count = |matches: fn(&TestOutcome) -> bool| tests.iter().filter(|t| matches(&t.outcome)).count();

        Self {
            passed: count(TestOutcome::is_passed),
            failed: count(|outcome| *outcome == TestOutcome::Failed),
            errored: count(TestOutcome::is_error),
            timed_out: count(|outcome| *outcome == TestOutcome::TimedOut),
            skipped: count(|outcome| *outcome == TestOutcome::Skipped),
            not_run,
        }
    }
}

/// Explains why a case was excluded by the selected subject OS.
pub fn skip_reason(subject_os: Os) -> String {
    format!("subject_os excludes {}", subject_os.name())
}

impl TestReport {
    /// Reports a case excluded by the selected subject OS.
    pub fn skipped(name: String, case_path: PathBuf, subject_os: Os) -> Self {
        Self {
            name,
            case_path: Some(case_path),
            outcome: TestOutcome::Skipped,
            skip_reason: Some(skip_reason(subject_os)),
            layout: None,
            participants: Vec::new(),
            collected: Vec::new(),
            duration_ms: 0,
            log_dir: None,
            artifacts: Vec::new(),
            phase_timings: Vec::new(),
            error: None,
            timeout: None,
            cause: None,
            reclaim: Vec::new(),
            issues: Vec::new(),
            diagnostic_lines: Vec::new(),
            diagnostic_lines_truncated: false,
            assertions: Vec::new(),
        }
    }

    /// Builds the per-test document.
    pub fn new(result: &TestResult) -> Self {
        let assertions = result
            .assertion_results
            .iter()
            .enumerate()
            .map(|(index, assertion)| AssertionReport {
                index,
                step: assertion.step,
                member: assertion.member,
                kind: assertion.name.clone(),
                status: assertion.status,
                passed: assertion.status == StepStatus::Passed,
                message: assertion.message.clone(),
                duration_ms: DurationMs(assertion.duration).as_millis(),
            })
            .collect();

        let artifacts = result.log_dir.as_deref().map(list_artifacts).unwrap_or_default();

        // Only non-passing tests get their output scanned. Error-looking lines are common in output
        // that every assertion accepted.
        let (diagnostic_lines, diagnostic_lines_truncated) = if result.outcome.is_passed() {
            (Vec::new(), false)
        } else {
            collect_diagnostic_lines(diagnostic_sources(result, &artifacts))
        };

        Self {
            name: result.name.clone(),
            case_path: result.case_path.clone(),
            outcome: result.outcome,
            skip_reason: None,
            layout: result.layout.clone(),
            participants: result.participants.clone(),
            collected: result.collected.clone(),
            duration_ms: DurationMs(result.duration).as_millis(),
            log_dir: result.log_dir.clone(),
            artifacts,
            phase_timings: result.phase_timings.iter().map(PhaseTimingReport::new).collect(),
            error: result.error.clone(),
            timeout: result.timeout.clone(),
            cause: result.cause.clone(),
            reclaim: result.reclaim.clone(),
            issues: result.issues.clone(),
            diagnostic_lines,
            diagnostic_lines_truncated,
            assertions,
        }
    }
}

/// Formats the build script's SHA and dirty flag as a revision string.
///
/// A value that is not a SHA collapses to `unknown` on its own, with no `-dirty` suffix to read as
/// provenance the build never had.
pub fn revision(sha: &str, dirty: &str) -> String {
    if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return UNKNOWN_REVISION.to_string();
    }

    if dirty == "true" {
        format!("{}-dirty", sha)
    } else {
        sha.to_string()
    }
}

fn diagnostic_sources(result: &TestResult, artifacts: &[PathBuf]) -> Vec<(PathBuf, CapturedStream)> {
    if result.participants.is_empty() {
        return artifacts
            .iter()
            .filter_map(|path| CapturedStream::from_artifact(path).map(|stream| (path.clone(), stream)))
            .collect();
    }
    let mut sources = Vec::new();
    for participant in &result.participants {
        for process in &participant.processes {
            for (path, stream) in [
                (&process.output.stdout, CapturedStream::Stdout),
                (&process.output.stderr, CapturedStream::Stderr),
                (&process.output.merged, CapturedStream::Combined),
            ] {
                if let Some(path) = path {
                    sources.push((path.clone(), stream));
                }
            }
        }
        if let Some(log_dir) = &result.log_dir {
            let dir = log_dir.join(&participant.name);
            let stdout = dir.join("container.stdout.log");
            let stderr = dir.join("container.stderr.log");
            let owned = participant.processes.iter().any(|process| {
                process.output.stdout.as_ref() == Some(&stdout) && process.output.stderr.as_ref() == Some(&stderr)
            });
            if !owned {
                sources.extend([(stdout, CapturedStream::Stdout), (stderr, CapturedStream::Stderr)]);
            }
        }
    }
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    sources.dedup_by(|a, b| a.0 == b.0);
    sources
}

/// Scans a test's captured output for lines worth reading first, capped and in artifact order.
///
/// Returns the lines and whether the cap cut the set short.
fn collect_diagnostic_lines(sources: Vec<(PathBuf, CapturedStream)>) -> (Vec<DiagnosticLine>, bool) {
    let mut lines = Vec::new();
    let mut truncated = false;

    for (artifact, stream) in sources {
        let Ok(file) = std::fs::File::open(&artifact) else {
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

/// Writes a test's `result.json`.
///
/// Called for every test regardless of output format, so that a run killed mid-flight still leaves
/// machine-readable evidence behind. Failures are logged, never fatal: losing an artifact should not
/// change a verdict.
pub fn write_test_report(result: &TestResult, log_dir: &Path) {
    let report = TestReport::new(result);
    let file = TestReportFile {
        schema_version: REPORT_SCHEMA_VERSION,
        report: &report,
    };
    write_json(&log_dir.join(TEST_REPORT_FILE_NAME), &file);
}

/// Writes the run-level `run.json`.
pub fn write_run_report(report: &RunReport) {
    write_json(&report.log_dir.join(RUN_REPORT_FILE_NAME), report);
}

/// Writes a temporary file, then renames it over `path` so readers see a complete report.
///
/// Failures are logged and leave the previous report in place.
fn write_json<T: Serialize>(path: &Path, value: &T) {
    let json = match serde_json::to_string_pretty(value) {
        Ok(json) => json,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to serialize machine-readable report.");
            return;
        }
    };

    let mut tmp_path = path.as_os_str().to_owned();
    tmp_path.push(".tmp");
    let tmp_path = PathBuf::from(tmp_path);

    let written = std::fs::write(&tmp_path, format!("{}\n", json)).and_then(|()| std::fs::rename(&tmp_path, path));
    if let Err(e) = written {
        warn!(path = %path.display(), error = %e, "Failed to write machine-readable report.");
        let _ = std::fs::remove_file(&tmp_path);
    }
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
    use crate::record::StepResult;
    use crate::result::{ErrorKind, TestOutcome, TimeoutAttribution};

    fn assertion(name: &str, status: StepStatus) -> StepResult {
        StepResult {
            step: 0,
            member: None,
            name: name.to_string(),
            status,
            message: "message".to_string(),
            duration: Duration::from_millis(5),
        }
    }

    #[test]
    fn a_skipped_test_report_names_its_reason_and_case_path_and_has_no_log_directory() {
        let report = TestReport::skipped("excluded".into(), PathBuf::from("/cases/excluded"), Os::MacOs);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["name"], "excluded");
        assert_eq!(json["case_path"], "/cases/excluded");
        assert_eq!(json["outcome"], "skipped");
        assert_eq!(json["skip_reason"], "subject_os excludes macos");
        assert_eq!(json["duration_ms"], 0);
        for field in ["layout", "log_dir", "error", "timeout", "cause"] {
            assert!(json[field].is_null(), "{field}");
        }
        for field in [
            "artifacts",
            "collected",
            "assertions",
            "phase_timings",
            "reclaim",
            "issues",
            "diagnostic_lines",
        ] {
            assert_eq!(json[field], serde_json::json!([]), "{field}");
        }
        assert_eq!(json["diagnostic_lines_truncated"], false);
        let run = RunReport::new(
            "unknown".into(),
            Vec::new(),
            Vec::new(),
            Os::MacOs,
            1,
            PathBuf::new(),
            vec!["runnable".into()],
            vec![report],
        );
        assert_eq!(run.tests[0].name, "excluded");
        assert_eq!(run.not_run, ["runnable"]);
        assert_eq!(run.totals.skipped, 1);
    }

    #[test]
    fn skip_reason_is_serialized_only_for_skipped_tests() {
        for (result, _) in crate::result::test_support::each_outcome() {
            let report = TestReport::new(&result);
            assert!(serde_json::to_value(&report).unwrap().get("skip_reason").is_none());
            let file = TestReportFile {
                schema_version: REPORT_SCHEMA_VERSION,
                report: &report,
            };
            let json = serde_json::to_value(file).unwrap();
            assert_eq!(json["schema_version"], 4);
            assert!(json.get("skip_reason").is_none());
        }
        let skipped = TestReport::skipped("excluded".into(), PathBuf::new(), Os::Linux);
        assert_eq!(
            serde_json::to_value(skipped).unwrap()["skip_reason"],
            "subject_os excludes linux"
        );
    }

    #[test]
    fn a_test_report_carries_the_result_layout_and_skipped_reports_have_null_layout() {
        let mut result = TestResult::from_steps("case", Duration::ZERO, vec![], vec![]);
        assert_eq!(
            serde_json::to_value(TestReport::new(&result)).unwrap().get("layout"),
            Some(&serde_json::Value::Null)
        );
        result.layout = Some(vec![LayoutEntry {
            participant: "subject".into(),
            participant_type: "fixture".into(),
            os: Os::Linux,
            realization: crate::layout::RealizationKind::LinuxContainer,
        }]);
        assert_eq!(
            serde_json::to_value(TestReport::new(&result)).unwrap()["layout"],
            serde_json::json!([{"participant": "subject", "type": "fixture", "os": "linux", "realization": "linux-container"}])
        );
        let skipped = TestReport::skipped("case".into(), PathBuf::new(), Os::MacOs);
        assert_eq!(
            serde_json::to_value(skipped).unwrap().get("layout"),
            Some(&serde_json::Value::Null)
        );
    }

    #[test]
    fn test_report_pins_the_contract_field_names() {
        let mut result = TestResult::from_steps(
            "dsd-plain",
            Duration::from_secs(3),
            vec![assertion("telemetry matches", StepStatus::Failed)],
            vec![PhaseTiming {
                phase: "analysis".to_string(),
                duration: Duration::from_millis(20),
            }],
        );
        let message = "mismatch\n".repeat(125);
        result.assertion_results[0].message = message.clone();
        result.case_path = Some(PathBuf::from("/cases/dsd-plain"));
        result.log_dir = Some(PathBuf::from("/logs/correctness/dsd-plain"));

        let json = serde_json::to_value(TestReport::new(&result)).expect("test report should serialize");

        assert_eq!(json["name"], "dsd-plain");
        assert_eq!(json.get("layout"), Some(&serde_json::Value::Null));
        assert_eq!(json["outcome"], "failed");
        assert_eq!(json["duration_ms"], 3000);
        assert_eq!(json["case_path"], "/cases/dsd-plain");
        assert_eq!(json["log_dir"], "/logs/correctness/dsd-plain");
        assert_eq!(json["phase_timings"][0]["phase"], "analysis");
        assert_eq!(json["phase_timings"][0]["duration_ms"], 20);
        assert_eq!(
            json["assertions"],
            serde_json::json!([{
                "index": 0,
                "step": 0,
                "member": null,
                "kind": "telemetry matches",
                "status": "failed",
                "passed": false,
                "message": message,
                "duration_ms": 5,
            }])
        );
        assert!(json["error"].is_null());
        assert!(json["cause"].is_null());
        assert_eq!(json["reclaim"], serde_json::json!([]));
        assert_eq!(json["issues"], serde_json::json!([]));
    }

    #[test]
    fn errored_test_report_carries_a_structured_error() {
        let result = TestResult::errored(
            "dsd-plain",
            ErrorKind::Setup,
            "Failed to spawn shared load container.",
            Duration::from_secs(1),
            Vec::new(),
        );

        let json = serde_json::to_value(TestReport::new(&result)).expect("test report should serialize");

        assert_eq!(json["outcome"], "errored");
        assert_eq!(json["error"]["kind"], "setup");
        assert_eq!(json["error"]["message"], "Failed to spawn shared load container.");
        assert!(json["assertions"]
            .as_array()
            .expect("assertions is an array")
            .is_empty());
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
            Some("cleanup".to_string()),
        ));

        let deadline = serde_json::to_value(TestReport::new(&deadline)).expect("test report should serialize");
        assert_eq!(deadline["outcome"], "timed_out");
        assert_eq!(deadline["timeout"]["deadline"], "test_deadline");
        assert_eq!(deadline["timeout"]["active_phase"], "container_start");
        assert_eq!(deadline["timeout"]["configured_ms"], 60_000);
        assert_eq!(deadline["timeout"]["test_deadline_ms"], 60_000);
        assert!(deadline["timeout"]["grace_phase"].is_null());

        let grace = serde_json::to_value(TestReport::new(&grace)).expect("test report should serialize");
        assert_eq!(grace["timeout"]["deadline"], "cleanup_grace");
        assert_eq!(grace["timeout"]["active_phase"], "assertions");
        assert_eq!(grace["timeout"]["grace_phase"], "cleanup");
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
        let result = TestResult::from_steps(
            "cancelled",
            Duration::from_secs(1),
            vec![assertion("cancelled", StepStatus::Failed)],
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
            "\n/bin/sh: 1: exec: /usr/bin/server: not found\n",
        )
        .expect("stderr log should be writable");
        // The harness's own files are not scanned, so its summaries never feed back as leads.
        std::fs::write(
            log_dir.path().join(RESULT_LOG_FILE_NAME),
            "FAIL log_contains: ERROR failed to bind socket\n",
        )
        .expect("result log should be writable");

        let mut result = TestResult::from_steps(
            "dsd-plain",
            Duration::from_secs(1),
            vec![assertion("log_contains", StepStatus::Failed)],
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
                    "/bin/sh: 1: exec: /usr/bin/server: not found",
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
    fn failed_test_report_surfaces_diagnostic_lines_from_container_logs() {
        // Kubernetes container logs arrive as one file per container, with stderr interleaved into
        // stdout, so they are named after the container rather than after a stream.
        let log_dir = tempfile::tempdir().expect("temp dir should be creatable");
        std::fs::write(
            log_dir.path().join("target.log"),
            "starting up\nERROR failed to bind socket\n",
        )
        .expect("target log should be writable");
        std::fs::write(
            log_dir.path().join("load.log"),
            "sending payloads\npanicked at src/main.rs:1:1\n",
        )
        .expect("load log should be writable");

        let mut result = TestResult::from_steps(
            "dsd-plain",
            Duration::from_secs(1),
            vec![assertion("telemetry matches", StepStatus::Failed)],
            Vec::new(),
        );
        result.log_dir = Some(log_dir.path().to_path_buf());

        let report = TestReport::new(&result);

        let selected: Vec<(&Path, usize, &str)> = report
            .diagnostic_lines
            .iter()
            .map(|line| (line.source.as_path(), line.line_number, line.text.as_str()))
            .collect();
        assert_eq!(
            selected,
            vec![
                (
                    log_dir.path().join("load.log").as_path(),
                    2,
                    "panicked at src/main.rs:1:1"
                ),
                (
                    log_dir.path().join("target.log").as_path(),
                    2,
                    "ERROR failed to bind socket"
                ),
            ]
        );
        assert!(report
            .diagnostic_lines
            .iter()
            .all(|line| line.reason == DiagnosticReason::SeverityMarker));
    }

    #[test]
    fn native_diagnostics_use_declared_streams_and_unowned_container_output_only() {
        use crate::{
            layout::RealizationKind,
            result::{ExitSource, ProcessExitReport, ProcessOutputReport, ProcessReport},
        };
        let logs = tempfile::tempdir().unwrap();
        let dir = logs.path().join("subject");
        std::fs::create_dir(&dir).unwrap();
        for (name, contents) in [
            ("a.stdout.log", "plain error stream\n"),
            ("b.stderr.log", "ordinary merged output\nERROR merged\n"),
            ("c.data", "ordinary stdout\nERROR stdout\n"),
            ("container.stderr.log", "runtime message\n"),
            ("container.stdout.log", "ordinary runtime output\nERROR runtime\n"),
            ("stray.stderr.log", "ERROR unrelated artifact\n"),
        ] {
            std::fs::write(dir.join(name), contents).unwrap();
        }
        let process = |name: &str, output| ProcessReport {
            name: name.into(),
            exit: ProcessExitReport::Running {
                source: ExitSource::Record,
            },
            output,
        };
        let mut result = TestResult::errored(
            "native",
            crate::result::ErrorKind::Evaluation,
            "failed",
            Duration::ZERO,
            vec![],
        );
        result.log_dir = Some(logs.path().into());
        result.participants = vec![ParticipantReport {
            name: "subject".into(),
            participant_type: "fixture".into(),
            os: Os::Linux,
            realization: RealizationKind::LinuxContainer,
            processes: vec![
                process(
                    "a",
                    ProcessOutputReport {
                        stderr: Some(dir.join("a.stdout.log")),
                        ..Default::default()
                    },
                ),
                process(
                    "b",
                    ProcessOutputReport {
                        merged: Some(dir.join("b.stderr.log")),
                        ..Default::default()
                    },
                ),
                process(
                    "c",
                    ProcessOutputReport {
                        stdout: Some(dir.join("c.data")),
                        ..Default::default()
                    },
                ),
            ],
        }];
        for owned in [false, true] {
            if owned {
                result.participants[0].processes.push(process(
                    "main",
                    ProcessOutputReport {
                        stdout: Some(dir.join("container.stdout.log")),
                        stderr: Some(dir.join("container.stderr.log")),
                        merged: None,
                    },
                ));
            }
            let report = TestReport::new(&result);
            assert_eq!(
                report
                    .diagnostic_lines
                    .iter()
                    .map(|line| (line.text.as_str(), line.reason))
                    .collect::<Vec<_>>(),
                [
                    ("plain error stream", DiagnosticReason::CapturedStderr),
                    ("ERROR merged", DiagnosticReason::SeverityMarker),
                    ("ERROR stdout", DiagnosticReason::SeverityMarker),
                    ("runtime message", DiagnosticReason::CapturedStderr),
                    ("ERROR runtime", DiagnosticReason::SeverityMarker),
                ]
            );
            assert_eq!(report.artifacts.len(), 6);
            assert_eq!(report.participants, result.participants);
            assert_eq!(
                serde_json::to_value(&report).unwrap()["participants"],
                serde_json::to_value(&result.participants).unwrap()
            );
        }
    }

    #[test]
    fn passed_test_report_surfaces_no_diagnostic_lines() {
        let log_dir = tempfile::tempdir().expect("temp dir should be creatable");
        std::fs::write(log_dir.path().join("stdout.log"), "ERROR failed to bind socket\n")
            .expect("stdout log should be writable");

        let mut result = TestResult::from_steps(
            "dsd-plain",
            Duration::from_secs(1),
            vec![assertion("log_contains", StepStatus::Passed)],
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

        let mut result = TestResult::from_steps(
            "dsd-plain",
            Duration::from_secs(1),
            vec![assertion("log_contains", StepStatus::Failed)],
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
    fn revision_collapses_to_unknown_without_a_sha() {
        assert_eq!(revision("", "true"), UNKNOWN_REVISION);
        assert_eq!(revision("VERGEN_IDEMPOTENT_OUTPUT", "true"), UNKNOWN_REVISION);
        assert_eq!(revision("abc123", "true"), "abc123-dirty");
        assert_eq!(revision("abc123", "false"), "abc123");
    }

    #[test]
    fn run_report_totals_separate_failures_from_errors() {
        let results = [
            TestResult::from_steps(
                "a",
                Duration::from_secs(1),
                vec![assertion("x", StepStatus::Passed)],
                Vec::new(),
            ),
            TestResult::from_steps(
                "b",
                Duration::from_secs(1),
                vec![assertion("x", StepStatus::Failed)],
                Vec::new(),
            ),
            TestResult::errored("c", ErrorKind::Setup, "boom", Duration::from_secs(1), Vec::new()),
            TestResult::errored("d", ErrorKind::Timeout, "too slow", Duration::from_secs(1), Vec::new()),
        ];
        let mut tests: Vec<TestReport> = results.iter().map(TestReport::new).collect();
        tests.push(TestReport::skipped(
            "excluded".into(),
            PathBuf::from("/cases/excluded"),
            Os::MacOs,
        ));

        let totals = RunTotals::new(&tests, 3);

        assert_eq!(totals.passed, 1);
        assert_eq!(totals.failed, 1);
        assert_eq!(totals.errored, 2);
        assert_eq!(totals.timed_out, 1);
        assert_eq!(totals.not_run, 3);
        assert_eq!(totals.skipped, 1);
        assert_eq!(tests[3].outcome, TestOutcome::TimedOut);
    }

    #[test]
    fn a_run_report_lists_verified_artifacts_even_when_none_are_needed() {
        for artifacts in [
            vec![],
            vec![VerifiedArtifact {
                role: "tool",
                reference: "tool:latest".into(),
                identity: "sha256:123".into(),
                kind: crate::artifact::ArtifactKind::Image { os: Os::Linux },
            }],
        ] {
            let expected = serde_json::to_value(&artifacts).unwrap();
            let report = RunReport::new(
                "unknown".into(),
                Vec::new(),
                artifacts,
                Os::Linux,
                1,
                PathBuf::new(),
                Vec::new(),
                Vec::new(),
            );
            assert_eq!(serde_json::to_value(report).unwrap()["artifacts"], expected);
        }
    }

    #[test]
    fn run_report_pins_the_schema_four_field_names() {
        let selected = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut report = RunReport::new(
            "0123abcd-dirty".to_string(),
            Vec::new(),
            Vec::new(),
            Os::Linux,
            1,
            PathBuf::new(),
            selected,
            Vec::new(),
        );
        let timed_out = TestResult::errored("a", ErrorKind::Timeout, "too slow", Duration::from_secs(1), Vec::new());
        report.tests = vec![TestReport::new(&timed_out)];
        report.running = vec!["b".to_string()];
        report.not_run = vec!["c".to_string()];
        report.state = RunState::Interrupted;
        report.interrupted_reason = Some(InterruptReason::Signal { source: "SIGTERM" });
        report.totals = RunTotals::new(&report.tests, report.not_run.len());

        let json = serde_json::to_value(&report).expect("run report should serialize");

        assert_eq!(
            json.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "artifacts",
                "build_revision",
                "duration_ms",
                "interrupted_reason",
                "log_dir",
                "not_run",
                "parallelism",
                "running",
                "schema_version",
                "started_at",
                "state",
                "subject_os",
                "suite_dirs",
                "tests",
                "totals"
            ]
        );
        assert_eq!(json["schema_version"], 4);
        assert_eq!(json["subject_os"], "linux");
        assert_eq!(json["artifacts"], serde_json::json!([]));
        assert!(json.get("runtime").is_none());
        assert_eq!(json["build_revision"], "0123abcd-dirty");
        assert_eq!(json["state"], "interrupted");
        assert_eq!(json["interrupted_reason"]["kind"], "signal");
        assert_eq!(json["interrupted_reason"]["source"], "SIGTERM");
        assert_eq!(json["running"], serde_json::json!(["b"]));
        assert_eq!(json["not_run"], serde_json::json!(["c"]));
        assert_eq!(json["totals"]["timed_out"], 1);
        assert_eq!(json["totals"]["errored"], 1);
        assert_eq!(json["totals"]["not_run"], 1);
        assert_eq!(json["tests"][0]["outcome"], "timed_out");
        assert_eq!(json["tests"][0].get("layout"), Some(&serde_json::Value::Null));
        assert!(
            json["tests"][0].get("schema_version").is_none(),
            "only the report as a whole carries the version"
        );
    }

    #[test]
    fn a_report_write_that_cannot_complete_leaves_the_previous_report_in_place() {
        let log_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let run_json = log_dir.path().join(RUN_REPORT_FILE_NAME);
        let tmp_path = log_dir.path().join(format!("{}.tmp", RUN_REPORT_FILE_NAME));
        let read_parallelism = || {
            let written = std::fs::read_to_string(&run_json).expect("run.json should exist");
            let json: serde_json::Value = serde_json::from_str(&written).expect("run.json should parse");
            json["parallelism"].clone()
        };

        let mut report = RunReport::new(
            "unknown".to_string(),
            Vec::new(),
            Vec::new(),
            Os::Linux,
            1,
            log_dir.path().to_path_buf(),
            Vec::new(),
            Vec::new(),
        );
        write_run_report(&report);
        assert_eq!(read_parallelism(), 1);

        // Block the temporary file to test that a failed write preserves `run.json`.
        std::fs::create_dir(&tmp_path).expect("the blocking directory should be creatable");
        report.parallelism = 2;
        write_run_report(&report);
        assert_eq!(read_parallelism(), 1);

        std::fs::remove_dir(&tmp_path).expect("the blocking directory should be removable");
        report.parallelism = 3;
        write_run_report(&report);
        assert_eq!(read_parallelism(), 3);
        assert!(!tmp_path.exists(), "the temporary file should not remain");
    }
}
