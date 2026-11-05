//! The Core Agent in the converged Linux image, with ADP disabled.

use std::collections::BTreeMap;

use panoramic_core::{
    artifact::ArtifactRole,
    backend::LateFact,
    endpoint::{ConsumedEndpoint, Endpoint, Evidence, Port},
    layout::{Os, Realization, RealizationKind},
    participant::{
        require_early_config_value, CaseValues, EffectiveConfig, FactDeclaration, Operation, ParticipantType,
        ProcessSpec, RenderedConfig,
    },
    recipe::Recipe,
};

use crate::agent_adp::{config_file::ConfigFile, linux, listeners, operations, pod};
use crate::AgentAdp;

/// The Core Agent without ADP.
pub struct Agent;

impl Agent {
    /// Participant type name used in case files.
    pub const NAME: &'static str = "agent";

    fn configured_endpoints(
        realization: Realization, env: &BTreeMap<String, String>, files: &BTreeMap<String, Vec<u8>>,
    ) -> Result<BTreeMap<String, Endpoint>, String> {
        let file = ConfigFile::parse(files)?;
        let mut endpoints = listeners::endpoints(realization, env, Some(&file), listeners::Product::Agent)?;
        let enabled = match env.get("DD_OTELCOLLECTOR_ENABLED") {
            Some(value) => value.clone(),
            None => file
                .value("OTELCOLLECTOR_ENABLED")?
                .map_or_else(|| "false".into(), |(_, value)| value.into_owned()),
        };
        if !enabled
            .parse::<bool>()
            .map_err(|_| "DD_OTELCOLLECTOR_ENABLED / otelcollector.enabled must be true or false")?
        {
            return Ok(endpoints);
        }
        let name = "otel-config.yaml";
        let key = "receivers.otlp.protocols.grpc.endpoint";
        let bytes = files
            .get(name)
            .ok_or("an enabled collector requires config.files.otel-config.yaml")?;
        let text = simdutf8::basic::from_utf8(bytes).map_err(|error| format!("{name} must be UTF-8: {error}"))?;
        let document: serde_json::Value =
            serde_saphyr::from_str(text).map_err(|error| format!("invalid {name}: {error}"))?;
        let value = document
            .pointer("/receivers/otlp/protocols/grpc/endpoint")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{name} key '{key}' must be a host:port string"))?;
        require_early_config_value(name, key, value)?;
        let (host, port) = listeners::host_port(value).map_err(|error| format!("{name} key '{key}' {error}"))?;
        if listeners::loopback_host(&host) || port == 0 {
            return Err(format!(
                "{name} key '{key}' must use a non-loopback host and a nonzero port"
            ));
        }
        if endpoints.contains_key("agent_otlp_grpc") {
            let receiver = match env.get("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT") {
                Some(value) => value.clone(),
                None => file
                    .value("OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT")?
                    .expect("the declared Agent listener has a configured address")
                    .1
                    .into_owned(),
            };
            if listeners::host_port(&receiver)
                .expect("the Agent listener was validated")
                .1
                == port
            {
                return Err(format!("otel_otlp_grpc and agent_otlp_grpc cannot share port {port}"));
            }
        }
        endpoints.insert(
            "otel_otlp_grpc".into(),
            Endpoint::grpc(Port::Fixed(port)).with_ready(Evidence::tcp_accepts("otel_otlp_grpc")),
        );
        Ok(endpoints)
    }
}

impl ParticipantType for Agent {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::Linux, RealizationKind::KubernetesPod),
        ]
    }

    fn artifacts(&self, realization: Realization) -> &'static [ArtifactRole] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer | RealizationKind::KubernetesPod) => &[AgentAdp::LINUX_IMAGE],
            _ => &[],
        }
    }

    fn recipe(&self, realization: Realization, config: &RenderedConfig) -> Result<Recipe, String> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::recipe(config, false),
            (Os::Linux, RealizationKind::KubernetesPod) => pod::recipe(config, false),
            _ => Err(format!(
                "participant type '{}' has no recipe for {} on {}",
                Self::NAME,
                realization.kind.name(),
                realization.os.name(),
            )),
        }
    }

    fn config_files(&self) -> &'static [&'static str] {
        &["datadog.yaml", "otel-config.yaml", "cont-init.d/", "files/"]
    }

    fn params(&self) -> &'static [&'static str] {
        AgentAdp.params()
    }

    fn validate_config(&self, realization: Realization, config: &CaseValues) -> Result<(), String> {
        if realization.kind == RealizationKind::KubernetesPod {
            pod::validate_config(config)?;
        }
        linux::validate_config(config)?;
        let file = ConfigFile::parse(&config.file_contents)?;
        let enabled = config.env.get("DD_DATA_PLANE_ENABLED").map(String::as_str);
        let file_enabled = if enabled.is_none() {
            file.value("DATA_PLANE_ENABLED")?
        } else {
            None
        };
        let enabled = enabled.or_else(|| file_enabled.as_ref().map(|(_, value)| value.as_ref()));
        if enabled.is_some_and(|value| value != "false") {
            return Err("agent requires DD_DATA_PLANE_ENABLED / datadog.yaml data_plane.enabled to be false".into());
        }
        let mut env = self.default_env(realization, config);
        env.extend(config.env.clone());
        Self::configured_endpoints(realization, &env, &config.file_contents)?;
        Ok(())
    }

    fn validate_realization_options(
        &self, realization: Realization, options: &BTreeMap<String, serde_json::Value>, config: &CaseValues,
    ) -> Result<(), String> {
        if realization.kind == RealizationKind::KubernetesPod {
            pod::validate_realization_options(options, config)?;
        }
        Ok(())
    }

    fn default_env(&self, realization: Realization, config: &CaseValues) -> BTreeMap<String, String> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::default_env(
                config,
                BTreeMap::from([("DD_DATA_PLANE_ENABLED".into(), "false".into())]),
            ),
            (Os::Linux, RealizationKind::KubernetesPod) => pod::default_env(
                config,
                BTreeMap::from([("DD_DATA_PLANE_ENABLED".into(), "false".into())]),
            ),
            _ => BTreeMap::new(),
        }
    }

    fn endpoints(
        &self, realization: Realization, config: &EffectiveConfig,
    ) -> Result<BTreeMap<String, Endpoint>, String> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer | RealizationKind::KubernetesPod) => {
                Self::configured_endpoints(realization, &config.env, &config.files)
            }
            _ => Ok(BTreeMap::new()),
        }
    }

    fn consumes(&self, realization: Realization, _: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::consumes(),
            (Os::Linux, RealizationKind::KubernetesPod) => {
                BTreeMap::from([("intake".into(), crate::agent_adp::intake())])
            }
            _ => BTreeMap::new(),
        }
    }

    fn facts(&self, realization: Realization) -> &'static [FactDeclaration] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => &[FactDeclaration {
                name: "files_dir",
                value: crate::CASE_FILES_DIR,
            }],
            _ => &[],
        }
    }

    fn substituted_late_facts(&self, realization: Realization) -> &'static [LateFact] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => &[LateFact::Ip],
            _ => &[],
        }
    }

    fn operations(&self, realization: Realization) -> &'static [&'static dyn Operation] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => operations::AGENT_OPERATIONS,
            _ => &[],
        }
    }

    fn processes(&self) -> &'static [&'static str] {
        &["agent"]
    }

    fn process_specs(&self, realization: Realization) -> &'static [ProcessSpec] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer | RealizationKind::KubernetesPod) => {
                &linux::AGENT_PROCESS_SPECS
            }
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::{
        case::FileSource,
        endpoint::{
            test_support::{assert_network_endpoint, assert_tls_endpoint},
            EndpointKind, Evidence,
        },
        participant::{ExitChannel, OutputChannel, OutputStream},
        recipe::test_support::{assert_image_recipe, files, rendered},
    };

    use super::*;

    const LINUX: Realization = Realization {
        os: Os::Linux,
        kind: RealizationKind::LinuxContainer,
    };

    const OTLP_FILE: &str = "\
otlp_config:
  receiver:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
      http:
        endpoint: 0.0.0.0:4318
";

    fn config(env: &[(&str, &str)], file: Option<&str>) -> EffectiveConfig {
        let mut env = env
            .iter()
            .map(|&(key, value)| (key.into(), value.into()))
            .collect::<BTreeMap<String, String>>();
        env.insert("DD_DATA_PLANE_ENABLED".into(), "false".into());
        EffectiveConfig {
            env,
            files: file
                .into_iter()
                .map(|file| ("datadog.yaml".into(), file.as_bytes().to_vec()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn agent_uses_the_converged_image_in_linux_containers_and_pods() {
        assert_eq!(Agent.name(), "agent");
        assert_eq!(
            Agent.supports(),
            &[
                (Os::Linux, RealizationKind::LinuxContainer),
                (Os::Linux, RealizationKind::KubernetesPod)
            ]
        );
        assert_eq!(Agent.artifacts(LINUX), &[AgentAdp::LINUX_IMAGE]);
        assert_eq!(
            Agent.config_files(),
            &["datadog.yaml", "otel-config.yaml", "cont-init.d/", "files/"]
        );
        assert!(!AgentAdp.config_files().contains(&"otel-config.yaml"));
        assert_eq!(Agent.params(), &["extra_hosts", "host_view"]);
        for realization in [
            Realization {
                os: Os::MacOs,
                kind: RealizationKind::HostProcessGroup,
            },
            Realization {
                os: Os::Windows,
                kind: RealizationKind::WindowsContainer,
            },
        ] {
            assert!(Agent.artifacts(realization).is_empty());
            assert!(Agent.operations(realization).is_empty());
            assert!(Agent.process_specs(realization).is_empty());
            assert!(Agent
                .recipe(realization, &rendered(BTreeMap::new(), BTreeMap::new()))
                .is_err());
        }
    }

    #[test]
    fn agent_keeps_image_defaults_except_for_disabling_adp() {
        let env = Agent.default_env(LINUX, &CaseValues::default());
        assert_eq!(env, BTreeMap::from([("DD_DATA_PLANE_ENABLED".into(), "false".into())]));
        let endpoints = Agent
            .endpoints(
                LINUX,
                &EffectiveConfig {
                    env: env.clone(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            endpoints.keys().map(String::as_str).collect::<Vec<_>>(),
            ["agent_cmd", "dogstatsd_uds"]
        );
        assert_network_endpoint(
            &endpoints["agent_cmd"],
            EndpointKind::Tcp,
            5001,
            Some(Evidence::tcp_accepts("agent_cmd")),
        );
        let recipe = Agent.recipe(LINUX, &rendered(env.clone(), BTreeMap::new())).unwrap();
        assert_image_recipe(&recipe, AgentAdp::LINUX_IMAGE, &env);
    }

    #[test]
    fn only_the_agent_service_is_observed_and_its_config_file_is_placed_verbatim() {
        assert_eq!(Agent.processes(), &["agent"]);
        assert_eq!(
            Agent.process_specs(LINUX),
            &[ProcessSpec {
                name: "agent",
                exit: ExitChannel::Record { file: "agent" },
                output: OutputChannel::File {
                    file: "agent.log",
                    stream: OutputStream::Merged
                },
            }]
        );
        let yaml = b"dogstatsd_port: 0\ndogstatsd_socket: /airlock/p/shared/metrics.sock\n";
        let recipe = Agent
            .recipe(
                LINUX,
                &rendered(
                    Agent.default_env(LINUX, &CaseValues::default()),
                    BTreeMap::from([("datadog.yaml".into(), yaml.to_vec())]),
                ),
            )
            .unwrap();
        let placed = files(&recipe);
        assert_eq!(placed.len(), 4);
        assert!(placed.contains(&("/etc/datadog-agent/datadog.yaml", 0o644, yaml.as_slice())));
        assert!(!placed.iter().any(|(path, _, _)| path.contains("data-plane")));
        for suffix in ["run", "finish", "log/run"] {
            assert!(placed
                .iter()
                .any(|(path, mode, _)| *path == format!("/etc/services.d/agent/{suffix}") && *mode == 0o755));
        }
        assert_image_recipe(
            &recipe,
            AgentAdp::LINUX_IMAGE,
            &BTreeMap::from([("DD_DATA_PLANE_ENABLED".into(), "false".into())]),
        );
    }

    #[test]
    fn agent_rejects_enabling_adp_and_accepts_an_explicit_disable_over_the_file() {
        for realization in [
            LINUX,
            Realization {
                os: Os::Linux,
                kind: RealizationKind::KubernetesPod,
            },
        ] {
            for value in ["true", "garbage", "{{ peer.late.container_id }}"] {
                let values = CaseValues {
                    env: BTreeMap::from([("DD_DATA_PLANE_ENABLED".into(), value.into())]),
                    ..Default::default()
                };
                assert!(Agent
                    .validate_config(realization, &values)
                    .unwrap_err()
                    .contains("DD_DATA_PLANE_ENABLED"));
                let mut values = CaseValues {
                    file_contents: BTreeMap::from([(
                        "datadog.yaml".into(),
                        format!("data_plane:\n  enabled: '{value}'").into_bytes(),
                    )]),
                    ..Default::default()
                };
                assert!(Agent
                    .validate_config(realization, &values)
                    .unwrap_err()
                    .contains("data_plane.enabled"));
                values.env.insert("DD_DATA_PLANE_ENABLED".into(), "false".into());
                Agent.validate_config(realization, &values).unwrap();
                let env = Agent
                    .default_env(realization, &values)
                    .into_iter()
                    .chain(values.env)
                    .collect();
                let endpoints = Agent
                    .endpoints(
                        realization,
                        &EffectiveConfig {
                            env,
                            files: values.file_contents,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                assert!(!endpoints.contains_key("api"));
            }
        }
    }

    #[test]
    fn linux_agent_otlp_http_tls_uses_agent_environment_over_file_and_requires_both_keys() {
        // The shared listener table advertised HTTP for the Agent's TLS receiver; both owners need TLS detection.
        for (file_cert, file_key, env_cert, env_key, tls) in [
            ("cert.pem", "key.pem", None, None, Some(true)),
            ("", "", Some("cert.pem"), Some("key.pem"), Some(true)),
            ("", "", None, None, Some(false)),
            ("cert.pem", "key.pem", Some(""), Some(""), Some(false)),
            ("cert.pem", "", None, Some("key.pem"), Some(true)),
            ("cert.pem", "", None, None, None),
            ("", "key.pem", None, None, None),
            ("", "", Some("cert.pem"), None, None),
            ("", "", None, Some("key.pem"), None),
            ("cert.pem", "key.pem", Some(""), None, None),
        ] {
            let file = format!(
                "{OTLP_FILE}        tls:\n          cert_file: {file_cert:?}\n          key_file: {file_key:?}\n"
            );
            let mut config = config(
                &[
                    ("ADP_DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE", "adp.pem"),
                    ("ADP_DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_KEY_FILE", ""),
                ],
                Some(&file),
            );
            for (key, value) in [
                ("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE", env_cert),
                ("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_KEY_FILE", env_key),
            ] {
                if let Some(value) = value {
                    config.env.insert(key.into(), value.into());
                }
            }
            let result = Agent.endpoints(LINUX, &config);
            match tls {
                Some(tls) => {
                    let endpoints = result.unwrap();
                    assert_network_endpoint(
                        &endpoints["agent_otlp_http"],
                        EndpointKind::Http,
                        4318,
                        Some(Evidence::tcp_accepts("agent_otlp_http")),
                    );
                    assert_tls_endpoint(&endpoints["agent_otlp_http"], tls);
                    assert_tls_endpoint(&endpoints["agent_otlp_grpc"], false);
                }
                None => {
                    let error = result.unwrap_err();
                    assert!(
                        error.contains("must both be nonempty to enable OTLP HTTP TLS"),
                        "{error}"
                    );
                }
            }
        }
    }

    #[test]
    fn linux_agent_declares_otlp_listeners_only_from_its_config_file_or_environment() {
        let endpoints = Agent.endpoints(LINUX, &config(&[], None)).unwrap();
        assert!(!endpoints.contains_key("agent_otlp_grpc"));
        assert!(!endpoints.contains_key("agent_otlp_http"));

        let endpoints = Agent.endpoints(LINUX, &config(&[], Some(OTLP_FILE))).unwrap();
        for (name, kind, port) in [
            ("agent_otlp_grpc", EndpointKind::Grpc, 4317),
            ("agent_otlp_http", EndpointKind::Http, 4318),
        ] {
            assert_network_endpoint(&endpoints[name], kind, port, Some(Evidence::tcp_accepts(name)));
        }

        let env = [("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT", "0.0.0.0:5317")];
        let endpoints = Agent.endpoints(LINUX, &config(&env, None)).unwrap();
        assert_network_endpoint(
            &endpoints["agent_otlp_grpc"],
            EndpointKind::Grpc,
            5317,
            Some(Evidence::tcp_accepts("agent_otlp_grpc")),
        );
        assert!(!endpoints.contains_key("agent_otlp_http"));
    }

    #[test]
    fn linux_agent_otlp_environment_overrides_the_config_file_including_empty_and_loopback_values() {
        for (value, port) in [
            ("0.0.0.0:5317", Some(5317)),
            ("agent.example:5317", Some(5317)),
            ("", None),
            ("127.0.0.1:5317", None),
            ("localhost:5317", None),
            ("[::1]:5317", None),
            ("0.0.0.0:0", None),
        ] {
            let env = [
                ("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT", value),
                ("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT", value),
            ];
            let endpoints = Agent.endpoints(LINUX, &config(&env, Some(OTLP_FILE))).unwrap();
            for (name, kind) in [
                ("agent_otlp_grpc", EndpointKind::Grpc),
                ("agent_otlp_http", EndpointKind::Http),
            ] {
                match port {
                    Some(port) => {
                        assert_network_endpoint(&endpoints[name], kind, port, Some(Evidence::tcp_accepts(name)))
                    }
                    None => assert!(!endpoints.contains_key(name), "{name}='{value}'"),
                }
            }
        }

        // A file endpoint that is loopback is skipped when the environment leaves it alone.
        let file = OTLP_FILE.replace("0.0.0.0", "127.0.0.1");
        let endpoints = Agent.endpoints(LINUX, &config(&[], Some(&file))).unwrap();
        assert!(!endpoints.contains_key("agent_otlp_grpc"));
        assert!(!endpoints.contains_key("agent_otlp_http"));
    }
    fn collector_case(endpoint: &str) -> CaseValues {
        CaseValues {
            files: ["datadog.yaml", "otel-config.yaml"]
                .into_iter()
                .map(|name| {
                    (
                        name.into(),
                        FileSource {
                            from: name.into(),
                            template: true,
                        },
                    )
                })
                .collect(),
            file_contents: BTreeMap::from([
                ("datadog.yaml".into(), b"otelcollector: {enabled: true}".to_vec()),
                (
                    "otel-config.yaml".into(),
                    serde_json::to_vec(
                        &serde_json::json!({"receivers": {"otlp": {"protocols": {"grpc": {"endpoint": endpoint}}}}, "exporters": {"datadog": {"traces": {"endpoint": "{{ peer.late.address }}"}}}}),
                    )
                    .unwrap(),
                ),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn collector_endpoint_uses_its_file_port_and_agent_only_recipe_path() {
        for port in [4317, 12345] {
            let mut values = collector_case(&format!("0.0.0.0:{port}"));
            values.env.insert(
                "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT".into(),
                "0.0.0.0:9437".into(),
            );
            Agent.validate_config(LINUX, &values).unwrap();
            let mut env = Agent.default_env(LINUX, &values);
            env.extend(values.env);
            let config = EffectiveConfig {
                env,
                files: values.file_contents,
                ..Default::default()
            };
            let endpoints = Agent.endpoints(LINUX, &config).unwrap();
            assert_network_endpoint(
                &endpoints["otel_otlp_grpc"],
                EndpointKind::Grpc,
                port,
                Some(Evidence::tcp_accepts("otel_otlp_grpc")),
            );
            assert_network_endpoint(
                &endpoints["agent_otlp_grpc"],
                EndpointKind::Grpc,
                9437,
                Some(Evidence::tcp_accepts("agent_otlp_grpc")),
            );
            let rendered = rendered(config.env, config.files.clone());
            let recipe = Agent.recipe(LINUX, &rendered).unwrap();
            assert!(files(&recipe).contains(&(
                "/etc/datadog-agent/otel-config.yaml",
                0o644,
                config.files["otel-config.yaml"].as_slice()
            )));
            assert!(AgentAdp
                .recipe(LINUX, &rendered)
                .err()
                .expect("collector file belongs to agent")
                .contains("otel-config.yaml"));
        }
    }

    #[test]
    fn collector_enablement_uses_agent_environment_over_file_and_ignores_adp_overrides() {
        for (file_enabled, env_enabled, adp_enabled, serves) in [
            (false, None, "true", false),
            (true, None, "false", true),
            (true, Some("false"), "true", false),
            (false, Some("true"), "false", true),
        ] {
            let mut values = collector_case("0.0.0.0:4317");
            values.file_contents.insert(
                "datadog.yaml".into(),
                format!("otelcollector: {{enabled: {file_enabled}}}").into_bytes(),
            );
            if let Some(value) = env_enabled {
                values.env.insert("DD_OTELCOLLECTOR_ENABLED".into(), value.into());
            }
            values
                .env
                .insert("ADP_DD_OTELCOLLECTOR_ENABLED".into(), adp_enabled.into());
            if !serves {
                values.file_contents.remove("otel-config.yaml");
            }
            Agent.validate_config(LINUX, &values).unwrap();
            let mut env = Agent.default_env(LINUX, &values);
            env.extend(values.env);
            let endpoints = Agent
                .endpoints(
                    LINUX,
                    &EffectiveConfig {
                        env,
                        files: values.file_contents,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(endpoints.contains_key("otel_otlp_grpc"), serves);
        }
        assert!(!Agent
            .endpoints(LINUX, &EffectiveConfig::default())
            .unwrap()
            .contains_key("otel_otlp_grpc"));
    }

    #[test]
    fn enabled_collector_rejects_missing_invalid_or_late_receiver_configuration() {
        for endpoint in [
            "",
            "localhost:4317",
            "127.0.0.1:4317",
            "[::1]:4317",
            "0.0.0.0:0",
            "0.0.0.0",
            "http://0.0.0.0:4317",
            "0.0.0.0:4317/path",
            "{{ peer.late.address }}",
        ] {
            let error = Agent.validate_config(LINUX, &collector_case(endpoint)).unwrap_err();
            assert!(
                error.contains("otel-config.yaml") && error.contains("receivers.otlp.protocols.grpc.endpoint"),
                "{endpoint}: {error}"
            );
        }
        for contents in [
            None,
            Some("bad: ["),
            Some("{}"),
            Some("receivers: {otlp: {protocols: {grpc: {endpoint: 4317}}}}"),
        ] {
            let mut values = collector_case("0.0.0.0:4317");
            if let Some(contents) = contents {
                values
                    .file_contents
                    .insert("otel-config.yaml".into(), contents.as_bytes().to_vec());
            } else {
                values.file_contents.remove("otel-config.yaml");
            }
            assert!(Agent
                .validate_config(LINUX, &values)
                .unwrap_err()
                .contains("otel-config.yaml"));
        }
        let mut values = collector_case("0.0.0.0:4317");
        values.env.insert("DD_OTELCOLLECTOR_ENABLED".into(), "maybe".into());
        assert!(Agent
            .validate_config(LINUX, &values)
            .unwrap_err()
            .contains("must be true or false"));
    }

    #[test]
    fn collector_and_core_agent_cannot_declare_the_same_grpc_port() {
        for env_override in [false, true] {
            let mut values = collector_case("0.0.0.0:4317");
            if env_override {
                values.env.insert(
                    "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT".into(),
                    "0.0.0.0:4317".into(),
                );
            } else {
                values.file_contents.insert("datadog.yaml".into(), b"otelcollector: {enabled: true}\notlp_config: {receiver: {protocols: {grpc: {endpoint: '0.0.0.0:4317'}}}}".to_vec());
            }
            assert_eq!(
                Agent.validate_config(LINUX, &values).unwrap_err(),
                "otel_otlp_grpc and agent_otlp_grpc cannot share port 4317"
            );
        }
    }
}
