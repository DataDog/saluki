//! Validates connections, resolves each one to the address its consumer sees and computes
//! the consumer's delivered environment.

use std::collections::BTreeMap;

use super::{
    endpoints::ParticipantEndpoints,
    ports::RunnerHostPorts,
    routes::{self, Unreachable},
    PlannedParticipant,
};
use crate::{
    case::{declared_names, CaseFile},
    endpoint::Delivery,
    layout::{describe_participant, Layout},
};

/// Checks each consumer's connections, then its required endpoints, all in name order.
/// Resolves each connection's address, drawing runner-host ports from `ports` in check order,
/// and stores each consumer's addresses and delivered environment variables.
pub(super) fn check_connections(
    file: &CaseFile, layout: &Layout, endpoints: &BTreeMap<String, ParticipantEndpoints>,
    participants: &mut BTreeMap<String, PlannedParticipant>, ports: &mut RunnerHostPorts<'_>,
) -> Result<(), String> {
    for (name, entry) in &file.participants {
        let realization = layout.get(name).expect("loaded participants have a layout");
        let type_name = &entry.participant_type;
        let holder = describe_participant(name, type_name, realization);
        let participant = participants.get_mut(name).expect("consumers are planned participants");
        let consumer_endpoints = &endpoints[name];
        let consumes = &consumer_endpoints.consumed;
        let mut delivered_keys = BTreeMap::new();
        let mut delivered_env = BTreeMap::new();
        for (connection, target) in &entry.connect {
            let Some(endpoint) = consumes.get(connection) else {
                return Err(format!(
                    "{holder} connects '{connection}', which it does not consume; it consumes {}",
                    declared_names(&consumes.keys().map(String::as_str).collect::<Vec<_>>()),
                ));
            };
            let target_name = &target.participant;
            let target_endpoint = &target.endpoint;
            let target_serves = &endpoints[target_name].served;
            let target_realization = layout.get(target_name).expect("connection targets have a layout");
            let target_type = &file.participants[target_name].participant_type;
            let target_holder = describe_participant(target_name, target_type, target_realization);
            let Some(served_endpoint) = target_serves.get(target_endpoint) else {
                return Err(format!(
                    "participant '{name}' connects '{connection}' to '{target_name}.{target_endpoint}', which {target_holder} does not serve; it serves {}",
                    declared_names(&target_serves.keys().map(String::as_str).collect::<Vec<_>>()),
                ));
            };
            if !endpoint.accepts.contains(&served_endpoint.kind) {
                return Err(format!(
                    "participant '{name}' connects '{connection}' to '{target_name}.{target_endpoint}', an endpoint of kind {}; '{connection}' of type '{type_name}' accepts {}",
                    served_endpoint.kind.name(),
                    declared_names(&endpoint.accepts.iter().map(|kind| kind.name()).collect::<Vec<_>>()),
                ));
            }
            let route = routes::reach(
                name, realization, target_name, target_realization, &endpoints[target_name], served_endpoint,
            ).map_err(|error| match error {
                Unreachable::NoRoute(constraint) => format!(
                    "participant '{name}' connects '{connection}' to '{target_name}.{target_endpoint}', an endpoint of kind {}; {holder} cannot reach {target_holder}: {constraint}",
                    served_endpoint.kind.name(),
                ),
                Unreachable::Socket { path, reason } => format!(
                    "participant '{name}' connects '{connection}' to '{target_name}.{target_endpoint}', a {} socket that {target_holder} serves at '{path}'{reason}",
                    served_endpoint.kind.name(),
                ),
            })?;
            let Delivery::Env { keys, value } = &endpoint.delivery;
            for key in keys {
                if consumer_endpoints.values.env.contains_key(key) {
                    return Err(format!(
                        "{holder} sets env '{key}', which its connection '{connection}' delivers; remove one of them"
                    ));
                }
                if let Some(endpoint) = consumer_endpoints.assigned_keys.get(key) {
                    return Err(format!(
                        "{holder} delivers env '{key}' through connection '{connection}', which also carries the assigned port of its endpoint '{endpoint}'"
                    ));
                }
                if let Some(earlier) = delivered_keys.insert(key, connection) {
                    return Err(format!(
                        "{holder} delivers env '{key}' through both '{earlier}' and '{connection}'"
                    ));
                }
            }
            let resolved = routes::resolve(route, target_name, target_endpoint, served_endpoint, ports).map_err(|error| {
                format!("{holder} cannot resolve connection '{connection}' to '{target_name}.{target_endpoint}': {error}")
            })?;
            let env = value(&resolved);
            if !env.keys().eq(keys.iter()) {
                return Err(format!(
                    "{holder} delivers connection '{connection}' as env {}, but its type declares {}",
                    declared_names(&env.keys().map(String::as_str).collect::<Vec<_>>()),
                    declared_names(&keys.iter().map(String::as_str).collect::<Vec<_>>()),
                ));
            }
            delivered_env.extend(env);
            participant.connections.insert(connection.clone(), resolved);
        }
        for (connection, endpoint) in consumes {
            if endpoint.required && !entry.connect.contains_key(connection) {
                return Err(format!(
                    "{holder} does not connect required endpoint '{connection}'; add '{connection}: <participant>.<endpoint>' to its 'connect'"
                ));
            }
        }
        participant.config.env.extend(delivered_env);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::{
        case::fixtures::minimal_procedure,
        endpoint::{EndpointKind, ResolvedAddress},
        layout::Os,
        plan::{
            fixtures::{
                assigned_server_case, compile, compile_files, compile_with, host_to_container_case,
                host_to_container_reference_case, planner, CONNECTION_CASE, FILE_CONFIG,
            },
            Planner, PortRegistry,
        },
    };

    #[test]
    fn a_connection_stores_the_address_its_delivery_received() {
        for (yaml, os, host, port) in [
            (CONNECTION_CASE.to_string(), Os::Linux, "server", 8080),
            (
                CONNECTION_CASE.replace("[linux]", "[macos]"),
                Os::MacOs,
                "127.0.0.1",
                8080,
            ),
            (host_to_container_case(), Os::MacOs, "127.0.0.1", 20000),
        ] {
            let reference = "{{ self.connections.upstream.url }}";
            let yaml = yaml.replace(
                "subject: true",
                &format!("subject: true, config: {{env: {{KEY: '{reference}'}}}}"),
            );
            let plan = compile(&yaml, os).unwrap().unwrap();
            let address = plan.connection_address("client", "upstream").unwrap();
            assert_eq!(
                address,
                &ResolvedAddress {
                    kind: EndpointKind::Http,
                    url: format!("http://{host}:{port}"),
                    host: Some(host.into()),
                    port: Some(port),
                    path: None,
                }
            );
            let config = plan.effective_config("client").unwrap();
            assert_eq!(address.url, config.env["UPSTREAM_URL"]);
            assert_eq!(config.env["KEY"], reference);
            assert!(plan.participants.as_ref().unwrap().planned.participants["client"]
                .endpoint_references
                .is_empty());
            assert_eq!(plan.connection_address("client", "side"), None);
            assert_eq!(plan.connection_address("server", "upstream"), None);
            assert_eq!(plan.connection_address("missing", "upstream"), None);
        }
    }

    #[test]
    fn a_connection_and_a_reference_from_one_holder_to_one_endpoint_store_the_same_address() {
        let yaml = host_to_container_case().replace(
            "subject: true",
            "subject: true, config: {env: {KEY: '{{ server.endpoints.api.url }} {{ self.connections.upstream.port }}'}}",
        );
        let mut planner = planner();
        let plan = compile_with(&mut planner, &yaml, Os::MacOs).unwrap().unwrap();
        let address = plan.connection_address("client", "upstream").unwrap();
        assert_eq!(
            address,
            &ResolvedAddress {
                kind: EndpointKind::Http,
                url: "http://127.0.0.1:20000".into(),
                host: Some("127.0.0.1".into()),
                port: Some(20000),
                path: None,
            }
        );
        assert_eq!(plan.endpoint_address("client", "server", "api"), Some(address));
        assert_eq!(
            plan.effective_config("client").unwrap().env["KEY"],
            "{{ server.endpoints.api.url }} {{ self.connections.upstream.port }}"
        );
        assert_eq!(planner.ports.allocate(), Ok(20001));
    }

    #[test]
    fn a_connection_delivering_an_assigned_port_key_fails() {
        let yaml = assigned_server_case("{params: {mode: consume-assigned-key}}")
            .replace(
                "type: assigned-server,",
                "type: assigned-server, connect: {upstream: other.api},",
            )
            .replace(
                minimal_procedure!(),
                concat!("  other: {type: server}\n", minimal_procedure!()),
            );
        assert_eq!(compile(&yaml, Os::MacOs).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'server' of type 'assigned-server' as host-process-group on macos delivers env 'API_PORT' through connection 'upstream', which also carries the assigned port of its endpoint 'api'");
        let yaml = yaml.replace(
            "mode: consume-assigned-key}",
            "mode: consume-assigned-key}, env: {API_PORT: '8081'}",
        );
        assert!(compile(&yaml, Os::MacOs)
            .err()
            .unwrap()
            .to_string()
            .contains("sets env 'API_PORT', which its connection 'upstream' delivers"));
    }

    #[test]
    fn a_connection_between_linux_containers_delivers_the_servers_name_and_port() {
        for port in [8080, 8123] {
            let yaml = CONNECTION_CASE.replace(
                "type: server}",
                &format!("type: server, config: {{env: {{PORT: '{port}'}}}}}}"),
            );
            let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
            assert_eq!(
                plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
                format!("http://server:{port}")
            );
        }
    }

    #[test]
    fn a_connection_between_host_process_groups_delivers_a_loopback_address() {
        let plan = compile(&CONNECTION_CASE.replace("[linux]", "[macos]"), Os::MacOs)
            .unwrap()
            .unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn a_host_process_group_reaches_a_linux_container_only_through_an_allocated_runner_host_port() {
        let plan = compile(&host_to_container_case(), Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
            "http://127.0.0.1:20000"
        );
    }

    #[test]
    fn a_linux_container_cannot_reach_a_host_process_group_and_no_other_route_is_tried() {
        let yaml = CONNECTION_CASE.replace("type: server}", "type: server, os: macos}");
        let mut planner = planner();
        assert_eq!(compile_with(&mut planner, &yaml, Os::Linux).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' connects 'upstream' to 'server.api', an endpoint of kind http; participant 'client' of type 'client' as linux-container on linux cannot reach participant 'server' of type 'server' as host-process-group on macos: network endpoints connect only participants with the same realization, or a host-process-group to a linux-container");
        assert_eq!(planner.ports.allocate(), Ok(20000));
    }

    #[test]
    fn consumers_of_one_container_endpoint_share_its_runner_host_port() {
        let yaml = host_to_container_case().replace(
            "participants:",
            "participants:\n  other: {type: client, connect: {upstream: server.api}}",
        );
        let mut planner = planner();
        let plan = compile_with(&mut planner, &yaml, Os::MacOs).unwrap().unwrap();
        for name in ["client", "other"] {
            assert_eq!(
                plan.effective_config(name).unwrap().env["UPSTREAM_URL"],
                "http://127.0.0.1:20000"
            );
        }
        assert_eq!(
            plan.participants.as_ref().unwrap().planned.runner_host_ports,
            BTreeMap::from([(("server".into(), "api".into()), 20000)])
        );
        assert_eq!(planner.ports.allocate(), Ok(20001));
    }

    #[test]
    fn tcp_and_udp_endpoints_on_one_port_get_their_own_schemes_and_runner_host_ports() {
        let yaml = concat!("timeout: 1s\nsubject_os: [macos]\nparticipants:\n  z: {type: every-kind-client, connect: {target: server.udp}}\n  a: {type: every-kind-client, subject: true, connect: {target: server.tcp}}\n  server: {type: every-kind-server, os: linux}\n", minimal_procedure!(), "\n");
        let plan = compile(yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("a").unwrap().env["TARGET"],
            "tcp://127.0.0.1:20000"
        );
        assert_eq!(
            plan.effective_config("z").unwrap().env["TARGET"],
            "udp://127.0.0.1:20001"
        );
        assert_eq!(
            plan.participants.as_ref().unwrap().planned.runner_host_ports,
            BTreeMap::from([
                (("server".into(), "tcp".into()), 20000),
                (("server".into(), "udp".into()), 20001),
            ])
        );
    }

    #[test]
    fn cases_planned_with_one_registry_get_distinct_runner_host_ports() {
        let mut planner = planner();
        for port in [20000, 20001] {
            let plan = compile_with(&mut planner, &host_to_container_case(), Os::MacOs)
                .unwrap()
                .unwrap();
            assert_eq!(
                plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
                format!("http://127.0.0.1:{port}")
            );
        }
    }

    #[test]
    fn only_the_selected_subject_os_draws_from_the_run_wide_registry() {
        let yaml = concat!("timeout: 1s\nsubject_os: [linux, macos, windows]\nparticipants:\n  subject: {type: every-kind-server, subject: true}\n  client: {type: client, os: macos, connect: {upstream: server.api}}\n  server: {type: server, os: linux}\n", minimal_procedure!(), "\n");
        for selected in [Os::Linux, Os::MacOs, Os::Windows] {
            let mut planner = planner();
            let plan = compile_with(&mut planner, yaml, selected).unwrap().unwrap();
            assert_eq!(
                plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
                "http://127.0.0.1:20000"
            );
            assert_eq!(
                plan.participants.as_ref().unwrap().planned.runner_host_ports,
                BTreeMap::from([(("server".into(), "api".into()), 20000)])
            );
            assert_eq!(planner.ports.allocate(), Ok(20001));
        }
        let mut planner = planner();
        assert!(compile_with(
            &mut planner,
            &yaml.replace("[linux, macos, windows]", "[linux, macos]"),
            Os::Windows
        )
        .unwrap()
        .is_none());
        assert_eq!(planner.ports.allocate(), Ok(20000));
    }

    #[test]
    fn a_host_process_groups_endpoint_reference_to_a_linux_container_gets_a_runner_host_port() {
        // Address-reference checks must reserve the port that creation uses to render the reference.
        for (config, sources) in [
            ("{env: {KEY: '{{ server.endpoints.api.url }}'}}", vec![]),
            (
                "{params: {mode: [{nested: ['{{ server.endpoints.api.url }}']}]}}",
                vec![],
            ),
            (FILE_CONFIG, vec![("source.yaml", "{{ server.endpoints.api.url }}")]),
        ] {
            let plan = compile_files(&host_to_container_reference_case(config), &sources, Os::MacOs).unwrap();
            assert_eq!(
                plan.participants.as_ref().unwrap().planned.runner_host_ports,
                BTreeMap::from([(("server".into(), "api".into()), 20000)]),
                "{config}"
            );
        }
    }

    #[test]
    fn a_connection_and_a_reference_to_one_container_endpoint_share_its_runner_host_port() {
        let yaml = host_to_container_case()
            .replace("subject: true", "subject: true, config: {env: {KEY: '{{ server.endpoints.api.url }} {{ self.connections.upstream.port }}'}}")
            .replace(minimal_procedure!(), concat!("  other: {type: server, config: {params: {mode: ['{{server.endpoints.api.host}}']}}}\n", minimal_procedure!()));
        let mut planner = planner();
        let plan = compile_with(&mut planner, &yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
            "http://127.0.0.1:20000"
        );
        assert_eq!(
            plan.participants.as_ref().unwrap().planned.runner_host_ports,
            BTreeMap::from([(("server".into(), "api".into()), 20000)])
        );
        assert_eq!(planner.ports.allocate(), Ok(20001));
    }

    #[test]
    fn references_reaching_an_endpoint_by_name_on_loopback_or_on_the_holder_get_no_runner_host_port() {
        for (os, target, endpoint, part) in [
            (Os::Linux, "server", "http", "url"),
            (Os::MacOs, "server", "http", "url"),
            (Os::Linux, "self", "http", "port"),
            (Os::MacOs, "subject", "http", "host"),
            (Os::Linux, "server", "stream", "path"),
            (Os::MacOs, "server", "datagram", "url"),
            (Os::Windows, "self", "pipe", "path"),
        ] {
            let yaml = host_to_container_reference_case(&format!(
                "{{env: {{KEY: '{{{{ {target}.endpoints.{endpoint}.{part} }}}}'}}}}"
            ))
            .replace("[macos]", &format!("[{}]", os.name()))
            .replace(
                "type: server, os: linux",
                "type: every-kind-server, config: {env: {PORT_OFFSET: '100', SOCKET_DIR: '{{ self.facts.shared_dir }}'}}",
            );
            let mut planner = planner();
            let plan = compile_with(&mut planner, &yaml, os).unwrap().unwrap();
            assert!(
                plan.participants.as_ref().unwrap().planned.runner_host_ports.is_empty(),
                "{yaml}"
            );
            assert_eq!(planner.ports.allocate(), Ok(20000), "{yaml}");
        }
    }

    #[test]
    fn runner_host_ports_are_drawn_for_connections_then_references_in_participant_name_order() {
        let yaml = concat!(
            r#"timeout: 1s
subject_os: [macos]
participants:
  z: {type: client, subject: true, connect: {upstream: server.api}}
  b: {type: server, config: {env: {PORT: '8081', METRICS: 'off', KEY: '{{ server.endpoints.extra.url }}'}}}
  a: {type: server, config: {env: {KEY: '{{ server.endpoints.metrics.url }}'}}}
  server: {type: server, os: linux, config: {endpoints: {extra: {port: 8123}}}}
"#,
            minimal_procedure!(),
            "\n"
        );
        let plan = compile(yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.participants.as_ref().unwrap().planned.runner_host_ports,
            BTreeMap::from([
                (("server".into(), "api".into()), 20000),
                (("server".into(), "metrics".into()), 20001),
                (("server".into(), "extra".into()), 20002),
            ])
        );

        let config = "{env: {Z: '{{server.endpoints.tcp.port}}', A: '{{server.endpoints.udp.host}}'}, params: {mode: [{nested: '{{server.endpoints.grpc.url}}'}]}, files: {settings.yaml: {from: source.yaml, template: true}}}";
        let yaml = host_to_container_reference_case(config).replace("type: server", "type: file-server");
        let plan = compile_files(&yaml, &[("source.yaml", "{{server.endpoints.http.url}}")], Os::MacOs).unwrap();
        assert_eq!(
            plan.participants.as_ref().unwrap().planned.runner_host_ports,
            BTreeMap::from([
                (("server".into(), "udp".into()), 20000),
                (("server".into(), "tcp".into()), 20001),
                (("server".into(), "grpc".into()), 20002),
                (("server".into(), "http".into()), 20003),
            ])
        );
    }

    #[test]
    fn an_invalid_address_part_fails_before_a_runner_host_port_is_drawn() {
        let yaml = host_to_container_reference_case("{env: {KEY: '{{ server.endpoints.api.path }}'}}");
        let mut planner = planner();
        assert_eq!(
            compile_with(&mut planner, &yaml, Os::MacOs).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'subject' of type 'file-server' as host-process-group on macos environment variable 'KEY' '{{ server.endpoints.api.path }}': a http endpoint has no 'path'; use url, host or port"
        );
        assert_eq!(planner.ports.allocate(), Ok(20000));
    }

    #[test]
    fn runner_host_port_exhaustion_in_a_reference_names_the_reference() {
        let yaml = host_to_container_case().replace(
            "subject: true",
            "subject: true, config: {env: {KEY: '{{ server.endpoints.metrics.url }}'}}",
        );
        let mut planner = Planner {
            ports: PortRegistry::over(21000..=21000),
            ..planner()
        };
        assert_eq!(
            compile_with(&mut planner, &yaml, Os::MacOs).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'client' of type 'client' as host-process-group on macos environment variable 'KEY' '{{ server.endpoints.metrics.url }}' references endpoint 'metrics' of participant 'server', which needs a runner-host port: all 1 runner-host ports are allocated"
        );
    }

    #[test]
    fn only_the_selected_subject_os_records_runner_host_ports() {
        let yaml = host_to_container_reference_case("{os: {macos: {env: {KEY: '{{ server.endpoints.api.url }}'}}}}")
            .replace("[macos]", "[linux, macos]");
        for selected in [Os::Linux, Os::MacOs] {
            let mut planner = planner();
            let plan = compile_with(&mut planner, &yaml, selected).unwrap().unwrap();
            if selected == Os::Linux {
                assert!(plan.participants.as_ref().unwrap().planned.runner_host_ports.is_empty());
                assert_eq!(planner.ports.allocate(), Ok(20000));
            } else {
                assert_eq!(
                    plan.participants.as_ref().unwrap().planned.runner_host_ports,
                    BTreeMap::from([(("server".into(), "api".into()), 20000)])
                );
                assert_eq!(planner.ports.allocate(), Ok(20001));
            }
        }
    }

    #[test]
    fn runner_host_port_exhaustion_names_the_connection_in_allocation_order() {
        let yaml =
            host_to_container_case().replace("upstream: server.api", "upstream: server.api, side: server.metrics");
        let mut planner = Planner {
            ports: PortRegistry::over(21000..=21000),
            ..planner()
        };
        assert_eq!(compile_with(&mut planner, &yaml, Os::MacOs).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'client' of type 'client' as host-process-group on macos cannot resolve connection 'upstream' to 'server.api': all 1 runner-host ports are allocated");
    }

    #[test]
    fn a_unix_socket_connection_delivers_the_servers_socket_path() {
        for (endpoint, expected) in [
            ("stream", "unix:///airlock/server/shared/stream.sock"),
            ("datagram", "unixgram:///airlock/server/shared/datagram.sock"),
        ] {
            let yaml = CONNECTION_CASE
                .replace("type: client", "type: every-kind-client")
                .replace(
                    "type: server",
                    "type: every-kind-server, config: {env: {SOCKET_DIR: '{{ self.facts.shared_dir }}'}}",
                )
                .replace("upstream: server.api", &format!("target: server.{endpoint}"));
            let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
            assert_eq!(plan.effective_config("client").unwrap().env["TARGET"], expected);
        }
    }

    #[test]
    fn a_delivery_writing_other_keys_than_it_declares_fails_compile_on_any_declared_subject_os() {
        for (mode, got) in [("wrong", "OTHER"), ("missing", "none"), ("extra", "OTHER, TARGET")] {
            let yaml = CONNECTION_CASE
                .replace("[linux]", "[linux, windows]")
                .replace(
                    "type: client",
                    &format!("type: every-kind-client, config: {{os: {{windows: {{env: {{DELIVERY: {mode}}}}}}}}}"),
                )
                .replace("upstream:", "target:");
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                assert_eq!(compile(&yaml, os).err().unwrap().to_string(), format!(
                    "case 'alpha' in cases/alpha/config.yaml: on subject OS 'windows', participant 'client' of type 'every-kind-client' as windows-container on windows delivers connection 'target' as env {got}, but its type declares TARGET"));
            }
        }
    }

    #[test]
    fn a_connection_must_name_an_endpoint_its_consumer_consumes() {
        let message = compile(&CONNECTION_CASE.replace("upstream:", "typo:"), Os::Linux)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' of type 'client' as linux-container on linux connects 'typo', which it does not consume; it consumes side, upstream"
        );
        assert!(compile(CONNECTION_CASE, Os::Linux).unwrap().is_some());
    }

    #[test]
    fn a_connection_must_name_an_endpoint_its_target_serves_with_its_configuration() {
        let yaml = CONNECTION_CASE
            .replace("upstream: server.api", "upstream: server.api, side: server.metrics")
            .replace("type: server}", "type: server, config: {env: {METRICS: 'off'}}}");
        let message = compile(&yaml, Os::Linux).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' connects 'side' to 'server.metrics', which participant 'server' of type 'server' as linux-container on linux does not serve; it serves api"
        );
        assert!(compile(&yaml.replace("METRICS: 'off'", "METRICS: 'on'"), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn an_endpoint_turned_off_by_a_type_default_is_not_served_unless_the_case_env_turns_it_on() {
        let yaml = CONNECTION_CASE
            .replace("[linux]", "[windows]")
            .replace("upstream: server.api", "upstream: server.api, side: server.metrics");
        let message = compile(&yaml, Os::Windows).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'windows', participant 'client' connects 'side' to 'server.metrics', which participant 'server' of type 'server' as windows-container on windows does not serve; it serves api"
        );
        let yaml = yaml.replace("type: server}", "type: server, config: {env: {METRICS: 'on'}}}");
        assert!(compile(&yaml, Os::Windows).unwrap().is_some());
    }

    #[test]
    fn an_os_overlay_can_turn_off_a_connected_endpoint_and_fail_compile_whatever_os_is_selected() {
        let yaml = CONNECTION_CASE
            .replace("[linux]", "[linux, macos, windows]")
            .replace("upstream: server.api", "upstream: server.api, side: server.metrics")
            .replace(
                "type: server}",
                "type: server, config: {env: {METRICS: 'on'}, os: {windows: {env: {METRICS: 'off'}}}}}",
            );
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let message = compile(&yaml, os).err().unwrap().to_string();
            assert_eq!(message,
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'windows', participant 'client' connects 'side' to 'server.metrics', which participant 'server' of type 'server' as windows-container on windows does not serve; it serves api"
            );
            assert!(compile(&yaml.replace("METRICS: 'off'", "METRICS: 'on'"), os)
                .unwrap()
                .is_some());
        }
    }

    #[test]
    fn a_connection_to_an_endpoint_of_a_kind_its_consumer_does_not_accept_fails_compile() {
        let message = compile(&CONNECTION_CASE.replace("server.api", "server.metrics"), Os::Linux)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' connects 'upstream' to 'server.metrics', an endpoint of kind udp; 'upstream' of type 'client' accepts http"
        );
        assert!(compile(CONNECTION_CASE, Os::Linux).unwrap().is_some());
    }

    #[test]
    fn a_connection_without_a_route_between_its_realizations_fails_compile_naming_both_participants_and_the_constraint()
    {
        let yaml = CONNECTION_CASE
            .replace("[linux]", "[macos]")
            .replace("type: client", "type: every-kind-client")
            .replace("type: server", "type: every-kind-server, os: linux")
            .replace("upstream: server.api", "target: server.stream");
        let message = compile(&yaml, Os::MacOs).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'client' connects 'target' to 'server.stream', an endpoint of kind unix-stream; participant 'client' of type 'every-kind-client' as host-process-group on macos cannot reach participant 'server' of type 'every-kind-server' as linux-container on linux: unix sockets connect only participants with the same realization, other than windows-container, because they need one shared filesystem"
        );
        assert!(compile(&yaml.replace("server.stream", "server.tcp"), Os::MacOs)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_connection_without_a_route_on_one_declared_subject_os_fails_compile_whatever_os_is_selected() {
        let yaml = CONNECTION_CASE
            .replace("[linux]", "[linux, macos]")
            .replace("type: client", "type: every-kind-client")
            .replace(
                "type: server",
                "type: every-kind-server, os: linux, config: {env: {SOCKET_DIR: '{{ self.facts.shared_dir }}'}}",
            )
            .replace("upstream: server.api", "target: server.stream");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let message = compile(&yaml, os).err().unwrap().to_string();
            assert_eq!(message,
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'client' connects 'target' to 'server.stream', an endpoint of kind unix-stream; participant 'client' of type 'every-kind-client' as host-process-group on macos cannot reach participant 'server' of type 'every-kind-server' as linux-container on linux: unix sockets connect only participants with the same realization, other than windows-container, because they need one shared filesystem"
            );
        }
        assert!(compile(&yaml.replace("[linux, macos]", "[linux]"), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_case_env_key_a_connection_delivers_fails_compile() {
        let yaml = CONNECTION_CASE.replace("type: client", "type: client, config: {env: {UPSTREAM_URL: literal}}");
        let message = compile(&yaml, Os::Linux).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' of type 'client' as linux-container on linux sets env 'UPSTREAM_URL', which its connection 'upstream' delivers; remove one of them"
        );
        assert!(compile(CONNECTION_CASE, Os::Linux).unwrap().is_some());
    }

    #[test]
    fn an_os_overlay_env_key_a_connection_delivers_fails_compile_whatever_os_is_selected() {
        let yaml = CONNECTION_CASE.replace("[linux]", "[linux, windows]").replace(
            "type: client",
            "type: client, config: {os: {windows: {env: {UPSTREAM_URL: literal}}}}",
        );
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let message = compile(&yaml, os).err().unwrap().to_string();
            assert_eq!(message,
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'windows', participant 'client' of type 'client' as windows-container on windows sets env 'UPSTREAM_URL', which its connection 'upstream' delivers; remove one of them"
            );
        }
        assert!(compile(&yaml.replace("UPSTREAM_URL: literal", ""), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_delivery_conflict_uses_the_consumers_own_os_overlay() {
        let yaml = CONNECTION_CASE
            .replace(
                "type: client, subject: true",
                "type: client, os: macos, config: {os: {macos: {env: {UPSTREAM_URL: literal}}}}",
            )
            .replace("type: server", "type: server, subject: true");
        let message = compile(&yaml, Os::Linux).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' of type 'client' as host-process-group on macos sets env 'UPSTREAM_URL', which its connection 'upstream' delivers; remove one of them"
        );
        assert!(compile(&yaml.replace("UPSTREAM_URL: literal", ""), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_case_may_set_the_delivery_keys_of_an_endpoint_it_does_not_connect() {
        let yaml = CONNECTION_CASE.replace(
            "type: client",
            "type: client, config: {env: {SIDE_HOST: literal, SIDE_PORT: '80'}}",
        );
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env,
            BTreeMap::from([
                ("SIDE_HOST".into(), "literal".into()),
                ("SIDE_PORT".into(), "80".into()),
                ("UPSTREAM_URL".into(), "http://server:8080".into()),
            ])
        );
    }

    #[test]
    fn a_type_default_for_a_delivered_key_is_not_a_conflict() {
        let yaml = CONNECTION_CASE
            .replace("type: client", "type: shared-delivery-client")
            .replace("upstream:", "a:");
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env,
            BTreeMap::from([("SHARED".into(), "http://server:8080".into())])
        );
        let message = compile(
            &yaml.replace("subject: true", "subject: true, config: {env: {SHARED: literal}}"),
            Os::Linux,
        )
        .err()
        .unwrap()
        .to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' of type 'shared-delivery-client' as linux-container on linux sets env 'SHARED', which its connection 'a' delivers; remove one of them"
        );
    }

    #[test]
    fn two_connections_of_one_participant_delivering_the_same_env_key_fail_compile() {
        let yaml = CONNECTION_CASE
            .replace("type: client", "type: shared-delivery-client")
            .replace("upstream: server.api", "z: server.api, a: server.api");
        let message = compile(&yaml, Os::Linux).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' of type 'shared-delivery-client' as linux-container on linux delivers env 'SHARED' through both 'a' and 'z'"
        );
        for connection in ["z: server.api, ", ", a: server.api"] {
            assert!(compile(&yaml.replace(connection, ""), Os::Linux).unwrap().is_some());
        }
    }

    #[test]
    fn connections_of_different_participants_may_deliver_the_same_env_keys() {
        let yaml = CONNECTION_CASE.replace(
            "participants:",
            "participants:\n  other: {type: client, connect: {upstream: server.api}}",
        );
        assert!(compile(&yaml, Os::Linux).unwrap().is_some());
    }

    #[test]
    fn a_required_consumed_endpoint_must_be_connected() {
        let yaml = CONNECTION_CASE.replace(", connect: {upstream: server.api}", "");
        let message = compile(&yaml, Os::Linux).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' of type 'client' as linux-container on linux does not connect required endpoint 'upstream'; add 'upstream: <participant>.<endpoint>' to its 'connect'"
        );
        assert!(compile(CONNECTION_CASE, Os::Linux).unwrap().is_some());
    }

    #[test]
    fn an_optional_consumed_endpoint_may_be_left_unconnected() {
        assert!(compile(CONNECTION_CASE, Os::Linux).unwrap().is_some());
    }

    #[test]
    fn a_consumed_endpoint_accepts_each_kind_it_lists() {
        for (endpoint, config) in [("metrics", ""), ("extra", ", config: {endpoints: {extra: {port: 90}}}")] {
            let yaml = CONNECTION_CASE
                .replace(
                    "upstream: server.api",
                    &format!("upstream: server.api, side: server.{endpoint}"),
                )
                .replace("type: server}", &format!("type: server{config}}}"));
            assert!(compile(&yaml, Os::Linux).unwrap().is_some());
        }
    }

    #[test]
    fn a_connection_target_on_another_os_is_checked_with_its_own_realization() {
        let yaml = CONNECTION_CASE
            .replace("[linux]", "[macos]")
            .replace("upstream: server.api", "upstream: server.api, side: server.metrics")
            .replace(
                "type: server}",
                "type: server, os: linux, config: {os: {linux: {env: {METRICS: 'off'}}}}}",
            );
        let message = compile(&yaml, Os::MacOs).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'client' connects 'side' to 'server.metrics', which participant 'server' of type 'server' as linux-container on linux does not serve; it serves api"
        );
        assert!(compile(&yaml.replace("METRICS: 'off'", "METRICS: 'on'"), Os::MacOs)
            .unwrap()
            .is_some());
    }
}
