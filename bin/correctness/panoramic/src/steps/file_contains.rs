//! A step that searches a file inside a participant for a pattern.

use serde::Deserialize;

use super::check_pattern;
use crate::config::{decode_fields, HumanDuration, Need, Step, StepDeclaration, StepFields, Template};

/// Waits until a file inside a participant contains a pattern.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileContains {
    /// Required participant whose file is searched; has no default.
    pub(crate) on: String,
    /// Required path inside the participant; may reference participant facts. Has no default.
    pub(crate) path: Template,
    /// Required text or regex to find; has no default.
    pub(crate) pattern: String,
    /// Whether `pattern` is a regex. Defaults to false.
    #[serde(default)]
    pub(crate) regex: bool,
    /// Required, positive time to wait for a match; has no default.
    pub(crate) timeout: HumanDuration,
}

impl Step for FileContains {
    fn needs(&self) -> Vec<Need> {
        std::iter::once(Need::Handle(self.on.clone()))
            .chain(
                self.path
                    .references()
                    .map(|(participant, fact)| Need::Fact(participant.into(), fact.into())),
            )
            .collect()
    }
}

/// Builds `file_contains` from its case-file fields.
pub(crate) const FILE_CONTAINS: StepDeclaration = StepDeclaration {
    name: "file_contains",
    template_fields: &["path"],
    build: |fields: &StepFields| {
        let step: FileContains = decode_fields(fields)?;
        check_pattern(&step.pattern, step.regex)?;
        Ok(Box::new(step))
    },
};

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::Value;

    use super::*;
    use crate::steps::tests::fields;

    #[test]
    fn file_contains_rejects_process_and_stream_fields() {
        for field in ["process", "stream"] {
            let mut fields = fields("file_contains");
            fields.insert(field.into(), Value::String("worker".into()));
            let message = (FILE_CONTAINS.build)(&fields).unwrap_err();
            assert!(message.contains(&format!("unknown field `{field}`")), "{message}");
        }
    }

    #[test]
    fn file_contains_requires_on_path_pattern_and_timeout() {
        for field in ["on", "path", "pattern", "timeout"] {
            let mut fields = fields("file_contains");
            fields.remove(field).unwrap();
            let message = (FILE_CONTAINS.build)(&fields).unwrap_err();
            assert!(message.contains(&format!("missing field `{field}`")), "{message}");
        }
    }

    #[test]
    fn file_contains_decodes_its_fields_and_keeps_paths_verbatim() {
        for path in ["/logs/output.log", r"C:\logs\output.log"] {
            let mut fields = fields("file_contains");
            fields.insert("path".into(), Value::String(path.into()));
            let step: FileContains = decode_fields(&fields).unwrap();
            assert_eq!(step.on, "subject");
            assert_eq!(step.path, path.parse().unwrap());
            assert_eq!(step.pattern, "ready");
            assert!(!step.regex);
            assert_eq!(step.timeout.0, Duration::from_secs(30));
        }
    }

    #[test]
    fn file_contains_needs_only_its_participant() {
        let step = (FILE_CONTAINS.build)(&fields("file_contains")).unwrap();
        assert_eq!(step.needs(), vec![Need::Handle("subject".into())]);
    }

    #[test]
    fn file_contains_reports_facts_in_path_order_after_its_participant() {
        let mut fields = fields("file_contains");
        fields.insert(
            "path".into(),
            Value::String("{{ other.facts.dir }}/{{ subject.facts.file }}/{{ other.facts.dir }}".into()),
        );
        let step = (FILE_CONTAINS.build)(&fields).unwrap();
        assert_eq!(
            step.needs(),
            [
                Need::Handle("subject".into()),
                Need::Fact("other".into(), "dir".into()),
                Need::Fact("subject".into(), "file".into()),
                Need::Fact("other".into(), "dir".into()),
            ]
        );
    }
}
