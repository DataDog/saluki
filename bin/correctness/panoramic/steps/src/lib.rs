//! Steps a case procedure may name, built from their case-file fields.
//!
//! Steps register as [`StepFactory`] values, which the case loader calls to build each step.
//! A step names participants, processes and facts, independently of any particular product or realization.

#![deny(warnings)]
#![deny(missing_docs)]

use panoramic_core::step::StepFactory;

mod file_contains;
mod log_contains;

pub use self::{
    file_contains::FileContains,
    log_contains::{LogContains, LogNotContains},
};

/// Steps available to case files, sorted by name.
pub const REGISTERED_STEPS: &[&dyn StepFactory] = &[
    &file_contains::FileContainsFactory,
    &log_contains::LogContainsFactory,
    &log_contains::LogNotContainsFactory,
];

/// Checks that `pattern` compiles when `regex` is true.
fn check_pattern(pattern: &str, regex: bool) -> Result<(), String> {
    if regex {
        regex::Regex::new(pattern).map_err(|error| format!("pattern '{pattern}' is not a valid regex: {error}"))?;
    }
    Ok(())
}

/// Fields that build each registered step, shared by tests that need a valid step.
#[cfg(any(test, feature = "test-util"))]
pub mod test_support {
    use panoramic_core::step::StepFields;

    use super::{FileContains, LogContains, LogNotContains};

    /// Returns fields that build the registered step `name`, on participant `subject`.
    ///
    /// # Panics
    ///
    /// Panics if `name` is not a registered step.
    pub fn valid_fields(name: &str) -> StepFields {
        let yaml = match name {
            LogContains::NAME => "on: subject\npattern: ready\ntimeout: 30s",
            LogNotContains::NAME => "on: subject\npattern: panic\nduring: 10s",
            FileContains::NAME => "on: subject\npath: /logs/output.log\npattern: ready\ntimeout: 30s",
            other => panic!("missing valid fields for '{other}'"),
        };
        serde_saphyr::from_str(yaml).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::step::{Need, StepFields};
    use serde_json::Value;

    use super::*;
    use crate::test_support::valid_fields;

    #[test]
    fn registered_steps_have_unique_names_in_sorted_order() {
        let names: Vec<_> = REGISTERED_STEPS.iter().map(|factory| factory.name()).collect();
        assert_eq!(names, ["file_contains", "log_contains", "log_not_contains"]);
        assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn each_step_type_name_is_its_registered_factory_name() {
        assert_eq!(
            REGISTERED_STEPS
                .iter()
                .map(|factory| factory.name())
                .collect::<Vec<_>>(),
            [FileContains::NAME, LogContains::NAME, LogNotContains::NAME]
        );
    }

    #[test]
    fn every_registered_step_rejects_an_unknown_field() {
        for factory in REGISTERED_STEPS {
            let mut fields = valid_fields(factory.name());
            let step = factory.build(&fields).unwrap();
            assert_eq!(step.needs(), vec![Need::Handle("subject".into())]);
            fields.insert("unknown".into(), Value::String("x".into()));
            let message = factory.build(&fields).unwrap_err();
            assert!(
                message.contains("unknown field `unknown`"),
                "{}: {message}",
                factory.name()
            );
        }
    }

    #[test]
    fn string_fields_reject_yaml_booleans_and_numbers() {
        for factory in REGISTERED_STEPS {
            let mut string_fields = vec!["on", "pattern"];
            if factory.name() == "file_contains" {
                string_fields.push("path");
            } else {
                string_fields.push("process");
            }
            for field in string_fields {
                for value in ["true", "3"] {
                    let mut fields = valid_fields(factory.name());
                    fields.extend(serde_saphyr::from_str::<StepFields>(&format!("{field}: {value}")).unwrap());
                    let message = factory.build(&fields).unwrap_err();
                    assert!(message.starts_with(&format!("{field}: invalid type:")), "{message}");
                    assert!(message.ends_with("expected a string"), "{message}");
                }
                let mut fields = valid_fields(factory.name());
                fields.extend(serde_saphyr::from_str::<StepFields>(&format!("{field}: \"true\"")).unwrap());
                assert!(factory.build(&fields).is_ok(), "{}: {field}", factory.name());
            }
        }
    }

    #[test]
    fn durations_must_be_strings() {
        for factory in REGISTERED_STEPS {
            let mut fields = valid_fields(factory.name());
            let field = if factory.name() == "log_not_contains" {
                "during"
            } else {
                "timeout"
            };
            for value in ["30", "true"] {
                fields.extend(serde_saphyr::from_str::<StepFields>(&format!("{field}: {value}")).unwrap());
                let message = factory.build(&fields).unwrap_err();
                assert!(message.starts_with(&format!("{field}: invalid type:")), "{message}");
                assert!(message.ends_with("expected a string"), "{message}");
            }
        }
    }

    #[test]
    fn a_regex_pattern_must_compile_and_a_literal_one_need_not() {
        for factory in REGISTERED_STEPS {
            let mut fields = valid_fields(factory.name());
            fields.insert("pattern".into(), Value::String("(".into()));
            fields.insert("regex".into(), Value::Bool(true));
            let message = factory.build(&fields).unwrap_err();
            assert!(message.starts_with("pattern '(' is not a valid regex:"), "{message}");
            fields.insert("regex".into(), Value::Bool(false));
            assert!(factory.build(&fields).is_ok(), "{}", factory.name());
            fields.insert("regex".into(), Value::Bool(true));
            fields.insert("pattern".into(), Value::String("ready|done".into()));
            assert!(factory.build(&fields).is_ok(), "{}", factory.name());
        }
    }

    #[test]
    fn regex_flags_must_be_booleans() {
        for factory in REGISTERED_STEPS {
            let mut fields = valid_fields(factory.name());
            fields.insert("regex".into(), Value::String("true".into()));
            let message = factory.build(&fields).unwrap_err();
            assert!(message.starts_with("regex: invalid type:"), "{message}");
            assert!(message.ends_with("expected a boolean"), "{message}");
        }
    }
}
