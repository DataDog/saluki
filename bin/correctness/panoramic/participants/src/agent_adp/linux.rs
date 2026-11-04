//! Linux configuration and service overlays for the converged image.
//!
//! Derives DogStatsD listeners, ADP APIs, and the Agent's command and IPC listeners from the environment.
//! DogStatsD's datagram socket uses the schema's Linux default unless its environment sets another path or disables it.

use std::{collections::BTreeMap, net::IpAddr};

use async_trait::async_trait;
use panoramic_core::{
    endpoint::{ConsumedEndpoint, Delivery, Endpoint, EndpointKind, Evidence, Port},
    participant::{ExitChannel, OpContext, Operation, OutputChannel, OutputStream, ProcessSpec, RenderedConfig},
    recipe::{ContainerRecipe, Recipe},
    step::HandleError,
};
use serde_json::Value;

use super::AgentAdp;

const ADP_CLI_BINARY_PATH: &str = "/opt/datadog-agent/embedded/bin/agent-data-plane";

pub(super) const OPERATIONS: &[&dyn Operation] = &[&ConfigDump];

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

pub(super) fn default_env() -> BTreeMap<String, String> {
    BTreeMap::from([
        // The Core Agent listens on this port; ADP uses it for IPC.
        ("DD_CMD_PORT".to_string(), "55001".to_string()),
        ("DD_GUI_PORT".to_string(), "-1".to_string()),
        // Isolate the other Agent listeners as well as those the procedure exercises.
        ("DD_EXPVAR_PORT".to_string(), "55000".to_string()),
        ("DD_APM_RECEIVER_PORT".to_string(), "58126".to_string()),
        ("DD_PROCESS_CONFIG_CMD_PORT".to_string(), "56062".to_string()),
        ("DD_AGENT_IPC_PORT".to_string(), "55004".to_string()),
        // DogStatsD can run in either process, depending on the pipeline switches.
        ("DD_DOGSTATSD_PORT".to_string(), "58125".to_string()),
        // The Core Agent streams these listen addresses to ADP.
        (
            "DD_DATA_PLANE_API_LISTEN_ADDRESS".to_string(),
            "tcp://0.0.0.0:55100".to_string(),
        ),
        (
            "DD_DATA_PLANE_SECURE_API_LISTEN_ADDRESS".to_string(),
            "tcp://0.0.0.0:55101".to_string(),
        ),
        (
            "DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR".to_string(),
            "tcp://0.0.0.0:55102".to_string(),
        ),
        // Core Agent OTLP listeners; ADP uses its own endpoint settings.
        (
            "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT".to_string(),
            "0.0.0.0:54317".to_string(),
        ),
        (
            "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT".to_string(),
            "0.0.0.0:54318".to_string(),
        ),
    ])
}

#[derive(Clone, Copy)]
enum Owner {
    Agent,
    Adp,
    AdpOtlp,
    DogStatsD,
}

#[derive(Clone, Copy)]
enum ValueKind {
    Port,
    PositivePort,
    Address,
    HostPort,
    Path,
}

struct Listener {
    name: &'static str,
    kind: EndpointKind,
    key: &'static str,
    default: &'static str,
    value: ValueKind,
    owner: Owner,
}

// Owner also selects the enabling keys: DATA_PLANE_ENABLED, DATA_PLANE_OTLP_ENABLED, and USE_DOGSTATSD.
const LISTENERS: &[Listener] = &[
    Listener {
        name: "dogstatsd_udp",
        kind: EndpointKind::Udp,
        key: "DOGSTATSD_PORT",
        default: "8125",
        value: ValueKind::Port,
        owner: Owner::DogStatsD,
    },
    Listener {
        name: "dogstatsd_uds",
        kind: EndpointKind::UnixDatagram,
        key: "DOGSTATSD_SOCKET",
        default: "/var/run/datadog/dsd.socket",
        value: ValueKind::Path,
        owner: Owner::DogStatsD,
    },
    Listener {
        name: "dogstatsd_uds_stream",
        kind: EndpointKind::UnixStream,
        key: "DOGSTATSD_STREAM_SOCKET",
        default: "",
        value: ValueKind::Path,
        owner: Owner::DogStatsD,
    },
    Listener {
        name: "api",
        kind: EndpointKind::Http,
        key: "DATA_PLANE_API_LISTEN_ADDRESS",
        default: "tcp://0.0.0.0:5100",
        value: ValueKind::Address,
        owner: Owner::Adp,
    },
    Listener {
        name: "secure_api",
        kind: EndpointKind::Tcp,
        key: "DATA_PLANE_SECURE_API_LISTEN_ADDRESS",
        default: "tcp://0.0.0.0:5101",
        value: ValueKind::Address,
        owner: Owner::Adp,
    },
    Listener {
        name: "otlp_grpc",
        kind: EndpointKind::Grpc,
        key: "DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY",
        default: "localhost:6317",
        value: ValueKind::HostPort,
        owner: Owner::AdpOtlp,
    },
    Listener {
        name: "otlp_http",
        kind: EndpointKind::Http,
        key: "DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY",
        default: "localhost:6318",
        value: ValueKind::HostPort,
        owner: Owner::AdpOtlp,
    },
    Listener {
        name: "agent_cmd",
        kind: EndpointKind::Tcp,
        key: "CMD_PORT",
        default: "5001",
        value: ValueKind::Port,
        owner: Owner::Agent,
    },
    Listener {
        name: "agent_ipc",
        kind: EndpointKind::Tcp,
        key: "AGENT_IPC_PORT",
        default: "0",
        value: ValueKind::PositivePort,
        owner: Owner::Agent,
    },
];

struct Environment<'a>(&'a BTreeMap<String, String>);

impl<'a> Environment<'a> {
    fn value(&self, owner: Owner, key: &str, default: &'a str) -> (String, &'a str) {
        if matches!(owner, Owner::Adp) {
            let key = format!("ADP_DD_{key}");
            if let Some(value) = self.0.get(&key) {
                return (key, value);
            }
        }
        let key = format!("DD_{key}");
        let value = self.0.get(&key).map_or(default, String::as_str);
        (key, value)
    }

    fn enabled(&self, owner: Owner, key: &str, default: &'a str) -> Result<bool, String> {
        let (key, value) = self.value(owner, key, default);
        value
            .parse()
            .map_err(|error| format!("{key}='{value}' must be true or false: {error}"))
    }
}

pub(super) fn endpoints(env: &BTreeMap<String, String>) -> Result<BTreeMap<String, Endpoint>, String> {
    let env = Environment(env);
    let adp = env.enabled(Owner::Agent, "DATA_PLANE_ENABLED", "false")?;
    let dogstatsd = env.enabled(Owner::Agent, "USE_DOGSTATSD", "true")?;
    let adp_dogstatsd = adp && env.enabled(Owner::Agent, "DATA_PLANE_DOGSTATSD_ENABLED", "true")?;
    let adp_otlp = adp && env.enabled(Owner::Adp, "DATA_PLANE_OTLP_ENABLED", "false")?;
    let mut endpoints = BTreeMap::new();
    for listener in LISTENERS {
        let owner = match listener.owner {
            Owner::Adp if !adp => continue,
            Owner::AdpOtlp if !adp_otlp => continue,
            Owner::AdpOtlp => Owner::Adp,
            Owner::DogStatsD if !dogstatsd => continue,
            Owner::DogStatsD => {
                if adp_dogstatsd {
                    Owner::Adp
                } else {
                    Owner::Agent
                }
            }
            owner => owner,
        };
        let (key, value) = env.value(owner, listener.key, listener.default);
        let invalid = |reason: &str| format!("{key}='{value}' {reason}");
        let endpoint = match listener.value {
            ValueKind::Path => {
                if value.is_empty() {
                    continue;
                }
                if !value.starts_with('/') {
                    return Err(invalid("must be an absolute socket path"));
                }
                match listener.kind {
                    EndpointKind::UnixDatagram => Endpoint::unix_datagram(value),
                    EndpointKind::UnixStream => Endpoint::unix_stream(value),
                    _ => unreachable!("path listeners are sockets"),
                }
                .with_ready(Evidence::socket_exists(listener.name))
            }
            ValueKind::Port | ValueKind::PositivePort | ValueKind::Address | ValueKind::HostPort => {
                let port = match listener.value {
                    ValueKind::HostPort => {
                        let address = url::Url::parse(&format!("tcp://{value}"))
                            .map_err(|_| invalid("must be host:port with no scheme"))?;
                        if address.host_str().is_none()
                            || !address.username().is_empty()
                            || address.password().is_some()
                            || !address.path().is_empty()
                            || address.query().is_some()
                            || address.fragment().is_some()
                            || value.contains('@')
                            || value.chars().any(char::is_whitespace)
                        {
                            return Err(invalid("must be host:port with no scheme"));
                        }
                        let port = address.port().ok_or_else(|| invalid("must specify a port"))?;
                        let host = url::Host::parse(address.host_str().unwrap())
                            .map_err(|_| invalid("must be host:port with no scheme"))?;
                        if loopback_host(&host.to_string()) {
                            continue;
                        }
                        port
                    }
                    ValueKind::Address => {
                        let address = url::Url::parse(value)
                            .map_err(|error| invalid(&format!("must be tcp://host:port: {error}")))?;
                        if address.scheme() != "tcp"
                            || address.host_str().is_none()
                            || !address.username().is_empty()
                            || address.password().is_some()
                            || !matches!(address.path(), "" | "/")
                            || address.query().is_some()
                            || address.fragment().is_some()
                        {
                            return Err(invalid("must be tcp://host:port"));
                        }
                        if loopback_host(address.host_str().unwrap()) {
                            return Err(invalid(
                                "must use a non-loopback host so other participants can reach it",
                            ));
                        }
                        address.port().ok_or_else(|| invalid("must specify a port"))?
                    }
                    ValueKind::PositivePort => {
                        let port: i64 = value
                            .parse()
                            .map_err(|error| invalid(&format!("must be an integer port: {error}")))?;
                        if port <= 0 {
                            continue;
                        }
                        port.try_into().map_err(|_| invalid("must be at most 65535"))?
                    }
                    _ => value
                        .parse::<u16>()
                        .map_err(|error| invalid(&format!("must be a port from 0 to 65535: {error}")))?,
                };
                if port == 0 {
                    continue;
                }
                match listener.kind {
                    EndpointKind::Tcp => {
                        Endpoint::tcp(Port::Fixed(port)).with_ready(Evidence::tcp_accepts(listener.name))
                    }
                    EndpointKind::Http => {
                        Endpoint::http(Port::Fixed(port)).with_ready(Evidence::tcp_accepts(listener.name))
                    }
                    EndpointKind::Grpc => {
                        Endpoint::grpc(Port::Fixed(port)).with_ready(Evidence::tcp_accepts(listener.name))
                    }
                    EndpointKind::Udp => {
                        if !env.enabled(owner, "DOGSTATSD_NON_LOCAL_TRAFFIC", "false")? {
                            let (_, host) = env.value(owner, "BIND_HOST", "");
                            if host.is_empty() || loopback_host(host) {
                                continue;
                            }
                        }
                        let endpoint = Endpoint::udp(Port::Fixed(port));
                        if matches!(owner, Owner::Adp) {
                            endpoint.with_ready(Evidence::output_matches(
                                "adp",
                                format!(r#"listen_addr:"udp://[^"]+:{port}" \| DogStatsD listener started\."#),
                            ))
                        } else {
                            // The Agent's DogStatsD log line varies by release; readiness follows container startup.
                            endpoint
                        }
                    }
                    _ => unreachable!("network listeners use ports"),
                }
            }
        };
        endpoints.insert(listener.name.into(), endpoint);
    }
    Ok(endpoints)
}

fn loopback_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']).trim_end_matches('.');
    host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.to_canonical().is_loopback())
}

pub(super) fn consumes() -> BTreeMap<String, ConsumedEndpoint> {
    BTreeMap::from([(
        "intake".into(),
        ConsumedEndpoint {
            accepts: vec![EndpointKind::Http],
            required: false,
            delivery: Delivery::Env {
                keys: ["DD_DD_URL".into(), "DD_APM_DD_URL".into()].into(),
                value: |address| {
                    BTreeMap::from([
                        ("DD_DD_URL".into(), address.url().into()),
                        ("DD_APM_DD_URL".into(), address.url().into()),
                    ])
                },
            },
        },
    )])
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
    if !config.files().is_empty() {
        return Err("agent-adp does not accept case configuration files".into());
    }
    let mut recipe = ContainerRecipe::new(AgentAdp::LINUX_IMAGE).with_env(config.env().clone());
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
        endpoint::test_support::{assert_network_endpoint, assert_socket_endpoint, http_address},
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
    fn the_dogstatsd_udp_listener_is_declared_only_when_it_accepts_non_local_traffic() {
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
                assert_eq!(
                    AgentAdp
                        .endpoints(LINUX, &config)
                        .unwrap()
                        .contains_key("dogstatsd_udp"),
                    declared,
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
                assert_eq!(
                    AgentAdp
                        .endpoints(LINUX, &config)
                        .unwrap()
                        .contains_key("dogstatsd_udp"),
                    expected,
                    "{enabled}: {overrides:?}"
                );
            }
        }
        for key in ["DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "ADP_DD_DOGSTATSD_NON_LOCAL_TRAFFIC"] {
            let error = AgentAdp
                .endpoints(LINUX, &config(&[("DD_DATA_PLANE_ENABLED", "true"), (key, "invalid")]))
                .unwrap_err();
            assert!(error.contains(key), "{error}");
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
        assert_eq!(consumes.len(), 1);
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
        assert!(AgentAdp.config_files().is_empty());
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
            "agent-adp does not accept case configuration files"
        );
    }
}
