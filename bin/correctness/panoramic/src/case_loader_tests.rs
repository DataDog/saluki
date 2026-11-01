//! Loads case files through `CASE_LOADER` with the participant types and steps the binary registers.

use std::path::Path;

use panoramic_core::case::{CaseSchema, Need, Os, ProcedureNode};
use panoramic_steps::{test_support::valid_fields, REGISTERED_STEPS};
use serde_json::Value;

use crate::config::tests::{write_case, PARTICIPANT_FIELDS};

#[test]
fn default_log_file_references_load_on_linux_and_windows_but_not_macos() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = PARTICIPANT_FIELDS.replace("procedure: []", "procedure: [{step: file_contains, on: adp, path: '{{ adp.facts.default_log_file }}', pattern: ready, timeout: 1s}]");
    for oses in ["linux", "windows", "linux, windows"] {
        let path = write_case(dir.path(), "alpha", &yaml.replace("[linux]", &format!("[{oses}]")));
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        let CaseSchema::Participants { procedure, .. } = cases[0].schema() else {
            panic!("expected participant data")
        };
        let [ProcedureNode::Step(step)] = procedure.as_slice() else {
            panic!("expected one step")
        };
        assert_eq!(
            step.needs(),
            [
                Need::Handle("adp".into()),
                Need::Fact("adp".into(), "default_log_file".into())
            ]
        );
    }
    let path = write_case(dir.path(), "alpha", &yaml.replace("[linux]", "[linux, macos, windows]"));
    let message = crate::CASE_LOADER
        .load_file(&path)
        .err()
        .expect("macOS has no declared default log file")
        .to_string();
    assert_eq!(message, format!(
        "case 'alpha' in {}: step 1 'file_contains' references fact 'default_log_file' of participant 'adp', which type 'agent-adp' does not provide on macos; it provides it on linux, windows",
        path.canonicalize().unwrap().display()
    ));
}

#[test]
fn agent_adp_log_steps_may_name_the_adp_and_agent_processes_only() {
    let dir = tempfile::tempdir().unwrap();
    for (step, duration) in [("log_contains", "timeout"), ("log_not_contains", "during")] {
        for process in ["adp", "agent", "core"] {
            let yaml = PARTICIPANT_FIELDS.replace(
                "procedure: []",
                &format!("procedure: [{{step: {step}, on: adp, process: {process}, pattern: ready, {duration}: 1s}}]"),
            );
            let path = write_case(dir.path(), "alpha", &yaml);
            if process == "core" {
                let error = crate::CASE_LOADER
                    .load_file(&path)
                    .err()
                    .expect("undeclared process must fail");
                assert_eq!(
                    error.to_string(),
                    format!(
                        "case 'alpha' in {}: step 1 '{step}' names process 'core' of participant 'adp', which type 'agent-adp' does not run; it runs adp, agent",
                        path.canonicalize().unwrap().display()
                    )
                );
            } else {
                let cases = crate::CASE_LOADER.load_file(&path).unwrap();
                assert_eq!(cases.len(), 1);
                let CaseSchema::Participants { procedure, .. } = cases[0].schema() else {
                    panic!("expected participant data");
                };
                let [ProcedureNode::Step(step)] = procedure.as_slice() else {
                    panic!("expected one built step");
                };
                assert_eq!(step.needs(), vec![Need::Process("adp".into(), process.into())]);
            }
        }
    }
}

#[test]
fn agent_adp_participants_accept_environment_variables_and_os_overlays_only() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = PARTICIPANT_FIELDS.replace(
        "subject: true",
        "subject: true, config: {env: {ANY_KEY: arbitrary}, os: {linux: {env: {ANY_KEY: linux}}}}",
    );
    let path = write_case(dir.path(), "alpha", &yaml);
    let cases = crate::CASE_LOADER.load_file(&path).unwrap();
    assert_eq!(cases.len(), 1);
    let CaseSchema::Participants { file, .. } = cases[0].schema() else {
        panic!("expected participant data");
    };
    let config = &file.participants["adp"].config;
    assert_eq!(config.env["ANY_KEY"], "arbitrary");
    assert_eq!(config.os[&Os::Linux].env["ANY_KEY"], "linux");

    for (fields, expected) in [
        ("config: {files: {settings.yaml: missing.yaml}}", "it declares none"),
        ("config: {endpoints: {api: {port: 80}}}", "it serves none"),
        ("config: {params: {mode: fast}}", "it accepts none"),
        ("connect: {upstream: other.api}", "it consumes none"),
        (
            "config: {os: {linux: {files: {settings.yaml: missing.yaml}}}}",
            "it declares none",
        ),
        ("config: {os: {linux: {params: {mode: fast}}}}", "it accepts none"),
    ] {
        let yaml = PARTICIPANT_FIELDS.replace("subject: true", &format!("subject: true, {fields}"));
        let path = write_case(dir.path(), "alpha", &yaml);
        let error = crate::CASE_LOADER
            .load_file(&path)
            .err()
            .expect("undeclared names must fail");
        let message = error.to_string();
        assert!(
            message.starts_with(&format!("case 'alpha' in {}: ", path.canonicalize().unwrap().display())),
            "{message}"
        );
        assert!(message.contains("which type 'agent-adp' does not"), "{message}");
        assert!(message.ends_with(expected), "{message}");
    }
}

#[test]
fn registered_template_fields_match_step_types_and_other_fields_reject_references() {
    for declaration in REGISTERED_STEPS {
        let mut fields = valid_fields(declaration.name);
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
