//! Loads case files through `CASE_LOADER` with the participant types and steps the binary registers.

use std::path::Path;

use panoramic_core::case::{CaseSchema, ProcedureNode};
use panoramic_core::layout::Os;
use panoramic_core::step::Need;
use panoramic_steps::{test_support::valid_fields, REGISTERED_STEPS};
use serde_json::Value;

use crate::config::{
    discover_in_scope,
    tests::{minimal_procedure, write_case, PARTICIPANT_FIELDS},
};

#[test]
fn default_log_file_references_plan_on_linux_and_windows_but_not_macos() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = PARTICIPANT_FIELDS.replace(minimal_procedure!(), "procedure: [{step: file_contains, on: adp, path: '{{ adp.facts.default_log_file }}', pattern: ready, timeout: 1s}]");
    for oses in ["linux", "windows", "linux, windows"] {
        let path = write_case(dir.path(), "alpha", &yaml.replace("[linux]", &format!("[{oses}]")));
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        let os = *cases[0].subject_os().first().unwrap();
        let plans = discover_in_scope(&[dir.path().to_path_buf()], os).unwrap();
        assert_eq!(plans.len(), 1);
        let CaseSchema::Participants { procedure, .. } = plans[0].case().schema() else {
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
    assert_eq!(crate::CASE_LOADER.load_file(&path).unwrap().len(), 1);
    let message = discover_in_scope(&[dir.path().to_path_buf()], Os::Linux)
        .err()
        .expect("macOS has no declared default log file")
        .to_string();
    assert_eq!(message, format!(
        "Failed to plan test cases:\n  - case 'alpha' in {}: on subject OS 'macos', step 1 references fact 'default_log_file' of participant 'adp', which type 'agent-adp' does not provide as host-process-group on macos; it provides it as linux-container on linux, windows-container on windows",
        path.canonicalize().unwrap().display()
    ));
}

#[test]
fn agent_adp_cases_cannot_reference_pod_uid() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = PARTICIPANT_FIELDS.replace(
        minimal_procedure!(),
        "procedure: [{step: file_contains, on: adp, path: '{{ adp.late.pod_uid }}', pattern: ready, timeout: 1s}]",
    );
    let path = write_case(dir.path(), "alpha", &yaml);
    assert_eq!(crate::CASE_LOADER.load_file(&path).unwrap().len(), 1);
    let message = discover_in_scope(&[dir.path().to_path_buf()], Os::Linux)
        .err()
        .unwrap()
        .to_string();
    assert_eq!(message, format!(
        "Failed to plan test cases:\n  - case 'alpha' in {}: on subject OS 'linux', step 1 references late fact 'pod_uid' of participant 'adp', which a linux-container does not have; a linux-container has container_id, ip",
        path.canonicalize().unwrap().display()
    ));
}

#[test]
fn agent_adp_log_steps_may_name_the_adp_and_agent_processes_only() {
    let dir = tempfile::tempdir().unwrap();
    for (step, duration) in [("log_contains", "timeout"), ("log_not_contains", "during")] {
        for process in ["adp", "agent", "core"] {
            let yaml = PARTICIPANT_FIELDS.replace(
                minimal_procedure!(),
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
        ("config: {params: {mode: fast}}", "it accepts none"),
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
fn agent_adp_participants_accept_no_endpoint_overrides_or_connections() {
    let dir = tempfile::tempdir().unwrap();
    for (fields, expected) in [
        (
            "config: {endpoints: {api: {port: 80}}}",
            "overrides endpoint 'api', which it does not serve; it serves none",
        ),
        (
            "connect: {upstream: other.api}",
            "connects 'upstream', which it does not consume; it consumes none",
        ),
    ] {
        let yaml = PARTICIPANT_FIELDS
            .replace("[linux]", "[linux, macos, windows]")
            .replace("participants:", "participants:\n  other: {type: agent-adp}")
            .replace("subject: true", &format!("subject: true, {fields}"));
        let path = write_case(dir.path(), "alpha", &yaml);
        assert_eq!(crate::CASE_LOADER.load_file(&path).unwrap().len(), 1);
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let message = discover_in_scope(&[dir.path().to_path_buf()], os)
                .err()
                .expect("undeclared endpoints must fail planning")
                .to_string();
            assert_eq!(message, format!(
                "Failed to plan test cases:\n  - case 'alpha' in {}: on subject OS 'linux', participant 'adp' of type 'agent-adp' as linux-container on linux {expected}",
                path.canonicalize().unwrap().display(),
            ));
        }
    }
}

#[test]
fn registered_template_fields_match_step_types_and_other_fields_reject_references() {
    for factory in REGISTERED_STEPS {
        let mut fields = valid_fields(factory.name());
        fields.insert("on".into(), Value::String("adp".into()));
        if factory.name() != "file_contains" {
            fields.insert("process".into(), Value::String("agent".into()));
        }
        assert!(factory
            .template_fields()
            .iter()
            .all(|field| fields.contains_key(*field)));
        for field in fields
            .keys()
            .filter(|field| !matches!(field.as_str(), "during" | "timeout"))
        {
            let mut referenced = fields.clone();
            referenced.insert(field.clone(), Value::String("{{ adp.facts.default_log_file }}".into()));
            let step = factory.build(&referenced).unwrap();
            let declared = factory.template_fields().contains(&field.as_str());
            assert_eq!(
                step.needs()
                    .contains(&Need::Fact("adp".into(), "default_log_file".into())),
                declared,
                "{}: {field}",
                factory.name()
            );
            referenced.insert("step".into(), Value::String(factory.name().into()));
            let mut file: Value = serde_saphyr::from_str(PARTICIPANT_FIELDS).unwrap();
            file["procedure"] = serde_json::json!([referenced]);
            let result = crate::CASE_LOADER.load_content(&file.to_string(), "alpha", Path::new("config.yaml"));
            if declared {
                let cases = result.unwrap_or_else(|error| panic!("{}: {field}: {error}", factory.name()));
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
                let accepted = if factory.template_fields().is_empty() {
                    "none".to_string()
                } else {
                    factory.template_fields().join(", ")
                };
                assert_eq!(result.err().expect("undeclared template field must fail").to_string(), format!(
                    "case 'alpha' in config.yaml: step 1 '{}': field '{field}' contains a template reference ('{{{{'); '{}' accepts references only in: {}",
                    factory.name(), factory.name(), accepted
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
            "'file_contains': path: '{{ adp.endpoints.api.url }}': step fields accept only fact and late fact references",
        ),
        (
            "step: file_contains, on: adp, path: '{{PANORAMIC_DYNAMIC_PATH}}', pattern: ready, timeout: 1s",
            "'file_contains': path: '{{PANORAMIC_DYNAMIC_PATH}}' is not a template reference",
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
            &PARTICIPANT_FIELDS.replace(minimal_procedure!(), &format!("procedure: [{{{fields}}}]")),
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

#[test]
fn agent_adp_env_placeholders_from_the_integration_schema_fail_at_load() {
    for (env, reason) in [
        (
            "{DD_BIND_HOST: '{{PANORAMIC_DYNAMIC_IP}}'}",
            "'DD_BIND_HOST' '{{PANORAMIC_DYNAMIC_IP}}' is not a template reference; write '{{ <participant>.facts.<fact> }}', '{{ <participant>.late.<fact> }}', '{{ <participant>.endpoints.<endpoint>.<part> }}' or '{{ self.connections.<endpoint>.<part> }}'",
        ),
        (
            "{'{{name}}': literal}",
            "'{{name}}': environment variable names can't contain template references",
        ),
    ] {
        for (config, prefix) in [
            (format!("{{env: {env}}}"), "config.env"),
            (format!("{{os: {{windows: {{env: {env}}}}}}}"), "config.os.windows.env"),
        ] {
            let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]").replace("subject: true", &format!("subject: true, config: {config}"));
            assert_eq!(crate::CASE_LOADER.load_content(&yaml, "alpha", Path::new("alpha/config.yaml")).err().unwrap().to_string(),
                format!("case 'alpha' in alpha/config.yaml: participant 'adp' {prefix} {reason}"));
        }
    }
}

#[test]
fn agent_adp_default_log_file_in_env_plans_on_linux_and_windows_but_not_macos() {
    use panoramic_core::plan::{CasePlan, PortRegistry, ScopeRegistry};

    let yaml = PARTICIPANT_FIELDS
        .replace(
            "subject: true",
            "subject: true, config: {env: {FILE: '{{ self.facts.default_log_file }}'}}",
        )
        .replace("[linux]", "[linux, windows]");
    for (os, expected) in [
        (Os::Linux, "/var/log/datadog/agent-data-plane.log"),
        (Os::Windows, r"C:\ProgramData\Datadog\logs\agent-data-plane.log"),
    ] {
        let case = crate::CASE_LOADER
            .load_content(&yaml, "alpha", Path::new("alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let plan = CasePlan::compile(
            case,
            os,
            &crate::REGISTRIES,
            &mut PortRegistry::new(),
            &mut ScopeRegistry::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.effective_config("adp").unwrap().env["FILE"], expected);
    }
    let case = crate::CASE_LOADER
        .load_content(
            &yaml.replace("[linux, windows]", "[linux, macos, windows]"),
            "alpha",
            Path::new("alpha/config.yaml"),
        )
        .unwrap()
        .remove(0);
    assert_eq!(CasePlan::compile(case, Os::Linux, &crate::REGISTRIES, &mut PortRegistry::new(), &mut ScopeRegistry::new()).err().unwrap().to_string(),
        "case 'alpha' in alpha/config.yaml: on subject OS 'macos', participant 'adp' of type 'agent-adp' as host-process-group on macos environment variable 'FILE' '{{ self.facts.default_log_file }}' references fact 'default_log_file' of participant 'adp', which type 'agent-adp' does not provide as host-process-group on macos; it provides it as linux-container on linux, windows-container on windows");
}

#[test]
fn agent_adp_cannot_use_its_own_ip_in_env_in_containers() {
    use panoramic_core::plan::{CasePlan, PortRegistry, ScopeRegistry};

    for (os, kind) in [("linux", "linux-container"), ("windows", "windows-container")] {
        for name in ["self", "adp"] {
            let yaml = PARTICIPANT_FIELDS.replace("[linux]", &format!("[{os}]")).replace(
                "subject: true",
                &format!("subject: true, config: {{env: {{IP: '{{{{{name}.late.ip}}}}'}}}}"),
            );
            let case = crate::CASE_LOADER
                .load_content(&yaml, "alpha", Path::new("alpha/config.yaml"))
                .unwrap()
                .remove(0);
            assert_eq!(CasePlan::compile(case, Os::Linux, &crate::REGISTRIES, &mut PortRegistry::new(), &mut ScopeRegistry::new()).err().unwrap().to_string(), format!(
                "case 'alpha' in alpha/config.yaml: on subject OS '{os}', participant 'adp' of type 'agent-adp' as {kind} on {os} environment variable 'IP' '{{{{{name}.late.ip}}}}' references its own late fact 'ip', which type 'agent-adp' does not substitute into its environment as {kind}"
            ));
        }
    }
}
