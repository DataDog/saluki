//! Steps that search a participant's output for a pattern.

use panoramic_core::case::{decode_fields, CaseDuration, Need, Step, StepDeclaration, StepFields};
use serde::Deserialize;

use super::check_pattern;

/// Waits until a participant's output contains a pattern.
///
/// Its fields are public so callers can read the decoded step.
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

impl Step for LogContains {
    fn needs(&self) -> Vec<Need> {
        match &self.process {
            Some(process) => vec![Need::Process(self.on.clone(), process.clone())],
            None => vec![Need::Handle(self.on.clone())],
        }
    }
}

/// Checks that a pattern stays absent from a participant's output.
///
/// Its fields are public so callers can read the decoded step.
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

impl Step for LogNotContains {
    fn needs(&self) -> Vec<Need> {
        match &self.process {
            Some(process) => vec![Need::Process(self.on.clone(), process.clone())],
            None => vec![Need::Handle(self.on.clone())],
        }
    }
}

/// Builds `log_contains` from its case-file fields.
pub(crate) const LOG_CONTAINS: StepDeclaration = StepDeclaration {
    name: "log_contains",
    template_fields: &[],
    build: |fields: &StepFields| {
        let step: LogContains = decode_fields(fields)?;
        check_pattern(&step.pattern, step.regex)?;
        Ok(Box::new(step))
    },
};

/// Builds `log_not_contains` from its case-file fields.
pub(crate) const LOG_NOT_CONTAINS: StepDeclaration = StepDeclaration {
    name: "log_not_contains",
    template_fields: &[],
    build: |fields: &StepFields| {
        let step: LogNotContains = decode_fields(fields)?;
        check_pattern(&step.pattern, step.regex)?;
        Ok(Box::new(step))
    },
};

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::Value;

    use super::*;
    use crate::test_support::valid_fields;

    #[test]
    fn log_steps_reject_a_stream_field() {
        for declaration in [&LOG_CONTAINS, &LOG_NOT_CONTAINS] {
            let mut fields = valid_fields(declaration.name);
            fields.insert("stream".into(), Value::String("stdout".into()));
            let message = (declaration.build)(&fields).unwrap_err();
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
        let message = (LOG_NOT_CONTAINS.build)(&fields).unwrap_err();
        assert!(message.contains("missing field `during`"), "{message}");
        fields.insert("timeout".into(), during);
        let message = (LOG_NOT_CONTAINS.build)(&fields).unwrap_err();
        assert!(message.contains("unknown field `timeout`"), "{message}");
    }

    #[test]
    fn log_steps_require_their_participant_pattern_and_duration() {
        for (declaration, duration) in [(&LOG_CONTAINS, "timeout"), (&LOG_NOT_CONTAINS, "during")] {
            for field in ["on", "pattern", duration] {
                let mut fields = valid_fields(declaration.name);
                fields.remove(field).unwrap();
                let message = (declaration.build)(&fields).unwrap_err();
                assert!(message.contains(&format!("missing field `{field}`")), "{message}");
            }
        }
    }

    #[test]
    fn a_log_step_with_a_process_needs_that_process_and_one_without_needs_its_participant() {
        for declaration in [&LOG_CONTAINS, &LOG_NOT_CONTAINS] {
            let mut fields = valid_fields(declaration.name);
            let step = (declaration.build)(&fields).unwrap();
            assert_eq!(step.needs(), vec![Need::Handle("subject".into())]);
            fields.insert("process".into(), Value::String("worker".into()));
            let step = (declaration.build)(&fields).unwrap();
            assert_eq!(step.needs(), vec![Need::Process("subject".into(), "worker".into())]);
            fields.insert("process".into(), Value::Null);
            let step = (declaration.build)(&fields).unwrap();
            assert_eq!(step.needs(), vec![Need::Handle("subject".into())]);
        }
    }
}
