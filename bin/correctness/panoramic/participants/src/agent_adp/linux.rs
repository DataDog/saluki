//! Linux configuration and service overlays for the converged image.

use std::collections::BTreeMap;

use async_trait::async_trait;
use panoramic_core::{
    endpoint::{ConsumedEndpoint, Delivery, EndpointKind},
    participant::{ExitChannel, OpContext, Operation, OutputChannel, OutputStream, ProcessSpec, RenderedConfig},
    recipe::{ContainerRecipe, Recipe},
    step::HandleError,
};
use serde_json::Value;

use super::AgentAdp;
use crate::CASE_FILES_DIR;

const ADP_CLI_BINARY_PATH: &str = "/opt/datadog-agent/embedded/bin/agent-data-plane";

pub(super) const OPERATIONS: &[&dyn Operation] = &[
    &super::operations::Command::CliAdp,
    &super::operations::Command::CliAgent,
    &ConfigDump,
    &super::operations::Command::ConfigSet,
    &super::operations::Command::DogstatsdCapture,
    &super::operations::Command::DogstatsdReplay,
    &super::operations::Command::DogstatsdStats,
];

struct ConfigDump;

#[async_trait]
impl Operation for ConfigDump {
    fn name(&self) -> &'static str {
        "config_dump"
    }

    async fn run(&self, cx: &OpContext<'_>, args: Value) -> Result<Value, HandleError> {
        let view = args
            .as_object()
            .filter(|args| args.len() == 1)
            .and_then(|args| args.get("view"))
            .and_then(Value::as_str)
            .filter(|view| matches!(*view, "source" | "runtime"))
            .ok_or_else(|| HandleError::Harness {
                message: "config_dump requires exactly {\"view\": \"source\"|\"runtime\"}".into(),
            })?;
        let mut argv = vec![ADP_CLI_BINARY_PATH.into(), "config".into(), "--json".into()];
        if view == "runtime" {
            argv.push("--runtime".into());
        }
        let output = cx.exec(argv).await?;
        match output.exit_code {
            0 => serde_json::from_slice(&output.stdout).map_err(|_| HandleError::Product {
                message: "ADP configuration CLI returned invalid JSON".into(),
            }),
            126 | 127 => Err(HandleError::Harness {
                message: format!("ADP configuration CLI could not run (exit {})", output.exit_code),
            }),
            code => Err(HandleError::Product {
                message: format!("ADP configuration CLI exited with {code}"),
            }),
        }
    }
}

pub(super) fn consumes() -> BTreeMap<String, ConsumedEndpoint> {
    BTreeMap::from([
        (
            "docker_api".into(),
            ConsumedEndpoint {
                accepts: vec![EndpointKind::UnixStream],
                required: false,
                delivery: Delivery::Env {
                    keys: ["DOCKER_HOST".into()].into(),
                    value: |address| BTreeMap::from([("DOCKER_HOST".into(), address.url().into())]),
                },
            },
        ),
        (
            "dogstatsd_forward".into(),
            ConsumedEndpoint {
                accepts: vec![EndpointKind::Udp],
                required: false,
                delivery: Delivery::Env {
                    keys: ["DD_STATSD_FORWARD_HOST".into(), "DD_STATSD_FORWARD_PORT".into()].into(),
                    value: |address| {
                        BTreeMap::from([
                            (
                                "DD_STATSD_FORWARD_HOST".into(),
                                address.host().expect("a UDP endpoint has a host").into(),
                            ),
                            (
                                "DD_STATSD_FORWARD_PORT".into(),
                                address.port().expect("a UDP endpoint has a port").to_string(),
                            ),
                        ])
                    },
                },
            },
        ),
        ("intake".into(), super::intake()),
    ])
}

struct Service {
    service: &'static str,
    process: &'static str,
    log: &'static str,
    run: &'static [u8],
    finish: &'static [u8],
}

impl Service {
    const fn process_spec(&self) -> ProcessSpec {
        ProcessSpec {
            name: self.process,
            exit: ExitChannel::Record { file: self.service },
            output: OutputChannel::File {
                file: self.log,
                stream: OutputStream::Merged,
            },
        }
    }
}

const SERVICES: [Service; 2] = [
    Service {
        service: "data-plane",
        process: "adp",
        log: "adp.log",
        run: include_bytes!("../../../../../../docker/s6-services/data-plane/run"),
        finish: include_bytes!("../../../../../../docker/s6-services/data-plane/finish"),
    },
    Service {
        service: "agent",
        process: "agent",
        log: "agent.log",
        run: include_bytes!("../../../../../../docker/s6-services/agent/run"),
        finish: include_bytes!("assets/s6/agent-finish"),
    },
];

pub(super) const PROCESS_SPECS: [ProcessSpec; 2] = [SERVICES[0].process_spec(), SERVICES[1].process_spec()];

pub(super) fn recipe(config: &RenderedConfig) -> Result<Recipe, String> {
    let mut recipe = ContainerRecipe::new(AgentAdp::LINUX_IMAGE).with_env(config.env().clone());
    if let Some(hook) = super::late::hook(config.env(), config.params()) {
        recipe = recipe.with_file("/etc/cont-init.d/10-panoramic-late-substitution.sh", 0o755, hook);
    }
    for (name, contents) in config.files() {
        let (path, mode) = if let Some(relative) = name.strip_prefix("files/") {
            (format!("{CASE_FILES_DIR}/{relative}"), 0o644)
        } else if let Some(hook) = name.strip_prefix("cont-init.d/") {
            if hook.contains('/') {
                return Err(format!("agent-adp cont-init.d hook '{name}' must not be nested"));
            }
            (format!("/etc/cont-init.d/50-{hook}"), 0o755)
        } else {
            return Err(format!(
                "internal error: agent-adp received undeclared configuration file '{name}'"
            ));
        };
        recipe = recipe.with_file(path, mode, contents.clone());
    }
    for service in &SERVICES {
        let prefix = format!(
            r"#!/usr/bin/execlineb -S1
foreground {{
    importas -i D PANORAMIC_EXIT_DIR
    backtick -n T {{ date +%s.%N }}
    importas -u T T
    redirfd -a 1 ${{D}}/{}
    echo ${{1}} ${{2}} ${{T}}
}}
",
            service.service
        );
        let finish = [prefix.as_bytes(), &body(service.finish)].concat();
        let run = [
            b"#!/usr/bin/execlineb -P\nfdmove -c 2 1\n".as_slice(),
            &body(service.run),
        ]
        .concat();
        let log = format!(
            "#!/usr/bin/execlineb -P\nimportas -i L PANORAMIC_LOG_DIR\ntee -a ${{L}}/{}\n",
            service.log
        );
        recipe = recipe
            .with_file(format!("/etc/services.d/{}/finish", service.service), 0o755, finish)
            .with_file(format!("/etc/services.d/{}/run", service.service), 0o755, run)
            .with_file(
                format!("/etc/services.d/{}/log/run", service.service),
                0o755,
                log.into_bytes(),
            );
    }
    Ok(Recipe::Container(recipe))
}

/// Returns a shipped service script without its shebang line, with LF line endings.
///
/// The scripts run under `execlineb` in Linux containers. A Windows checkout may still hold them
/// with CRLF line endings, which `execlineb` would read as part of each word.
fn body(script: &[u8]) -> Vec<u8> {
    let body = &script[script
        .iter()
        .position(|&byte| byte == b'\n')
        .expect("embedded service script has a shebang")
        + 1..];
    let mut lf = Vec::with_capacity(body.len());
    for (index, &byte) in body.iter().enumerate() {
        if byte != b'\r' || body.get(index + 1) != Some(&b'\n') {
            lf.push(byte);
        }
    }
    lf
}

#[cfg(test)]
mod tests {
    use panoramic_core::{
        endpoint::{
            test_support::{
                assert_network_endpoint, assert_socket_endpoint, assert_tls_endpoint, http_address, unix_stream_address,
            },
            Evidence,
        },
        layout::{Os, Realization, RealizationKind},
        participant::{test_support::run_operation, CaseValues, EffectiveConfig, ParticipantType},
        recipe::test_support::{assert_image_recipe, files, rendered},
        step::ExecOutput,
    };

    use super::*;

    const LINUX: Realization = Realization {
        os: Os::Linux,
        kind: RealizationKind::LinuxContainer,
    };

    fn config(overrides: &[(&str, &str)]) -> EffectiveConfig {
        let mut env = AgentAdp.default_env(LINUX, &CaseValues::default());
        env.extend(overrides.iter().map(|(key, value)| ((*key).into(), (*value).into())));
        EffectiveConfig {
            env,
            ..Default::default()
        }
    }

    fn otlp_config(overrides: &[(&str, &str)]) -> EffectiveConfig {
        let mut config = config(&[
            ("DD_DATA_PLANE_ENABLED", "true"),
            ("DD_DATA_PLANE_OTLP_ENABLED", "true"),
            ("DD_DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY", "0.0.0.0:56317"),
            ("DD_DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY", "0.0.0.0:56318"),
        ]);
        config
            .env
            .extend(overrides.iter().map(|(key, value)| ((*key).into(), (*value).into())));
        config
    }

    fn cli_output(exit_code: i64, stdout: &[u8]) -> ExecOutput {
        ExecOutput {
            stdout: stdout.into(),
            stderr: b"stderr-secret".to_vec(),
            exit_code,
        }
    }

    #[tokio::test]
    async fn config_dump_runs_the_cli_for_the_requested_view_and_returns_parsed_json() {
        let expected = serde_json::json!({"domains": {"remote_configuration": {"enabled": true}}});
        for view in ["source", "runtime"] {
            let value = run_operation(&ConfigDump, serde_json::json!({"view": view}), |command| {
                let mut argv = vec!["/opt/datadog-agent/embedded/bin/agent-data-plane", "config", "--json"];
                if view == "runtime" {
                    argv.push("--runtime");
                }
                assert_eq!(command.argv, argv);
                assert!(command.env.is_empty());
                assert!(command.stdin.is_none());
                Ok(cli_output(0, &serde_json::to_vec(&expected).unwrap()))
            })
            .await
            .unwrap();
            assert_eq!(value, expected, "{view}");
        }
    }

    #[tokio::test]
    async fn config_dump_rejects_every_argument_shape_except_one_known_view_before_exec() {
        for args in [
            Value::Null,
            serde_json::json!("source"),
            serde_json::json!(["source"]),
            serde_json::json!({}),
            serde_json::json!({"view": "unknown-view-secret"}),
            serde_json::json!({"view": null}),
            serde_json::json!({"view": true}),
            serde_json::json!({"view": 1}),
            serde_json::json!({"view": ["source"]}),
            serde_json::json!({"view": {"source": true}}),
            serde_json::json!({"view": "source", "extra": "argument-secret"}),
            serde_json::json!({"view": "runtime", "extra": "argument-secret"}),
            serde_json::json!({"extra": "argument-secret"}),
        ] {
            let error = run_operation(&ConfigDump, args.clone(), |_| panic!("invalid arguments must not exec"))
                .await
                .unwrap_err();
            assert!(matches!(error, HandleError::Harness { .. }), "{args}: {error}");
            assert!(error.to_string().contains("view"));
            assert!(!error.to_string().contains("secret"), "{error}");
        }
    }

    #[tokio::test]
    async fn config_dump_classifies_cli_exits_without_disclosing_output() {
        for code in [1, 3, 10, 125, 126, 127, 128, 255] {
            let error = run_operation(&ConfigDump, serde_json::json!({"view": "runtime"}), |_| {
                Ok(cli_output(code, b"stdout-secret"))
            })
            .await
            .unwrap_err();
            let expected = match code {
                126 | 127 => HandleError::Harness {
                    message: format!("ADP configuration CLI could not run (exit {code})"),
                },
                _ => HandleError::Product {
                    message: format!("ADP configuration CLI exited with {code}"),
                },
            };
            assert_eq!(error, expected);
            assert!(!error.to_string().contains("secret"), "{error}");
        }
    }

    #[tokio::test]
    async fn config_dump_reports_invalid_json_as_a_product_error_without_disclosing_output() {
        for stdout in [
            b"stdout-secret".as_slice(),
            b"{\"stdout-secret\":}".as_slice(),
            b"{\"api_key\":\"stdout-secret\"} trailing".as_slice(),
            b"\xffstdout-secret".as_slice(),
            b"".as_slice(),
        ] {
            let error = run_operation(&ConfigDump, serde_json::json!({"view": "source"}), |_| {
                Ok(cli_output(0, stdout))
            })
            .await
            .unwrap_err();
            assert_eq!(
                error,
                HandleError::Product {
                    message: "ADP configuration CLI returned invalid JSON".into(),
                }
            );
            assert!(!error.to_string().contains("secret"), "{error}");
        }
    }

    #[tokio::test]
    async fn config_dump_preserves_exec_failure_classification() {
        for error in [
            HandleError::Harness {
                message: "exec unavailable".into(),
            },
            HandleError::Product {
                message: "exec timed out".into(),
            },
        ] {
            let actual = run_operation(&ConfigDump, serde_json::json!({"view": "source"}), |_| {
                Err(error.clone())
            })
            .await
            .unwrap_err();
            assert_eq!(actual, error);
        }
    }

    #[test]
    fn linux_defaults_isolate_listener_ports_and_keep_the_other_image_defaults() {
        let config = config(&[("DD_DATA_PLANE_ENABLED", "true")]);
        assert_eq!(AgentAdp.default_env(LINUX, &CaseValues::default()).len(), 12);
        for (key, value) in [
            ("DD_CMD_PORT", "55001"),
            ("DD_GUI_PORT", "-1"),
            ("DD_EXPVAR_PORT", "55000"),
            ("DD_APM_RECEIVER_PORT", "58126"),
            ("DD_PROCESS_CONFIG_CMD_PORT", "56062"),
            ("DD_AGENT_IPC_PORT", "55004"),
            ("DD_DOGSTATSD_PORT", "58125"),
            ("DD_DATA_PLANE_API_LISTEN_ADDRESS", "tcp://0.0.0.0:55100"),
            ("DD_DATA_PLANE_SECURE_API_LISTEN_ADDRESS", "tcp://0.0.0.0:55101"),
            ("DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR", "tcp://0.0.0.0:55102"),
            ("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT", "0.0.0.0:54317"),
            ("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT", "0.0.0.0:54318"),
        ] {
            assert_eq!(config.env[key], value, "{key}");
        }
        let endpoints = AgentAdp.endpoints(LINUX, &config).unwrap();
        for (name, kind, port) in [
            ("api", EndpointKind::Http, 55100),
            ("secure_api", EndpointKind::Tcp, 55101),
            ("agent_cmd", EndpointKind::Tcp, 55001),
            ("agent_ipc", EndpointKind::Tcp, 55004),
        ] {
            assert_network_endpoint(&endpoints[name], kind, port, Some(Evidence::tcp_accepts(name)));
        }
        assert_eq!(endpoints.len(), 5);
        assert!(!endpoints.contains_key("dogstatsd_udp"));
        assert!(AgentAdp.readiness(LINUX, &config).is_empty());
        assert_eq!(AgentAdp.stop_grace(LINUX), std::time::Duration::from_secs(10));
    }

    #[test]
    fn adp_listeners_prefer_adp_overrides_and_agent_listeners_ignore_them() {
        let endpoints = AgentAdp
            .endpoints(
                LINUX,
                &config(&[
                    ("DD_DATA_PLANE_ENABLED", "true"),
                    ("ADP_DD_DOGSTATSD_PORT", "8126"),
                    ("ADP_DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "true"),
                    ("ADP_DD_DATA_PLANE_API_LISTEN_ADDRESS", "tcp://0.0.0.0:8080"),
                    ("ADP_DD_DATA_PLANE_SECURE_API_LISTEN_ADDRESS", "tcp://[::]:8081"),
                    ("ADP_DD_CMD_PORT", "invalid"),
                    ("ADP_DD_AGENT_IPC_PORT", "invalid"),
                ]),
            )
            .unwrap();
        assert_network_endpoint(
            &endpoints["api"],
            EndpointKind::Http,
            8080,
            Some(Evidence::tcp_accepts("api")),
        );
        assert_network_endpoint(
            &endpoints["secure_api"],
            EndpointKind::Tcp,
            8081,
            Some(Evidence::tcp_accepts("secure_api")),
        );
        assert_network_endpoint(
            &endpoints["agent_cmd"],
            EndpointKind::Tcp,
            55001,
            Some(Evidence::tcp_accepts("agent_cmd")),
        );
        assert_network_endpoint(
            &endpoints["agent_ipc"],
            EndpointKind::Tcp,
            55004,
            Some(Evidence::tcp_accepts("agent_ipc")),
        );
        assert_network_endpoint(
            &endpoints["dogstatsd_udp"],
            EndpointKind::Udp,
            8126,
            Some(Evidence::output_matches(
                "adp",
                r#"listen_addr:"udp://[^"]+:8126" \| DogStatsD listener started\."#,
            )),
        );
    }

    #[test]
    fn dogstatsd_ownership_and_enablement_follow_the_agents_pipeline_gates() {
        for overrides in [
            vec![],
            vec![
                ("DD_DATA_PLANE_ENABLED", "true"),
                ("DD_DATA_PLANE_DOGSTATSD_ENABLED", "false"),
            ],
        ] {
            let mut config = config(&overrides);
            config.env.insert("ADP_DD_DOGSTATSD_PORT".into(), "invalid".into());
            config
                .env
                .insert("DD_DOGSTATSD_NON_LOCAL_TRAFFIC".into(), "true".into());
            let endpoints = AgentAdp.endpoints(LINUX, &config).unwrap();
            assert_network_endpoint(&endpoints["dogstatsd_udp"], EndpointKind::Udp, 58125, None);
        }
        for (key, value) in [
            ("DD_USE_DOGSTATSD", "false"),
            ("DD_DOGSTATSD_PORT", "0"),
            ("ADP_DD_DOGSTATSD_PORT", "0"),
        ] {
            let endpoints = AgentAdp
                .endpoints(
                    LINUX,
                    &config(&[
                        ("DD_DATA_PLANE_ENABLED", "true"),
                        ("DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "true"),
                        (key, value),
                    ]),
                )
                .unwrap();
            assert!(!endpoints.contains_key("dogstatsd_udp"));
            if key == "DD_USE_DOGSTATSD" {
                assert!(!endpoints.contains_key("dogstatsd_uds"));
            }
        }
    }

    #[test]
    fn dogstatsd_tcp_requires_adp_to_own_an_enabled_dogstatsd_pipeline() {
        for adp in ["false", "true"] {
            for dogstatsd in ["false", "true"] {
                for adp_dogstatsd in ["false", "true"] {
                    let enabled = adp == "true" && dogstatsd == "true" && adp_dogstatsd == "true";
                    for non_local in ["false", "true"] {
                        for port in ["9126", "invalid"] {
                            let config = config(&[
                                ("DD_DATA_PLANE_ENABLED", adp),
                                ("DD_USE_DOGSTATSD", dogstatsd),
                                ("DD_DATA_PLANE_DOGSTATSD_ENABLED", adp_dogstatsd),
                                ("DD_DOGSTATSD_TCP_PORT", port),
                                ("DD_DOGSTATSD_NON_LOCAL_TRAFFIC", non_local),
                            ]);
                            let result = AgentAdp.endpoints(LINUX, &config);
                            if enabled && port == "invalid" {
                                assert!(result.unwrap_err().contains("DD_DOGSTATSD_TCP_PORT"));
                                continue;
                            }
                            let endpoints = result.unwrap();
                            assert_eq!(
                                endpoints.contains_key("dogstatsd_tcp"),
                                enabled && non_local == "true",
                                "ADP={adp}, DogStatsD={dogstatsd}, ADP DogStatsD={adp_dogstatsd}, port={port}, non-local={non_local}"
                            );
                            if enabled && non_local == "true" {
                                assert_network_endpoint(
                                    &endpoints["dogstatsd_tcp"],
                                    EndpointKind::Tcp,
                                    9126,
                                    Some(Evidence::tcp_accepts("dogstatsd_tcp")),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn dogstatsd_tcp_defaults_to_disabled_and_adp_port_overrides_win() {
        for (dd_port, adp_port, expected) in [
            (None, None, None),
            (Some("0"), None, None),
            (Some("9126"), None, Some(9126)),
            (None, Some("9127"), Some(9127)),
            (Some("9126"), Some("9127"), Some(9127)),
            (Some("invalid"), Some("9127"), Some(9127)),
            (Some("9126"), Some("0"), None),
            (Some("0"), Some("9127"), Some(9127)),
        ] {
            let mut config = config(&[
                ("DD_DATA_PLANE_ENABLED", "true"),
                ("DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "true"),
            ]);
            for (key, value) in [
                ("DD_DOGSTATSD_TCP_PORT", dd_port),
                ("ADP_DD_DOGSTATSD_TCP_PORT", adp_port),
            ] {
                if let Some(value) = value {
                    config.env.insert(key.into(), value.into());
                }
            }
            let endpoints = AgentAdp.endpoints(LINUX, &config).unwrap();
            match expected {
                Some(port) => assert_network_endpoint(
                    &endpoints["dogstatsd_tcp"],
                    EndpointKind::Tcp,
                    port,
                    Some(Evidence::tcp_accepts("dogstatsd_tcp")),
                ),
                None => assert!(!endpoints.contains_key("dogstatsd_tcp")),
            }
            assert!(endpoints.contains_key("dogstatsd_uds"));
        }
    }

    #[test]
    fn adp_udp_readiness_matches_the_console_format_and_only_its_port() {
        // ADP's console formatter writes quoted fields before the message.
        let line = r#"2026-11-04 10:00:00 UTC | DATAPLANE | INFO | (lib/saluki-components/src/sources/dogstatsd/mod.rs:1265 in saluki_components::sources::dogstatsd) | listen_addr:"udp://0.0.0.0:58125" | DogStatsD listener started."#;
        let pattern = r#"listen_addr:"udp://[^"]+:58125" \| DogStatsD listener started\."#;
        let endpoints = AgentAdp
            .endpoints(
                LINUX,
                &config(&[
                    ("DD_DATA_PLANE_ENABLED", "true"),
                    ("DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "true"),
                ]),
            )
            .unwrap();
        assert_network_endpoint(
            &endpoints["dogstatsd_udp"],
            EndpointKind::Udp,
            58125,
            Some(Evidence::output_matches("adp", pattern)),
        );
        let regex = regex::Regex::new(pattern).unwrap();
        assert!(regex.is_match(line));
        assert!(!regex.is_match(&line.replace(":58125", ":58126")));
        assert!(!regex.is_match(&line.replace("DogStatsD listener started.", "DogStatsD listener stopped.")));
    }

    #[test]
    fn dogstatsd_network_listeners_are_declared_only_when_they_accept_non_local_traffic() {
        for enabled in ["false", "true"] {
            for (overrides, declared) in [
                (vec![], false),
                (vec![("DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "true")], true),
                (vec![("DD_BIND_HOST", "0.0.0.0")], true),
                (vec![("DD_BIND_HOST", "::")], true),
                (vec![("DD_BIND_HOST", "192.0.2.1")], true),
                (vec![("DD_BIND_HOST", "listener.example")], true),
                (vec![("DD_BIND_HOST", "127.0.0.1")], false),
                (vec![("DD_BIND_HOST", "127.1.2.3")], false),
                (vec![("DD_BIND_HOST", "::1")], false),
                (vec![("DD_BIND_HOST", "::ffff:127.0.0.1")], false),
                (vec![("DD_BIND_HOST", "localhost")], false),
                (vec![("DD_BIND_HOST", "LOCALHOST.")], false),
                (
                    vec![
                        ("DD_BIND_HOST", "127.0.0.1"),
                        ("DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "true"),
                    ],
                    true,
                ),
            ] {
                let mut config = config(&overrides);
                config.env.insert("DD_DATA_PLANE_ENABLED".into(), enabled.into());
                config.env.insert("DD_DOGSTATSD_TCP_PORT".into(), "9126".into());
                let endpoints = AgentAdp.endpoints(LINUX, &config).unwrap();
                assert_eq!(
                    endpoints.contains_key("dogstatsd_udp"),
                    declared,
                    "{enabled}: {overrides:?}"
                );
                assert_eq!(
                    endpoints.contains_key("dogstatsd_tcp"),
                    enabled == "true" && declared,
                    "{enabled}: {overrides:?}"
                );
            }
        }
        for (overrides, adp, agent) in [
            (
                vec![
                    ("DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "true"),
                    ("ADP_DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "false"),
                ],
                false,
                true,
            ),
            (
                vec![
                    ("DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "false"),
                    ("ADP_DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "true"),
                ],
                true,
                false,
            ),
            (
                vec![("DD_BIND_HOST", "0.0.0.0"), ("ADP_DD_BIND_HOST", "127.0.0.1")],
                false,
                true,
            ),
            (
                vec![("DD_BIND_HOST", "127.0.0.1"), ("ADP_DD_BIND_HOST", "0.0.0.0")],
                true,
                false,
            ),
        ] {
            for (enabled, expected) in [("true", adp), ("false", agent)] {
                let mut config = config(&overrides);
                config.env.insert("DD_DATA_PLANE_ENABLED".into(), enabled.into());
                config.env.insert("DD_DOGSTATSD_TCP_PORT".into(), "9126".into());
                let endpoints = AgentAdp.endpoints(LINUX, &config).unwrap();
                assert_eq!(
                    endpoints.contains_key("dogstatsd_udp"),
                    expected,
                    "{enabled}: {overrides:?}"
                );
                assert_eq!(
                    endpoints.contains_key("dogstatsd_tcp"),
                    enabled == "true" && expected,
                    "{enabled}: {overrides:?}"
                );
            }
        }
        for key in ["DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "ADP_DD_DOGSTATSD_NON_LOCAL_TRAFFIC"] {
            for (udp_port, tcp_port) in [("58125", "0"), ("0", "9126")] {
                let error = AgentAdp
                    .endpoints(
                        LINUX,
                        &config(&[
                            ("DD_DATA_PLANE_ENABLED", "true"),
                            ("DD_DOGSTATSD_PORT", udp_port),
                            ("DD_DOGSTATSD_TCP_PORT", tcp_port),
                            (key, "invalid"),
                        ]),
                    )
                    .unwrap_err();
                assert!(error.contains(key), "{error}");
            }
        }
    }

    #[test]
    fn loopback_api_addresses_are_configuration_errors_with_the_owners_precedence() {
        for key in [
            "DD_DATA_PLANE_API_LISTEN_ADDRESS",
            "DD_DATA_PLANE_SECURE_API_LISTEN_ADDRESS",
            "ADP_DD_DATA_PLANE_API_LISTEN_ADDRESS",
            "ADP_DD_DATA_PLANE_SECURE_API_LISTEN_ADDRESS",
        ] {
            for host in [
                "127.0.0.1",
                "127.9.8.7",
                "[::1]",
                "[::ffff:127.0.0.1]",
                "localhost",
                "localhost.",
            ] {
                let value = format!("tcp://{host}:8080");
                let config = config(&[("DD_DATA_PLANE_ENABLED", "true"), (key, &value)]);
                let error = AgentAdp
                    .validate_config(
                        LINUX,
                        &CaseValues {
                            env: config.env.clone(),
                            ..Default::default()
                        },
                    )
                    .unwrap_err();
                assert!(error.contains(key) && error.contains("non-loopback"), "{error}");
                assert_eq!(AgentAdp.endpoints(LINUX, &config).unwrap_err(), error);
            }
        }
        let endpoints = AgentAdp
            .endpoints(
                LINUX,
                &config(&[
                    ("DD_DATA_PLANE_ENABLED", "true"),
                    ("DD_DATA_PLANE_API_LISTEN_ADDRESS", "tcp://127.0.0.1:8080"),
                    ("ADP_DD_DATA_PLANE_API_LISTEN_ADDRESS", "tcp://0.0.0.0:8081"),
                ]),
            )
            .unwrap();
        assert_network_endpoint(
            &endpoints["api"],
            EndpointKind::Http,
            8081,
            Some(Evidence::tcp_accepts("api")),
        );
    }

    #[test]
    fn otlp_endpoints_use_non_loopback_addresses_and_tcp_readiness_including_in_proxy_mode() {
        for host in ["0.0.0.0", "192.0.2.1", "listener.example", "[::]", "[2001:db8::1]"] {
            for proxy in ["true", "false"] {
                let grpc = format!("{host}:56317");
                let http = format!("{host}:56318");
                let config = otlp_config(&[
                    ("DD_DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY", &grpc),
                    ("DD_DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY", &http),
                    ("DD_DATA_PLANE_OTLP_PROXY_ENABLED", proxy),
                ]);
                AgentAdp
                    .validate_config(
                        LINUX,
                        &CaseValues {
                            env: config.env.clone(),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                let endpoints = AgentAdp.endpoints(LINUX, &config).unwrap();
                for (name, kind, port) in [
                    ("otlp_grpc", EndpointKind::Grpc, 56317),
                    ("otlp_http", EndpointKind::Http, 56318),
                ] {
                    assert_network_endpoint(&endpoints[name], kind, port, Some(Evidence::tcp_accepts(name)));
                }
            }
        }
    }

    #[test]
    fn otlp_http_uses_https_when_its_tls_certificate_and_key_are_set() {
        for prefix in ["DD_", "ADP_DD_"] {
            let cert = format!("{prefix}OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE");
            let key = format!("{prefix}OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_KEY_FILE");
            let config = otlp_config(&[(&cert, "/cert.pem"), (&key, "/key.pem")]);
            AgentAdp
                .validate_config(
                    LINUX,
                    &CaseValues {
                        env: config.env.clone(),
                        ..Default::default()
                    },
                )
                .unwrap();
            let endpoints = AgentAdp.endpoints(LINUX, &config).unwrap();
            assert_network_endpoint(
                &endpoints["otlp_http"],
                EndpointKind::Http,
                56318,
                Some(Evidence::tcp_accepts("otlp_http")),
            );
            assert_tls_endpoint(&endpoints["otlp_http"], true);
            assert_tls_endpoint(&endpoints["otlp_grpc"], false);
            assert_tls_endpoint(&endpoints["api"], false);
        }
    }

    #[test]
    fn otlp_http_stays_plain_without_http_tls_keys() {
        for overrides in [
            vec![],
            vec![
                ("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE", ""),
                ("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_KEY_FILE", ""),
            ],
            vec![
                ("ADP_DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE", ""),
                ("ADP_DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_KEY_FILE", ""),
            ],
            vec![
                ("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_TLS_CERT_FILE", "/cert.pem"),
                ("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_TLS_KEY_FILE", "/key.pem"),
            ],
            vec![
                ("ADP_DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_TLS_CERT_FILE", "/cert.pem"),
                ("ADP_DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_TLS_KEY_FILE", "/key.pem"),
            ],
        ] {
            let endpoints = AgentAdp.endpoints(LINUX, &otlp_config(&overrides)).unwrap();
            assert_tls_endpoint(&endpoints["otlp_http"], false);
            assert_tls_endpoint(&endpoints["otlp_grpc"], false);
        }
    }

    #[test]
    fn one_otlp_http_tls_key_alone_is_a_configuration_error() {
        for prefix in ["DD_", "ADP_DD_"] {
            let cert_key = format!("{prefix}OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE");
            let key_key = format!("{prefix}OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_KEY_FILE");
            for (cert, key) in [
                (Some("/cert.pem"), None),
                (None, Some("/key.pem")),
                (Some("/cert.pem"), Some("")),
                (Some(""), Some("/key.pem")),
            ] {
                for address in ["0.0.0.0:56318", "127.0.0.1:56318", "0.0.0.0:0"] {
                    let mut config = otlp_config(&[("DD_DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY", address)]);
                    for (name, value) in [(&cert_key, cert), (&key_key, key)] {
                        if let Some(value) = value {
                            config.env.insert(name.clone(), value.into());
                        }
                    }
                    let error = AgentAdp
                        .validate_config(
                            LINUX,
                            &CaseValues {
                                env: config.env.clone(),
                                ..Default::default()
                            },
                        )
                        .unwrap_err();
                    assert!(
                        error.contains("OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE"),
                        "{error}"
                    );
                    assert!(
                        error.contains("OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_KEY_FILE"),
                        "{error}"
                    );
                    assert_eq!(AgentAdp.endpoints(LINUX, &config).unwrap_err(), error);
                }
            }
        }
    }

    #[test]
    fn otlp_http_tls_prefers_adp_overrides_including_empty_values() {
        for (dd_cert, dd_key, adp_cert, adp_key, expected) in [
            (None, None, None, None, Some(false)),
            (Some("cert"), Some("key"), None, None, Some(true)),
            (Some(""), Some(""), Some("cert"), Some("key"), Some(true)),
            (Some("cert"), Some("key"), Some(""), Some(""), Some(false)),
            (Some("cert"), Some("key"), Some("adp-cert"), Some("adp-key"), Some(true)),
            (Some(""), Some("key"), Some("adp-cert"), None, Some(true)),
            (Some("cert"), Some(""), None, Some("adp-key"), Some(true)),
            (None, Some("key"), Some("adp-cert"), None, Some(true)),
            (Some("cert"), None, None, Some("adp-key"), Some(true)),
            (Some("cert"), None, Some(""), None, Some(false)),
            (None, Some("key"), None, Some(""), Some(false)),
            (Some("cert"), Some("key"), Some(""), None, None),
            (Some("cert"), Some("key"), None, Some(""), None),
            (Some("cert"), Some("key"), Some("adp-cert"), Some(""), None),
            (Some("cert"), Some("key"), Some(""), Some("adp-key"), None),
        ] {
            let mut config = otlp_config(&[]);
            for (name, value) in [
                ("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE", dd_cert),
                ("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_KEY_FILE", dd_key),
                ("ADP_DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE", adp_cert),
                ("ADP_DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_KEY_FILE", adp_key),
            ] {
                if let Some(value) = value {
                    config.env.insert(name.into(), value.into());
                }
            }
            let result = AgentAdp.endpoints(LINUX, &config);
            match expected {
                Some(tls) => assert_tls_endpoint(&result.unwrap()["otlp_http"], tls),
                None => {
                    let error = result.unwrap_err();
                    let cert_prefix = if adp_cert.is_some() { "ADP_DD_" } else { "DD_" };
                    let key_prefix = if adp_key.is_some() { "ADP_DD_" } else { "DD_" };
                    assert_eq!(
                        error,
                        format!(
                            "{cert_prefix}OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE and \
                             {key_prefix}OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_KEY_FILE must both be nonempty \
                             to enable OTLP HTTP TLS, or both empty"
                        )
                    );
                }
            }
        }
    }

    #[test]
    fn disabled_otlp_ignores_http_tls_configuration() {
        for overrides in [
            vec![("DD_DATA_PLANE_ENABLED", "false")],
            vec![("DD_DATA_PLANE_OTLP_ENABLED", "false")],
            vec![("ADP_DD_DATA_PLANE_OTLP_ENABLED", "false")],
        ] {
            let mut config = otlp_config(&overrides);
            config.env.insert(
                "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE".into(),
                "cert.pem".into(),
            );
            let endpoints = AgentAdp.endpoints(LINUX, &config).unwrap();
            assert!(!endpoints.contains_key("otlp_http"));
        }
    }

    #[test]
    fn otlp_addresses_and_enablement_prefer_adp_overrides() {
        let config = otlp_config(&[
            ("DD_DATA_PLANE_OTLP_ENABLED", "invalid"),
            ("ADP_DD_DATA_PLANE_OTLP_ENABLED", "true"),
            ("DD_DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY", "invalid"),
            ("DD_DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY", "invalid"),
            (
                "ADP_DD_DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY",
                "0.0.0.0:57317",
            ),
            ("ADP_DD_DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY", "[::]:57318"),
        ]);
        let endpoints = AgentAdp.endpoints(LINUX, &config).unwrap();
        for (name, kind, port) in [
            ("otlp_grpc", EndpointKind::Grpc, 57317),
            ("otlp_http", EndpointKind::Http, 57318),
        ] {
            assert_network_endpoint(&endpoints[name], kind, port, Some(Evidence::tcp_accepts(name)));
        }
        let endpoints = AgentAdp
            .endpoints(
                LINUX,
                &otlp_config(&[
                    ("DD_DATA_PLANE_OTLP_ENABLED", "false"),
                    ("ADP_DD_DATA_PLANE_OTLP_ENABLED", "true"),
                ]),
            )
            .unwrap();
        assert!(endpoints.contains_key("otlp_grpc") && endpoints.contains_key("otlp_http"));
    }

    #[test]
    fn otlp_endpoints_require_both_adp_and_its_otlp_pipeline_to_be_enabled() {
        for overrides in [
            vec![("DD_DATA_PLANE_ENABLED", "false")],
            vec![
                ("DD_DATA_PLANE_ENABLED", "false"),
                ("ADP_DD_DATA_PLANE_ENABLED", "true"),
                ("DD_DATA_PLANE_OTLP_ENABLED", "invalid"),
            ],
            vec![("DD_DATA_PLANE_OTLP_ENABLED", "false")],
            vec![("ADP_DD_DATA_PLANE_OTLP_ENABLED", "false")],
        ] {
            let mut config = otlp_config(&overrides);
            for key in [
                "DD_DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY",
                "ADP_DD_DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY",
            ] {
                config.env.insert(key.into(), "invalid".into());
            }
            let endpoints = AgentAdp.endpoints(LINUX, &config).unwrap();
            assert!(
                !endpoints.contains_key("otlp_grpc") && !endpoints.contains_key("otlp_http"),
                "{overrides:?}"
            );
        }
        for missing in ["DD_DATA_PLANE_ENABLED", "DD_DATA_PLANE_OTLP_ENABLED"] {
            let mut config = otlp_config(&[]);
            config.env.remove(missing);
            let endpoints = AgentAdp.endpoints(LINUX, &config).unwrap();
            assert!(!endpoints.contains_key("otlp_grpc") && !endpoints.contains_key("otlp_http"));
        }
        for key in ["DD_DATA_PLANE_OTLP_ENABLED", "ADP_DD_DATA_PLANE_OTLP_ENABLED"] {
            let error = AgentAdp
                .endpoints(LINUX, &otlp_config(&[(key, "invalid")]))
                .unwrap_err();
            assert!(error.contains(key) && error.contains("true or false"), "{error}");
        }
    }

    #[test]
    fn otlp_defaults_and_explicit_loopback_addresses_omit_endpoints() {
        let defaults = AgentAdp
            .endpoints(
                LINUX,
                &config(&[
                    ("DD_DATA_PLANE_ENABLED", "true"),
                    ("DD_DATA_PLANE_OTLP_ENABLED", "true"),
                ]),
            )
            .unwrap();
        assert!(!defaults.contains_key("otlp_grpc") && !defaults.contains_key("otlp_http"));
        for host in [
            "localhost",
            "LOCALHOST.",
            "receiver.localhost",
            "127.0.0.1",
            "127.9.8.7",
            "127.1",
            "2130706433",
            "[::1]",
            "[::ffff:127.0.0.1]",
        ] {
            for prefix in ["DD_", "ADP_DD_"] {
                let grpc_key = format!("{prefix}DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY");
                let http_key = format!("{prefix}DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY");
                let address = format!("{host}:56317");
                let endpoints = AgentAdp
                    .endpoints(LINUX, &otlp_config(&[(&grpc_key, &address), (&http_key, &address)]))
                    .unwrap();
                assert!(
                    !endpoints.contains_key("otlp_grpc") && !endpoints.contains_key("otlp_http"),
                    "{prefix} {host}"
                );
            }
        }
    }

    #[test]
    fn a_zero_otlp_port_disables_only_that_endpoint_including_with_adp_overrides() {
        for prefix in ["DD_", "ADP_DD_"] {
            for (name, suffix, other) in [("otlp_grpc", "GRPC", "otlp_http"), ("otlp_http", "HTTP", "otlp_grpc")] {
                let key = format!("{prefix}DATA_PLANE_OTLP_RECEIVER_{suffix}_ENDPOINT_TEMPORARY");
                let endpoints = AgentAdp.endpoints(LINUX, &otlp_config(&[(&key, "0.0.0.0:0")])).unwrap();
                assert!(!endpoints.contains_key(name), "{key}");
                assert!(endpoints.contains_key(other), "{key}");
            }
        }
    }

    #[test]
    fn invalid_otlp_addresses_are_configuration_errors_naming_the_effective_key() {
        for prefix in ["DD_", "ADP_DD_"] {
            for protocol in ["GRPC", "HTTP"] {
                let key = format!("{prefix}DATA_PLANE_OTLP_RECEIVER_{protocol}_ENDPOINT_TEMPORARY");
                for value in [
                    "",
                    "listener.example",
                    "0.0.0.0",
                    ":56317",
                    "0.0.0.0:",
                    "0.0.0.0:-1",
                    "0.0.0.0:65536",
                    "0.0.0.0:garbage",
                    "::1:56317",
                    "tcp://0.0.0.0:56317",
                    "http://0.0.0.0:56317",
                    "https://0.0.0.0:56317",
                    "0.0.0.0:56317/",
                    "0.0.0.0:56317/path",
                    "0.0.0.0:56317?query",
                    "0.0.0.0:56317#fragment",
                    "user@0.0.0.0:56317",
                    "@0.0.0.0:56317",
                    " 0.0.0.0:56317",
                    "0.0.0.0:56317\n",
                ] {
                    let config = otlp_config(&[(&key, value)]);
                    let error = AgentAdp
                        .validate_config(
                            LINUX,
                            &CaseValues {
                                env: config.env.clone(),
                                ..Default::default()
                            },
                        )
                        .unwrap_err();
                    assert!(error.contains(&key), "{key}={value:?}: {error}");
                    assert_eq!(AgentAdp.endpoints(LINUX, &config).unwrap_err(), error);
                }
            }
        }
    }

    #[test]
    fn socket_paths_use_the_linux_default_and_explicit_empty_values_disable_them() {
        for (overrides, datagram, stream) in [
            (vec![], Some("/var/run/datadog/dsd.socket"), None),
            (vec![("DD_DOGSTATSD_SOCKET", "")], None, None),
            (
                vec![
                    ("DD_DOGSTATSD_SOCKET", "/run/dsd"),
                    ("DD_DOGSTATSD_STREAM_SOCKET", "/run/stream"),
                ],
                Some("/run/dsd"),
                Some("/run/stream"),
            ),
            (
                vec![
                    ("DD_DATA_PLANE_ENABLED", "true"),
                    ("DD_DOGSTATSD_SOCKET", "/ignored"),
                    ("ADP_DD_DOGSTATSD_SOCKET", "/run/override"),
                    ("ADP_DD_DOGSTATSD_STREAM_SOCKET", "/run/stream"),
                ],
                Some("/run/override"),
                Some("/run/stream"),
            ),
        ] {
            let endpoints = AgentAdp.endpoints(LINUX, &config(&overrides)).unwrap();
            for (name, kind, path) in [
                ("dogstatsd_uds", EndpointKind::UnixDatagram, datagram),
                ("dogstatsd_uds_stream", EndpointKind::UnixStream, stream),
            ] {
                match path {
                    Some(path) => {
                        assert_socket_endpoint(&endpoints[name], kind, path, Some(Evidence::socket_exists(name)))
                    }
                    None => assert!(!endpoints.contains_key(name)),
                }
            }
        }
    }

    #[test]
    fn adp_apis_require_adp_and_zero_ports_disable_network_listeners() {
        let disabled = AgentAdp.endpoints(LINUX, &config(&[])).unwrap();
        assert!(!disabled.contains_key("api") && !disabled.contains_key("secure_api"));
        let disabled = AgentAdp
            .endpoints(
                LINUX,
                &config(&[
                    ("DD_DATA_PLANE_ENABLED", "true"),
                    ("DD_DATA_PLANE_API_LISTEN_ADDRESS", "tcp://0.0.0.0:0"),
                    ("DD_DATA_PLANE_SECURE_API_LISTEN_ADDRESS", "tcp://0.0.0.0:0"),
                    ("DD_CMD_PORT", "0"),
                    ("DD_AGENT_IPC_PORT", "-1"),
                    ("DD_DATA_PLANE_TELEMETRY_ENABLED", "true"),
                ]),
            )
            .unwrap();
        for name in ["api", "secure_api", "agent_cmd", "agent_ipc", "telemetry"] {
            assert!(!disabled.contains_key(name));
        }
    }

    #[test]
    fn invalid_listener_values_and_enabling_keys_are_configuration_errors() {
        for (key, value) in [
            ("DD_DOGSTATSD_PORT", "garbage"),
            ("ADP_DD_DOGSTATSD_PORT", "65536"),
            ("DD_DOGSTATSD_TCP_PORT", "garbage"),
            ("DD_DOGSTATSD_TCP_PORT", "-1"),
            ("DD_DOGSTATSD_TCP_PORT", "65536"),
            ("ADP_DD_DOGSTATSD_TCP_PORT", "garbage"),
            ("ADP_DD_DOGSTATSD_TCP_PORT", "-1"),
            ("ADP_DD_DOGSTATSD_TCP_PORT", "65536"),
            ("DD_DATA_PLANE_API_LISTEN_ADDRESS", "0.0.0.0:8080"),
            ("ADP_DD_DATA_PLANE_SECURE_API_LISTEN_ADDRESS", "tcp://host"),
            ("DD_DATA_PLANE_API_LISTEN_ADDRESS", "udp://0.0.0.0:8080"),
            ("DD_CMD_PORT", "-1"),
            ("DD_AGENT_IPC_PORT", "oops"),
            ("DD_USE_DOGSTATSD", "yes"),
            ("DD_DATA_PLANE_ENABLED", "bad"),
            ("DD_DATA_PLANE_DOGSTATSD_ENABLED", "maybe"),
            ("DD_DOGSTATSD_SOCKET", "relative.sock"),
            ("ADP_DD_DOGSTATSD_STREAM_SOCKET", "relative.sock"),
        ] {
            let config = config(&[("DD_DATA_PLANE_ENABLED", "true"), (key, value)]);
            let error = AgentAdp
                .validate_config(
                    LINUX,
                    &CaseValues {
                        env: config.env.clone(),
                        ..Default::default()
                    },
                )
                .unwrap_err();
            assert!(error.contains(key) && error.contains(value), "{error}");
            assert_eq!(AgentAdp.endpoints(LINUX, &config).unwrap_err(), error);
        }
    }

    #[test]
    fn connecting_an_intake_delivers_its_url_to_the_agent_and_apm() {
        let consumes = AgentAdp.consumes(LINUX, &config(&[]));
        assert_eq!(
            consumes.keys().map(String::as_str).collect::<Vec<_>>(),
            ["docker_api", "dogstatsd_forward", "intake"]
        );
        let intake = &consumes["intake"];
        assert_eq!(intake.accepts, [EndpointKind::Http]);
        assert!(!intake.required);
        let Delivery::Env { keys, value } = &intake.delivery;
        let address = http_address("intake", 2049);
        let delivered = value(&address);
        assert_eq!(keys.iter().collect::<Vec<_>>(), delivered.keys().collect::<Vec<_>>());
        assert_eq!(
            delivered,
            BTreeMap::from([
                ("DD_DD_URL".into(), "http://intake:2049".into()),
                ("DD_APM_DD_URL".into(), "http://intake:2049".into())
            ])
        );
    }

    #[test]
    fn docker_api_delivers_docker_host_as_a_unix_url() {
        let config = config(&[]);
        assert!(!config.env.contains_key("DOCKER_HOST"));
        let consumes = AgentAdp.consumes(LINUX, &config);
        let docker_api = &consumes["docker_api"];
        assert_eq!(docker_api.accepts, [EndpointKind::UnixStream]);
        assert!(!docker_api.required);
        let Delivery::Env { keys, value } = &docker_api.delivery;
        let delivered = value(&unix_stream_address("/airlock/docker-api/shared/docker.sock"));
        assert_eq!(keys.iter().collect::<Vec<_>>(), delivered.keys().collect::<Vec<_>>());
        assert_eq!(
            delivered,
            BTreeMap::from([(
                "DOCKER_HOST".into(),
                "unix:///airlock/docker-api/shared/docker.sock".into()
            )])
        );
    }

    #[test]
    fn connecting_a_forward_target_delivers_its_host_and_port() {
        let consumes = AgentAdp.consumes(LINUX, &config(&[]));
        let forward = &consumes["dogstatsd_forward"];
        assert_eq!(forward.accepts, [EndpointKind::Udp]);
        assert!(!forward.required);
        let Delivery::Env { keys, value } = &forward.delivery;
        for (host, port) in [
            ("intake", 9125),
            ("forward-receiver", 18125),
            ("127.0.0.1", 1),
            ("::1", 65535),
        ] {
            let delivered = value(&http_address(host, port));
            assert_eq!(keys.iter().collect::<Vec<_>>(), delivered.keys().collect::<Vec<_>>());
            assert_eq!(
                delivered,
                BTreeMap::from([
                    ("DD_STATSD_FORWARD_HOST".into(), host.into()),
                    ("DD_STATSD_FORWARD_PORT".into(), port.to_string()),
                ])
            );
        }
    }

    #[test]
    fn case_directories_land_under_files_dir_and_cont_init_d() {
        let config = rendered(
            config(&[]).env,
            BTreeMap::from([
                ("files/server.pem".into(), b"certificate".to_vec()),
                ("files/nested/data.txt".into(), b"data".to_vec()),
                (
                    "cont-init.d/empty-resolv-conf.sh".into(),
                    b"#!/bin/sh\n: > /etc/resolv.conf\n".to_vec(),
                ),
            ]),
        );
        let recipe = AgentAdp.recipe(LINUX, &config).unwrap();
        assert_image_recipe(&recipe, AgentAdp::LINUX_IMAGE, config.env());
        let files = files(&recipe);
        assert_eq!(files.len(), 9);
        for expected in [
            ("/var/lib/panoramic/files/server.pem", 0o644, b"certificate".as_slice()),
            ("/var/lib/panoramic/files/nested/data.txt", 0o644, b"data".as_slice()),
            (
                "/etc/cont-init.d/50-empty-resolv-conf.sh",
                0o755,
                b"#!/bin/sh\n: > /etc/resolv.conf\n".as_slice(),
            ),
        ] {
            assert!(files.contains(&expected), "missing {expected:?}");
        }
    }

    #[test]
    fn a_nested_cont_init_hook_is_rejected() {
        let name = "cont-init.d/nested/hook.sh";
        let config = rendered(
            BTreeMap::new(),
            BTreeMap::from([(name.into(), b"#!/bin/sh\n".to_vec())]),
        );
        assert_eq!(
            AgentAdp.recipe(LINUX, &config).err().unwrap(),
            format!("agent-adp cont-init.d hook '{name}' must not be nested")
        );
    }

    #[test]
    fn service_script_bodies_drop_the_shebang_and_crlf_line_endings() {
        assert_eq!(body(b"#!/bin/sh\r\nfoo\r\nbar\rbaz\n"), b"foo\nbar\rbaz\n");
    }

    #[test]
    fn service_overlays_preserve_shipped_bodies_and_match_process_record_and_log_paths() {
        let config = rendered(config(&[]).env, BTreeMap::new());
        let recipe = AgentAdp.recipe(LINUX, &config).unwrap();
        assert_image_recipe(&recipe, AgentAdp::LINUX_IMAGE, config.env());
        let files = files(&recipe);
        assert_eq!(files.len(), 6);
        let finish = files
            .iter()
            .find(|(path, _, _)| *path == "/etc/services.d/data-plane/finish")
            .unwrap()
            .2;
        assert_eq!(
            finish,
            br#"#!/usr/bin/execlineb -S1
foreground {
    importas -i D PANORAMIC_EXIT_DIR
    backtick -n T { date +%s.%N }
    importas -u T T
    redirfd -a 1 ${D}/data-plane
    echo ${1} ${2} ${T}
}

# Disable the service if it exits with code 0, else wait 2 seconds before restarting it

ifthenelse
    { s6-test ${1} -eq 0 }
    {
        foreground { /initlog.sh "agent-data-plane exited with code ${1}, disabling" }
        foreground { /bin/s6-svc -d /var/run/s6/services/data-plane/ }
    }
    {
        foreground { /initlog.sh "agent-data-plane exited with code ${1}, signal ${2}, restarting in 2 seconds" }
        foreground { s6-sleep 2 }
    }
"#
        );
        assert_eq!(AgentAdp.artifacts(LINUX), &[AgentAdp::LINUX_IMAGE]);
        assert_eq!(AgentAdp.config_files(), ["cont-init.d/", "files/"]);
        assert_eq!(AgentAdp.processes(), ["adp", "agent"]);
        for (service, spec) in SERVICES.iter().zip(AgentAdp.process_specs(LINUX)) {
            assert_eq!(spec.name, service.process);
            assert_eq!(spec.exit, ExitChannel::Record { file: service.service });
            assert_eq!(
                spec.output,
                OutputChannel::File {
                    file: service.log,
                    stream: OutputStream::Merged
                }
            );
            let find = |suffix| {
                let path = format!("/etc/services.d/{}/{suffix}", service.service);
                let (_, mode, content) = files.iter().find(|(name, _, _)| *name == path).unwrap();
                assert_eq!(*mode, 0o755);
                *content
            };
            let shipped_finish = service.finish.splitn(2, |&byte| byte == b'\n').nth(1).unwrap();
            assert!(find("finish").ends_with(shipped_finish));
            let record_path = format!("redirfd -a 1 ${{D}}/{}\n", service.service);
            assert!(find("finish")
                .windows(record_path.len())
                .any(|bytes| bytes == record_path.as_bytes()));
            let shipped_run = service.run.splitn(2, |&byte| byte == b'\n').nth(1).unwrap();
            assert_eq!(
                find("run"),
                [b"#!/usr/bin/execlineb -P\nfdmove -c 2 1\n".as_slice(), shipped_run].concat()
            );
            assert_eq!(
                find("log/run"),
                format!(
                    "#!/usr/bin/execlineb -P\nimportas -i L PANORAMIC_LOG_DIR\ntee -a ${{L}}/{}\n",
                    service.log
                )
                .as_bytes()
            );
        }
        let config = rendered(BTreeMap::new(), BTreeMap::from([("settings".into(), b"data".to_vec())]));
        assert_eq!(
            AgentAdp.recipe(LINUX, &config).err().unwrap(),
            "internal error: agent-adp received undeclared configuration file 'settings'"
        );
    }
}
