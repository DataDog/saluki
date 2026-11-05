//! Loads case files through `CASE_LOADER` with the participant types and steps the binary registers.

use std::path::Path;

use panoramic_core::case::{CaseSchema, ProcedureNode};
use panoramic_core::layout::{Os, Realization, RealizationKind};
use panoramic_core::step::Need;
use panoramic_steps::{test_support::valid_fields, REGISTERED_STEPS};
use serde_json::Value;

use crate::config::{
    discover_in_scope,
    tests::{minimal_procedure, planner, write_case, PARTICIPANT_FIELDS},
};

#[test]
fn default_log_file_references_plan_on_linux_and_windows_but_not_macos() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = PARTICIPANT_FIELDS.replace(minimal_procedure!(), "procedure: [{step: file_contains, on: adp, path: '{{ adp.facts.default_log_file }}', pattern: ready, timeout: 1s}]");
    for oses in ["linux", "windows", "linux, windows"] {
        let path = write_case(dir.path(), "alpha", &yaml.replace("[linux]", &format!("[{oses}]")));
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        let os = *cases[0].subject_os().first().unwrap();
        let plans = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], os)
            .unwrap()
            .plans;
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
    let message = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], Os::Linux)
        .err()
        .expect("macOS has no declared default log file")
        .to_string();
    assert_eq!(message, format!(
        "Failed to plan test cases:\n  - case 'alpha' in {}: on subject OS 'macos', step 1 'file_contains' references fact 'default_log_file' of participant 'adp', which type 'agent-adp' does not provide as host-process-group on macos; it provides it as linux-container on linux, windows-container on windows",
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
    let message = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], Os::Linux)
        .err()
        .unwrap()
        .to_string();
    assert_eq!(message, format!(
        "Failed to plan test cases:\n  - case 'alpha' in {}: on subject OS 'linux', step 1 'file_contains' references late fact 'pod_uid' of participant 'adp', which a linux-container does not have; a linux-container has container_id, ip",
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
fn agent_adp_participants_accept_environment_overlays_and_only_declared_file_names() {
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
        (
            "config: {files: {settings.yaml: missing.yaml}}",
            "it declares cont-init.d/, files/",
        ),
        ("config: {params: {mode: fast}}", "it accepts extra_hosts"),
        (
            "config: {os: {linux: {files: {settings.yaml: missing.yaml}}}}",
            "it declares cont-init.d/, files/",
        ),
        (
            "config: {os: {linux: {params: {mode: fast}}}}",
            "it accepts extra_hosts",
        ),
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
fn agent_adp_rejects_overriding_derived_ports_and_undeclared_connections() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = PARTICIPANT_FIELDS
        .replace("participants:", "participants:\n  intake: {type: datadog-intake}")
        .replace("subject: true", "subject: true, connect: {intake: intake.api}");
    write_case(dir.path(), "alpha", &yaml);
    let plans = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], Os::Linux)
        .unwrap()
        .plans;
    assert_eq!(plans.len(), 1);
    let env = &plans[0].effective_config("adp").unwrap().env;
    assert_eq!(env["DD_DD_URL"], "http://intake:2049");
    assert_eq!(env["DD_APM_DD_URL"], "http://intake:2049");
    for (fields, expected) in [
        (
            "config: {endpoints: {agent_cmd: {port: 80}}}",
            "overrides endpoint 'agent_cmd', whose port its type derives from its configuration",
        ),
        (
            "connect: {upstream: other.api}",
            "connects 'upstream', which it does not consume; it consumes docker_api, dogstatsd_forward, intake",
        ),
    ] {
        let yaml = PARTICIPANT_FIELDS
            .replace("[linux]", "[linux, macos, windows]")
            .replace("participants:", "participants:\n  other: {type: agent-adp}")
            .replace("subject: true", &format!("subject: true, {fields}"));
        let path = write_case(dir.path(), "alpha", &yaml);
        assert_eq!(crate::CASE_LOADER.load_file(&path).unwrap().len(), 1);
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let message = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], os)
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
fn intake_cases_plan_with_subject_realizations_urls_and_artifact_roles_on_every_os() {
    let cases_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../test/integration/cases");
    for name in ["dogstatsd-host-tag", "dogstatsd-intake-metric"] {
        for (os, kind, roles) in [
            (
                Os::Linux,
                RealizationKind::LinuxContainer,
                &[
                    "agent-adp-linux-image",
                    "courier-linux-binary",
                    "fixup-image",
                    "tools-linux-image",
                ][..],
            ),
            (
                Os::MacOs,
                RealizationKind::HostProcessGroup,
                &["adp-host-binary", "agent-host-install", "intake-host-binary"][..],
            ),
            (
                Os::Windows,
                RealizationKind::WindowsContainer,
                &[
                    "agent-adp-windows-image",
                    "courier-windows-binary",
                    "tools-windows-image",
                ][..],
            ),
        ] {
            let case = crate::CASE_LOADER
                .load_file(&cases_dir.join(name).join("config.yaml"))
                .unwrap()
                .remove(0);
            assert_eq!(
                case.subject_os().iter().copied().collect::<Vec<_>>(),
                [Os::Linux, Os::MacOs, Os::Windows],
                "{name}"
            );
            let plan = planner().compile(case, os).unwrap().into_plan().unwrap();
            assert_eq!(
                plan.layout().unwrap().iter().collect::<Vec<_>>(),
                [("adp", Realization { os, kind }), ("intake", Realization { os, kind })],
                "{name} on {os:?}"
            );
            let expected_url = if os == Os::MacOs {
                let listen = plan.effective_config("intake").unwrap().env["INTAKE_HTTP_LISTEN_ADDR"]
                    .parse::<std::net::SocketAddr>()
                    .unwrap();
                assert_eq!(listen.ip(), std::net::Ipv4Addr::LOCALHOST);
                assert!((20000..30000).contains(&listen.port()), "{name}: {listen}");
                format!("http://{listen}")
            } else {
                "http://intake:2049".to_string()
            };
            let env = &plan.effective_config("adp").unwrap().env;
            assert_eq!(env["DD_DD_URL"], expected_url, "{name} on {os:?}");
            assert_eq!(env["DD_APM_DD_URL"], expected_url, "{name} on {os:?}");
            assert_eq!(
                plan.artifacts()
                    .iter()
                    .map(|binding| binding.role.name)
                    .collect::<Vec<_>>(),
                roles,
                "{name} on {os:?}"
            );
        }
    }
}

#[test]
fn environment_only_cases_plan_natively_with_their_exit_and_api_needs_on_every_os() {
    use panoramic_core::endpoint::EndpointOperation;

    let cases_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../test/integration/cases");
    for (name, exits, requests, probes) in [
        ("adp-cmd-port", 0, 0, 0),
        ("adp-config-check-exit", 1, 0, 0),
        ("adp-config-check-no-warn", 0, 0, 0),
        ("adp-config-check-warn", 0, 0, 0),
        ("adp-disabled-exit", 1, 0, 0),
        ("adp-memory-mode-disabled", 0, 0, 0),
        ("adp-memory-mode-permissive-exceeds-limit", 0, 0, 0),
        ("adp-memory-mode-permissive-within-limit", 0, 0, 0),
        ("adp-memory-mode-strict-exceeds-limit", 1, 0, 0),
        ("adp-memory-mode-strict-within-limit", 0, 0, 0),
        ("basic-startup", 0, 0, 0),
        ("dogstatsd-autoscale-udp", 0, 0, 0),
        ("dogstatsd-enabled", 0, 0, 0),
        ("telemetry-endpoint", 0, 2, 1),
        ("unprivileged-api-endpoints", 0, 3, 0),
    ] {
        for (os, kind) in [
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::MacOs, RealizationKind::HostProcessGroup),
            (Os::Windows, RealizationKind::WindowsContainer),
        ] {
            let case = crate::CASE_LOADER
                .load_file(&cases_dir.join(name).join("config.yaml"))
                .unwrap()
                .remove(0);
            assert_eq!(
                case.subject_os().iter().copied().collect::<Vec<_>>(),
                [Os::Linux, Os::MacOs, Os::Windows],
                "{name}"
            );
            let plan = planner().compile(case, os).unwrap().into_plan().unwrap();
            assert_eq!(
                plan.layout().unwrap().iter().collect::<Vec<_>>(),
                [("adp", Realization { os, kind })],
                "{name} on {os:?}"
            );
            let CaseSchema::Participants { procedure, .. } = plan.case().schema() else {
                panic!("{name} must use native participants");
            };
            let steps: Vec<_> = procedure
                .iter()
                .flat_map(|node| match node {
                    ProcedureNode::Step(step) => std::slice::from_ref(step),
                    ProcedureNode::Parallel(steps) => steps.as_slice(),
                })
                .collect();
            for (step_name, count, need) in [
                ("exits_with", exits, Need::Process("adp".into(), "adp".into())),
                (
                    "http_check",
                    requests,
                    Need::Endpoint("adp".into(), "api".into(), EndpointOperation::Request),
                ),
                (
                    "port_listening",
                    probes,
                    Need::Endpoint("adp".into(), "api".into(), EndpointOperation::Probe),
                ),
            ] {
                let matching: Vec<_> = steps.iter().filter(|step| step.name() == step_name).collect();
                assert_eq!(matching.len(), count, "{name} on {os:?}: {step_name}");
                for step in matching {
                    assert_eq!(
                        step.needs().as_slice(),
                        std::slice::from_ref(&need),
                        "{name} on {os:?}: {step_name}"
                    );
                }
            }
            if name == "adp-cmd-port" {
                assert_eq!(plan.effective_config("adp").unwrap().env["DD_CMD_PORT"], "7777");
            }
        }
    }
}

#[test]
fn registered_template_fields_match_step_types_and_other_fields_reject_references() {
    for factory in REGISTERED_STEPS {
        let mut fields = valid_fields(factory.name());
        fields.insert("on".into(), Value::String("adp".into()));
        if matches!(
            factory.name(),
            "exits_with" | "log_contains" | "log_not_contains" | "process_stable_for"
        ) {
            fields.insert("process".into(), Value::String("agent".into()));
        }
        assert!(factory
            .template_fields()
            .iter()
            .all(|field| fields.contains_key(*field)));
        for field in fields.keys().filter(|field| {
            (fields[*field].is_string() || field.as_str() == "argv")
                && !matches!(
                    field.as_str(),
                    "duration" | "during" | "timeout" | "capture_duration" | "stats_duration"
                )
        }) {
            let mut referenced = fields.clone();
            let reference = if field == "path" {
                "/{{ adp.facts.default_log_file }}"
            } else {
                "{{ adp.facts.default_log_file }}"
            };
            referenced.insert(
                field.clone(),
                if field == "argv" {
                    serde_json::json!(["tool", reference])
                } else {
                    Value::String(reference.into())
                },
            );
            let declared = factory.template_fields().contains(&field.as_str());
            let decode_error = if factory.name() == "config_key_equals" && field == "view" {
                let message = factory.build(&referenced).unwrap_err();
                assert_eq!(
                    message,
                    "view: unknown variant `{{ adp.facts.default_log_file }}`, expected `source` or `runtime`"
                );
                assert!(!declared);
                Some(message)
            } else {
                let step = factory.build(&referenced).unwrap();
                assert_eq!(
                    step.needs()
                        .contains(&Need::Fact("adp".into(), "default_log_file".into())),
                    declared,
                    "{}: {field}",
                    factory.name()
                );
                None
            };
            referenced.insert("step".into(), Value::String(factory.name().into()));
            let mut file: Value = serde_saphyr::from_str(PARTICIPANT_FIELDS).unwrap();
            file["procedure"] = serde_json::json!([referenced]);
            let result = crate::CASE_LOADER.load_content(&file.to_string(), "alpha", Path::new("config.yaml"));
            if let Some(message) = decode_error {
                assert_eq!(
                    result.err().expect("invalid view must fail").to_string(),
                    format!("case 'alpha' in config.yaml: step 1 '{}': {message}", factory.name())
                );
            } else if declared {
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
            "names unknown step 'wait_ready'; registered steps: cli, config_key_equals, config_set, dogstatsd_replay, exec, exits_with, file_contains, http_check, intake_has_forwarded_packets, intake_has_metric, intake_has_service_check, log_contains, log_not_contains, port_listening, process_stable_for, send",
        ),
        (
            "step: adp_cli, on: adp, args: [status]",
            "names unknown step 'adp_cli'; registered steps: cli, config_key_equals, config_set, dogstatsd_replay, exec, exits_with, file_contains, http_check, intake_has_forwarded_packets, intake_has_metric, intake_has_service_check, log_contains, log_not_contains, port_listening, process_stable_for, send",
        ),
        (
            "step: port_listening, on: adp, timeout: 1s",
            "missing field `endpoint`",
        ),
        (
            "step: config_key_equals, on: adp, key: domains.enabled, view: runtime, timeout: 1s",
            "missing field `value`",
        ),
        (
            "step: config_key_equals, on: adp, key: domains.enabled, value: null, timeout: 1s",
            "missing field `view`",
        ),
        (
            "step: config_key_equals, on: adp, key: domains.enabled, value: null, view: combined, timeout: 1s",
            "'config_key_equals': view: unknown variant `combined`, expected `source` or `runtime`",
        ),
        (
            "step: config_key_equals, on: missing, key: domains.enabled, value: null, view: source, timeout: 1s",
            "'config_key_equals' calls operation 'config_dump' of unknown participant 'missing'; participants: adp",
        ),
        (
            "step: config_key_equals, on: adp, key: domains.enabled, value: {nested: ['{{ adp.facts.default_log_file }}']}, view: source, timeout: 1s",
            "'config_key_equals': field 'value' contains a template reference",
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
        let plan = planner().compile(case, os).unwrap().into_plan().unwrap();
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
    assert_eq!(planner().compile(case, Os::Linux).err().unwrap().to_string(),
        "case 'alpha' in alpha/config.yaml: on subject OS 'macos', participant 'adp' of type 'agent-adp' as host-process-group on macos environment variable 'FILE' '{{ self.facts.default_log_file }}' references fact 'default_log_file' of participant 'adp', which type 'agent-adp' does not provide as host-process-group on macos; it provides it as linux-container on linux, windows-container on windows");
}

#[test]
fn agent_adp_accepts_and_canonicalizes_its_own_ip_in_container_env() {
    for os in [Os::Linux, Os::Windows] {
        for reference in [
            "{{self.late.ip}}",
            "{{adp.late.ip}}",
            "{{ self.late.ip }}",
            "{{  adp.late.ip  }}",
        ] {
            let yaml = PARTICIPANT_FIELDS
                .replace("[linux]", &format!("[{}]", os.name()))
                .replace(
                    "subject: true",
                    &format!("subject: true, config: {{env: {{IP: '{reference}'}}}}"),
                );
            let case = crate::CASE_LOADER
                .load_content(&yaml, "alpha", Path::new("alpha/config.yaml"))
                .unwrap()
                .remove(0);
            let plan = planner().compile(case, os).unwrap().into_plan().unwrap();
            assert_eq!(plan.case_values("adp").unwrap().env["IP"], "{{ self.late.ip }}");
            assert_eq!(plan.effective_config("adp").unwrap().env["IP"], "{{ self.late.ip }}");
        }
    }
}

#[test]
fn agent_adp_rejects_every_spelling_of_its_own_ip_in_an_adp_override() {
    for reference in [
        "{{self.late.ip}}",
        "{{adp.late.ip}}",
        "{{ self.late.ip }}",
        "{{  adp.late.ip  }}",
    ] {
        let yaml = PARTICIPANT_FIELDS.replace(
            "subject: true",
            &format!("subject: true, config: {{env: {{ADP_DD_BIND_HOST: '{reference}'}}}}"),
        );
        let case = crate::CASE_LOADER
            .load_content(&yaml, "alpha", Path::new("alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let error = planner().compile(case, Os::Linux).err().unwrap().to_string();
        assert!(
            error.contains("ADP_DD_BIND_HOST cannot reference its own ip"),
            "{error}"
        );
        assert!(error.contains("use DD_BIND_HOST instead"), "{error}");
    }
}

#[test]
fn forwarding_case_frames_and_telemetry_match_their_wire_contracts() {
    let case = crate::CASE_LOADER
        .load_content(
            include_str!("../../../../test/integration/cases/dogstatsd-forwarding/config.yaml"),
            "dogstatsd-forwarding",
            Path::new("test/integration/cases/dogstatsd-forwarding/config.yaml"),
        )
        .unwrap()
        .remove(0);
    let CaseSchema::Participants { file, .. } = case.schema() else {
        panic!("forwarding declares participants");
    };
    let steps: Vec<_> = file
        .procedure
        .iter()
        .filter_map(|node| match node {
            ProcedureNode::Step(step) => Some(step),
            ProcedureNode::Parallel(_) => None,
        })
        .collect();
    for (endpoint, body) in [
        (
            "dogstatsd_tcp",
            "adp.forwarding.stream_one:1|c\nadp.forwarding.stream_two:2|c\n",
        ),
        ("dogstatsd_uds_stream", "adp.forwarding.uds_stream:1|c\n"),
    ] {
        let step = steps
            .iter()
            .find(|step| step.name == "send" && step.fields["endpoint"] == endpoint)
            .unwrap();
        let payload = step.fields["payload"].as_str().unwrap().as_bytes();
        assert_eq!(&payload[4..], body.as_bytes());
        assert_eq!(
            u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize,
            body.len()
        );
    }

    let telemetry = steps.iter().find(|step| step.name == "http_check").unwrap();
    assert_eq!(telemetry.fields["endpoint"], "api");
    assert_eq!(telemetry.fields["path"], "/metrics");
    assert_eq!(telemetry.fields["status"], serde_json::json!({"equal": 200}));
    assert_eq!(telemetry.fields["regex"], true);
    let pattern = regex::Regex::new(telemetry.fields["pattern"].as_str().unwrap()).unwrap();
    // ADP's /metrics response prefixes the received-event counter with `adp__`.
    let observed = r#"adp__component_events_received_total{component_id="dsd_in",component_type="source",listener_type="udp",message_type="metrics"} 200"#;
    assert!(pattern.is_match(observed));
    assert!(pattern.is_match(&observed.replace(
        r#"listener_type="udp",message_type="metrics""#,
        r#"message_type="metrics",listener_type="udp""#,
    )));
    for mismatched in [
        observed.replace("} 200", "} 0"),
        observed.replace("} 200", "} -1"),
        observed.replace(r#"listener_type="udp""#, r#"listener_type="tcp""#),
        observed.replace(r#"message_type="metrics""#, r#"message_type="events""#),
        observed.replace(r#"listener_type="udp","#, ""),
        observed.replace(r#",message_type="metrics""#, ""),
        observed.replace(",message_type", ",\nmessage_type"),
    ] {
        assert!(!pattern.is_match(&mismatched), "{mismatched}");
    }
}

#[test]
fn dogstatsd_top_tls_and_dns_cases_place_their_case_files_under_the_definitions_paths() {
    use panoramic_core::plan::test_support::assert_request_scheme;

    let cases_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../test/integration/cases");
    for name in ["adp-ipc-no-dns", "otlp-tls", "dogstatsd-top"] {
        let case = crate::CASE_LOADER
            .load_file(&cases_dir.join(name).join("config.yaml"))
            .unwrap()
            .remove(0);
        let plan = planner().compile(case, Os::Linux).unwrap().into_plan().unwrap();
        assert!(plan.participants().is_some());
        assert_eq!(plan.fact("adp", "files_dir"), Some("/var/lib/panoramic/files"));
        if name == "otlp-tls" {
            let env = &plan.effective_config("adp").unwrap().env;
            for key in [
                "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE",
                "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_KEY_FILE",
                "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_TLS_CERT_FILE",
                "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_TLS_KEY_FILE",
            ] {
                assert_eq!(env[key], "/var/lib/panoramic/files/server.pem");
            }
            assert_request_scheme(&plan, "adp", "otlp_http", "https");
        }
    }
}

#[test]
fn liveness_case_creates_adp_before_its_fixture_and_starts_the_fixture_before_adp() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../test/integration/cases/liveness-container-tags/config.yaml");
    let case = crate::CASE_LOADER.load_file(&path).unwrap().remove(0);
    let plan = planner().compile(case, Os::Linux).unwrap().into_plan().unwrap();
    assert_eq!(
        plan.create_order().unwrap(),
        [
            vec!["adp".to_string(), "intake".to_string()],
            vec!["docker-api".to_string()]
        ]
    );
    assert_eq!(
        plan.start_order().unwrap(),
        [
            vec!["docker-api".to_string(), "intake".to_string()],
            vec!["adp".to_string()]
        ]
    );
    let env = &plan.effective_config("adp").unwrap().env;
    assert_eq!(env["DOCKER_HOST"], "unix:///airlock/docker-api/shared/docker.sock");
    assert_eq!(env["DD_DD_URL"], "http://intake:2049");
    assert_eq!(env["DD_USE_V2_API_SERIES"], "false");
    assert_eq!(env["DD_SERIALIZER_COMPRESSOR_KIND"], "gzip");
}

#[test]
fn realization_options_are_checked_against_the_backend() {
    for (kind, options, error) in [
        ("linux-container", "{host_cgroup_namespace: true}", None),
        ("linux-container", "{host_cgroup_namespace: false}", None),
        ("linux-container", "{unknown: true}", Some("unknown")),
        (
            "linux-container",
            "{host_cgroup_namespace: 'true'}",
            Some("host_cgroup_namespace"),
        ),
        (
            "linux-container",
            "{host_cgroup_namespace: {enabled: true}}",
            Some("host_cgroup_namespace"),
        ),
        (
            "host-process-group",
            "{host_cgroup_namespace: true}",
            Some("host_cgroup_namespace"),
        ),
    ] {
        let yaml = PARTICIPANT_FIELDS.replace(
            "subject: true",
            &format!("subject: true, realization: {{kind: {kind}, options: {options}}}"),
        );
        let loaded = crate::CASE_LOADER.load_content(&yaml, "alpha", Path::new("config.yaml"));
        if let Some(option) = error {
            let message = loaded.err().expect("invalid option must fail at load").to_string();
            assert!(message.contains("participant 'adp'"), "{message}");
            assert!(message.contains(kind), "{message}");
            assert!(message.contains(option), "{message}");
        } else {
            assert_eq!(loaded.unwrap().len(), 1);
        }
    }
}
