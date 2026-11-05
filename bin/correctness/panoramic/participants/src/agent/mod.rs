//! The Core Agent in the converged Linux image, with ADP disabled.

use std::collections::BTreeMap;

use panoramic_core::{
    artifact::ArtifactRole,
    backend::LateFact,
    endpoint::{ConsumedEndpoint, Endpoint},
    layout::{Os, Realization, RealizationKind},
    participant::{
        CaseValues, EffectiveConfig, FactDeclaration, Operation, ParticipantType, ProcessSpec, RenderedConfig,
    },
    recipe::Recipe,
};

use crate::agent_adp::{config_file::ConfigFile, linux, listeners, operations};
use crate::AgentAdp;

/// The Core Agent without ADP.
pub struct Agent;

impl Agent {
    /// Participant type name used in case files.
    pub const NAME: &'static str = "agent";
}

impl ParticipantType for Agent {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[(Os::Linux, RealizationKind::LinuxContainer)]
    }

    fn artifacts(&self, realization: Realization) -> &'static [ArtifactRole] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => &[AgentAdp::LINUX_IMAGE],
            _ => &[],
        }
    }

    fn recipe(&self, realization: Realization, config: &RenderedConfig) -> Result<Recipe, String> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::recipe(config, false),
            _ => Err(format!(
                "participant type '{}' has no recipe for {} on {}",
                Self::NAME,
                realization.kind.name(),
                realization.os.name(),
            )),
        }
    }

    fn config_files(&self) -> &'static [&'static str] {
        AgentAdp.config_files()
    }

    fn params(&self) -> &'static [&'static str] {
        AgentAdp.params()
    }

    fn validate_config(&self, realization: Realization, config: &CaseValues) -> Result<(), String> {
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
        listeners::endpoints(realization, &env, Some(&file), listeners::Product::Agent)?;
        Ok(())
    }

    fn default_env(&self, realization: Realization, config: &CaseValues) -> BTreeMap<String, String> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::default_env(
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
            (Os::Linux, RealizationKind::LinuxContainer) => {
                let file = ConfigFile::parse(&config.files)?;
                listeners::endpoints(realization, &config.env, Some(&file), listeners::Product::Agent)
            }
            _ => Ok(BTreeMap::new()),
        }
    }

    fn consumes(&self, realization: Realization, _: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::consumes(),
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
            (Os::Linux, RealizationKind::LinuxContainer) => &linux::AGENT_PROCESS_SPECS,
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::{
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
    fn agent_uses_the_converged_image_only_in_linux_containers() {
        assert_eq!(Agent.name(), "agent");
        assert_eq!(Agent.supports(), &[(Os::Linux, RealizationKind::LinuxContainer)]);
        assert_eq!(Agent.artifacts(LINUX), &[AgentAdp::LINUX_IMAGE]);
        assert_eq!(Agent.config_files(), AgentAdp.config_files());
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
            Realization {
                os: Os::Linux,
                kind: RealizationKind::KubernetesPod,
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
        for value in ["true", "garbage", "{{ peer.late.container_id }}"] {
            let values = CaseValues {
                env: BTreeMap::from([("DD_DATA_PLANE_ENABLED".into(), value.into())]),
                ..Default::default()
            };
            assert!(Agent
                .validate_config(LINUX, &values)
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
                .validate_config(LINUX, &values)
                .unwrap_err()
                .contains("data_plane.enabled"));
            values.env.insert("DD_DATA_PLANE_ENABLED".into(), "false".into());
            Agent.validate_config(LINUX, &values).unwrap();
            let env = Agent
                .default_env(LINUX, &values)
                .into_iter()
                .chain(values.env)
                .collect();
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
            assert!(!endpoints.contains_key("api"));
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
}
