//! A step that searches a file inside a participant for a pattern.

use std::time::Duration;

use async_trait::async_trait;
use panoramic_core::record::{AssertionStatus, StepReport};
use panoramic_core::step::{decode_fields, HandleError, Need, Step, StepContext, StepFactory, StepFields};
use panoramic_core::{
    case::{CaseDuration, StepTemplate},
    probe::POLL_INTERVAL,
};
use serde::Deserialize;
use tokio::time::{sleep_until, timeout_at, Instant};

use super::Matcher;

/// Waits until a file inside a participant contains a pattern.
///
/// Callers can decode the public fields with [`decode_fields`]. The factory compiles the pattern once at build time.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileContains {
    /// Required participant whose file is searched; has no default.
    pub on: String,
    /// Required path inside the participant; may reference facts and late facts of named participants. Has no default.
    pub path: StepTemplate,
    /// Required text or regex to find; has no default.
    pub pattern: String,
    /// Whether `pattern` is a regex. Defaults to false.
    #[serde(default)]
    pub regex: bool,
    /// Required, positive time to wait for a match; has no default.
    pub timeout: CaseDuration,
}

impl FileContains {
    /// Registry name used in case procedures.
    pub const NAME: &'static str = "file_contains";
}

/// A built file step: the decoded fields with the pattern compiled once at build time.
#[derive(Debug)]
struct FileStep {
    on: String,
    path: StepTemplate,
    matcher: Matcher,
    budget: Duration,
}

#[async_trait]
impl Step for FileStep {
    fn name(&self) -> &'static str {
        FileContains::NAME
    }
    fn needs(&self) -> Vec<Need> {
        std::iter::once(Need::Handle(self.on.clone()))
            .chain(self.path.needs())
            .collect()
    }
    fn budget(&self) -> Duration {
        self.budget
    }
    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let deadline = started + self.budget;
        let path = match cx.resolve(&self.path) {
            Ok(path) => path,
            Err(reason) => return StepReport::new(started, AssertionStatus::Errored, reason),
        };
        let Some(handle) = cx.handle(&self.on) else {
            return StepReport::new(
                started,
                AssertionStatus::Errored,
                format!("participant '{}' has not started", self.on),
            );
        };
        let mut last = "read has not completed".to_string();
        loop {
            match timeout_at(deadline, handle.read_file(&path)).await {
                Ok(Ok(Some(bytes))) => {
                    let text = String::from_utf8_lossy(&bytes);
                    if self.matcher.is_match(&text) {
                        return StepReport::new(
                            started,
                            AssertionStatus::Passed,
                            format!("file '{path}' contains pattern '{}'", self.matcher.pattern()),
                        );
                    }
                    last = format!("present with {} bytes and no match", bytes.len());
                }
                Ok(Ok(None)) => last = "missing".into(),
                Ok(Err(HandleError::Harness { message })) => {
                    return StepReport::new(started, AssertionStatus::Errored, message)
                }
                Ok(Err(HandleError::Product { message })) => last = message,
                Err(_) => break,
            }
            if Instant::now() >= deadline {
                break;
            }
            sleep_until(deadline.min(Instant::now() + POLL_INTERVAL)).await;
        }
        StepReport::new(
            started,
            AssertionStatus::Failed,
            format!(
                "file '{path}' did not contain pattern '{}' within {:?}; last observed: {last}",
                self.matcher.pattern(),
                self.budget
            ),
        )
    }
}

/// Builds `file_contains` from its case-file fields.
pub(crate) struct FileContainsFactory;

impl StepFactory for FileContainsFactory {
    fn name(&self) -> &'static str {
        FileContains::NAME
    }

    fn template_fields(&self) -> &'static [&'static str] {
        &["path"]
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step: FileContains = decode_fields(fields)?;
        let matcher = Matcher::build(&step.pattern, step.regex)?;
        Ok(Box::new(FileStep {
            on: step.on,
            path: step.path,
            matcher,
            budget: step.timeout.into(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use panoramic_core::backend::LateFact;
    use panoramic_core::step::test_support::{run_step_with, FakeParticipant};
    use serde_json::Value;

    use super::*;
    use crate::test_support::valid_fields;

    #[tokio::test(start_paused = true)]
    async fn file_contains_retries_and_matches_the_resolved_file_lossily() {
        let fake = FakeParticipant::default();
        let mut fields = valid_fields("file_contains");
        fields.insert("path".into(), Value::String("{{ subject.facts.file }}".into()));
        fields.insert("pattern".into(), Value::String("r.ady".into()));
        fields.insert("regex".into(), Value::Bool(true));
        let (report, ()) = tokio::join!(run_step_with(&FileContainsFactory, &fields, Some(&fake)), async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            fake.set_file("/logs/output.log", Ok(Some(b"not yet".to_vec())));
            tokio::time::sleep(Duration::from_secs(1)).await;
            fake.set_file("/logs/output.log", Ok(Some(b"ready\xff".to_vec())));
        });
        assert_eq!(report.status, AssertionStatus::Passed);
        assert!(report.message.contains("/logs/output.log"));
    }

    #[tokio::test(start_paused = true)]
    async fn file_contains_reports_its_last_observation_at_timeout() {
        for (answer, expected) in [
            (Ok(None), "missing"),
            (Ok(Some(b"not yet".to_vec())), "present with 7 bytes and no match"),
            (
                Err(HandleError::Product {
                    message: "read refused".into(),
                }),
                "read refused",
            ),
        ] {
            let fake = FakeParticipant::default();
            fake.set_file("/logs/output.log", answer);
            let started = Instant::now();
            let report = run_step_with(&FileContainsFactory, &valid_fields("file_contains"), Some(&fake)).await;
            assert_eq!(report.status, AssertionStatus::Failed);
            assert_eq!(started.elapsed(), Duration::from_secs(30));
            assert!(report.message.contains(expected), "{}", report.message);
            assert!(report.message.contains("/logs/output.log"));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn file_contains_errors_on_harness_failure_or_an_unresolved_path() {
        let fake = FakeParticipant::default();
        fake.set_file(
            "/logs/output.log",
            Err(HandleError::Harness {
                message: "read connection lost".into(),
            }),
        );
        let mut fields = valid_fields("file_contains");
        let started = Instant::now();
        let report = run_step_with(&FileContainsFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Errored);
        assert_eq!(report.message, "read connection lost");
        assert_eq!(started.elapsed(), Duration::ZERO);
        fields.insert("path".into(), Value::String("{{ subject.late.ip }}".into()));
        let report = run_step_with(&FileContainsFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Errored);
        assert_eq!(report.message, "participant 'subject' has no late fact 'ip'");
    }

    #[test]
    fn file_contains_rejects_process_and_stream_fields() {
        for field in ["process", "stream"] {
            let mut fields = valid_fields("file_contains");
            fields.insert(field.into(), Value::String("worker".into()));
            let message = FileContainsFactory.build(&fields).unwrap_err();
            assert!(message.contains(&format!("unknown field `{field}`")), "{message}");
        }
    }

    #[test]
    fn file_contains_requires_on_path_pattern_and_timeout() {
        for field in ["on", "path", "pattern", "timeout"] {
            let mut fields = valid_fields("file_contains");
            fields.remove(field).unwrap();
            let message = FileContainsFactory.build(&fields).unwrap_err();
            assert!(message.contains(&format!("missing field `{field}`")), "{message}");
        }
    }

    #[test]
    fn file_contains_decodes_its_fields_and_keeps_paths_verbatim() {
        for path in ["/logs/output.log", r"logs\output.log"] {
            let mut fields = valid_fields("file_contains");
            fields.insert("path".into(), Value::String(path.into()));
            let step: FileContains = decode_fields(&fields).unwrap();
            assert_eq!(step.on, "subject");
            assert_eq!(step.path, path.parse().unwrap());
            assert_eq!(step.pattern, "ready");
            assert!(!step.regex);
            assert_eq!(Duration::from(step.timeout), Duration::from_secs(30));
        }
    }

    #[test]
    fn file_contains_rejects_self_and_endpoint_references_in_its_path() {
        for (path, expected) in [
            (
                "{{ self.facts.file }}",
                "a step has no 'self'; name the participant, as in '{{ <participant>.facts.<fact> }}'",
            ),
            (
                "{{ self.late.ip }}",
                "a step has no 'self'; name the participant, as in '{{ <participant>.late.ip }}'",
            ),
            (
                "{{ subject.endpoints.api.url }}",
                "step fields accept only fact and late fact references to named participants",
            ),
            (
                "{{ self.endpoints.api.url }}",
                "step fields accept only fact and late fact references to named participants",
            ),
            (
                "{{ self.connections.api.url }}",
                "step fields accept only fact and late fact references to named participants",
            ),
        ] {
            let mut fields = valid_fields("file_contains");
            fields.insert("path".into(), Value::String(path.into()));
            assert_eq!(
                FileContainsFactory.build(&fields).unwrap_err(),
                format!("path: '{path}': {expected}")
            );
        }
    }

    #[test]
    fn file_contains_needs_only_its_participant() {
        let step = FileContainsFactory.build(&valid_fields("file_contains")).unwrap();
        assert_eq!(step.needs(), vec![Need::Handle("subject".into())]);
    }

    #[test]
    fn file_contains_reports_fact_and_late_fact_needs_for_its_path() {
        let mut fields = valid_fields("file_contains");
        fields.insert(
            "path".into(),
            Value::String(
                "{{ other.facts.dir }}/{{ subject.late.ip }}/{{ subject.facts.file }}/{{ other.facts.dir }}".into(),
            ),
        );
        let step = FileContainsFactory.build(&fields).unwrap();
        assert_eq!(
            step.needs(),
            [
                Need::Handle("subject".into()),
                Need::Fact("other".into(), "dir".into()),
                Need::Late("subject".into(), LateFact::Ip),
                Need::Fact("subject".into(), "file".into()),
                Need::Fact("other".into(), "dir".into()),
            ]
        );
    }
}
