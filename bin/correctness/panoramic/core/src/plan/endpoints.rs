//! Derives each participant's effective configuration and endpoints, assigning host process group ports.

use std::collections::BTreeMap;

use super::PortRegistry;
use crate::{
    case::{declared_names, Template},
    endpoint::{ConsumedEndpoint, Endpoint, Listener, Port},
    layout::{Os, Realization, RealizationKind},
    participant::{CaseValues, EffectiveConfig, ParticipantType},
};

/// Configured endpoints and the case values used to derive them.
pub(super) struct ParticipantEndpoints {
    /// The participant's `shared_dir` fact; consumed sockets must lie under it.
    pub(super) shared_dir: String,
    /// The OS of the participant's realization, which sets its path separator and socket path limit.
    pub(super) os: Os,
    /// Endpoints served with the effective configuration.
    pub(super) served: BTreeMap<String, Endpoint>,
    /// Endpoints consumed with the effective configuration.
    pub(super) consumed: BTreeMap<String, ConsumedEndpoint>,
    /// Case values after the OS overlay and fact substitution.
    pub(super) values: CaseValues,
    /// Assigned listener ports, keyed by endpoint name.
    pub(super) assigned_ports: BTreeMap<String, u16>,
    /// Environment keys carrying assigned ports, mapped to their endpoint names.
    pub(super) assigned_keys: BTreeMap<String, String>,
}

struct PortAssignments {
    ports: BTreeMap<String, u16>,
    keys: BTreeMap<String, String>,
}

/// Derives listeners, assigning host ports through type defaults before deriving them again.
pub(super) fn derive_endpoints(
    holder: &str, participant_type: &dyn ParticipantType, realization: Realization, values: CaseValues,
    shared_dir: String, ports: &mut PortRegistry,
) -> Result<(EffectiveConfig, ParticipantEndpoints), String> {
    let invalid_config = |message| format!("{holder} has invalid configuration: {message}");
    participant_type
        .validate_config(realization, &values)
        .map_err(&invalid_config)?;
    let mut defaults = participant_type.default_env(realization, &values);
    let mut config = EffectiveConfig::assemble(defaults.clone(), values.clone());
    let mut served = participant_type
        .endpoints(realization, &config)
        .map_err(invalid_config)?;
    let PortAssignments {
        ports: assigned_ports,
        keys: assigned_keys,
    } = assign_ports(holder, realization, &served, &mut defaults, ports)?;
    if !assigned_ports.is_empty() {
        config = EffectiveConfig::assemble(defaults, values.clone());
        let rederived = participant_type
            .endpoints(realization, &config)
            .map_err(|error| assignment_error(holder, &assigned_ports, &assigned_keys, None, &error))?;
        check_rederived(holder, &served, &rederived, &assigned_ports, &assigned_keys)?;
        served = rederived;
    }
    resolve_listeners(holder, &config, &mut served)?;
    let consumed = participant_type.consumes(realization, &config);
    Ok((
        config,
        ParticipantEndpoints {
            shared_dir,
            os: realization.os,
            served,
            consumed,
            values,
            assigned_ports,
            assigned_keys,
        },
    ))
}

fn assign_ports(
    holder: &str, realization: Realization, served: &BTreeMap<String, Endpoint>,
    defaults: &mut BTreeMap<String, String>, ports: &mut PortRegistry,
) -> Result<PortAssignments, String> {
    let mut assigned_ports = BTreeMap::new();
    let mut assigned_keys = BTreeMap::new();
    for (name, endpoint) in served {
        let Listener::Port(Port::Assigned { deliver }) = endpoint.listener else {
            continue;
        };
        if realization.kind != RealizationKind::HostProcessGroup {
            return Err(format!(
                "{holder} serves endpoint '{name}' with an assigned port; only host process groups get assigned ports"
            ));
        }
        let port = ports
            .allocate()
            .map_err(|error| format!("{holder} cannot assign a port to endpoint '{name}': {error}"))?;
        let env = deliver(port);
        for key in env.keys() {
            if let Some(earlier) = assigned_keys.insert(key.clone(), name.clone()) {
                return Err(format!(
                    "{holder} delivers the assigned ports of endpoints '{earlier}' and '{name}' through the same env '{key}'"
                ));
            }
        }
        defaults.extend(env);
        assigned_ports.insert(name.clone(), port);
    }
    Ok(PortAssignments {
        ports: assigned_ports,
        keys: assigned_keys,
    })
}

fn check_rederived(
    holder: &str, served: &BTreeMap<String, Endpoint>, rederived: &BTreeMap<String, Endpoint>,
    assigned_ports: &BTreeMap<String, u16>, assigned_keys: &BTreeMap<String, String>,
) -> Result<(), String> {
    let invalid_derivation = |detail: &str| assignment_error(holder, assigned_ports, assigned_keys, None, detail);
    for (name, endpoint) in served {
        let Some(new_endpoint) = rederived.get(name) else {
            return Err(invalid_derivation(&format!("endpoint '{name}' was removed")));
        };
        if new_endpoint.kind != endpoint.kind {
            return Err(invalid_derivation(&format!(
                "endpoint '{name}' changed kind from {} to {}",
                endpoint.kind.name(),
                new_endpoint.kind.name(),
            )));
        }
        if let Some(&port) = assigned_ports.get(name) {
            if !matches!(new_endpoint.listener, Listener::Port(Port::Fixed(actual)) if actual == port) {
                return Err(assignment_error(
                    holder,
                    assigned_ports,
                    assigned_keys,
                    Some((name, port)),
                    "the re-derived listener does not use that port",
                ));
            }
        }
    }
    for (name, endpoint) in rederived {
        if !served.contains_key(name) {
            return Err(invalid_derivation(&format!("endpoint '{name}' was added")));
        }
        if matches!(endpoint.listener, Listener::Port(Port::Assigned { .. })) {
            return Err(invalid_derivation(&format!(
                "endpoint '{name}' still reports an assigned port"
            )));
        }
    }
    Ok(())
}

fn assignment_error(
    holder: &str, assigned_ports: &BTreeMap<String, u16>, assigned_keys: &BTreeMap<String, String>,
    endpoint: Option<(&str, u16)>, detail: &str,
) -> String {
    let delivered_keys = |name: &str| {
        declared_names(
            &assigned_keys
                .iter()
                .filter_map(|(key, endpoint)| (endpoint == name).then_some(key.as_str()))
                .collect::<Vec<_>>(),
        )
    };
    let header = if let Some((name, port)) = endpoint {
        format!(
            "{holder} does not serve endpoint '{name}' at its assigned port {port} (delivered as env {})",
            delivered_keys(name)
        )
    } else {
        let assignments = assigned_ports
            .iter()
            .map(|(name, port)| format!("{name} {port} via {}", delivered_keys(name)))
            .collect::<Vec<_>>()
            .join("; ");
        format!("{holder} does not derive its assigned ports ({assignments}) after delivery")
    };
    format!("{header}: {detail}; a type reports an assigned port only when its configuration sets none")
}

/// Checks overrides, then fills case-supplied ports and validates socket paths in endpoint name order.
fn resolve_listeners(
    holder: &str, config: &EffectiveConfig, endpoints: &mut BTreeMap<String, Endpoint>,
) -> Result<(), String> {
    for (name, overridden) in &config.endpoints {
        let Some(endpoint) = endpoints.get(name) else {
            return Err(format!(
                "{holder} overrides endpoint '{name}', which it does not serve; it serves {}",
                declared_names(&endpoints.keys().map(String::as_str).collect::<Vec<_>>()),
            ));
        };
        let derived = match endpoint.listener {
            Listener::Port(Port::FromCase) => {
                if overridden.port == 0 {
                    return Err(format!("{holder} overrides endpoint '{name}' with port 0"));
                }
                continue;
            }
            Listener::Port(Port::Fixed(_)) => "port",
            Listener::Port(Port::Assigned { .. }) => unreachable!("assigned ports are re-derived before overrides"),
            Listener::Path(_) => "path",
            Listener::Pipe(_) => "pipe name",
        };
        return Err(format!(
            "{holder} overrides endpoint '{name}', whose {derived} its type derives from its configuration"
        ));
    }
    for (name, endpoint) in endpoints {
        match &endpoint.listener {
            Listener::Port(Port::FromCase) => {
                let Some(overridden) = config.endpoints.get(name) else {
                    return Err(format!(
                        "{holder} serves endpoint '{name}', whose port the case must give as config.endpoints.{name}.port"
                    ));
                };
                endpoint.listener = Listener::Port(Port::Fixed(overridden.port));
            }
            Listener::Path(path) => {
                if !path
                    .parse::<Template>()
                    .is_ok_and(|template| template.references().next().is_none())
                {
                    return Err(format!(
                        "{holder} serves endpoint '{name}' at '{path}', which holds a reference resolved only after planning"
                    ));
                }
            }
            Listener::Port(Port::Assigned { .. }) => unreachable!("assigned ports are re-derived before overrides"),
            Listener::Port(Port::Fixed(_)) | Listener::Pipe(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        case::{
            fixtures::{minimal_procedure, write_case, Client, FixtureTypes, Server, PARTICIPANT_FIELDS, STEPS},
            CaseLoader, FileSource, Registries,
        },
        plan::{
            fixtures::{
                assigned_server_case, compile, compile_files, compile_with_ports, CONFIG_LOADER, CONFIG_REGISTRIES,
                CONNECTION_CASE,
            },
            CasePlan, ScopeRegistry,
        },
    };

    #[test]
    fn a_host_process_groups_assigned_port_is_delivered_into_its_type_defaults_and_rederived() {
        let mut ports = PortRegistry::new();
        for port in [20000, 20002] {
            let plan = compile_with_ports(&assigned_server_case("{}"), Os::MacOs, &mut ports)
                .unwrap()
                .unwrap();
            assert_eq!(
                plan.effective_config("server").unwrap().env,
                BTreeMap::from([
                    ("API_PORT".into(), port.to_string()),
                    ("API_URL".into(), format!("http://0.0.0.0:{port}")),
                    ("METRICS_PORT".into(), (port + 1).to_string()),
                ])
            );
            assert_eq!(
                plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
                format!("http://127.0.0.1:{port}")
            );
            assert!(plan.runner_host_ports.is_empty());
        }
        assert_eq!(ports.allocate(), Ok(20004));
    }

    #[test]
    fn a_case_env_key_pins_an_assigned_port_and_draws_none() {
        let mut ports = PortRegistry::new();
        let plan = compile_with_ports(
            &assigned_server_case("{env: {API_PORT: '8081', METRICS_PORT: '9001'}}"),
            Os::MacOs,
            &mut ports,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            plan.effective_config("server").unwrap().env,
            BTreeMap::from([
                ("API_PORT".into(), "8081".into()),
                ("API_URL".into(), "default".into()),
                ("METRICS_PORT".into(), "9001".into()),
            ])
        );
        assert_eq!(
            plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
            "http://127.0.0.1:8081"
        );
        assert_eq!(ports.allocate(), Ok(20000));
    }

    #[test]
    fn case_env_replaces_assigned_defaults_without_changing_the_assigned_port() {
        let plan = compile(&assigned_server_case("{env: {API_URL: literal}}"), Os::MacOs)
            .unwrap()
            .unwrap();
        let config = plan.effective_config("server").unwrap();
        assert_eq!(config.env["API_URL"], "literal");
        assert_eq!(config.env["API_PORT"], "20000");
        assert_eq!(
            plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
            "http://127.0.0.1:20000"
        );
    }

    #[test]
    fn assigned_ports_are_drawn_per_participant_and_endpoint_in_name_order_before_connection_ports() {
        let yaml = concat!(
            r#"timeout: 1s
subject_os: [macos]
participants:
  z: {type: assigned-server}
  client: {type: client, subject: true, connect: {upstream: container.api}, config: {env: {REF: '{{container.endpoints.metrics.port}}'}}}
  a: {type: assigned-server}
  container: {type: server, os: linux}
"#,
            minimal_procedure!(),
            "\n"
        );
        let mut ports = PortRegistry::new();
        let plan = compile_with_ports(yaml, Os::MacOs, &mut ports).unwrap().unwrap();
        for (name, api, metrics) in [("a", "20000", "20001"), ("z", "20002", "20003")] {
            let config = plan.effective_config(name).unwrap();
            assert_eq!(config.env["API_PORT"], api);
            assert_eq!(config.env["METRICS_PORT"], metrics);
        }
        assert_eq!(
            plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
            "http://127.0.0.1:20004"
        );
        assert_eq!(
            plan.runner_host_ports,
            BTreeMap::from([
                (("container".into(), "api".into()), 20004),
                (("container".into(), "metrics".into()), 20005),
            ])
        );
        assert_eq!(ports.allocate(), Ok(20006));
    }

    #[test]
    fn an_assigned_port_outside_a_host_process_group_fails_naming_the_endpoint() {
        let yaml = assigned_server_case("{params: {mode: assign-anywhere}}").replace("[macos]", "[linux]");
        let mut ports = PortRegistry::new();
        assert_eq!(compile_with_ports(&yaml, Os::Linux, &mut ports).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'server' of type 'assigned-server' as linux-container on linux serves endpoint 'api' with an assigned port; only host process groups get assigned ports");
        assert_eq!(ports.allocate(), Ok(20000));
    }

    #[test]
    fn a_type_that_does_not_serve_its_assigned_port_after_delivery_fails() {
        let wrong_api = "does not serve endpoint 'api' at its assigned port 20000 (delivered as env API_PORT, API_URL)";
        let both_ports = "does not derive its assigned ports (api 20000 via API_PORT, API_URL; metrics 20001 via METRICS_PORT) after delivery";
        let api_only = "does not derive its assigned ports (api 20000 via API_PORT, API_URL) after delivery";
        for (mode, env, header, detail) in [
            (
                "keep-assigned",
                "",
                wrong_api,
                "the re-derived listener does not use that port",
            ),
            (
                "keep-assigned",
                "API_PORT: '8081'",
                wrong_api,
                "the re-derived listener does not use that port",
            ),
            (
                "wrong-port",
                "",
                wrong_api,
                "the re-derived listener does not use that port",
            ),
            ("extra-after-assignment", "", both_ports, "endpoint 'extra' was added"),
            ("remove-after-assignment", "", both_ports, "endpoint 'api' was removed"),
            (
                "change-kind",
                "",
                both_ports,
                "endpoint 'api' changed kind from http to grpc",
            ),
            (
                "remove-metrics-after-assignment",
                "METRICS_PORT: '9001'",
                api_only,
                "endpoint 'metrics' was removed",
            ),
            (
                "change-metrics-kind",
                "METRICS_PORT: '9001'",
                api_only,
                "endpoint 'metrics' changed kind from udp to tcp",
            ),
            (
                "from-case-after-assignment",
                "",
                wrong_api,
                "the re-derived listener does not use that port",
            ),
            ("error-after-assignment", "", both_ports, "cannot read delivered port"),
            (
                "assign-another",
                "METRICS_PORT: '9001'",
                api_only,
                "endpoint 'metrics' still reports an assigned port",
            ),
        ] {
            let yaml = assigned_server_case(&format!("{{params: {{mode: {mode}}}, env: {{{env}}}}}"));
            let message = compile(&yaml, Os::MacOs).err().unwrap().to_string();
            assert_eq!(message, format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'server' of type 'assigned-server' as host-process-group on macos {header}: {detail}; a type reports an assigned port only when its configuration sets none"
            ), "{mode}");
        }
    }

    #[test]
    fn two_assigned_endpoints_delivering_one_env_key_fail() {
        let yaml = assigned_server_case("{params: {mode: shared-key}}");
        assert_eq!(compile(&yaml, Os::MacOs).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'server' of type 'assigned-server' as host-process-group on macos delivers the assigned ports of endpoints 'api' and 'metrics' through the same env 'API_PORT'");
    }

    #[test]
    fn consumed_endpoints_are_derived_from_the_configuration_with_assigned_ports() {
        let yaml = assigned_server_case("{params: {mode: consume-after-assignment}}")
            .replace(
                "type: assigned-server,",
                "type: assigned-server, connect: {upstream: other.api},",
            )
            .replace(
                minimal_procedure!(),
                concat!("  other: {type: server}\n", minimal_procedure!()),
            );
        let mut ports = PortRegistry::new();
        let plan = compile_with_ports(&yaml, Os::MacOs, &mut ports).unwrap().unwrap();
        let config = plan.effective_config("server").unwrap();
        assert_eq!(config.env["API_PORT"], "20000");
        assert_eq!(config.env["UPSTREAM_URL"], "http://127.0.0.1:8080");
        assert_eq!(ports.allocate(), Ok(20002));
    }

    #[test]
    fn assigned_port_exhaustion_names_the_endpoint() {
        let mut ports = PortRegistry::over(21000..=21000);
        assert_eq!(compile_with_ports(&assigned_server_case("{}"), Os::MacOs, &mut ports).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'server' of type 'assigned-server' as host-process-group on macos cannot assign a port to endpoint 'metrics': all 1 runner-host ports are allocated");
    }

    #[test]
    fn only_the_selected_subject_os_draws_assigned_ports_from_the_run_registry() {
        let yaml = assigned_server_case("{}").replace("[macos]", "[linux, macos]");
        for (selected, next) in [(Os::Linux, 20000), (Os::MacOs, 20002), (Os::Windows, 20000)] {
            let mut ports = PortRegistry::new();
            let plan = compile_with_ports(&yaml, selected, &mut ports).unwrap();
            if selected == Os::MacOs {
                assert_eq!(
                    plan.unwrap().effective_config("server").unwrap().env["API_PORT"],
                    "20000"
                );
            } else if selected == Os::Linux {
                assert!(!plan
                    .unwrap()
                    .effective_config("server")
                    .unwrap()
                    .env
                    .contains_key("API_PORT"));
            } else {
                assert!(plan.is_none());
            }
            assert_eq!(ports.allocate(), Ok(next));
        }
        let yaml = yaml
            .replace(
                "type: assigned-server",
                "type: assigned-server, realization: {kind: host-process-group}",
            )
            .replace(
                "type: client",
                "type: every-kind-client, realization: {kind: host-process-group}",
            );
        let yaml = yaml.replace("upstream: server.api", "target: server.api");
        let mut ports = PortRegistry::new();
        let plan = compile_with_ports(&yaml, Os::Linux, &mut ports).unwrap().unwrap();
        assert_eq!(plan.effective_config("server").unwrap().env["API_PORT"], "20000");
        assert_eq!(ports.allocate(), Ok(20002));
    }

    #[test]
    fn participant_type_hooks_see_configuration_without_connection_deliveries() {
        struct Hooks(std::sync::Mutex<Vec<&'static str>>);
        impl ParticipantType for Hooks {
            fn name(&self) -> &'static str {
                "hooks"
            }
            fn supports(&self) -> &'static [(Os, RealizationKind)] {
                Client.supports()
            }
            fn validate_config(&self, _realization: Realization, config: &CaseValues) -> Result<(), String> {
                assert_eq!(config.env, BTreeMap::from([("CASE".into(), "value".into())]));
                self.0.lock().unwrap().push("validate");
                Ok(())
            }
            fn default_env(&self, _realization: Realization, config: &CaseValues) -> BTreeMap<String, String> {
                assert_eq!(config.env, BTreeMap::from([("CASE".into(), "value".into())]));
                self.0.lock().unwrap().push("defaults");
                BTreeMap::from([
                    ("UPSTREAM_URL".into(), "default".into()),
                    ("CASE".into(), "default".into()),
                ])
            }
            fn endpoints(
                &self, _realization: Realization, config: &EffectiveConfig,
            ) -> Result<BTreeMap<String, Endpoint>, String> {
                assert_eq!(config.env["UPSTREAM_URL"], "default");
                assert_eq!(config.env["CASE"], "value");
                self.0.lock().unwrap().push("endpoints");
                Ok(BTreeMap::new())
            }
            fn consumes(
                &self, realization: Realization, config: &EffectiveConfig,
            ) -> BTreeMap<String, ConsumedEndpoint> {
                assert_eq!(config.env["UPSTREAM_URL"], "default");
                assert_eq!(config.env["CASE"], "value");
                self.0.lock().unwrap().push("consumes");
                Client.consumes(realization, config)
            }
            fn processes(&self) -> &'static [&'static str] {
                &[]
            }
        }
        let hooks = Hooks(std::sync::Mutex::new(Vec::new()));
        let registries = Registries {
            participant_types: &[&hooks, &Server],
            steps: STEPS,
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture-participants");
        let yaml = CONNECTION_CASE.replace("type: client", "type: hooks, config: {env: {CASE: value}}");
        let case = loader
            .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let plan = CasePlan::compile(
            case,
            Os::Linux,
            &registries,
            &mut PortRegistry::new(),
            &mut ScopeRegistry::with_run("testrun0"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            *hooks.0.lock().unwrap(),
            ["validate", "defaults", "endpoints", "consumes"]
        );
        assert_eq!(
            plan.effective_config("client").unwrap().env,
            BTreeMap::from([
                ("UPSTREAM_URL".into(), "http://server:8080".into()),
                ("CASE".into(), "value".into()),
            ])
        );
    }

    #[test]
    fn an_endpoint_override_supplies_a_port_only_the_case_knows() {
        let yaml = CONNECTION_CASE
            .replace("upstream: server.api", "upstream: server.api, side: server.extra")
            .replace(
                "type: server}",
                "type: server, config: {endpoints: {extra: {port: 90}}}}",
            );
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env,
            BTreeMap::from([
                ("SIDE_HOST".into(), "server".into()),
                ("SIDE_PORT".into(), "90".into()),
                ("UPSTREAM_URL".into(), "http://server:8080".into()),
            ])
        );
    }

    #[test]
    fn an_endpoint_override_with_port_zero_fails_compile_on_every_declared_subject_os() {
        for (declared, kind) in [
            (Os::Linux, "linux-container"),
            (Os::MacOs, "host-process-group"),
            (Os::Windows, "windows-container"),
        ] {
            let yaml = CONNECTION_CASE
                .replace("[linux]", &format!("[{}]", declared.name()))
                .replace("upstream: server.api", "upstream: server.api, side: server.extra")
                .replace(
                    "type: server}",
                    "type: server, config: {endpoints: {extra: {port: 0}}}}",
                );
            for selected in [Os::Linux, Os::MacOs, Os::Windows] {
                assert_eq!(compile(&yaml, selected).err().unwrap().to_string(), format!(
                    "case 'alpha' in cases/alpha/config.yaml: on subject OS '{os}', participant 'server' of type 'server' as {kind} on {os} overrides endpoint 'extra' with port 0",
                    os = declared.name(),
                ));
            }
            for port in [1, u16::MAX] {
                let plan = compile(&yaml.replace("port: 0", &format!("port: {port}")), declared)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    plan.effective_config("client").unwrap().env["SIDE_PORT"],
                    port.to_string()
                );
            }
        }
    }

    #[test]
    fn an_endpoint_whose_port_only_the_case_knows_needs_an_override() {
        let yaml = PARTICIPANT_FIELDS.replace(
            "type: server",
            "type: file-server, config: {env: {CASE_PORT: required}}",
        );
        assert_eq!(compile_files(&yaml, &[], Os::Linux).err().unwrap(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'subject' of type 'file-server' as linux-container on linux serves endpoint 'tcp', whose port the case must give as config.endpoints.tcp.port");
        let yaml = yaml.replace(
            "env: {CASE_PORT: required}",
            "env: {CASE_PORT: required}, endpoints: {tcp: {port: 90}}",
        );
        let plan = compile_files(&yaml, &[], Os::Linux).unwrap();
        assert_eq!(plan.effective_config("subject").unwrap().endpoints["tcp"].port, 90);
    }

    #[test]
    fn overriding_an_endpoint_whose_port_the_type_derives_fails_compile() {
        for (endpoint, derived) in [("tcp", "port"), ("stream", "path"), ("pipe", "pipe name")] {
            let yaml = PARTICIPANT_FIELDS.replace(
                "type: server",
                &format!("type: every-kind-server, config: {{endpoints: {{{endpoint}: {{port: 80}}}}}}"),
            );
            assert_eq!(compile(&yaml, Os::Linux).err().unwrap().to_string(), format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'subject' of type 'every-kind-server' as linux-container on linux overrides endpoint '{endpoint}', whose {derived} its type derives from its configuration"));
        }
    }

    #[test]
    fn a_served_socket_path_holding_a_late_reference_fails_compile() {
        for (mode, path) in [
            ("late-path", "{{self.late.ip}}/stream.sock"),
            ("malformed-path", "{{unfinished"),
        ] {
            let yaml = PARTICIPANT_FIELDS.replace(
                "type: server",
                &format!("type: file-server, config: {{params: {{mode: {mode}}}}}"),
            );
            assert_eq!(compile_files(&yaml, &[], Os::Linux).err().unwrap(), format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'subject' of type 'file-server' as linux-container on linux serves endpoint 'stream' at '{path}', which holds a reference resolved only after planning"));
        }
        let yaml = concat!("timeout: 1s\nsubject_os: [linux]\nparticipants:\n  client: {type: file-server, subject: true, connect: {target: server.stream}}\n  server: {type: file-server, config: {env: {SOCKET_PATH: '{{self.facts.shared_dir}}/stream.sock'}}}\n", minimal_procedure!(), "\n");
        let plan = compile_files(yaml, &[], Os::Linux).unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env["TARGET"],
            "unix:///airlock/server/shared/stream.sock"
        );
    }

    #[test]
    fn an_endpoint_override_must_name_an_endpoint_the_type_serves_with_that_override() {
        let yaml = CONNECTION_CASE.replace(
            "type: server}",
            "type: server, config: {endpoints: {extra: {port: 90}}}}",
        );
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(plan.effective_config("server").unwrap().endpoints["extra"].port, 90);
        let message = compile(&yaml.replace("extra:", "typo:"), Os::Linux)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'server' of type 'server' as linux-container on linux overrides endpoint 'typo', which it does not serve; it serves api, metrics"
        );
    }

    #[test]
    fn an_endpoint_derivation_error_fails_compile_naming_the_participant_and_realization() {
        let yaml = CONNECTION_CASE.replace("type: server}", "type: server, config: {env: {PORT: x}}}");
        let message = compile(&yaml, Os::Linux).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'server' of type 'server' as linux-container on linux has invalid configuration: PORT 'x' is not a number"
        );
        assert!(compile(&yaml.replace("PORT: x", "PORT: '80'"), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_plan_holds_each_participants_effective_config_for_the_selected_subject_os() {
        let yaml = concat!(
            r#"timeout: 1s
subject_os: [windows, linux]
participants:
  subject:
    type: configured
    subject: true
    config:
      env: {LEVEL: debug, EXTRA: base}
      os: {windows: {env: {LEVEL: trace, EXTRA: windows}}}
  other: {type: configured, config: {env: {PORT: "2"}}}
"#,
            minimal_procedure!(),
            "\n"
        );
        for (os, level, extra) in [(Os::Linux, "debug", "base"), (Os::Windows, "trace", "windows")] {
            let plan = compile(yaml, os).unwrap().unwrap();
            assert_eq!(
                plan.effective_config("subject"),
                Some(&EffectiveConfig {
                    env: BTreeMap::from([
                        ("LEVEL".into(), level.into()),
                        ("PORT".into(), "1".into()),
                        ("EXTRA".into(), extra.into()),
                    ]),
                    ..EffectiveConfig::default()
                })
            );
            assert_eq!(
                plan.effective_config("other"),
                Some(&EffectiveConfig {
                    env: BTreeMap::from([("LEVEL".into(), "info".into()), ("PORT".into(), "2".into())]),
                    ..EffectiveConfig::default()
                })
            );
            assert_eq!(plan.effective_config("unknown"), None);
        }
    }

    #[test]
    fn a_participant_with_its_own_os_gets_the_overlay_for_that_os() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
            "participants:",
            "participants:\n  other: {type: configured, os: linux, config: {env: {LEVEL: debug}, os: {linux: {env: {LEVEL: trace}}}}}",
        );
        let plan = compile(&yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("other").unwrap().env,
            BTreeMap::from([("LEVEL".into(), "trace".into()), ("PORT".into(), "1".into()),])
        );
        assert_eq!(plan.effective_config("subject"), Some(&EffectiveConfig::default()));
    }

    #[test]
    fn type_defaults_see_the_overlaid_case_values() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]").replace(
            "type: server, subject: true",
            "type: configured, subject: true, config: {os: {windows: {files: {settings.yaml: {from: windows.yaml, template: true}}}}}",
        );
        let source = write_case(dir.path(), "alpha", &yaml);
        std::fs::write(source.parent().unwrap().join("windows.yaml"), "port: 7").unwrap();
        for os in [Os::Linux, Os::Windows] {
            let case = CONFIG_LOADER.load_file(&source).unwrap().remove(0);
            let plan = CasePlan::compile(
                case,
                os,
                &CONFIG_REGISTRIES,
                &mut PortRegistry::new(),
                &mut ScopeRegistry::with_run("testrun0"),
            )
            .unwrap()
            .unwrap();
            let effective = plan.effective_config("subject").unwrap();
            assert_eq!(effective.env["LEVEL"], "info");
            if os == Os::Windows {
                assert_eq!(effective.env.get("PORT"), None);
                assert_eq!(
                    effective.files,
                    BTreeMap::from([(
                        "settings.yaml".into(),
                        FileSource {
                            from: "windows.yaml".into(),
                            template: true,
                        }
                    )])
                );
            } else {
                assert_eq!(effective.env.get("PORT").map(String::as_str), Some("1"));
                assert!(effective.files.is_empty());
            }
        }
    }

    #[test]
    fn a_config_its_type_rejects_on_any_declared_subject_os_fails_compile_whatever_os_is_selected() {
        for config in [
            "{params: {mode: broken}}",
            "{params: {mode: working}, os: {windows: {params: {mode: broken}}}}",
        ] {
            let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[windows, linux]").replace(
                "type: server, subject: true",
                &format!("type: configured, subject: true, config: {config}"),
            );
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                assert_eq!(compile(&yaml, os).err().unwrap().to_string(),
                    "case 'alpha' in cases/alpha/config.yaml: on subject OS 'windows', participant 'subject' of type 'configured' as windows-container on windows has invalid configuration: mode 'broken' is unavailable on windows"
                );
            }
        }
    }

    #[test]
    fn a_participant_type_that_declares_no_endpoints_serves_and_consumes_nothing() {
        struct Minimal;

        impl ParticipantType for Minimal {
            fn name(&self) -> &'static str {
                "minimal"
            }

            fn supports(&self) -> &'static [(Os, RealizationKind)] {
                &[(Os::Linux, RealizationKind::LinuxContainer)]
            }

            fn processes(&self) -> &'static [&'static str] {
                &["main"]
            }
        }

        let registries = Registries {
            participant_types: &[&Minimal, &Client],
            steps: STEPS,
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture-participants");
        for (participants, expected) in [
            (
                "  subject: {type: minimal, subject: true, config: {endpoints: {api: {port: 80}}}}",
                "participant 'subject' of type 'minimal' as linux-container on linux overrides endpoint 'api', which it does not serve; it serves none",
            ),
            (
                "  subject: {type: minimal, subject: true, connect: {upstream: other.api}}\n  other: {type: minimal}",
                "participant 'subject' of type 'minimal' as linux-container on linux connects 'upstream', which it does not consume; it consumes none",
            ),
            (
                "  subject: {type: minimal, subject: true}\n  client: {type: client, connect: {upstream: subject.api}}",
                "participant 'client' connects 'upstream' to 'subject.api', which participant 'subject' of type 'minimal' as linux-container on linux does not serve; it serves none",
            ),
        ] {
            let yaml = format!("timeout: 1s\nsubject_os: [linux]\nparticipants:\n{participants}\n{}\n", minimal_procedure!());
            let case = loader.load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml")).unwrap().remove(0);
            let message = CasePlan::compile(case, Os::Linux, &registries, &mut PortRegistry::new(), &mut ScopeRegistry::with_run("testrun0")).err().unwrap().to_string();
            assert_eq!(message, format!("case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', {expected}"));
        }
    }
}
