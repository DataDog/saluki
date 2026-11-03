//! Steps that search a participant's output for a pattern.

use std::time::Duration;

use async_trait::async_trait;
use panoramic_core::case::CaseDuration;
use panoramic_core::record::{AssertionStatus, StepReport};
use panoramic_core::step::{decode_fields, Need, Step, StepContext, StepFactory, StepFields};
use serde::Deserialize;
use tokio::time::Instant;

use super::check_pattern;

/// Waits until a participant's output contains a pattern.
///
/// Callers can decode the public fields with [`decode_fields`]. The factory's only additional validation checks
/// that regex patterns compile.
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

#[async_trait]
impl Step for LogContains {
    fn name(&self) -> &'static str {
        Self::NAME
    }
    fn budget(&self) -> Duration {
        self.timeout.into()
    }
    async fn run(&self, _cx: &StepContext<'_>) -> StepReport {
        StepReport::new(
            Instant::now(),
            AssertionStatus::Errored,
            format!("step '{}' cannot run on natively executed participants yet", Self::NAME),
        )
    }

    fn needs(&self) -> Vec<Need> {
        match &self.process {
            Some(process) => vec![Need::Process(self.on.clone(), process.clone())],
            None => vec![Need::Handle(self.on.clone())],
        }
    }
}

/// Checks that a pattern stays absent from a participant's output.
///
/// Callers can decode the public fields with [`decode_fields`]. The factory's only additional validation checks
/// that regex patterns compile.
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

#[async_trait]
impl Step for LogNotContains {
    fn name(&self) -> &'static str {
        Self::NAME
    }
    fn budget(&self) -> Duration {
        self.during.into()
    }
    async fn run(&self, _cx: &StepContext<'_>) -> StepReport {
        StepReport::new(
            Instant::now(),
            AssertionStatus::Errored,
            format!("step '{}' cannot run on natively executed participants yet", Self::NAME),
        )
    }

    fn needs(&self) -> Vec<Need> {
        match &self.process {
            Some(process) => vec![Need::Process(self.on.clone(), process.clone())],
            None => vec![Need::Handle(self.on.clone())],
        }
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
        check_pattern(&step.pattern, step.regex)?;
        Ok(Box::new(step))
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
        check_pattern(&step.pattern, step.regex)?;
        Ok(Box::new(step))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::Value;

    use super::*;
    use crate::test_support::valid_fields;

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
