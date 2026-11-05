//! Run-wide allocation of runner-host ports.

use std::{
    collections::{btree_map::Entry, BTreeMap},
    ops::RangeInclusive,
};

use super::endpoints::ParticipantEndpoints;
use crate::{
    case::CaseFile,
    endpoint::{Listener, Port},
    layout::{describe_participant, Layout, RealizationKind},
};

const RUNNER_HOST_PORTS: RangeInclusive<u16> = 20000..=29999;

/// Records runner-host port numbers handed out during planning.
///
/// Ports come from `20000..=29999`, below every default ephemeral range (Linux 32768+,
/// macOS and Windows 49152+), so the OS doesn't hand these ports to outgoing connections.
pub(crate) struct PortRegistry {
    available: RangeInclusive<u16>,
    capacity: usize,
}

impl PortRegistry {
    /// Creates an empty allocation registry over `20000..=29999`.
    pub(crate) fn new() -> Self {
        Self::over(RUNNER_HOST_PORTS)
    }

    pub(crate) fn allocate(&mut self) -> Result<u16, String> {
        self.available
            .next()
            .ok_or_else(|| format!("all {} runner-host ports are allocated", self.capacity))
    }

    pub(super) fn over(range: RangeInclusive<u16>) -> Self {
        let capacity = range.clone().count();
        Self {
            available: range,
            capacity,
        }
    }
}

/// Connections, configuration references and step needs reaching one endpoint through a runner-host port
/// share that port within a case and OS.
pub(super) struct RunnerHostPorts<'a> {
    registry: &'a mut PortRegistry,
    ports: BTreeMap<(String, String), u16>,
}

impl<'a> RunnerHostPorts<'a> {
    pub(super) fn new(registry: &'a mut PortRegistry) -> Self {
        Self {
            registry,
            ports: BTreeMap::new(),
        }
    }

    pub(super) fn for_endpoint(&mut self, server: &str, endpoint: &str) -> Result<u16, String> {
        let entry = self.ports.entry((server.into(), endpoint.into()));
        match entry {
            Entry::Occupied(entry) => Ok(*entry.get()),
            Entry::Vacant(entry) => Ok(*entry.insert(self.registry.allocate()?)),
        }
    }

    pub(super) fn into_ports(self) -> BTreeMap<(String, String), u16> {
        self.ports
    }
}

/// Checks host-process-group network listeners in participant and endpoint name order.
/// Rejects unassigned ports inside `RUNNER_HOST_PORTS` and listeners sharing a transport and port.
pub(super) fn check_host_listeners(
    file: &CaseFile, layout: &Layout, endpoints: &BTreeMap<String, ParticipantEndpoints>,
) -> Result<(), String> {
    let mut listeners = BTreeMap::new();
    for (name, realization) in layout.iter() {
        if realization.kind != RealizationKind::HostProcessGroup {
            continue;
        }
        let holder = describe_participant(name, &file.participants[name].participant_type, realization);
        let participant = &endpoints[name];
        for (endpoint_name, endpoint) in &participant.served {
            let Some(transport) = endpoint.kind.runner_host_transport() else {
                continue;
            };
            let Listener::Port(Port::Fixed(port)) = endpoint.listener else {
                unreachable!("network listener ports are resolved before host listener checks");
            };
            let transport_name = transport.name();
            if RUNNER_HOST_PORTS.contains(&port) && !participant.assigned_ports.contains_key(endpoint_name) {
                return Err(format!(
                    "{holder} serves endpoint '{endpoint_name}' on runner-host {transport_name} port {port}, inside {}-{}, where panoramic assigns ports; use a port outside that range",
                    RUNNER_HOST_PORTS.start(), RUNNER_HOST_PORTS.end(),
                ));
            }
            if let Some((earlier_name, earlier_endpoint)) = listeners.insert((transport, port), (name, endpoint_name)) {
                return Err(format!(
                    "{holder} serves endpoint '{endpoint_name}' on runner-host {transport_name} port {port}, which participant '{earlier_name}' already serves as endpoint '{earlier_endpoint}'"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        case::fixtures::{minimal_procedure, PARTICIPANT_FIELDS},
        endpoint::{EndpointKind, Transport},
        layout::Os,
        plan::{
            fixtures::{assigned_server_case, compile, compile_with, planner, CONNECTION_CASE},
            Planner,
        },
    };

    #[test]
    fn network_endpoint_kinds_map_to_their_runner_host_transport() {
        for (kind, expected) in [
            (EndpointKind::Tcp, Some(Transport::Tcp)),
            (EndpointKind::Http, Some(Transport::Tcp)),
            (EndpointKind::Grpc, Some(Transport::Tcp)),
            (EndpointKind::Udp, Some(Transport::Udp)),
            (EndpointKind::UnixDatagram, None),
            (EndpointKind::UnixStream, None),
            (EndpointKind::NamedPipe, None),
        ] {
            assert_eq!(kind.runner_host_transport(), expected);
        }
    }

    #[test]
    fn the_registry_never_hands_out_a_port_twice() {
        let mut registry = PortRegistry::new();
        for expected in 20000..=29999 {
            assert_eq!(registry.allocate(), Ok(expected));
        }
    }

    #[test]
    fn the_registry_allocates_below_every_default_ephemeral_range() {
        let registry = PortRegistry::new();
        for start in [32768, 49152] {
            assert!(*registry.available.start() < start);
            assert!(*registry.available.end() < start);
        }
    }

    #[test]
    fn an_exhausted_registry_reports_how_many_ports_it_had() {
        let mut registry = PortRegistry::over(21000..=21001);
        assert_eq!(registry.allocate(), Ok(21000));
        assert_eq!(registry.allocate(), Ok(21001));
        for _ in 0..2 {
            assert_eq!(registry.allocate(), Err("all 2 runner-host ports are allocated".into()));
        }
    }

    #[test]
    fn runner_host_port_exhaustion_in_a_step_names_the_step() {
        let yaml = PARTICIPANT_FIELDS.replace(
            minimal_procedure!(),
            "procedure: [{step: reach, endpoint: subject.http, operation: request}, {parallel: [{step: reach, endpoint: subject.grpc, operation: request}]}]",
        );
        let yaml = yaml.replace("type: server", "type: every-kind-server");
        let mut planner = Planner {
            ports: PortRegistry::over(21000..=21000),
            ..planner()
        };
        assert_eq!(compile_with(&mut planner, &yaml, Os::Linux).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', step 2.1 'reach' reaches endpoint 'grpc' of participant 'subject', which needs a runner-host port: all 1 runner-host ports are allocated");
    }

    #[test]
    fn a_fixed_case_supplied_or_pinned_host_port_inside_the_runner_host_range_fails() {
        for (participant_type, config, endpoint, port) in [
            ("server", "{env: {PORT: '20000'}}", "api", 20000),
            ("server", "{endpoints: {extra: {port: 25000}}}", "extra", 25000),
            ("assigned-server", "{env: {API_PORT: '29999'}}", "api", 29999),
        ] {
            let yaml =
                assigned_server_case(config).replace("type: assigned-server", &format!("type: {participant_type}"));
            // A custom registry must not change the reserved runner-host range.
            let mut planner = Planner {
                ports: PortRegistry::over(21000..=21010),
                ..planner()
            };
            assert_eq!(compile_with(&mut planner, &yaml, Os::MacOs).err().unwrap().to_string(), format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'server' of type '{participant_type}' as host-process-group on macos serves endpoint '{endpoint}' on runner-host tcp port {port}, inside 20000-29999, where panoramic assigns ports; use a port outside that range"));
            for boundary in [19999, 30000] {
                let plan = compile(&yaml.replace(&port.to_string(), &boundary.to_string()), Os::MacOs)
                    .unwrap()
                    .unwrap();
                if endpoint == "api" {
                    assert_eq!(
                        plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
                        format!("http://127.0.0.1:{boundary}")
                    );
                } else {
                    assert_eq!(plan.effective_config("server").unwrap().endpoints["extra"], boundary);
                }
            }
        }
        let yaml = assigned_server_case("{env: {METRICS_PORT: '25000'}}");
        assert!(compile(&yaml, Os::MacOs)
            .err()
            .unwrap()
            .to_string()
            .contains("endpoint 'metrics' on runner-host udp port 25000, inside 20000-29999"));
    }

    #[test]
    fn container_ports_inside_the_runner_host_range_are_accepted() {
        let yaml = CONNECTION_CASE.replace(
            "type: server}",
            "type: server, config: {env: {PORT: '20000'}, endpoints: {extra: {port: 25000}}}}",
        );
        let mut planner = planner();
        let plan = compile_with(&mut planner, &yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
            "http://server:20000"
        );
        assert_eq!(plan.effective_config("server").unwrap().endpoints["extra"], 25000);
        assert_eq!(planner.ports.allocate(), Ok(20000));
    }

    #[test]
    fn two_host_process_groups_serving_one_tcp_port_fail_naming_both() {
        for (other, config, port, earlier) in [
            ("server", "{}", 8080, "api"),
            ("every-kind-server", "{env: {PORT: '1004'}}", 1004, "grpc"),
        ] {
            let yaml = format!("timeout: 1s\nsubject_os: [macos]\nparticipants:\n  z: {{type: server, subject: true, config: {config}}}\n  a: {{type: {other}}}\n{}\n", minimal_procedure!());
            assert_eq!(compile(&yaml, Os::MacOs).err().unwrap().to_string(), format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'z' of type 'server' as host-process-group on macos serves endpoint 'api' on runner-host tcp port {port}, which participant 'a' already serves as endpoint '{earlier}'"));
        }
    }

    #[test]
    fn two_endpoints_of_one_host_process_group_on_one_port_and_transport_fail() {
        let yaml = CONNECTION_CASE.replace("[linux]", "[macos]").replace(
            "type: server}",
            "type: server, config: {endpoints: {extra: {port: 8080}}}}",
        );
        assert_eq!(compile(&yaml, Os::MacOs).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'server' of type 'server' as host-process-group on macos serves endpoint 'extra' on runner-host tcp port 8080, which participant 'server' already serves as endpoint 'api'");
    }

    #[test]
    fn a_tcp_and_a_udp_host_listener_may_share_a_port_number() {
        let yaml = CONNECTION_CASE
            .replace("[linux]", "[macos]")
            .replace("type: server}", "type: server, config: {env: {PORT: '9000'}}}")
            .replace("upstream: server.api", "upstream: server.api, side: server.metrics");
        let mut planner = planner();
        let plan = compile_with(&mut planner, &yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env,
            BTreeMap::from([
                ("UPSTREAM_URL".into(), "http://127.0.0.1:9000".into()),
                ("SIDE_HOST".into(), "127.0.0.1".into()),
                ("SIDE_PORT".into(), "9000".into()),
            ])
        );
        assert_eq!(planner.ports.allocate(), Ok(20000));
    }

    #[test]
    fn host_port_conflicts_fail_on_an_unselected_subject_os() {
        let yaml = concat!(
            r#"timeout: 1s
subject_os: [linux, macos]
participants:
  a: {type: server, subject: true}
  z: {type: server, config: {env: {PORT: '8081', METRICS: 'off'}, os: {macos: {env: {PORT: '8080'}}}}}
"#,
            minimal_procedure!(),
            "\n"
        );
        assert_eq!(compile(yaml, Os::Linux).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'z' of type 'server' as host-process-group on macos serves endpoint 'api' on runner-host tcp port 8080, which participant 'a' already serves as endpoint 'api'");
    }
}
