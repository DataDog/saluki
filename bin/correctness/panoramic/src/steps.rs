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
    use std::path::Path;

    use serde_json::Value;

    use super::*;
    use crate::config::{
        case::CaseSchema,
        tests::{write_case, PARTICIPANT_FIELDS},
        Need, ProcedureNode, StepFields,
    };

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

    #[test]
    fn registered_template_fields_match_step_types_and_other_fields_reject_references() {
        for declaration in REGISTERED_STEPS {
            let mut fields = fields(declaration.name);
            fields.insert("on".into(), Value::String("adp".into()));
            if declaration.name != "file_contains" {
                fields.insert("process".into(), Value::String("agent".into()));
            }
            assert!(declaration
                .template_fields
                .iter()
                .all(|field| fields.contains_key(*field)));
            for field in fields
                .keys()
                .filter(|field| !matches!(field.as_str(), "during" | "timeout"))
            {
                let mut referenced = fields.clone();
                referenced.insert(field.clone(), Value::String("{{ adp.facts.default_log_file }}".into()));
                let step = (declaration.build)(&referenced).unwrap();
                let declared = declaration.template_fields.contains(&field.as_str());
                assert_eq!(
                    step.needs()
                        .contains(&Need::Fact("adp".into(), "default_log_file".into())),
                    declared,
                    "{}: {field}",
                    declaration.name
                );
                referenced.insert("step".into(), Value::String(declaration.name.into()));
                let mut file: Value = serde_saphyr::from_str(PARTICIPANT_FIELDS).unwrap();
                file["procedure"] = serde_json::json!([referenced]);
                let result = crate::CASE_LOADER.load_content(&file.to_string(), "alpha", Path::new("config.yaml"));
                if declared {
                    let cases = result.unwrap_or_else(|error| panic!("{}: {field}: {error}", declaration.name));
                    let CaseSchema::Participants { procedure, .. } = cases[0].schema() else {
                        panic!("expected participant data");
                    };
                    let [ProcedureNode::Step(step)] = procedure.as_slice() else {
                        panic!("expected one built step");
                    };
                    assert!(step
                        .needs()
                        .contains(&Need::Fact("adp".into(), "default_log_file".into())));
                } else {
                    let accepted = if declaration.template_fields.is_empty() {
                        "none".to_string()
                    } else {
                        declaration.template_fields.join(", ")
                    };
                    assert_eq!(result.err().expect("undeclared template field must fail").to_string(), format!(
                        "case 'alpha' in config.yaml: step 1 '{}': field '{field}' contains a template reference ('{{{{'); '{}' accepts references only in: {}",
                        declaration.name, declaration.name, accepted
                    ));
                }
            }
        }
    }

    #[test]
    fn participant_step_errors_name_the_canonical_file_and_step() {
        let dir = tempfile::tempdir().unwrap();
        for (fields, expected) in [
            (
                "step: wait_ready",
                "names unknown step 'wait_ready'; registered steps: file_contains, log_contains, log_not_contains",
            ),
            (
                "step: adp_cli, on: adp, args: [status]",
                "names unknown step 'adp_cli'; registered steps: file_contains, log_contains, log_not_contains",
            ),
            (
                "step: port_listening, on: adp, port: 58125, protocol: udp, timeout: 1s",
                "names unknown step 'port_listening'; registered steps: file_contains, log_contains, log_not_contains",
            ),
            (
                "step: file_contains, on: adp, path: /logs/output.log, pattern: null, timeout: 1s",
                "'file_contains': pattern: invalid type: null, expected a string",
            ),
            (
                "step: log_contains, on: adp, pattern: ready, timeout: 1s, unknown: x",
                "'log_contains': unknown: unknown field `unknown`",
            ),
            ("step: log_contains, on: adp, timeout: 1s", "missing field `pattern`"),
            (
                "step: log_contains, on: adp, pattern: true, timeout: 1s",
                "'log_contains': pattern: invalid type: boolean `true`, expected a string",
            ),
            (
                "step: log_contains, on: adp, pattern: ready, timeout: 30",
                "'log_contains': timeout: invalid type: integer `30`, expected a string",
            ),
            (
                "step: log_contains, on: adp, pattern: ready, timeout: '30'",
                "'log_contains': timeout: duration '30' ends with",
            ),
            (
                "step: log_not_contains, on: adp, pattern: panic, during: '30'",
                "'log_not_contains': during: duration '30' ends with",
            ),
            (
                "step: file_contains, on: adp, path: /logs/output.log, pattern: ready, timeout: '30'",
                "'file_contains': timeout: duration '30' ends with",
            ),
            (
                "step: log_contains, on: adp, pattern: '(', regex: true, timeout: 1s",
                "'log_contains': pattern '(' is not a valid regex",
            ),
            (
                "step: file_contains, on: adp, path: /logs/output.log, pattern: '{{ adp.facts.default_log_file }}', timeout: 1s",
                "'file_contains': field 'pattern' contains a template reference",
            ),
            (
                "step: file_contains, on: adp, path: '{{ self.facts.default_log_file }}', pattern: ready, timeout: 1s",
                "'file_contains': path: '{{ self.facts.default_log_file }}': a step has no 'self'; name the participant",
            ),
            (
                "step: file_contains, on: adp, path: '{{ adp.endpoints.api.url }}', pattern: ready, timeout: 1s",
                "'file_contains': path: '{{ adp.endpoints.api.url }}' is not a fact reference",
            ),
            (
                "step: file_contains, on: adp, path: '{{PANORAMIC_DYNAMIC_PATH}}', pattern: ready, timeout: 1s",
                "'file_contains': path: '{{PANORAMIC_DYNAMIC_PATH}}' is not a fact reference",
            ),
            (
                "step: log_contains, on: missing, pattern: ready, timeout: 1s",
                "'log_contains' runs on unknown participant 'missing'; participants: adp",
            ),
            (
                "step: log_contains, on: agent, pattern: ready, timeout: 1s",
                "'log_contains' runs on unknown participant 'agent'; participants: adp",
            ),
            (
                "step: log_contains, on: adp, pattern: ready, timeout: 1s, stream: stdout",
                "'log_contains': stream: unknown field `stream`",
            ),
            (
                "step: file_contains, on: adp, path: /logs/output.log, pattern: ready, timeout: 1s, process: adp",
                "unknown field `process`",
            ),
        ] {
            let path = write_case(
                dir.path(),
                "alpha",
                &PARTICIPANT_FIELDS.replace("procedure: []", &format!("procedure: [{{{fields}}}]")),
            );
            let error = crate::CASE_LOADER.load_file(&path).err().expect("invalid step must fail");
            let message = error.to_string();
            assert!(
                message.starts_with(&format!(
                    "case 'alpha' in {}: step 1 ",
                    path.canonicalize().unwrap().display()
                )),
                "{message}"
            );
            assert!(message.contains(expected), "expected {expected}: {message}");
        }
    }
}
