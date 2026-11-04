//! Steps that search a participant's output for a pattern.

use std::time::Duration;

use async_trait::async_trait;
use panoramic_core::record::{AssertionStatus, StepReport};
use panoramic_core::step::{decode_fields, Need, OutputEnd, Step, StepContext, StepFactory, StepFields};
use panoramic_core::{case::CaseDuration, probe::POLL_INTERVAL};
use serde::Deserialize;
use tokio::time::{sleep_until, Instant};

use super::Matcher;

/// Waits until a participant's output contains a pattern.
///
/// Callers can decode the public fields with [`decode_fields`]. The factory compiles the pattern once at build time.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogContains {
    /// Required participant whose output is searched; has no default.
    pub on: String,
    /// Process whose output is searched. Defaults to every process of the participant.
    #[serde(default)]
    pub process: Option<String>,
    /// Required text or regex to find; has no default.
    pub pattern: String,
    /// Whether `pattern` is a regex. Defaults to false.
    #[serde(default)]
    pub regex: bool,
    /// Required, positive time to wait for a match; has no default.
    pub timeout: CaseDuration,
}

impl LogContains {
    /// Registry name used in case procedures.
    pub const NAME: &'static str = "log_contains";
}

/// Checks that a pattern stays absent from a participant's output.
///
/// Callers can decode the public fields with [`decode_fields`]. The factory compiles the pattern once at build time.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogNotContains {
    /// Required participant whose output is searched; has no default.
    pub on: String,
    /// Process whose output is searched. Defaults to every process of the participant.
    #[serde(default)]
    pub process: Option<String>,
    /// Required text or regex that must stay absent; has no default.
    pub pattern: String,
    /// Whether `pattern` is a regex. Defaults to false.
    #[serde(default)]
    pub regex: bool,
    /// Required, positive time the pattern must stay absent; has no default.
    pub during: CaseDuration,
}

impl LogNotContains {
    /// Registry name used in case procedures.
    pub const NAME: &'static str = "log_not_contains";
}

/// What a built log step asserts about its pattern in the participant's output.
#[derive(Debug, PartialEq, Eq)]
enum Expect {
    /// The pattern must be found.
    Present,
    /// The pattern must stay absent for the step's whole window.
    Absent,
}

/// A built log step: the decoded fields with the pattern compiled once at build time.
#[derive(Debug)]
struct LogStep {
    on: String,
    process: Option<String>,
    matcher: Matcher,
    expect: Expect,
    budget: Duration,
}

#[async_trait]
impl Step for LogStep {
    fn name(&self) -> &'static str {
        match self.expect {
            Expect::Present => LogContains::NAME,
            Expect::Absent => LogNotContains::NAME,
        }
    }
    fn needs(&self) -> Vec<Need> {
        match &self.process {
            Some(process) => vec![Need::Process(self.on.clone(), process.clone())],
            None => vec![Need::Handle(self.on.clone())],
        }
    }
    fn budget(&self) -> Duration {
        self.budget
    }
    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let deadline = started + self.budget;
        let pattern = self.matcher.pattern();
        let Some(handle) = cx.handle(&self.on) else {
            return StepReport::new(
                started,
                AssertionStatus::Errored,
                format!("participant '{}' has not started", self.on),
            );
        };
        let mut output = handle.output(self.process.as_deref());
        loop {
            let batch = output.read();
            if let Some(line) = batch.lines.iter().find(|line| self.matcher.is_match(line)) {
                let line = line.chars().take(200).collect::<String>();
                let status = match self.expect {
                    Expect::Present => AssertionStatus::Passed,
                    Expect::Absent => AssertionStatus::Failed,
                };
                return StepReport::new(started, status, format!("pattern '{pattern}' found in line: {line:?}"));
            }
            match batch.end {
                OutputEnd::Failed(reason) => return StepReport::new(started, AssertionStatus::Errored, reason),
                OutputEnd::Complete if self.expect == Expect::Present => {
                    return StepReport::new(
                        started,
                        AssertionStatus::Failed,
                        format!("output ended without a match for pattern '{pattern}'"),
                    )
                }
                OutputEnd::Complete => {
                    sleep_until(deadline).await;
                    break;
                }
                OutputEnd::Open => {}
            }
            if Instant::now() >= deadline {
                break;
            }
            sleep_until(deadline.min(Instant::now() + POLL_INTERVAL)).await;
        }
        let (status, message) = match self.expect {
            Expect::Present => (
                AssertionStatus::Failed,
                format!("pattern '{pattern}' not found within {:?}", self.budget),
            ),
            Expect::Absent => (
                AssertionStatus::Passed,
                format!("pattern '{pattern}' stayed absent for {:?}", self.budget),
            ),
        };
        StepReport::new(started, status, message)
    }
}

/// Builds `log_contains` from its case-file fields.
pub(crate) struct LogContainsFactory;

impl StepFactory for LogContainsFactory {
    fn name(&self) -> &'static str {
        LogContains::NAME
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step: LogContains = decode_fields(fields)?;
        let matcher = Matcher::build(&step.pattern, step.regex)?;
        Ok(Box::new(LogStep {
            on: step.on,
            process: step.process,
            matcher,
            expect: Expect::Present,
            budget: step.timeout.into(),
        }))
    }
}

/// Builds `log_not_contains` from its case-file fields.
pub(crate) struct LogNotContainsFactory;

impl StepFactory for LogNotContainsFactory {
    fn name(&self) -> &'static str {
        LogNotContains::NAME
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step: LogNotContains = decode_fields(fields)?;
        let matcher = Matcher::build(&step.pattern, step.regex)?;
        Ok(Box::new(LogStep {
            on: step.on,
            process: step.process,
            matcher,
            expect: Expect::Absent,
            budget: step.during.into(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use panoramic_core::step::test_support::{run_step_with, FakeParticipant};
    use serde_json::Value;

    use super::*;
    use crate::test_support::valid_fields;

    #[tokio::test(start_paused = true)]
    async fn log_contains_searches_all_output_or_only_the_named_process() {
        for (process, pattern, regex, status) in [
            (None, "ready", false, AssertionStatus::Passed),
            (Some("worker"), "ready", false, AssertionStatus::Passed),
            (Some("main"), "ready", false, AssertionStatus::Failed),
            (Some("worker"), "r.ady$", true, AssertionStatus::Passed),
            (None, "r.ady$", false, AssertionStatus::Failed),
        ] {
            let fake = FakeParticipant::default();
            fake.push_line(Some("worker"), "now ready");
            let fields = serde_saphyr::from_str(&format!(
                "on: subject\npattern: '{pattern}'\nregex: {regex}\ntimeout: 1s\nprocess: {}",
                process.unwrap_or("null")
            ))
            .unwrap();
            let report = run_step_with(&LogContainsFactory, &fields, Some(&fake)).await;
            assert_eq!(report.status, status, "{report:?}");
            assert!(report.message.contains(pattern));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn log_contains_waits_for_new_lines_and_times_out_at_its_budget() {
        let fake = FakeParticipant::default();
        let fields = valid_fields("log_contains");
        let (report, ()) = tokio::join!(run_step_with(&LogContainsFactory, &fields, Some(&fake)), async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            fake.push_line(None, "ready");
        });
        assert_eq!(report.status, AssertionStatus::Passed);
        let started = Instant::now();
        let report = run_step_with(&LogContainsFactory, &fields, Some(&FakeParticipant::default())).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert!(report.message.contains("pattern 'ready' not found within 30s"));
    }

    #[tokio::test(start_paused = true)]
    async fn log_steps_report_capture_failure_but_scan_available_lines_first() {
        for factory in [&LogContainsFactory as &dyn StepFactory, &LogNotContainsFactory] {
            let fake = FakeParticipant::default();
            fake.fail_capture("capture lost");
            let fields = valid_fields(factory.name());
            let report = run_step_with(factory, &fields, Some(&fake)).await;
            assert_eq!(report.status, AssertionStatus::Errored);
            assert_eq!(report.message, "capture lost");
            fake.push_line(None, fields["pattern"].as_str().unwrap());
            let report = run_step_with(factory, &fields, Some(&fake)).await;
            assert_eq!(
                report.status,
                if factory.name() == "log_contains" {
                    AssertionStatus::Passed
                } else {
                    AssertionStatus::Failed
                }
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn completed_output_fails_contains_but_not_contains_keeps_its_window() {
        let fake = FakeParticipant::default();
        fake.complete();
        let report = run_step_with(&LogContainsFactory, &valid_fields("log_contains"), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert!(report.message.contains("output ended without a match"));
        let started = Instant::now();
        let report = run_step_with(&LogNotContainsFactory, &valid_fields("log_not_contains"), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(started.elapsed(), Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn log_not_contains_passes_after_its_window_and_quotes_a_matching_line() {
        let fake = FakeParticipant::default();
        let fields = valid_fields("log_not_contains");
        let started = Instant::now();
        let report = run_step_with(&LogNotContainsFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(started.elapsed(), Duration::from_secs(10));
        fake.push_line(None, &format!("panic {}", "é".repeat(220)));
        let report = run_step_with(&LogNotContainsFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert!(
            report.message.contains(&format!("\"panic {}\"", "é".repeat(194))),
            "{}",
            report.message
        );
        assert!(!report.message.contains(&"é".repeat(195)));
    }

    #[test]
    fn log_steps_reject_a_stream_field() {
        for factory in [&LogContainsFactory as &dyn StepFactory, &LogNotContainsFactory] {
            let mut fields = valid_fields(factory.name());
            fields.insert("stream".into(), Value::String("stdout".into()));
            let message = factory.build(&fields).unwrap_err();
            assert!(message.contains("unknown field `stream`"), "{message}");
        }
    }

    #[test]
    fn log_contains_decodes_its_fields_and_regex_defaults_to_false() {
        let fields = valid_fields("log_contains");
        let step: LogContains = decode_fields(&fields).unwrap();
        assert_eq!(step.on, "subject");
        assert_eq!(step.process, None);
        assert_eq!(step.pattern, "ready");
        assert!(!step.regex);
        assert_eq!(Duration::from(step.timeout), Duration::from_secs(30));
    }

    #[test]
    fn log_not_contains_requires_during_and_rejects_timeout() {
        let mut fields = valid_fields("log_not_contains");
        let step: LogNotContains = decode_fields(&fields).unwrap();
        assert_eq!(step.on, "subject");
        assert_eq!(step.process, None);
        assert_eq!(step.pattern, "panic");
        assert!(!step.regex);
        assert_eq!(Duration::from(step.during), Duration::from_secs(10));
        let during = fields.remove("during").unwrap();
        let message = LogNotContainsFactory.build(&fields).unwrap_err();
        assert!(message.contains("missing field `during`"), "{message}");
        fields.insert("timeout".into(), during);
        let message = LogNotContainsFactory.build(&fields).unwrap_err();
        assert!(message.contains("unknown field `timeout`"), "{message}");
    }

    #[test]
    fn log_steps_require_their_participant_pattern_and_duration() {
        for (factory, duration) in [
            (&LogContainsFactory as &dyn StepFactory, "timeout"),
            (&LogNotContainsFactory, "during"),
        ] {
            for field in ["on", "pattern", duration] {
                let mut fields = valid_fields(factory.name());
                fields.remove(field).unwrap();
                let message = factory.build(&fields).unwrap_err();
                assert!(message.contains(&format!("missing field `{field}`")), "{message}");
            }
        }
    }

    #[test]
    fn a_log_step_with_a_process_needs_that_process_and_one_without_needs_its_participant() {
        for factory in [&LogContainsFactory as &dyn StepFactory, &LogNotContainsFactory] {
            let mut fields = valid_fields(factory.name());
            let step = factory.build(&fields).unwrap();
            assert_eq!(step.needs(), vec![Need::Handle("subject".into())]);
            fields.insert("process".into(), Value::String("worker".into()));
            let step = factory.build(&fields).unwrap();
            assert_eq!(step.needs(), vec![Need::Process("subject".into(), "worker".into())]);
            fields.insert("process".into(), Value::Null);
            let step = factory.build(&fields).unwrap();
            assert_eq!(step.needs(), vec![Need::Handle("subject".into())]);
        }
    }
}
