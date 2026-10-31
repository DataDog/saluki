//! Steps a case procedure may name, and the registry the loader builds procedures from.

use crate::config::StepDeclaration;

pub(crate) mod file_contains;
pub(crate) mod log_contains;

/// Steps available to case files, sorted by name.
pub(crate) const REGISTERED_STEPS: &[&StepDeclaration] = &[
    &file_contains::FILE_CONTAINS,
    &log_contains::LOG_CONTAINS,
    &log_contains::LOG_NOT_CONTAINS,
];

/// Checks that `pattern` compiles when `regex` is true.
fn check_pattern(pattern: &str, regex: bool) -> Result<(), String> {
    if regex {
        regex::Regex::new(pattern).map_err(|error| format!("pattern '{pattern}' is not a valid regex: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::Value;

    use super::*;
    use crate::config::{Need, StepFields};

    pub(crate) fn fields(name: &str) -> StepFields {
        let yaml = match name {
            "log_contains" => "on: subject\npattern: ready\ntimeout: 30s",
            "log_not_contains" => "on: subject\npattern: panic\nduring: 10s",
            "file_contains" => "on: subject\npath: /logs/output.log\npattern: ready\ntimeout: 30s",
            other => panic!("missing valid fields for '{other}'"),
        };
        serde_saphyr::from_str(yaml).unwrap()
    }

    #[test]
    fn registered_steps_have_unique_names_in_sorted_order() {
        let names: Vec<_> = REGISTERED_STEPS.iter().map(|step| step.name).collect();
        assert_eq!(names, ["file_contains", "log_contains", "log_not_contains"]);
        assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn every_registered_step_rejects_an_unknown_field() {
        for declaration in REGISTERED_STEPS {
            let mut fields = fields(declaration.name);
            let step = (declaration.build)(&fields).unwrap();
            assert_eq!(step.needs(), vec![Need::Handle("subject".into())]);
            fields.insert("unknown".into(), Value::String("x".into()));
            let message = (declaration.build)(&fields).unwrap_err();
            assert!(
                message.contains("unknown field `unknown`"),
                "{}: {message}",
                declaration.name
            );
        }
    }

    #[test]
    fn string_fields_reject_yaml_booleans_and_numbers() {
        for declaration in REGISTERED_STEPS {
            let mut string_fields = vec!["on", "pattern"];
            if declaration.name == "file_contains" {
                string_fields.push("path");
            } else {
                string_fields.push("process");
            }
            for field in string_fields {
                for value in ["true", "3"] {
                    let mut fields = fields(declaration.name);
                    fields.extend(serde_saphyr::from_str::<StepFields>(&format!("{field}: {value}")).unwrap());
                    let message = (declaration.build)(&fields).unwrap_err();
                    assert!(message.starts_with(&format!("{field}: invalid type:")), "{message}");
                    assert!(message.ends_with("expected a string"), "{message}");
                }
                let mut fields = fields(declaration.name);
                fields.extend(serde_saphyr::from_str::<StepFields>(&format!("{field}: \"true\"")).unwrap());
                assert!((declaration.build)(&fields).is_ok(), "{}: {field}", declaration.name);
            }
        }
    }

    #[test]
    fn durations_must_be_strings() {
        for declaration in REGISTERED_STEPS {
            let mut fields = fields(declaration.name);
            let field = if declaration.name == "log_not_contains" {
                "during"
            } else {
                "timeout"
            };
            for value in ["30", "true"] {
                fields.extend(serde_saphyr::from_str::<StepFields>(&format!("{field}: {value}")).unwrap());
                let message = (declaration.build)(&fields).unwrap_err();
                assert!(message.starts_with(&format!("{field}: invalid type:")), "{message}");
                assert!(message.ends_with("expected a string"), "{message}");
            }
        }
    }

    #[test]
    fn a_regex_pattern_must_compile_and_a_literal_one_need_not() {
        for declaration in REGISTERED_STEPS {
            let mut fields = fields(declaration.name);
            fields.insert("pattern".into(), Value::String("(".into()));
            fields.insert("regex".into(), Value::Bool(true));
            let message = (declaration.build)(&fields).unwrap_err();
            assert!(message.starts_with("pattern '(' is not a valid regex:"), "{message}");
            fields.insert("regex".into(), Value::Bool(false));
            assert!((declaration.build)(&fields).is_ok(), "{}", declaration.name);
            fields.insert("regex".into(), Value::Bool(true));
            fields.insert("pattern".into(), Value::String("ready|done".into()));
            assert!((declaration.build)(&fields).is_ok(), "{}", declaration.name);
        }
    }

    #[test]
    fn regex_flags_must_be_booleans() {
        for declaration in REGISTERED_STEPS {
            let mut fields = fields(declaration.name);
            fields.insert("regex".into(), Value::String("true".into()));
            let message = (declaration.build)(&fields).unwrap_err();
            assert!(message.starts_with("regex: invalid type:"), "{message}");
            assert!(message.ends_with("expected a boolean"), "{message}");
        }
    }
}
