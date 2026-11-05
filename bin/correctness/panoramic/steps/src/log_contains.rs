//! Steps that search a participant's output for a pattern.

use std::time::Duration;

use async_trait::async_trait;
use panoramic_core::record::{AssertionStatus, StepReport};
use panoramic_core::step::{decode_fields, Need, OutputEnd, Step, StepContext, StepFactory, StepFields};
use panoramic_core::{
    case::{CaseDuration, StepTemplate},
    probe::POLL_INTERVAL,
};
use serde::Deserialize;
use tokio::time::{sleep_until, Instant};

use super::Matcher;

/// Waits until a participant's output contains a pattern.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LogContains {
    /// Required participant whose output is searched; has no default.
    on: String,
    /// Process whose output is searched. Defaults to every process of the participant.
    #[serde(default)]
    process: Option<String>,
    /// Required text or regex to find; literal patterns may reference participant facts. Has no default.
    pattern: StepTemplate,
    /// Whether `pattern` is a regex. Defaults to false.
    #[serde(default)]
    regex: bool,
    /// Required, positive time to wait for a match; has no default.
    timeout: CaseDuration,
}

impl LogContains {
    /// Registry name used in case procedures.
    pub(crate) const NAME: &'static str = "log_contains";
}

/// Checks that a pattern stays absent from a participant's output.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LogNotContains {
    /// Required participant whose output is searched; has no default.
    on: String,
    /// Process whose output is searched. Defaults to every process of the participant.
    #[serde(default)]
    process: Option<String>,
    /// Required text or regex to keep absent; literal patterns may reference participant facts. Has no default.
    pattern: StepTemplate,
    /// Whether `pattern` is a regex. Defaults to false.
    #[serde(default)]
    regex: bool,
    /// Required, positive time the pattern must stay absent; has no default.
    during: CaseDuration,
}

impl LogNotContains {
    /// Registry name used in case procedures.
    pub(crate) const NAME: &'static str = "log_not_contains";
}

/// What a built log step asserts about its pattern in the participant's output.
#[derive(Debug, PartialEq, Eq)]
enum Expect {
    /// The pattern must be found.
    Present,
    /// The pattern must stay absent for the step's whole window.
    Absent,
}

/// A built log step with either a compiled pattern or a literal template.
#[derive(Debug)]
struct LogStep {
    on: String,
    process: Option<String>,
    pattern: StepTemplate,
    matcher: Option<Matcher>,
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
        let participant = match &self.process {
            Some(process) => Need::Process(self.on.clone(), process.clone()),
            None => Need::Handle(self.on.clone()),
        };
        std::iter::once(participant).chain(self.pattern.needs()).collect()
    }
    fn budget(&self) -> Duration {
        self.budget
    }
    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let deadline = started + self.budget;
        let resolved;
        let matcher = match &self.matcher {
            Some(matcher) => matcher,
            None => match cx.resolve(&self.pattern) {
                Ok(pattern) => {
                    resolved = Matcher::Literal(pattern);
                    &resolved
                }
                Err(message) => {
                    return StepReport::new(started, AssertionStatus::Errored, format!("pattern: {message}"))
                }
            },
        };
        let pattern = matcher.pattern();
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
            if let Some(line) = batch.lines.iter().find(|line| matcher.is_match(line)) {
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

fn compile_pattern(pattern: &StepTemplate, regex: bool) -> Result<Option<Matcher>, String> {
    match pattern.literal() {
        Some(text) => Matcher::build(&text, regex).map(Some),
        None if regex => Err("pattern: template references are allowed only in literal patterns (regex: false)".into()),
        None => Ok(None),
    }
}

/// Builds `log_contains` from its case-file fields.
pub(crate) struct LogContainsFactory;

impl StepFactory for LogContainsFactory {
    fn name(&self) -> &'static str {
        LogContains::NAME
    }

    fn template_fields(&self) -> &'static [&'static str] {
        &["pattern"]
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step: LogContains = decode_fields(fields)?;
        let matcher = compile_pattern(&step.pattern, step.regex)?;
        Ok(Box::new(LogStep {
            on: step.on,
            process: step.process,
            pattern: step.pattern,
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

    fn template_fields(&self) -> &'static [&'static str] {
        &["pattern"]
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step: LogNotContains = decode_fields(fields)?;
        let matcher = compile_pattern(&step.pattern, step.regex)?;
        Ok(Box::new(LogStep {
            on: step.on,
            process: step.process,
            pattern: step.pattern,
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
    async fn log_patterns_match_resolved_facts_and_late_facts_and_quote_the_resolved_pattern() {
        for factory in [&LogContainsFactory as &dyn StepFactory, &LogNotContainsFactory] {
            for (pattern, resolved) in [
                ("listen={{ subject.late.ip }}", "listen=192.0.2.7"),
                ("file={{subject.facts.file}}", "file=/logs/output.log"),
            ] {
                let mut fake = FakeParticipant::default();
                fake.set_late(panoramic_core::backend::LateFact::Ip, "192.0.2.7");
                let mut fields = valid_fields(factory.name());
                fields.insert("pattern".into(), pattern.into());
                let absent = run_step_with(factory, &fields, Some(&fake)).await;
                fake.push_line(None, resolved);
                let present = run_step_with(factory, &fields, Some(&fake)).await;
                let (passed, failed) = if factory.name() == "log_contains" {
                    (present, absent)
                } else {
                    (absent, present)
                };
                assert_eq!(passed.status, AssertionStatus::Passed, "{passed:?}");
                assert_eq!(failed.status, AssertionStatus::Failed, "{failed:?}");
                for report in [passed, failed] {
                    assert!(report.message.contains(&format!("pattern '{resolved}'")), "{report:?}");
                    assert!(!report.message.contains("{{"), "{report:?}");
                }
            }
        }
    }

    #[tokio::test]
    async fn a_log_pattern_reference_to_a_participant_that_has_not_started_errors_the_step() {
        for factory in [&LogContainsFactory as &dyn StepFactory, &LogNotContainsFactory] {
            let mut fields = valid_fields(factory.name());
            fields.insert("pattern".into(), "{{subject.late.ip}}".into());
            for fake in [None, Some(FakeParticipant::default())] {
                let report = run_step_with(factory, &fields, fake.as_ref()).await;
                assert_eq!(report.status, AssertionStatus::Errored, "{report:?}");
                assert_eq!(report.message, "pattern: participant 'subject' has no late fact 'ip'");
            }
        }
    }

    #[test]
    fn a_regex_log_pattern_with_a_reference_is_rejected_at_build() {
        for factory in [&LogContainsFactory as &dyn StepFactory, &LogNotContainsFactory] {
            for reference in ["{{subject.facts.file}}", "{{subject.late.ip}}"] {
                let mut fields = valid_fields(factory.name());
                fields.insert("pattern".into(), reference.into());
                fields.insert("regex".into(), true.into());
                assert_eq!(
                    factory.build(&fields).unwrap_err(),
                    "pattern: template references are allowed only in literal patterns (regex: false)"
                );
            }
        }
    }

    #[test]
    fn log_steps_report_pattern_references_as_needs() {
        use panoramic_core::backend::LateFact;
        for factory in [&LogContainsFactory as &dyn StepFactory, &LogNotContainsFactory] {
            let mut fields = valid_fields(factory.name());
            fields.insert("pattern".into(), "{{other.late.ip}}/{{subject.facts.file}}".into());
            assert_eq!(
                factory.build(&fields).unwrap().needs(),
                vec![
                    Need::Handle("subject".into()),
                    Need::Late("other".into(), LateFact::Ip),
                    Need::Fact("subject".into(), "file".into()),
                ]
            );
        }
    }

    #[test]
    fn log_patterns_accept_only_named_fact_and_late_fact_references() {
        for factory in [&LogContainsFactory as &dyn StepFactory, &LogNotContainsFactory] {
            for reference in [
                "{{self.late.ip}}",
                "{{self.facts.file}}",
                "{{subject.endpoints.http.url}}",
                "{{self.connections.target.url}}",
            ] {
                let mut fields = valid_fields(factory.name());
                fields.insert("pattern".into(), reference.into());
                let error = factory.build(&fields).unwrap_err();
                assert!(error.starts_with("pattern:"), "{error}");
                assert!(error.contains(reference), "{error}");
            }
        }
    }

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
        assert_eq!(step.pattern.literal().unwrap(), "ready");
        assert!(!step.regex);
        assert_eq!(Duration::from(step.timeout), Duration::from_secs(30));
    }

    #[test]
    fn log_not_contains_requires_during_and_rejects_timeout() {
        let mut fields = valid_fields("log_not_contains");
        let step: LogNotContains = decode_fields(&fields).unwrap();
        assert_eq!(step.on, "subject");
        assert_eq!(step.process, None);
        assert_eq!(step.pattern.literal().unwrap(), "panic");
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
