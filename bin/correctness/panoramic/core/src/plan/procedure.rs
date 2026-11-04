//! Checks a case's procedure steps against each subject OS's plan.

use std::collections::BTreeMap;

use super::{
    endpoints::ParticipantEndpoints,
    ports::RunnerHostPorts,
    references::{late_fact_timing, planned_fact},
    routes::plan_runner_host_route,
    PlannedParticipant,
};
use crate::{
    case::{declared_names, CaseFile, ProcedureNode},
    endpoint::{EndpointOperation, RunnerHostRoute},
    layout::Layout,
    participant::{find_type, ParticipantType},
    record::step_label,
    step::{Need, Step},
};

/// Checks fact, late-fact, operation and endpoint needs, returning the runner-host routes steps use.
///
/// Participant and process needs were checked at load and are independent of the layout.
///
/// # Errors
///
/// Rejects the first unavailable fact, late fact, operation or endpoint, unreachable endpoint or exhausted port allocation.
pub(super) fn check_step_references(
    file: &CaseFile, procedure: &[ProcedureNode<Box<dyn Step>>], layout: &Layout, types: &[&dyn ParticipantType],
    participants: &BTreeMap<String, PlannedParticipant>, endpoints: &BTreeMap<String, ParticipantEndpoints>,
    ports: &mut RunnerHostPorts<'_>,
) -> Result<BTreeMap<(String, String, EndpointOperation), RunnerHostRoute>, String> {
    let mut routes = BTreeMap::new();
    for (index, node) in procedure.iter().enumerate() {
        let (steps, parallel) = match node {
            ProcedureNode::Step(step) => (std::slice::from_ref(step), false),
            ProcedureNode::Parallel(steps) => (steps.as_slice(), true),
        };
        for (member, step) in steps.iter().enumerate() {
            for need in step.needs() {
                let result = match need {
                    Need::Fact(participant, fact) => {
                        let realization = layout.get(&participant).expect("step needs name laid-out participants");
                        let participant_type = find_type(types, &file.participants[&participant].participant_type)
                            .expect("laid-out participants have registered types");
                        planned_fact(
                            &participant,
                            participant_type,
                            realization,
                            &participants[&participant].facts,
                            &fact,
                        )
                        .map(|_| ())
                    }
                    Need::Late(participant, fact) => {
                        let realization = layout.get(&participant).expect("step needs name laid-out participants");
                        late_fact_timing(&participant, realization.kind, fact).map(|_| ())
                    }
                    Need::Operation(participant, operation) => {
                        let offered = participants[&participant].operations;
                        if let Some(offered) = offered.iter().find(|offered| offered.name() == operation) {
                            offered.http_endpoints().iter().try_for_each(|&endpoint| {
                                plan_runner_host_route(
                                    &format!(
                                        "calls operation '{operation}' of participant '{participant}', which requests endpoint '{endpoint}'"
                                    ),
                                    &participant,
                                    endpoint,
                                    EndpointOperation::Request,
                                    layout,
                                    endpoints,
                                    ports,
                                )
                                .map(|route| {
                                    routes.insert(
                                        (participant.clone(), endpoint.into(), EndpointOperation::Request),
                                        route,
                                    );
                                })
                            })
                        } else {
                            let realization = layout.get(&participant).expect("step needs name laid-out participants");
                            Err(format!(
                                "calls operation '{operation}' of participant '{participant}', which type '{}' doesn't offer as {} on {}; it offers: {}",
                                file.participants[&participant].participant_type,
                                realization.kind.name(), realization.os.name(),
                                declared_names(&offered.iter().map(|operation| operation.name()).collect::<Vec<_>>()),
                            ))
                        }
                    }
                    Need::Endpoint(participant, endpoint, operation) => plan_runner_host_route(
                        &format!("reaches endpoint '{endpoint}' of participant '{participant}'"),
                        &participant,
                        &endpoint,
                        operation,
                        layout,
                        endpoints,
                        ports,
                    )
                    .map(|route| {
                        routes.insert((participant, endpoint, operation), route);
                    }),
                    Need::Handle(_) | Need::Process(_, _) => continue,
                };
                result.map_err(|reason| {
                    format!(
                        "step {} '{}' {reason}",
                        step_label(index, parallel.then_some(member)),
                        step.name()
                    )
                })?;
            }
        }
    }
    Ok(routes)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        backend,
        case::{
            fixtures::{minimal_procedure, FixtureTypes, PodServer, PARTICIPANT_FIELDS, STEPS},
            CaseLoader, CaseSchema, Registries,
        },
        endpoint::{EndpointKind, ResolvedAddress, RunnerHostPort, Transport},
        layout::{Os, RealizationKind},
        plan::{
            fixtures::{compile, compile_with, host_to_container_case, planner, FACT_CASE},
            Planner,
        },
    };

    fn reach_case(os: Os, kind: RealizationKind, endpoint: &str) -> String {
        PARTICIPANT_FIELDS
            .replace("[linux]", &format!("[{}]", os.name()))
            .replace(
                "type: server",
                &format!("type: every-kind-server, realization: {{kind: {}}}", kind.name()),
            )
            .replace(
                minimal_procedure!(),
                &format!("procedure: [{{step: reach, endpoint: subject.{endpoint}}}]"),
            )
    }

    #[test]
    fn operation_needs_are_checked_against_the_planned_realization() {
        use crate::case::fixtures::{CallFactory, OperationServer};
        let registries = Registries {
            participant_types: &[&OperationServer],
            steps: &[&CallFactory],
            typed_case_roles: &[],
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture");
        for (os, operation, expected) in [
            (Os::Linux, "config_dump", None),
            (Os::Linux, "missing", Some("calls operation 'missing' of participant 'subject', which type 'server' doesn't offer as linux-container on linux; it offers: config_dump")),
            (Os::MacOs, "config_dump", Some("calls operation 'config_dump' of participant 'subject', which type 'server' doesn't offer as host-process-group on macos; it offers: none")),
        ] {
            let yaml = format!("timeout: 1s\nsubject_os: [{}]\nparticipants:\n  subject: {{type: server, subject: true}}\nprocedure: [{{step: call, on: subject, operation: {operation}}}]", os.name());
            let case = loader.load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml")).unwrap().remove(0);
            let result = Planner::new(registries, Default::default()).unwrap().compile(case, os);
            if let Some(expected) = expected {
                let error = result.err().unwrap().to_string();
                assert_eq!(error, format!("case 'alpha' in cases/alpha/config.yaml: on subject OS '{}', step 1 'call' {expected}", os.name()));
            } else {
                let plan = result.unwrap().into_plan().unwrap();
                let plan = plan.participants().unwrap();
                assert_eq!(plan.operations("subject").iter().map(|operation| operation.name()).collect::<Vec<_>>(), ["config_dump"]);
                assert!(plan.operations("missing").is_empty());
            }
        }
    }

    #[test]
    fn operations_plan_request_routes_and_publish_only_the_endpoints_they_declare() {
        use crate::case::fixtures::{CallFactory, HttpOperationServer};

        let registries = Registries {
            participant_types: &[&HttpOperationServer(&["api", "other"])],
            steps: &[&CallFactory],
            typed_case_roles: &[],
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let yaml = format!(
                "timeout: 1s\nsubject_os: [{}]\nparticipants:\n  subject: {{type: server, subject: true}}\nprocedure: [{{step: call, on: subject, operation: http_dump}}, {{step: call, on: subject, operation: http_dump}}]",
                os.name()
            );
            let case = loader
                .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap()
                .remove(0);
            let mut planner = Planner::new(registries, Default::default()).unwrap();
            let plan = planner.compile(case, os).unwrap().into_plan().unwrap();
            let port = if os == Os::Linux { 20000 } else { 1003 };
            let host = if os == Os::Windows { "subject" } else { "127.0.0.1" };
            let address = ResolvedAddress {
                kind: EndpointKind::Http,
                url: format!("http://{host}:{port}"),
                host: Some(host.into()),
                port: Some(port),
                path: None,
            };
            let route = if os == Os::Windows {
                RunnerHostRoute::Courier(address)
            } else {
                RunnerHostRoute::Direct(address)
            };
            assert_eq!(
                plan.runner_host_route("subject", "api", EndpointOperation::Request),
                Some(&route)
            );
            assert_eq!(
                plan.runner_host_route("subject", "other", EndpointOperation::Request),
                None
            );
            assert_eq!(plan.runner_host_route("subject", "api", EndpointOperation::Send), None);
            let ports = if os == Os::Linux {
                vec![RunnerHostPort {
                    transport: Transport::Tcp,
                    container_port: 1003,
                    host_port: 20000,
                }]
            } else {
                vec![]
            };
            assert_eq!(plan.runner_host_ports("subject"), ports);
            assert_eq!(
                planner.ports.allocate(),
                Ok(if os == Os::Linux { 20001 } else { 20000 })
            );
        }
    }

    #[test]
    fn an_operations_missing_http_endpoint_fails_planning_with_the_operation_and_endpoint() {
        use crate::case::fixtures::{CallFactory, HttpOperationServer};

        let registries = Registries {
            participant_types: &[&HttpOperationServer(&["other"])],
            steps: &[&CallFactory],
            typed_case_roles: &[],
        };
        let case = CaseLoader::new(registries, FixtureTypes, "fixture")
            .load_content(
                "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {type: server, subject: true}\nprocedure: [{step: call, on: subject, operation: http_dump}]",
                "alpha",
                Path::new("cases/alpha/config.yaml"),
            )
            .unwrap()
            .remove(0);
        let error = Planner::new(registries, Default::default())
            .unwrap()
            .compile(case, Os::Linux)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            error,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', step 1 'call' calls operation 'http_dump' of participant 'subject', which requests endpoint 'api', which it does not serve; it serves other"
        );
    }

    #[test]
    fn only_request_steps_to_containers_or_pods_allocate_runner_host_ports() {
        for realization in [RealizationKind::LinuxContainer, RealizationKind::KubernetesPod] {
            for (operation, field) in [
                (EndpointOperation::Probe, "probe"),
                (EndpointOperation::Send, "send"),
                (EndpointOperation::Request, "request"),
            ] {
                for (endpoint, kind, port) in [
                    ("tcp", EndpointKind::Tcp, 1001),
                    ("udp", EndpointKind::Udp, 1001),
                    ("http", EndpointKind::Http, 1003),
                    ("grpc", EndpointKind::Grpc, 1004),
                ] {
                    let yaml = reach_case(Os::Linux, realization, endpoint)
                        .replace("step: reach,", &format!("step: reach, operation: {field},"));
                    let mut planner = planner();
                    if operation == EndpointOperation::Request
                        && !matches!(kind, EndpointKind::Http | EndpointKind::Grpc)
                    {
                        assert_eq!(compile_with(&mut planner, &yaml, Os::Linux).err().unwrap().to_string(), format!(
                            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', step 1 'reach' reaches endpoint '{endpoint}' of participant 'subject', which the runner host cannot reach: request/response operations require an http or grpc endpoint"));
                        assert_eq!(planner.ports.allocate(), Ok(20000));
                        continue;
                    }
                    let plan = compile_with(&mut planner, &yaml, Os::Linux).unwrap().unwrap();
                    let expected = if operation == EndpointOperation::Request {
                        RunnerHostRoute::Direct(ResolvedAddress {
                            kind,
                            url: format!("{endpoint}://127.0.0.1:20000"),
                            host: Some("127.0.0.1".into()),
                            port: Some(20000),
                            path: None,
                        })
                    } else {
                        RunnerHostRoute::Courier(ResolvedAddress {
                            kind,
                            url: format!("{endpoint}://subject:{port}"),
                            host: Some("subject".into()),
                            port: Some(port),
                            path: None,
                        })
                    };
                    assert_eq!(plan.runner_host_route("subject", endpoint, operation), Some(&expected));
                    let ports = if operation == EndpointOperation::Request {
                        vec![RunnerHostPort {
                            transport: Transport::Tcp,
                            container_port: port,
                            host_port: 20000,
                        }]
                    } else {
                        vec![]
                    };
                    assert_eq!(plan.runner_host_ports("subject"), ports);
                    assert_eq!(
                        planner.ports.allocate(),
                        Ok(if ports.is_empty() { 20000 } else { 20001 })
                    );
                    assert_eq!(plan.runner_host_route("subject", "unused", operation), None);
                    assert_eq!(plan.runner_host_route("missing", endpoint, operation), None);
                }
            }
        }
    }

    #[test]
    fn a_step_reaching_a_host_process_group_endpoint_gets_its_loopback_port_and_no_runner_host_port() {
        for os in [Os::Linux, Os::MacOs] {
            let mut planner = planner();
            let plan = compile_with(
                &mut planner,
                &reach_case(os, RealizationKind::HostProcessGroup, "tcp"),
                os,
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                plan.runner_host_route("subject", "tcp", EndpointOperation::Probe),
                Some(&RunnerHostRoute::Direct(ResolvedAddress {
                    kind: EndpointKind::Tcp,
                    url: "tcp://127.0.0.1:1001".into(),
                    host: Some("127.0.0.1".into()),
                    port: Some(1001),
                    path: None,
                }))
            );
            assert!(plan.runner_host_ports("subject").is_empty());
            assert_eq!(planner.ports.allocate(), Ok(20000));
        }
    }

    #[test]
    fn a_step_reaching_a_host_socket_gets_its_path_directly() {
        let plan = compile(
            &reach_case(Os::MacOs, RealizationKind::HostProcessGroup, "stream"),
            Os::MacOs,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            plan.runner_host_route("subject", "stream", EndpointOperation::Probe),
            Some(&RunnerHostRoute::Direct(ResolvedAddress {
                kind: EndpointKind::UnixStream,
                url: "unix:///run/every/stream.sock".into(),
                host: None,
                port: None,
                path: Some("/run/every/stream.sock".into()),
            }))
        );
        assert!(plan.runner_host_ports("subject").is_empty());
    }

    #[test]
    fn a_step_reaching_a_container_socket_or_a_windows_container_gets_a_courier_route_with_the_servers_own_address() {
        for (os, realization, endpoint, kind, url, host, port, path) in [
            (
                Os::Linux,
                RealizationKind::LinuxContainer,
                "stream",
                EndpointKind::UnixStream,
                "unix:///run/every/stream.sock",
                None,
                None,
                Some("/run/every/stream.sock"),
            ),
            (
                Os::Linux,
                RealizationKind::LinuxContainer,
                "datagram",
                EndpointKind::UnixDatagram,
                "unixgram:///run/every/datagram.sock",
                None,
                None,
                Some("/run/every/datagram.sock"),
            ),
            (
                Os::Windows,
                RealizationKind::WindowsContainer,
                "pipe",
                EndpointKind::NamedPipe,
                r"\\.\pipe\every-pipe",
                None,
                None,
                Some(r"\\.\pipe\every-pipe"),
            ),
            (
                Os::Windows,
                RealizationKind::WindowsContainer,
                "tcp",
                EndpointKind::Tcp,
                "tcp://subject:1001",
                Some("subject"),
                Some(1001),
                None,
            ),
            (
                Os::Linux,
                RealizationKind::KubernetesPod,
                "udp",
                EndpointKind::Udp,
                "udp://subject:1001",
                Some("subject"),
                Some(1001),
                None,
            ),
            (
                Os::Linux,
                RealizationKind::KubernetesPod,
                "stream",
                EndpointKind::UnixStream,
                "unix:///run/every/stream.sock",
                None,
                None,
                Some("/run/every/stream.sock"),
            ),
        ] {
            let mut planner = planner();
            let plan = compile_with(&mut planner, &reach_case(os, realization, endpoint), os)
                .unwrap()
                .unwrap();
            assert_eq!(
                plan.runner_host_route("subject", endpoint, EndpointOperation::Probe),
                Some(&RunnerHostRoute::Courier(ResolvedAddress {
                    kind,
                    url: url.into(),
                    host: host.map(String::from),
                    port,
                    path: path.map(String::from),
                }))
            );
            assert!(plan.runner_host_ports("subject").is_empty());
            assert_eq!(planner.ports.allocate(), Ok(20000));
        }
    }

    #[test]
    fn a_step_reaching_an_endpoint_its_participant_does_not_serve_fails_naming_the_step() {
        let yaml = reach_case(Os::Linux, RealizationKind::LinuxContainer, "tcp")
            .replace(
                "type: every-kind-server",
                "type: file-server, config: {env: {TCP: 'off'}}",
            )
            .replace("procedure: [", "procedure: [{step: noop}, {parallel: [{step: noop}, ")
            .replace("endpoint: subject.tcp}]", "endpoint: subject.tcp}]}]");
        assert_eq!(compile(&yaml, Os::Linux).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', step 2.2 'reach' reaches endpoint 'tcp' of participant 'subject', which it does not serve; it serves datagram, grpc, http, pipe, stream, udp");
    }

    #[test]
    fn a_step_reaching_an_unroutable_endpoint_fails_naming_the_constraint() {
        for (os, realization, endpoint, reason) in [
            (
                Os::Linux,
                RealizationKind::LinuxContainer,
                "pipe",
                "a named pipe is reachable only in a windows-container",
            ),
            (
                Os::MacOs,
                RealizationKind::HostProcessGroup,
                "pipe",
                "a named pipe is reachable only in a windows-container",
            ),
            (
                Os::Windows,
                RealizationKind::WindowsContainer,
                "stream",
                "unix sockets are not supported in a windows-container",
            ),
        ] {
            assert_eq!(compile(&reach_case(os, realization, endpoint), os).err().unwrap().to_string(), format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS '{}', step 1 'reach' reaches endpoint '{endpoint}' of participant 'subject', which the runner host cannot reach: {reason}", os.name()));
        }
    }

    #[test]
    fn a_step_and_a_host_process_group_reaching_one_container_endpoint_share_its_runner_host_port() {
        let yaml = host_to_container_case()
            .replace("subject: true", "subject: true, config: {env: {REF: '{{server.endpoints.api.url}}'}}")
            .replace(minimal_procedure!(), "procedure: [{step: reach, endpoint: server.api, operation: request}, {step: reach, endpoint: server.metrics}, {step: reach, endpoint: server.api}, {step: reach, endpoint: server.api, operation: send}, {step: reach, endpoint: server.api, operation: request}]");
        let mut planner = planner();
        let plan = compile_with(&mut planner, &yaml, Os::MacOs).unwrap().unwrap();
        let address = ResolvedAddress {
            kind: EndpointKind::Http,
            url: "http://127.0.0.1:20000".into(),
            host: Some("127.0.0.1".into()),
            port: Some(20000),
            path: None,
        };
        assert_eq!(plan.connection_address("client", "upstream"), Some(&address));
        assert_eq!(plan.endpoint_address("client", "server", "api"), Some(&address));
        assert_eq!(
            plan.runner_host_route("server", "api", EndpointOperation::Request),
            Some(&RunnerHostRoute::Direct(address))
        );
        for operation in [EndpointOperation::Probe, EndpointOperation::Send] {
            assert_eq!(
                plan.runner_host_route("server", "api", operation),
                Some(&RunnerHostRoute::Courier(ResolvedAddress {
                    kind: EndpointKind::Http,
                    url: "http://server:8080".into(),
                    host: Some("server".into()),
                    port: Some(8080),
                    path: None,
                }))
            );
        }
        assert_eq!(plan.participants.as_ref().unwrap().planned.runner_host_routes.len(), 4);
        assert_eq!(
            plan.runner_host_ports("server"),
            vec![RunnerHostPort {
                transport: Transport::Tcp,
                container_port: 8080,
                host_port: 20000
            },]
        );
        assert_eq!(planner.ports.allocate(), Ok(20001));
    }

    #[test]
    fn step_endpoint_needs_on_an_unselected_subject_os_draw_from_a_scratch_registry() {
        let yaml = concat!(
            "timeout: 1s\nsubject_os: [linux, macos]\nparticipants:\n",
            "  subject: {type: every-kind-server, subject: true}\n  server: {type: server, os: linux}\n",
            "procedure: [{step: reach, endpoint: subject.http, operation: request}, {step: reach, endpoint: server.api, operation: request}]\n",
        );
        let mut selected_planner = planner();
        let plan = compile_with(&mut selected_planner, yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.runner_host_route("subject", "http", EndpointOperation::Request),
            Some(&RunnerHostRoute::Direct(ResolvedAddress {
                kind: EndpointKind::Http,
                url: "http://127.0.0.1:1003".into(),
                host: Some("127.0.0.1".into()),
                port: Some(1003),
                path: None,
            }))
        );
        assert_eq!(
            plan.runner_host_route("server", "api", EndpointOperation::Request),
            Some(&RunnerHostRoute::Direct(ResolvedAddress {
                kind: EndpointKind::Http,
                url: "http://127.0.0.1:20000".into(),
                host: Some("127.0.0.1".into()),
                port: Some(20000),
                path: None,
            }))
        );
        assert!(plan.runner_host_ports("subject").is_empty());
        assert_eq!(selected_planner.ports.allocate(), Ok(20001));
        let mut excluded_planner = planner();
        assert!(compile_with(&mut excluded_planner, yaml, Os::Windows)
            .unwrap()
            .is_none());
        assert_eq!(excluded_planner.ports.allocate(), Ok(20000));
    }

    #[test]
    fn undeclared_step_facts_fail_at_plan_time() {
        for (participant, participant_type, fact) in [("subject", "server", "missing"), ("other", "client", "file")] {
            let yaml = FACT_CASE
                .replace(
                    "participants:",
                    "participants:\n  other: {type: client, connect: {upstream: subject.api}}",
                )
                .replace("subject.facts.file", &format!("{participant}.facts.{fact}"));
            let message = compile(&yaml, Os::Linux).err().unwrap().to_string();
            assert_eq!(message, format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', step 1 'probe' references fact '{fact}' of participant '{participant}', which type '{participant_type}' does not provide as linux-container on linux; it provides it as none"
            ));
        }
    }

    #[test]
    fn a_step_and_a_configuration_reference_to_a_fact_the_realization_lacks_report_the_same_reason() {
        for fact in ["file", "missing"] {
            let step_yaml = FACT_CASE
                .replace("[linux]", "[macos]")
                .replace("subject.facts.file", &format!("subject.facts.{fact}"));
            let config_yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
                "subject: true",
                &format!("subject: true, config: {{env: {{KEY: '{{{{ subject.facts.{fact} }}}}'}}}}"),
            );
            let step_error = compile(&step_yaml, Os::MacOs).err().unwrap().to_string();
            let config_error = compile(&config_yaml, Os::MacOs).err().unwrap().to_string();
            let step_reason = step_error
                .strip_prefix("case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', step 1 'probe' ")
                .unwrap();
            let config_reason = config_error
                .strip_prefix(&format!("case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'subject' of type 'server' as host-process-group on macos environment variable 'KEY' '{{{{ subject.facts.{fact} }}}}' "))
                .unwrap();
            assert_eq!(step_reason, config_reason);
        }
    }

    #[test]
    fn a_step_and_a_configuration_reference_to_a_late_fact_the_realization_lacks_report_the_same_reason() {
        let step_yaml = FACT_CASE
            .replace("[linux]", "[macos]")
            .replace("subject.facts.file", "subject.late.container_id");
        let config_yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
            "subject: true",
            "subject: true, config: {env: {KEY: '{{ subject.late.container_id }}'}}",
        );
        let step_error = compile(&step_yaml, Os::MacOs).err().unwrap().to_string();
        let config_error = compile(&config_yaml, Os::MacOs).err().unwrap().to_string();
        let step_reason = step_error
            .strip_prefix("case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', step 1 'probe' ")
            .unwrap();
        let config_reason = config_error
            .strip_prefix("case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'subject' of type 'server' as host-process-group on macos environment variable 'KEY' '{{ subject.late.container_id }}' ")
            .unwrap();
        assert_eq!(step_reason, config_reason);
        assert!(step_reason.ends_with("; a host-process-group has ip"));
    }

    #[test]
    fn facts_must_be_provided_by_the_participants_realization_on_every_declared_subject_os() {
        for declared in ["[windows, macos, linux]", "[macos]"] {
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                let message = compile(&FACT_CASE.replace("[linux]", declared), os)
                    .err()
                    .unwrap()
                    .to_string();
                assert_eq!(message,
                    "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', step 1 'probe' references fact 'file' of participant 'subject', which type 'server' does not provide as host-process-group on macos; it provides it as linux-container on linux, windows-container on windows"
                );
            }
        }
        assert!(compile(&FACT_CASE.replace("[linux]", "[linux, windows]"), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn late_facts_must_be_provided_by_the_participants_realization_on_every_declared_subject_os() {
        let yaml = FACT_CASE
            .replace("[linux]", "[linux, macos]")
            .replace("subject.facts.file", "subject.late.container_id");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            assert_eq!(compile(&yaml, os).err().unwrap().to_string(),
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', step 1 'probe' references late fact 'container_id' of participant 'subject', which a host-process-group does not have; a host-process-group has ip"
            );
        }
        assert!(compile(&yaml.replace("[linux, macos]", "[linux, windows]"), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_late_fact_every_realization_has_compiles_on_every_declared_subject_os() {
        let yaml = FACT_CASE
            .replace("[linux]", "[linux, macos, windows]")
            .replace("subject.facts.file", "subject.late.ip");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let plan = compile(&yaml, os).unwrap().unwrap();
            assert_eq!(plan.subject_os(), os);
            let CaseSchema::Participants { procedure, .. } = plan.case().schema() else {
                panic!("expected participant data");
            };
            let [ProcedureNode::Step(step)] = procedure.as_slice() else {
                panic!("expected one step");
            };
            assert_eq!(
                step.needs(),
                [
                    Need::Handle("subject".into()),
                    Need::Late("subject".into(), backend::LateFact::Ip)
                ]
            );
        }
    }

    #[test]
    fn pod_uid_is_available_only_to_a_kubernetes_pod() {
        let registries = Registries {
            participant_types: &[&PodServer],
            steps: STEPS,
            ..Default::default()
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture-participants");
        let yaml = FACT_CASE
            .replace("type: server", "type: pod-server, realization: {kind: kubernetes-pod}")
            .replace("subject.facts.file", "subject.late.pod_uid");
        let case = loader
            .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let plan = Planner::with_run(registries, "testrun0")
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap();
        assert_eq!(
            plan.layout().unwrap().get("subject").unwrap().kind,
            RealizationKind::KubernetesPod
        );
        let case = loader
            .load_content(
                &yaml.replace("kubernetes-pod", "linux-container"),
                "alpha",
                Path::new("cases/alpha/config.yaml"),
            )
            .unwrap()
            .remove(0);
        assert_eq!(Planner::with_run(registries, "testrun0").compile(case, Os::Linux).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', step 1 'probe' references late fact 'pod_uid' of participant 'subject', which a linux-container does not have; a linux-container has container_id, ip"
        );
    }

    #[test]
    fn fact_and_late_fact_errors_follow_procedure_order_including_parallel_members() {
        let mut yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
            minimal_procedure!(),
            r#"procedure:
  - {step: probe, on: subject, path: '{{ subject.late.container_id }}{{ subject.facts.file }}'}
  - parallel:
    - {step: probe, on: subject, path: '{{ subject.late.ip }}'}
    - {step: probe, on: subject, path: '{{ subject.facts.file }}{{ subject.late.pod_uid }}'}
  - {step: probe, on: subject, path: '{{ subject.late.container_id }}'}"#,
        );
        for (reference, label, kind, fact) in [
            ("{{ subject.late.container_id }}", "1", "late fact", "container_id"),
            ("{{ subject.facts.file }}", "1", "fact", "file"),
            ("{{ subject.facts.file }}", "2.2", "fact", "file"),
            ("{{ subject.late.pod_uid }}", "2.2", "late fact", "pod_uid"),
            ("{{ subject.late.container_id }}", "3", "late fact", "container_id"),
        ] {
            let message = compile(&yaml, Os::MacOs).err().unwrap().to_string();
            assert!(
                message.contains(&format!(
                    "on subject OS 'macos', step {label} 'probe' references {kind} '{fact}' of participant 'subject'"
                )),
                "{message}"
            );
            yaml = yaml.replacen(reference, "literal", 1);
        }
        assert!(compile(&yaml, Os::MacOs).unwrap().is_some());
    }

    #[test]
    fn a_fact_provided_for_one_realization_is_missing_for_another_realization_on_the_same_os() {
        let registries = Registries {
            participant_types: &[&PodServer],
            steps: STEPS,
            ..Default::default()
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture-participants");
        let yaml = FACT_CASE.replace("type: server", "type: pod-server, realization: {kind: kubernetes-pod}");
        let case = loader
            .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let message = Planner::with_run(registries, "testrun0")
            .compile(case, Os::Linux)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', step 1 'probe' references fact 'file' of participant 'subject', which type 'pod-server' does not provide as kubernetes-pod on linux; it provides it as linux-container on linux"
        );
        let case = loader
            .load_content(
                &yaml.replace("kubernetes-pod", "linux-container"),
                "alpha",
                Path::new("config.yaml"),
            )
            .unwrap()
            .remove(0);
        let plan = Planner::with_run(registries, "testrun0")
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap();
        assert_eq!(plan.fact("subject", "file"), Some("/logs/output.log"));
    }

    #[test]
    fn a_participant_with_its_own_os_needs_facts_only_from_its_realization_on_that_os() {
        let yaml = FACT_CASE
            .replace("[linux]", "[macos]")
            .replace(
                "participants:",
                "participants:\n  other: {type: server, os: linux, config: {env: {PORT: '8081', METRICS: 'off'}}}",
            )
            .replace("subject.facts.file", "other.facts.file");
        let plan = compile(&yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(plan.fact("other", "file"), Some("/logs/output.log"));
        assert_eq!(plan.fact("subject", "file"), None);
        let message = compile(&yaml.replace(", os: linux", ""), Os::MacOs)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', step 1 'probe' references fact 'file' of participant 'other', which type 'server' does not provide as host-process-group on macos; it provides it as linux-container on linux, windows-container on windows"
        );
    }

    #[test]
    fn fact_errors_follow_subject_os_order_then_procedure_order_including_parallel_members() {
        let yaml = PARTICIPANT_FIELDS
            .replace("[linux]", "[macos, linux]")
            .replace("participants:", "participants:\n  other: {type: server, os: macos, config: {env: {PORT: '8081', METRICS: 'off'}}}")
            .replace(minimal_procedure!(), "procedure:\n  - {step: probe, on: subject, path: '{{ subject.facts.file }}'}\n  - parallel:\n    - {step: probe, on: subject}\n    - {step: probe, on: subject, path: '{{ other.facts.file }}'}\n  - {step: probe, on: subject, path: '{{ other.facts.file }}'}");
        let message = compile(&yaml, Os::MacOs).err().unwrap().to_string();
        assert!(
            message.contains("on subject OS 'linux', step 2.2 'probe' references fact 'file' of participant 'other'"),
            "{message}"
        );
        let message = compile(&yaml.replace("{{ other.facts.file }}", "literal"), Os::Linux)
            .err()
            .unwrap()
            .to_string();
        assert!(
            message.contains("on subject OS 'macos', step 1 'probe' references fact 'file' of participant 'subject'"),
            "{message}"
        );
    }
}
