//! Validates readiness declarations and plans the evidence each consumer waits for.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    endpoints::{check_absolute_path, holds_no_reference, ParticipantEndpoints, PathError},
    ports::RunnerHostPorts,
    routes::plan_runner_host_route,
};
use crate::{
    case::{declared_names, CaseFile},
    endpoint::{Check, Endpoint, EndpointKind, EndpointOperation, Evidence, Listener, RunnerHostRoute},
    layout::{Layout, Os},
};

/// A consumer's readiness check, with its route chosen at planning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StartGate {
    pub(crate) server: String,
    /// The connected endpoint whose evidence this is; `None` for participant-level evidence.
    pub(crate) endpoint: Option<String>,
    pub(crate) evidence: Evidence,
    pub(crate) check: PlannedCheck,
}

/// Where and how the runner observes readiness evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PlannedCheck {
    /// The route for a TCP probe.
    Accepts(RunnerHostRoute),
    /// The route and path for an HTTP request.
    HttpOk { route: RunnerHostRoute, path: String },
    /// The socket listener path as the server sees it.
    Socket { path: String },
    /// A path in the server's filesystem.
    File { path: String },
}

pub(super) fn check_readiness(
    holder: &str, os: Os, served: &BTreeMap<String, Endpoint>, readiness: &[Evidence],
) -> Result<(), String> {
    for (name, endpoint) in served {
        if let Some(evidence) = &endpoint.ready {
            check_evidence(holder, &format!("endpoint '{name}'"), evidence, served, os)?;
        }
    }
    for evidence in readiness {
        check_evidence(holder, "the participant", evidence, served, os)?;
    }
    Ok(())
}

fn check_evidence(
    holder: &str, owner: &str, evidence: &Evidence, served: &BTreeMap<String, Endpoint>, os: Os,
) -> Result<(), String> {
    let context = format!("{holder} declares {} for {owner}", evidence.describe());
    let target: Option<(&str, &[EndpointKind])> = match &evidence.check {
        Check::TcpAccepts { endpoint } => {
            Some((endpoint, &[EndpointKind::Tcp, EndpointKind::Http, EndpointKind::Grpc]))
        }
        Check::HttpOk { endpoint, .. } => Some((endpoint, &[EndpointKind::Http])),
        Check::SocketExists { endpoint } => Some((endpoint, &[EndpointKind::UnixDatagram, EndpointKind::UnixStream])),
        Check::FileExists { .. } => None,
    };
    if let Some((name, accepts)) = target {
        let endpoint = served.get(name).ok_or_else(|| {
            format!(
                "{context}, but it does not serve endpoint '{name}'; it serves {}",
                declared_names(&served.keys().map(String::as_str).collect::<Vec<_>>()),
            )
        })?;
        if !accepts.contains(&endpoint.kind) {
            return Err(format!(
                "{context}, but endpoint '{name}' is {}; it requires {}",
                endpoint.kind.name(),
                declared_names(&accepts.iter().map(|kind| kind.name()).collect::<Vec<_>>()),
            ));
        }
    }
    match &evidence.check {
        Check::HttpOk { path, .. } => {
            if !path.starts_with('/') || !path.bytes().all(|byte| byte.is_ascii_graphic()) || !holds_no_reference(path)
            {
                return Err(format!(
                    "{context}, but the HTTP path must be an absolute URI path without spaces or references"
                ));
            }
        }
        Check::FileExists { path } => check_absolute_path(path, os).map_err(|error| match error {
            PathError::UnresolvedReference => {
                format!("{context}, but the path holds a reference resolved only after planning")
            }
            PathError::Relative => format!("{context}, but file paths must be absolute"),
        })?,
        _ => {}
    }
    if evidence.within.is_zero() {
        return Err(format!("{context}, but its readiness bound must be nonzero"));
    }
    Ok(())
}

/// Plans connected endpoint evidence in connection-key order, then each distinct server's participant evidence.
pub(super) fn plan_start_gates(
    file: &CaseFile, layout: &Layout, endpoints: &BTreeMap<String, ParticipantEndpoints>,
    ports: &mut RunnerHostPorts<'_>,
) -> Result<BTreeMap<String, Vec<StartGate>>, String> {
    let mut start_gates = BTreeMap::new();
    for (consumer, entry) in &file.participants {
        let mut declarations = Vec::new();
        let mut servers = BTreeSet::new();
        for target in entry.connect.values() {
            let server = &target.participant;
            if server == consumer {
                continue;
            }
            servers.insert(server);
            if let Some(evidence) = &endpoints[server].served[&target.endpoint].ready {
                declarations.push((server, Some(target.endpoint.clone()), evidence));
            }
        }
        for server in servers {
            for evidence in &endpoints[server].readiness {
                declarations.push((server, None, evidence));
            }
        }
        let mut gates = Vec::new();
        for (server, endpoint, evidence) in declarations {
            let context = format!(
                "participant '{consumer}' waits for {} of participant '{server}'",
                evidence.describe(),
            );
            let mut route = |name: &str, operation| {
                plan_runner_host_route(&context, server, name, operation, layout, endpoints, ports)
            };
            let check = match &evidence.check {
                Check::TcpAccepts { endpoint } => PlannedCheck::Accepts(route(endpoint, EndpointOperation::Probe)?),
                Check::HttpOk { endpoint, path } => PlannedCheck::HttpOk {
                    route: route(endpoint, EndpointOperation::Request)?,
                    path: path.clone(),
                },
                Check::SocketExists { endpoint } => {
                    let Listener::Path(path) = &endpoints[server].served[endpoint].listener else {
                        unreachable!("socket evidence names a validated socket listener");
                    };
                    PlannedCheck::Socket { path: path.clone() }
                }
                Check::FileExists { path } => PlannedCheck::File { path: path.clone() },
            };
            let gate = StartGate {
                server: server.clone(),
                endpoint,
                evidence: evidence.clone(),
                check,
            };
            if !gates.contains(&gate) {
                gates.push(gate);
            }
        }
        if !gates.is_empty() {
            start_gates.insert(consumer.clone(), gates);
        }
    }
    Ok(start_gates)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        case::fixtures::minimal_procedure,
        endpoint::{ResolvedAddress, RunnerHostPort, Transport},
        plan::{
            fixtures::{compile, compile_with, planner, CONNECTION_CASE},
            ports::PortRegistry,
            Planner,
        },
    };

    fn ready_case(env: &str) -> String {
        CONNECTION_CASE.replace("  client:", "  consumer:").replace(
            "type: server}",
            &format!("type: ready-server, config: {{env: {{{env}}}}}}}"),
        )
    }

    #[track_caller]
    fn assert_error(yaml: &str, os: Os, detail: &str) {
        assert_eq!(
            compile(yaml, os).err().unwrap().to_string(),
            format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS '{}', {detail}",
                os.name(),
            ),
        );
    }

    fn http_route(host: &str, port: u16) -> ResolvedAddress {
        ResolvedAddress {
            kind: EndpointKind::Http,
            url: format!("http://{host}:{port}"),
            host: Some(host.into()),
            port: Some(port),
            path: None,
        }
    }

    #[test]
    fn endpoint_evidence_must_name_an_endpoint_its_participant_serves() {
        assert_error(
            &ready_case("READY_TCP: missing"),
            Os::Linux,
            "participant 'server' of type 'ready-server' as linux-container on linux declares tcp-accepts on 'missing' for endpoint 'api', but it does not serve endpoint 'missing'; it serves api, metrics",
        );
    }

    #[test]
    fn each_evidence_kind_requires_a_compatible_endpoint_kind() {
        for kind in [
            "tcp",
            "udp",
            "http",
            "grpc",
            "unix-datagram",
            "unix-stream",
            "named-pipe",
        ] {
            for (env, description, owner, accepted) in [
                (
                    "READY_TCP: api",
                    "tcp-accepts on 'api'",
                    "api",
                    &["tcp", "http", "grpc"][..],
                ),
                (
                    "READY_HTTP: /ready",
                    "http-ok on 'api' at '/ready'",
                    "api",
                    &["http"][..],
                ),
                (
                    "READY_SOCKET: /log.sock, READY_SOCKET_TARGET: api",
                    "socket-exists on 'api'",
                    "log",
                    &["unix-datagram", "unix-stream"][..],
                ),
            ] {
                let yaml = ready_case(&format!("{env}, READY_KIND: {kind}"))
                    .replace(", connect: {upstream: server.api}", "")
                    .replace("type: client", "type: every-kind-client");
                if accepted.contains(&kind) {
                    let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
                    assert!(plan.start_gates("consumer").is_empty());
                } else {
                    assert_error(&yaml, Os::Linux, &format!(
                        "participant 'server' of type 'ready-server' as linux-container on linux declares {description} for endpoint '{owner}', but endpoint 'api' is {kind}; it requires {}",
                        accepted.join(", "),
                    ));
                }
            }
        }
    }

    #[test]
    fn an_http_ok_path_must_be_an_absolute_uri_path_without_spaces_or_references() {
        let holder = "participant 'server' of type 'ready-server' as linux-container on linux";
        let constraint = "the HTTP path must be an absolute URI path without spaces or references";
        for path in [
            "ready",
            "",
            "http://server/ready",
            "/a b",
            "/a\tb",
            "/réady",
            "/{{self.late.ip}}",
            "/{{unfinished",
        ] {
            let endpoints = BTreeMap::from([(
                "api".into(),
                Endpoint::http(crate::endpoint::Port::Fixed(8080)).with_ready(Evidence::http_ok("api", path)),
            )]);
            let expected =
                format!("{holder} declares http-ok on 'api' at '{path}' for endpoint 'api', but {constraint}");
            assert_eq!(check_readiness(holder, Os::Linux, &endpoints, &[]), Err(expected));
        }
        for path in ["ready", "", "http://server/ready", "/a b", "/a\tb"] {
            let expected =
                format!("{holder} declares http-ok on 'api' at '{path}' for endpoint 'api', but {constraint}");
            assert_error(&ready_case(&format!("READY_HTTP: '{path}'")), Os::Linux, &expected);
        }
        for path in ["/", "/ready?probe=1", "/a%20b"] {
            let plan = compile(&ready_case(&format!("READY_HTTP: '{path}'")), Os::Linux)
                .unwrap()
                .unwrap();
            assert_eq!(plan.start_gates("consumer")[0].evidence, Evidence::http_ok("api", path));
        }
    }

    #[test]
    fn a_file_evidence_path_must_be_absolute_for_its_participants_os_and_hold_no_reference() {
        for (os, kind, path, valid) in [
            (Os::Linux, "linux-container", "/ready", true),
            (Os::Linux, "linux-container", "ready", false),
            (Os::Linux, "linux-container", r"C:\ready", false),
            (Os::MacOs, "host-process-group", "/ready", true),
            (Os::MacOs, "host-process-group", r"\ready", false),
            (Os::Windows, "windows-container", r"C:\ready", true),
            (Os::Windows, "windows-container", "c:/ready", true),
            (Os::Windows, "windows-container", r"\ready", true),
            (Os::Windows, "windows-container", "/ready", true),
            (Os::Windows, "windows-container", "C:ready", false),
            (Os::Windows, "windows-container", r"1:\ready", false),
            (Os::Windows, "windows-container", "", false),
        ] {
            let yaml = ready_case(&format!("READY_FILE: '{path}'")).replace("[linux]", &format!("[{}]", os.name()));
            if valid {
                let plan = compile(&yaml, os).unwrap().unwrap();
                assert_eq!(
                    plan.start_gates("consumer")[0].check,
                    PlannedCheck::File { path: path.into() }
                );
            } else {
                assert_error(&yaml, os, &format!(
                    "participant 'server' of type 'ready-server' as {kind} on {} declares file-exists at '{path}' for the participant, but file paths must be absolute",
                    os.name(),
                ));
            }
        }
        for (mode, path) in [
            ("late-path", "{{self.late.ip}}/ready"),
            ("malformed-path", "{{unfinished"),
        ] {
            let yaml = ready_case("READY_FILE: /ready").replace(
                "env: {READY_FILE",
                &format!("params: {{mode: {mode}}}, env: {{READY_FILE"),
            );
            assert_error(&yaml, Os::Linux, &format!(
                "participant 'server' of type 'ready-server' as linux-container on linux declares file-exists at '{path}' for the participant, but the path holds a reference resolved only after planning",
            ));
        }
        let plan = compile(&ready_case("READY_FILE: '{{self.facts.shared_dir}}/ready'"), Os::Linux)
            .unwrap()
            .unwrap();
        assert_eq!(
            plan.start_gates("consumer")[0].check,
            PlannedCheck::File {
                path: "/airlock/server/shared/ready".into()
            }
        );
    }

    #[test]
    fn evidence_with_a_zero_bound_is_rejected() {
        for (env, description, owner) in [
            ("READY_TCP: api", "tcp-accepts on 'api'", "endpoint 'api'"),
            ("READY_HTTP: /ready", "http-ok on 'api' at '/ready'", "endpoint 'api'"),
            ("READY_SOCKET: /log.sock", "socket-exists on 'log'", "endpoint 'log'"),
            ("READY_FILE: /ready", "file-exists at '/ready'", "the participant"),
        ] {
            assert_error(&ready_case(&format!("{env}, READY_WITHIN: '0'")), Os::Linux, &format!(
                "participant 'server' of type 'ready-server' as linux-container on linux declares {description} for {owner}, but its readiness bound must be nonzero",
            ));
        }
        let plan = compile(
            &ready_case("READY_TCP: api, READY_FILE: /ready, READY_WITHIN: '2'"),
            Os::Linux,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            plan.start_gates("consumer")
                .iter()
                .map(|gate| gate.evidence.within)
                .collect::<Vec<_>>(),
            [Duration::from_secs(2); 2]
        );
    }

    #[test]
    fn readiness_errors_follow_listener_errors_and_precede_connection_errors() {
        let mut yaml = ready_case("PORT: '0', READY_TCP: missing, READY_SOCKET: /log.sock, READY_SOCKET_TARGET: missing, READY_FILE: relative")
            .replace("upstream: server.api", "upstream: server.missing");
        for (bad, fix, detail) in [
            ("PORT: '0'", "PORT: '8080'", "serves endpoint 'api' on port 0; a listener needs a port from 1 to 65535"),
            ("READY_TCP: missing", "READY_TCP: api", "declares tcp-accepts on 'missing' for endpoint 'api', but it does not serve endpoint 'missing'; it serves api, log, metrics"),
            ("READY_SOCKET_TARGET: missing", "READY_SOCKET_TARGET: log", "declares socket-exists on 'missing' for endpoint 'log', but it does not serve endpoint 'missing'; it serves api, log, metrics"),
            ("READY_FILE: relative", "READY_FILE: /ready", "declares file-exists at 'relative' for the participant, but file paths must be absolute"),
        ] {
            assert_error(&yaml, Os::Linux, &format!("participant 'server' of type 'ready-server' as linux-container on linux {detail}"));
            yaml = yaml.replace(bad, fix);
        }
        assert_error(&yaml, Os::Linux, "participant 'consumer' connects 'upstream' to 'server.missing', which participant 'server' of type 'ready-server' as linux-container on linux does not serve; it serves api, log, metrics");
    }

    #[test]
    fn a_consumer_gates_on_each_connected_endpoints_evidence_then_on_its_servers_participant_evidence() {
        let yaml = concat!(
            r#"timeout: 1s
subject_os: [linux]
participants:
  consumer:
    type: ready-server
    subject: true
    connect: {z: alpha.api, c: zeta.api, b: zeta.log, a: zeta.api}
  alpha: {type: ready-server, config: {env: {READY_TCP: api, READY_FILE: /a}}}
  zeta: {type: ready-server, config: {env: {READY_TCP: api, READY_SOCKET: '{{self.facts.shared_dir}}/log.sock', READY_FILE: /z2, READY_FILE_2: /z1}}}
"#,
            minimal_procedure!()
        );
        let plan = compile(yaml, Os::Linux).unwrap().unwrap();
        let gate = |server: &str, endpoint: Option<&str>, evidence, check| StartGate {
            server: server.into(),
            endpoint: endpoint.map(str::to_string),
            evidence,
            check,
        };
        assert_eq!(
            plan.start_gates("consumer"),
            [
                gate(
                    "zeta",
                    Some("api"),
                    Evidence::tcp_accepts("api"),
                    PlannedCheck::Accepts(RunnerHostRoute::Courier(http_route("zeta", 8080)))
                ),
                gate(
                    "zeta",
                    Some("log"),
                    Evidence::socket_exists("log"),
                    PlannedCheck::Socket {
                        path: "/airlock/zeta/shared/log.sock".into()
                    }
                ),
                gate(
                    "alpha",
                    Some("api"),
                    Evidence::tcp_accepts("api"),
                    PlannedCheck::Accepts(RunnerHostRoute::Courier(http_route("alpha", 8080)))
                ),
                gate(
                    "alpha",
                    None,
                    Evidence::file_exists("/a"),
                    PlannedCheck::File { path: "/a".into() }
                ),
                gate(
                    "zeta",
                    None,
                    Evidence::file_exists("/z2"),
                    PlannedCheck::File { path: "/z2".into() }
                ),
                gate(
                    "zeta",
                    None,
                    Evidence::file_exists("/z1"),
                    PlannedCheck::File { path: "/z1".into() }
                ),
            ]
        );
        assert_eq!(plan.start_gates.keys().collect::<Vec<_>>(), ["consumer"]);
        assert_eq!(plan.start_order().unwrap(), [vec!["alpha", "zeta"], vec!["consumer"]]);
        assert!(plan.runner_host_ports("zeta").is_empty());
    }

    #[test]
    fn an_endpoint_without_evidence_adds_no_gate() {
        let plan = compile(&ready_case(""), Os::Linux).unwrap().unwrap();
        assert!(plan.start_gates.is_empty());
        for name in ["consumer", "server", "unknown"] {
            assert!(plan.start_gates(name).is_empty());
        }
        let typed = compile("type: fixture", Os::MacOs).unwrap().unwrap();
        assert!(typed.start_gates("consumer").is_empty());
        let plan = compile(&ready_case("READY_FILE: /ready"), Os::Linux).unwrap().unwrap();
        assert_eq!(plan.start_gates("consumer").len(), 1);
        assert_eq!(plan.start_gates("consumer")[0].endpoint, None);
    }

    #[test]
    fn a_participant_connecting_to_its_own_endpoint_does_not_wait_for_itself() {
        let yaml = ready_case("READY_HTTP: /ready, READY_FILE: /ready")
            .replace(
                "  consumer: {type: client, subject: true, connect: {upstream: server.api}}\n",
                "",
            )
            .replace(
                "type: ready-server,",
                "type: ready-server, subject: true, connect: {a: server.api},",
            );
        let mut planner = planner();
        let plan = compile_with(&mut planner, &yaml, Os::Linux).unwrap().unwrap();
        assert!(plan.start_gates.is_empty());
        assert!(plan.runner_host_ports("server").is_empty());
        assert_eq!(plan.start_order().unwrap(), [vec!["server"]]);
        assert_eq!(planner.ports.allocate(), Ok(20000));
    }

    #[test]
    fn http_evidence_on_a_linux_container_draws_a_published_runner_host_port_shared_with_steps() {
        let yaml = ready_case("READY_HTTP: /ready").replace(
            minimal_procedure!(),
            "procedure: [{step: reach, endpoint: server.api, operation: request}]",
        );
        let mut planner = planner();
        let plan = compile_with(&mut planner, &yaml, Os::Linux).unwrap().unwrap();
        let route = RunnerHostRoute::Direct(http_route("127.0.0.1", 20000));
        assert_eq!(
            plan.start_gates("consumer"),
            [StartGate {
                server: "server".into(),
                endpoint: Some("api".into()),
                evidence: Evidence::http_ok("api", "/ready"),
                check: PlannedCheck::HttpOk {
                    route: route.clone(),
                    path: "/ready".into()
                },
            }]
        );
        assert_eq!(
            plan.runner_host_route("server", "api", EndpointOperation::Request),
            Some(&route)
        );
        assert_eq!(
            plan.runner_host_ports("server"),
            [RunnerHostPort {
                transport: Transport::Tcp,
                container_port: 8080,
                host_port: 20000
            }]
        );
        assert_eq!(planner.ports.allocate(), Ok(20001));
    }

    #[test]
    fn tcp_evidence_on_a_linux_container_probes_through_the_servers_courier_and_on_a_host_process_group_directly() {
        for (os, route) in [
            (Os::Linux, RunnerHostRoute::Courier(http_route("server", 8080))),
            (Os::MacOs, RunnerHostRoute::Direct(http_route("127.0.0.1", 8080))),
        ] {
            let yaml = ready_case("READY_TCP: api").replace("[linux]", &format!("[{}]", os.name()));
            let mut planner = planner();
            let plan = compile_with(&mut planner, &yaml, os).unwrap().unwrap();
            assert_eq!(plan.start_gates("consumer")[0].check, PlannedCheck::Accepts(route));
            assert!(plan.runner_host_ports("server").is_empty());
            assert_eq!(planner.ports.allocate(), Ok(20000));
        }
    }

    #[test]
    fn evidence_of_an_unconnected_server_draws_no_runner_host_port() {
        let yaml = ready_case("READY_HTTP: /ready, READY_FILE: /ready")
            .replace(", connect: {upstream: server.api}", "")
            .replace("type: client", "type: every-kind-client");
        let mut planner = planner();
        let plan = compile_with(&mut planner, &yaml, Os::Linux).unwrap().unwrap();
        assert!(plan.start_gates.is_empty());
        assert!(plan.runner_host_ports("server").is_empty());
        assert_eq!(planner.ports.allocate(), Ok(20000));
    }

    #[test]
    fn endpoint_evidence_may_name_another_endpoint_of_the_same_participant() {
        let yaml = ready_case("READY_HTTP: /ready, READY_ON: metrics")
            .replace("type: client,", "type: every-kind-client,")
            .replace("upstream: server.api", "target: server.metrics");
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(
            plan.start_gates("consumer"),
            [StartGate {
                server: "server".into(),
                endpoint: Some("metrics".into()),
                evidence: Evidence::http_ok("api", "/ready"),
                check: PlannedCheck::HttpOk {
                    route: RunnerHostRoute::Direct(http_route("127.0.0.1", 20000)),
                    path: "/ready".into()
                },
            }]
        );
        assert_eq!(
            plan.runner_host_ports("server"),
            [RunnerHostPort {
                transport: Transport::Tcp,
                container_port: 8080,
                host_port: 20000
            }]
        );
    }

    #[test]
    fn configuration_endpoint_references_add_no_start_gates() {
        let yaml = ready_case("READY_HTTP: /ready, READY_FILE: /ready")
            .replace("type: client", "type: every-kind-client")
            .replace(
                ", connect: {upstream: server.api}",
                ", config: {env: {REF: '{{server.endpoints.api.url}}'}}",
            );
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert!(plan.start_gates.is_empty());
        assert_eq!(plan.start_order().unwrap(), [vec!["consumer", "server"]]);
        assert!(plan.runner_host_ports("server").is_empty());
    }

    #[test]
    fn cyclic_start_dependencies_fail_before_readiness_draws_any_port() {
        let yaml = ready_case("READY_HTTP: /ready")
            .replace(
                "type: client,",
                "type: ready-server, config: {env: {READY_HTTP: /ready}},",
            )
            .replace("upstream: server.api", "a: server.api")
            .replace(
                "  server: {type: ready-server,",
                "  server: {type: ready-server, connect: {a: consumer.api},",
            );
        let mut planner = planner();
        assert_eq!(compile_with(&mut planner, &yaml, Os::Linux).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participants can't be started in any order: 'consumer' must be started before 'server' because participant 'server' connects 'a' to 'consumer.api'; 'server' must be started before 'consumer' because participant 'consumer' connects 'a' to 'server.api'");
        assert_eq!(planner.ports.allocate(), Ok(20000));
    }

    #[test]
    fn readiness_sees_the_configuration_after_listener_port_assignment() {
        let yaml = super::super::fixtures::assigned_server_case("{params: {mode: readiness-after-assignment}}");
        let plan = compile(&yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.start_gates("client"),
            [StartGate {
                server: "server".into(),
                endpoint: None,
                evidence: Evidence::file_exists("/ports/20000"),
                check: PlannedCheck::File {
                    path: "/ports/20000".into()
                },
            }]
        );
    }

    #[test]
    fn repeated_participant_evidence_adds_one_gate_per_consumer() {
        let plan = compile(&ready_case("READY_FILE: /ready, READY_FILE_2: /ready"), Os::Linux)
            .unwrap()
            .unwrap();
        assert_eq!(
            plan.start_gates("consumer"),
            [StartGate {
                server: "server".into(),
                endpoint: None,
                evidence: Evidence::file_exists("/ready"),
                check: PlannedCheck::File { path: "/ready".into() },
            }]
        );
    }

    #[test]
    fn gates_draw_ports_in_consumer_and_connection_key_order_before_step_needs() {
        let yaml = r#"timeout: 1s
subject_os: [linux]
participants:
  z: {type: ready-server, connect: {a: one.api}}
  a: {type: ready-server, subject: true, connect: {z: two.api, a: three.api}}
  one: {type: ready-server, config: {env: {READY_HTTP: /ready}}}
  two: {type: ready-server, config: {env: {READY_HTTP: /ready}}}
  three: {type: ready-server, config: {env: {READY_HTTP: /ready}}}
  four: {type: ready-server}
procedure: [{step: reach, endpoint: four.api, operation: request}]
"#;
        let mut planner = planner();
        let plan = compile_with(&mut planner, yaml, Os::Linux).unwrap().unwrap();
        for (server, port) in [("three", 20000), ("two", 20001), ("one", 20002), ("four", 20003)] {
            assert_eq!(
                plan.runner_host_ports(server),
                [RunnerHostPort {
                    transport: Transport::Tcp,
                    container_port: 8080,
                    host_port: port
                }]
            );
        }
        assert_eq!(
            plan.start_gates("a")
                .iter()
                .map(|gate| gate.server.as_str())
                .collect::<Vec<_>>(),
            ["three", "two"]
        );
        assert_eq!(plan.start_gates("z")[0].server, "one");
        assert_eq!(planner.ports.allocate(), Ok(20004));
    }

    #[test]
    fn readiness_is_checked_on_unselected_subject_operating_systems() {
        let yaml = ready_case("")
            .replace("[linux]", "[linux, windows]")
            .replace("env: {}", "os: {windows: {env: {READY_FILE: relative}}}");
        assert_error(&yaml, Os::Windows, "participant 'server' of type 'ready-server' as windows-container on windows declares file-exists at 'relative' for the participant, but file paths must be absolute");
        for os in [Os::Linux, Os::MacOs] {
            assert_eq!(
                compile(&yaml, os).err().unwrap().to_string(),
                compile(&yaml, Os::Windows).err().unwrap().to_string()
            );
        }
    }

    #[test]
    fn a_gate_port_allocation_failure_names_the_consumer_server_and_evidence() {
        let mut planner = Planner {
            ports: PortRegistry::over(21000..=21000),
            ..planner()
        };
        assert_eq!(planner.ports.allocate(), Ok(21000));
        assert_eq!(compile_with(&mut planner, &ready_case("READY_HTTP: /ready"), Os::Linux).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'consumer' waits for http-ok on 'api' at '/ready' of participant 'server', which needs a runner-host port: all 1 runner-host ports are allocated");
    }
}
