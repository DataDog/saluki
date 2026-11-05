//! Loads case files through `CASE_LOADER` with the participant types and steps the binary registers.

use std::{collections::BTreeSet, fs, path::Path};

use panoramic_core::case::ProcedureNode;
use panoramic_core::endpoint::{test_support::assert_socket_endpoint, EndpointKind, Evidence};
use panoramic_core::layout::{Os, Realization, RealizationKind};
use panoramic_core::participant::ParticipantType;
use panoramic_core::step::Need;
use panoramic_participants::{Agent, AgentAdp};
use panoramic_steps::{test_support::valid_fields, REGISTERED_STEPS};
use serde_json::Value;

use crate::config::{
    discover_in_scope,
    tests::{minimal_procedure, planner, write_case, PARTICIPANT_FIELDS},
};

#[test]
fn tracked_test_files_are_not_symlinks() {
    // Git can check out symlinks as plain link-target text on Windows. Check the index mode too.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let output = std::process::Command::new("git")
        .current_dir(&root)
        .args(["ls-files", "--stage", "-z", "--", "test/"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let entries = String::from_utf8(output.stdout).unwrap();
    assert!(!entries.is_empty(), "expected tracked test files");
    let mut links = Vec::new();
    for entry in entries.split('\0').filter(|entry| !entry.is_empty()) {
        let (index, path) = entry.split_once('\t').unwrap();
        let metadata = match fs::symlink_metadata(root.join(path)) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("{path}: {error}"),
        };
        if index.starts_with("120000 ") || metadata.is_symlink() {
            links.push(path);
        }
    }
    assert!(
        links.is_empty(),
        "test files must not be symlinks: {}",
        links.join(", ")
    );
}

#[test]
fn every_checked_in_case_file_is_discovered() {
    // Discovery reads only direct subdirectories; nested cases must not silently drop out of runs.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../test")
        .canonicalize()
        .unwrap();
    let roots = ["integration/cases", "correctness/cases", "correctness/disabled"].map(|path| root.join(path));
    let mut files = BTreeSet::new();
    for root in &roots {
        let mut directories = vec![root.clone()];
        while let Some(directory) = directories.pop() {
            for entry in fs::read_dir(&directory).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    if directory == *root {
                        assert!(
                            path.join("config.yaml").is_file(),
                            "{}: missing config.yaml",
                            path.display()
                        );
                    }
                    directories.push(path);
                } else if entry.file_name() == "config.yaml" || entry.file_name() == "config.yml" {
                    assert!(
                        entry.file_name() == "config.yaml"
                            && path.parent().and_then(Path::parent) == Some(root.as_path()),
                        "{}: expected <case>/config.yaml under {}",
                        path.display(),
                        root.display()
                    );
                    files.insert(path.canonicalize().unwrap());
                }
            }
        }
    }
    let cases = crate::CASE_LOADER.discover_cases(&roots).unwrap();
    assert_eq!(
        files,
        cases
            .iter()
            .map(|case| case.source().to_path_buf())
            .collect::<BTreeSet<_>>()
    );
    for case in cases {
        // Compilation validates every declared subject OS, not just the selected one.
        let os = *case.subject_os().first().unwrap();
        planner().compile(case, os).unwrap().into_plan().unwrap();
    }
}

#[test]
fn apm_socket_file_templates_resolve_to_each_participants_shared_directory() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("test/correctness/cases");
    write_case(
        &directory,
        "alpha",
        "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  adp:\n    type: agent-adp\n    subject: true\n    config: {files: {datadog.yaml: {from: datadog.yaml, template: true}}}\n  agent:\n    type: agent\n    config: {files: {datadog.yaml: {from: datadog.yaml, template: true}}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
    );
    fs::write(
        directory.join("alpha/datadog.yaml"),
        "apm_config:\n  enabled: true\n  receiver_socket: '{{ self.facts.shared_dir }}/apm.socket'\n",
    )
    .unwrap();
    let case = crate::CASE_LOADER.discover_cases(&[directory]).unwrap().pop().unwrap();
    let plan = planner().compile(case, Os::Linux).unwrap().into_plan().unwrap();
    for participant in [&AgentAdp as &dyn ParticipantType, &Agent] {
        let name = if participant.name() == "agent-adp" {
            "adp"
        } else {
            "agent"
        };
        let config = plan.effective_config(name).unwrap();
        let endpoints = participant
            .endpoints(
                Realization {
                    os: Os::Linux,
                    kind: RealizationKind::LinuxContainer,
                },
                config,
            )
            .unwrap();
        assert_socket_endpoint(
            &endpoints["apm_uds"],
            EndpointKind::UnixStream,
            &format!("/airlock/{name}/shared/apm.socket"),
            Some(Evidence::socket_exists("apm_uds")),
        );
    }
}

#[test]
fn pod_host_view_requires_host_pid_at_plan_compile_for_both_agent_types() {
    for participant_type in ["agent", "agent-adp"] {
        for (host_view, options, valid) in [
            (true, "{}", false),
            (true, "{host_pid: false}", false),
            (true, "{host_pid: true}", true),
            (false, "{}", true),
            (false, "{host_pid: false}", true),
        ] {
            let yaml = format!("timeout: 1s\nsubject_os: [linux]\nparticipants:\n  target:\n    type: {participant_type}\n    subject: true\n    realization: {{kind: kubernetes-pod, options: {options}}}\n    config: {{params: {{host_view: {host_view}}}}}\nprocedure: [{{step: wait, duration: 1ms}}]\n");
            let case = crate::CASE_LOADER
                .load_content("integration", &yaml, "host-view", Path::new("host-view/config.yaml"))
                .unwrap();
            let result = planner().compile(case, Os::Linux);
            if valid {
                result.unwrap();
            } else {
                let error = result.err().unwrap().to_string();
                assert!(
                    error.contains(&format!("participant 'target' of type '{participant_type}'")),
                    "{error}"
                );
                assert!(
                    error.contains("pod host_view: true requires realization.options.host_pid: true"),
                    "{error}"
                );
            }
        }
    }
}

#[test]
fn default_log_file_references_plan_on_linux_and_windows_but_not_macos() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = PARTICIPANT_FIELDS.replace(minimal_procedure!(), "procedure: [{step: file_contains, on: adp, path: '{{ adp.facts.default_log_file }}', pattern: ready, timeout: 1s}]");
    for oses in ["linux", "windows", "linux, windows"] {
        let path = write_case(dir.path(), "alpha", &yaml.replace("[linux]", &format!("[{oses}]")));
        let case = crate::CASE_LOADER.load_file(&path).unwrap();
        let os = *case.subject_os().first().unwrap();
        let plans = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], os)
            .unwrap()
            .plans;
        assert_eq!(plans.len(), 1);
        let procedure = plans[0].case().procedure();
        let [ProcedureNode::Step(step)] = procedure else {
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
    assert_eq!(crate::CASE_LOADER.load_file(&path).unwrap().name(), "alpha");
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
    assert_eq!(crate::CASE_LOADER.load_file(&path).unwrap().name(), "alpha");
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
                let case = crate::CASE_LOADER.load_file(&path).unwrap();
                let procedure = case.procedure();
                let [ProcedureNode::Step(step)] = procedure else {
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
    let path = write_case(dir.path(), "alpha", &yaml.replace("[linux]", "[linux, windows]"));
    for (os, expected) in [(Os::Linux, "linux"), (Os::Windows, "arbitrary")] {
        let case = crate::CASE_LOADER.load_file(&path).unwrap();
        let plan = planner().compile(case, os).unwrap().into_plan().unwrap();
        assert_eq!(plan.effective_config("adp").unwrap().env["ANY_KEY"], expected);
    }

    for (fields, expected) in [
        (
            "config: {files: {settings.yaml: missing.yaml}}",
            "it declares datadog.yaml, cont-init.d/, files/",
        ),
        ("config: {params: {mode: fast}}", "it accepts extra_hosts, host_view"),
        (
            "config: {os: {linux: {files: {settings.yaml: missing.yaml}}}}",
            "it declares datadog.yaml, cont-init.d/, files/",
        ),
        (
            "config: {os: {linux: {params: {mode: fast}}}}",
            "it accepts extra_hosts, host_view",
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
        assert_eq!(crate::CASE_LOADER.load_file(&path).unwrap().name(), "alpha");
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
                .unwrap();
            assert_eq!(
                case.subject_os().iter().copied().collect::<Vec<_>>(),
                [Os::Linux, Os::MacOs, Os::Windows],
                "{name}"
            );
            let plan = planner().compile(case, os).unwrap().into_plan().unwrap();
            assert_eq!(
                plan.layout().iter().collect::<Vec<_>>(),
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
                .unwrap();
            assert_eq!(
                case.subject_os().iter().copied().collect::<Vec<_>>(),
                [Os::Linux, Os::MacOs, Os::Windows],
                "{name}"
            );
            let plan = planner().compile(case, os).unwrap().into_plan().unwrap();
            assert_eq!(
                plan.layout().iter().collect::<Vec<_>>(),
                [("adp", Realization { os, kind })],
                "{name} on {os:?}"
            );
            let procedure = plan.case().procedure();
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
        if let Some(on) = fields.get_mut("on") {
            *on = Value::String("adp".into());
        }
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
            let decode_error = if (factory.name() == "config_key_equals" && field == "view")
                || (factory.name() == "compare" && field == "kind")
            {
                let message = factory.build(&referenced).unwrap_err();
                assert_eq!(
                    message,
                    if factory.name() == "compare" {
                        "kind: unknown variant `{{ adp.facts.default_log_file }}`, expected one of `events`, `metrics`, `service_checks`, `traces`"
                    } else {
                        "view: unknown variant `{{ adp.facts.default_log_file }}`, expected `source` or `runtime`"
                    }
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
            let result = crate::CASE_LOADER.load_content("cases", &file.to_string(), "alpha", Path::new("config.yaml"));
            if let Some(message) = decode_error {
                assert_eq!(
                    result.err().expect("invalid view must fail").to_string(),
                    format!("case 'alpha' in config.yaml: step 1 '{}': {message}", factory.name())
                );
            } else if declared {
                let case = result.unwrap_or_else(|error| panic!("{}: {field}: {error}", factory.name()));
                let procedure = case.procedure();
                let [ProcedureNode::Step(step)] = procedure else {
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
            "names unknown step 'wait_ready'; registered steps: cli, compare, config_key_equals, config_set, dogstatsd_replay, exec, exits_with, file_contains, http_check, intake_has_forwarded_packets, intake_has_metric, intake_has_service_check, log_contains, log_not_contains, port_listening, process_stable_for, send",
        ),
        (
            "step: adp_cli, on: adp, args: [status]",
            "names unknown step 'adp_cli'; registered steps: cli, compare, config_key_equals, config_set, dogstatsd_replay, exec, exits_with, file_contains, http_check, intake_has_forwarded_packets, intake_has_metric, intake_has_service_check, log_contains, log_not_contains, port_listening, process_stable_for, send",
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
            assert_eq!(crate::CASE_LOADER.load_content("cases", &yaml, "alpha", Path::new("alpha/config.yaml")).err().unwrap().to_string(),
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
            .load_content("cases", &yaml, "alpha", Path::new("alpha/config.yaml"))
            .unwrap();
        let plan = planner().compile(case, os).unwrap().into_plan().unwrap();
        assert_eq!(plan.effective_config("adp").unwrap().env["FILE"], expected);
    }
    let case = crate::CASE_LOADER
        .load_content(
            "cases",
            &yaml.replace("[linux, windows]", "[linux, macos, windows]"),
            "alpha",
            Path::new("alpha/config.yaml"),
        )
        .unwrap();
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
                .load_content("cases", &yaml, "alpha", Path::new("alpha/config.yaml"))
                .unwrap();
            let plan = planner().compile(case, os).unwrap().into_plan().unwrap();
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
            .load_content("cases", &yaml, "alpha", Path::new("alpha/config.yaml"))
            .unwrap();
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
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../test/integration/cases/dogstatsd-forwarding/config.yaml");
    let case = crate::CASE_LOADER.load_file(&source).unwrap();
    planner().compile(case, Os::Linux).unwrap().into_plan().unwrap();
    let document: Value = serde_saphyr::from_str(&fs::read_to_string(source).unwrap()).unwrap();
    let steps = document["procedure"].as_array().unwrap();
    for (endpoint, body) in [
        (
            "dogstatsd_tcp",
            "adp.forwarding.stream_one:1|c\nadp.forwarding.stream_two:2|c\n",
        ),
        ("dogstatsd_uds_stream", "adp.forwarding.uds_stream:1|c\n"),
    ] {
        let step = steps
            .iter()
            .find(|step| step["step"] == "send" && step["endpoint"] == endpoint)
            .unwrap();
        let payload = step["payload"].as_str().unwrap().as_bytes();
        assert_eq!(&payload[4..], body.as_bytes());
        assert_eq!(
            u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize,
            body.len()
        );
    }

    let telemetry = steps.iter().find(|step| step["step"] == "http_check").unwrap();
    assert_eq!(telemetry["endpoint"], "api");
    assert_eq!(telemetry["path"], "/metrics");
    assert_eq!(telemetry["status"], serde_json::json!({"equal": 200}));
    assert_eq!(telemetry["regex"], true);
    let pattern = regex::Regex::new(telemetry["pattern"].as_str().unwrap()).unwrap();
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
            .unwrap();
        let plan = planner().compile(case, Os::Linux).unwrap().into_plan().unwrap();
        assert!(plan.layout().iter().any(|(name, _)| name == "adp"));
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
    let case = crate::CASE_LOADER.load_file(&path).unwrap();
    let plan = planner().compile(case, Os::Linux).unwrap().into_plan().unwrap();
    assert_eq!(
        plan.create_order(),
        [
            vec!["adp".to_string(), "intake".to_string()],
            vec!["docker-api".to_string()]
        ]
    );
    assert_eq!(
        plan.start_order(),
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
        let case = crate::CASE_LOADER.load_content("cases", &yaml, "alpha", Path::new("config.yaml"));
        if let Some(option) = error {
            let message = case.err().expect("invalid option must fail at load").to_string();
            assert!(message.contains("participant 'adp'"), "{message}");
            assert!(message.contains(kind), "{message}");
            assert!(message.contains(option), "{message}");
        } else {
            assert_eq!(case.unwrap().name(), "alpha");
        }
    }
}

#[test]
fn uds_comparisons_share_one_paired_sender_and_preserve_their_configured_workload() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../test/correctness/cases");
    let expected_seed = serde_json::json!([
        2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97, 101, 103, 107,
        109, 113, 127, 131
    ]);
    for (name, origin) in [("dsd-plain", false), ("dsd-origin-detection", true)] {
        let case = crate::CASE_LOADER
            .load_file(&root.join(name).join("config.yaml"))
            .unwrap();
        let plan = planner().compile(case, Os::Linux).unwrap().into_plan().unwrap();
        assert_eq!(plan.case().suite(), "correctness");
        let procedure = plan.case().procedure();
        let entries = plan.layout_entries();
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.participant.as_str(), entry.participant_type.as_str()))
                .collect::<Vec<_>>(),
            [
                ("adp", "agent-adp"),
                ("agent", "agent"),
                ("intake-adp", "datadog-intake"),
                ("intake-agent", "datadog-intake"),
                ("load", "millstone")
            ]
        );
        for participant in ["adp", "agent"] {
            let config = plan.effective_config(participant).unwrap();
            let settings: Value = serde_saphyr::from_str(
                String::from_utf8(config.files["datadog.yaml"].clone())
                    .unwrap()
                    .as_str(),
            )
            .unwrap();
            assert_eq!(settings["hostname"], "correctness-testing");
            assert_eq!(settings["dogstatsd_port"], 0);
            assert_eq!(settings["dogstatsd_workers_count"], 1);
            assert_eq!(settings["dogstatsd_origin_detection"], origin);
            assert_eq!(
                settings["dogstatsd_socket"],
                format!("/airlock/{participant}/shared/metrics.sock")
            );
            assert!(settings.get("dd_url").is_none());
            assert!(!config.env.contains_key("DD_DOGSTATSD_PORT"));
            assert_eq!(
                config.params.get("host_view").and_then(Value::as_bool).unwrap_or(false),
                origin
            );
        }
        let load = plan.effective_config("load").unwrap();
        assert_ne!(load.env["MILLSTONE_TARGET"], load.env["MILLSTONE_BASELINE_TARGET"]);
        let workload: Value = serde_saphyr::from_str(
            String::from_utf8(load.files["millstone.yaml"].clone())
                .unwrap()
                .as_str(),
        )
        .unwrap();
        assert_eq!(workload["seed"], expected_seed);
        assert_eq!(workload["target"], load.env["MILLSTONE_TARGET"]);
        assert_eq!(workload["volume"], 10000);
        assert_eq!(workload["corpus"]["payload"]["dogstatsd"]["contexts"]["constant"], 3000);
        let names = procedure
            .iter()
            .map(|node| match node {
                ProcedureNode::Step(step) => step.name(),
                ProcedureNode::Parallel(_) => panic!("senders finish before the stability window"),
            })
            .collect::<Vec<_>>();
        assert_eq!(names, ["log_contains", "log_contains", "process_stable_for", "compare"]);
        let ProcedureNode::Step(window) = &procedure[2] else {
            unreachable!()
        };
        assert_eq!(window.budget(), std::time::Duration::from_secs(32));
    }
}

#[test]
fn wait_is_rejected_in_parallel() {
    let yaml = PARTICIPANT_FIELDS.replace(
        minimal_procedure!(),
        "procedure: [{parallel: [{step: wait, duration: 1s}]}]",
    );
    let error = crate::CASE_LOADER
        .load_content("integration", &yaml, "case", Path::new("cases/case/config.yaml"))
        .err()
        .expect("an action cannot run in parallel");
    assert_eq!(
        error.to_string(),
        "case 'case' in cases/case/config.yaml: step 1.1 'wait' is an action; parallel blocks accept only assertions"
    );
}

#[test]
fn the_listener_failure_cases_run_natively_on_linux_and_macos() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../test/integration/cases");
    for name in [
        "dogstatsd-no-listener-continues",
        "dogstatsd-require-listener-exit",
        "dogstatsd-skip-failed-listener",
    ] {
        for os in [Os::Linux, Os::MacOs] {
            let case = crate::CASE_LOADER
                .load_file(&root.join(name).join("config.yaml"))
                .unwrap();
            assert_eq!(
                case.subject_os().iter().copied().collect::<Vec<_>>(),
                [Os::Linux, Os::MacOs]
            );
            let plan = planner().compile(case, os).unwrap().into_plan().unwrap();
            assert_eq!(plan.case().name(), name);
        }
    }
}

#[test]
fn correctness_cases_compare_the_subject_and_baseline_receiving_their_own_load() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../test/correctness");
    let cases = crate::CASE_LOADER
        .discover_cases(&[root.join("cases"), root.join("disabled")])
        .unwrap();
    assert!(!cases.is_empty());
    let mut planner = planner();
    for case in cases {
        let name = case.name();
        let document: Value = serde_saphyr::from_str(&fs::read_to_string(case.source()).unwrap()).unwrap();
        let comparisons: Vec<_> = document["procedure"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|node| {
                node.get("parallel")
                    .map_or(std::slice::from_ref(node), |steps| steps.as_array().unwrap().as_slice())
            })
            .filter(|step| step["step"] == "compare")
            .collect();
        assert_eq!(comparisons.len(), 1, "{name}: exactly one compare step");
        let comparison = comparisons[0];
        let subject_intake = comparison["subject"].as_str().unwrap();
        let baseline_intake = comparison["baseline"].as_str().unwrap();
        assert_ne!(subject_intake, baseline_intake, "{name}: distinct intakes");
        let participants = document["participants"].as_object().unwrap();
        let intakes: std::collections::BTreeSet<_> = participants
            .iter()
            .filter(|(_, entry)| entry["type"] == "datadog-intake")
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(
            intakes,
            std::collections::BTreeSet::from([subject_intake, baseline_intake]),
            "{name}: compare both intakes"
        );
        let subjects: Vec<_> = participants
            .iter()
            .filter(|(_, entry)| entry["subject"] == true)
            .collect();
        assert_eq!(subjects.len(), 1, "{name}: exactly one subject");
        let (subject_name, subject) = subjects[0];
        assert_eq!(subject["type"], "agent-adp", "{name}: subject type");
        assert_eq!(
            subject["connect"]["intake"],
            format!("{subject_intake}.api"),
            "{name}: subject intake"
        );
        let agents: Vec<_> = participants
            .iter()
            .filter(|(_, entry)| entry["type"] == "agent")
            .collect();
        assert_eq!(agents.len(), 1, "{name}: exactly one baseline Agent");
        let (agent_name, agent) = agents[0];
        assert_eq!(
            agent["connect"]["intake"],
            format!("{baseline_intake}.api"),
            "{name}: baseline intake"
        );
        let loads: Vec<_> = participants
            .values()
            .filter(|entry| entry["type"] == "millstone")
            .collect();
        assert_eq!(loads.len(), 1, "{name}: one paired load generator");
        for (connection, receiver) in [("target", subject_name), ("baseline_target", agent_name)] {
            let target = loads[0]["connect"][connection].as_str().unwrap();
            assert_eq!(
                target.split_once('.').unwrap().0,
                receiver,
                "{name}: millstone {connection} must name {receiver}"
            );
        }
        let os = *case.subject_os().first().unwrap();
        planner.compile(case, os).unwrap().into_plan().unwrap();
    }
}
