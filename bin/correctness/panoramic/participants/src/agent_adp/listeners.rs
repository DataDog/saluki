//! Listener ownership, configuration precedence and port delivery for the Agent and ADP.

use std::{collections::BTreeMap, net::IpAddr};

use panoramic_core::{
    endpoint::{Endpoint, EndpointKind, Evidence, Port},
    layout::{Os, Realization, RealizationKind},
};

#[derive(Clone, Copy)]
enum Owner {
    Agent,
    Adp,
    AdpOtlp,
    AdpDogStatsD,
    AgentOtlpProxy,
    AgentOtlpGrpc,
    DogStatsD,
}

#[derive(Clone, Copy)]
enum ValueKind {
    Port(fn(u16) -> BTreeMap<String, String>),
    PositivePort(fn(u16) -> BTreeMap<String, String>),
    Address(fn(u16) -> BTreeMap<String, String>),
    HostPort(fn(u16) -> BTreeMap<String, String>),
    Path,
    Pipe,
}

struct Listener {
    name: &'static str,
    kind: EndpointKind,
    key: &'static str,
    default: &'static str,
    value: ValueKind,
    owner: Owner,
    macos_only: bool,
    windows_only: bool,
}

// Owners select DATA_PLANE_ENABLED, DATA_PLANE_DOGSTATSD_ENABLED, DATA_PLANE_OTLP_ENABLED and USE_DOGSTATSD.
// On macOS, DATA_PLANE_OTLP_PROXY_ENABLED selects the Agent proxy receiver in place of its normal gRPC receiver.
const LISTENERS: &[Listener] = &[
    Listener {
        name: "dogstatsd_udp",
        kind: EndpointKind::Udp,
        key: "DOGSTATSD_PORT",
        default: "8125",
        value: ValueKind::Port(|port| BTreeMap::from([("DD_DOGSTATSD_PORT".into(), port.to_string())])),
        owner: Owner::DogStatsD,
        macos_only: false,
        windows_only: false,
    },
    Listener {
        name: "dogstatsd_tcp",
        kind: EndpointKind::Tcp,
        key: "DOGSTATSD_TCP_PORT",
        default: "0",
        value: ValueKind::Port(|port| BTreeMap::from([("DD_DOGSTATSD_TCP_PORT".into(), port.to_string())])),
        owner: Owner::AdpDogStatsD,
        macos_only: false,
        windows_only: false,
    },
    Listener {
        name: "dogstatsd_uds",
        kind: EndpointKind::UnixDatagram,
        key: "DOGSTATSD_SOCKET",
        default: "/var/run/datadog/dsd.socket",
        value: ValueKind::Path,
        owner: Owner::DogStatsD,
        macos_only: false,
        windows_only: false,
    },
    Listener {
        name: "dogstatsd_uds_stream",
        kind: EndpointKind::UnixStream,
        key: "DOGSTATSD_STREAM_SOCKET",
        default: "",
        value: ValueKind::Path,
        owner: Owner::DogStatsD,
        macos_only: false,
        windows_only: false,
    },
    Listener {
        name: "dogstatsd_pipe",
        kind: EndpointKind::NamedPipe,
        key: "DOGSTATSD_PIPE_NAME",
        default: "",
        value: ValueKind::Pipe,
        owner: Owner::DogStatsD,
        macos_only: false,
        windows_only: true,
    },
    Listener {
        name: "api",
        kind: EndpointKind::Http,
        key: "DATA_PLANE_API_LISTEN_ADDRESS",
        default: "tcp://0.0.0.0:5100",
        value: ValueKind::Address(|port| {
            BTreeMap::from([(
                "DD_DATA_PLANE_API_LISTEN_ADDRESS".into(),
                format!("tcp://0.0.0.0:{port}"),
            )])
        }),
        owner: Owner::Adp,
        macos_only: false,
        windows_only: false,
    },
    Listener {
        name: "secure_api",
        kind: EndpointKind::Tcp,
        key: "DATA_PLANE_SECURE_API_LISTEN_ADDRESS",
        default: "tcp://0.0.0.0:5101",
        value: ValueKind::Address(|port| {
            BTreeMap::from([(
                "DD_DATA_PLANE_SECURE_API_LISTEN_ADDRESS".into(),
                format!("tcp://0.0.0.0:{port}"),
            )])
        }),
        owner: Owner::Adp,
        macos_only: false,
        windows_only: false,
    },
    Listener {
        name: "otlp_grpc",
        kind: EndpointKind::Grpc,
        key: "DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY",
        default: "localhost:6317",
        value: ValueKind::HostPort(|port| {
            BTreeMap::from([(
                "DD_DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY".into(),
                format!("127.0.0.1:{port}"),
            )])
        }),
        owner: Owner::AdpOtlp,
        macos_only: false,
        windows_only: false,
    },
    Listener {
        name: "otlp_http",
        kind: EndpointKind::Http,
        key: "DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY",
        default: "localhost:6318",
        value: ValueKind::HostPort(|port| {
            BTreeMap::from([(
                "DD_DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY".into(),
                format!("127.0.0.1:{port}"),
            )])
        }),
        owner: Owner::AdpOtlp,
        macos_only: false,
        windows_only: false,
    },
    Listener {
        name: "agent_cmd",
        kind: EndpointKind::Tcp,
        key: "CMD_PORT",
        default: "5001",
        value: ValueKind::Port(|port| BTreeMap::from([("DD_CMD_PORT".into(), port.to_string())])),
        owner: Owner::Agent,
        macos_only: false,
        windows_only: false,
    },
    Listener {
        name: "agent_ipc",
        kind: EndpointKind::Tcp,
        key: "AGENT_IPC_PORT",
        default: "0",
        value: ValueKind::PositivePort(|port| BTreeMap::from([("DD_AGENT_IPC_PORT".into(), port.to_string())])),
        owner: Owner::Agent,
        macos_only: false,
        windows_only: false,
    },
    Listener {
        name: "agent_expvar",
        kind: EndpointKind::Tcp,
        key: "EXPVAR_PORT",
        default: "5000",
        value: ValueKind::Port(|port| BTreeMap::from([("DD_EXPVAR_PORT".into(), port.to_string())])),
        owner: Owner::Agent,
        macos_only: true,
        windows_only: false,
    },
    Listener {
        name: "agent_otlp_grpc",
        kind: EndpointKind::Grpc,
        key: "OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT",
        default: "",
        value: ValueKind::HostPort(|port| {
            BTreeMap::from([(
                "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT".into(),
                format!("0.0.0.0:{port}"),
            )])
        }),
        owner: Owner::AgentOtlpGrpc,
        macos_only: true,
        windows_only: false,
    },
    Listener {
        name: "agent_otlp_http",
        kind: EndpointKind::Http,
        key: "OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT",
        default: "",
        value: ValueKind::HostPort(|port| {
            BTreeMap::from([(
                "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT".into(),
                format!("0.0.0.0:{port}"),
            )])
        }),
        owner: Owner::Agent,
        macos_only: true,
        windows_only: false,
    },
    Listener {
        name: "agent_otlp_proxy_grpc",
        kind: EndpointKind::Grpc,
        key: "DATA_PLANE_OTLP_PROXY_RECEIVER_PROTOCOLS_GRPC_ENDPOINT",
        default: "localhost:4319",
        value: ValueKind::HostPort(|port| {
            BTreeMap::from([(
                "DD_DATA_PLANE_OTLP_PROXY_RECEIVER_PROTOCOLS_GRPC_ENDPOINT".into(),
                format!("127.0.0.1:{port}"),
            )])
        }),
        owner: Owner::AgentOtlpProxy,
        macos_only: true,
        windows_only: false,
    },
];

#[derive(Clone, Copy, PartialEq)]
enum Platform {
    LinuxContainer,
    WindowsContainer,
    MacOsHost,
}

struct Environment<'a> {
    values: &'a BTreeMap<String, String>,
    platform: Platform,
}

impl<'a> Environment<'a> {
    fn value(&self, owner: Owner, key: &str, default: &'a str) -> (String, &'a str) {
        if self.platform == Platform::LinuxContainer && matches!(owner, Owner::Adp) {
            let key = format!("ADP_DD_{key}");
            if let Some(value) = self.values.get(&key) {
                return (key, value);
            }
        }
        let key = format!("DD_{key}");
        let value = self.values.get(&key).map_or(default, String::as_str);
        (key, value)
    }

    fn enabled(&self, owner: Owner, key: &str, default: &'a str) -> Result<bool, String> {
        let (key, value) = self.value(owner, key, default);
        value
            .parse()
            .map_err(|error| format!("{key}='{value}' must be true or false: {error}"))
    }
}

pub(super) fn otlp_proxy_enabled(env: &BTreeMap<String, String>) -> Result<bool, String> {
    let env = Environment {
        values: env,
        platform: Platform::MacOsHost,
    };
    Ok(env.enabled(Owner::Agent, "DATA_PLANE_ENABLED", "false")?
        && env.enabled(Owner::Agent, "DATA_PLANE_OTLP_ENABLED", "false")?
        && env.enabled(Owner::Agent, "DATA_PLANE_OTLP_PROXY_ENABLED", "false")?)
}

pub(super) fn endpoints(
    realization: Realization, env: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Endpoint>, String> {
    let platform = match (realization.os, realization.kind) {
        (Os::Linux, RealizationKind::LinuxContainer) => Platform::LinuxContainer,
        (Os::Windows, RealizationKind::WindowsContainer) => Platform::WindowsContainer,
        (Os::MacOs, RealizationKind::HostProcessGroup) => Platform::MacOsHost,
        _ => unreachable!("agent-adp dispatches listener configuration only for supported realizations"),
    };
    let env = Environment { values: env, platform };
    let adp = env.enabled(Owner::Agent, "DATA_PLANE_ENABLED", "false")?;
    let dogstatsd = env.enabled(Owner::Agent, "USE_DOGSTATSD", "true")?;
    let adp_dogstatsd = adp && env.enabled(Owner::Agent, "DATA_PLANE_DOGSTATSD_ENABLED", "true")?;
    let adp_otlp = adp && env.enabled(Owner::Adp, "DATA_PLANE_OTLP_ENABLED", "false")?;
    let proxy = platform == Platform::MacOsHost && otlp_proxy_enabled(env.values)?;
    let mut endpoints = BTreeMap::new();
    for listener in LISTENERS {
        if platform == Platform::WindowsContainer && matches!(listener.value, ValueKind::Path) {
            let (key, value) = env.value(Owner::Agent, listener.key, "");
            if !value.is_empty() {
                return Err(format!(
                    "{key}='{value}' is unsupported on windows-container; unix socket listeners need a Linux container or macOS host"
                ));
            }
        }
        if (listener.macos_only && platform != Platform::MacOsHost)
            || (listener.windows_only && platform != Platform::WindowsContainer)
        {
            continue;
        }
        let owner = match listener.owner {
            Owner::AgentOtlpGrpc => Owner::Agent,
            Owner::AgentOtlpProxy if !proxy => continue,
            Owner::AgentOtlpProxy => Owner::Agent,
            Owner::Adp if !adp => continue,
            Owner::AdpOtlp if !adp_otlp => continue,
            Owner::AdpDogStatsD if !(dogstatsd && adp_dogstatsd) => continue,
            Owner::AdpDogStatsD
                if platform == Platform::MacOsHost && !env.values.contains_key(&format!("DD_{}", listener.key)) =>
            {
                // TCP is opt-in; assigning an unset port would enable another listener.
                continue;
            }
            Owner::AdpOtlp | Owner::AdpDogStatsD => Owner::Adp,
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
        let tls = if listener.name == "otlp_http" {
            let (cert_key, cert) = env.value(owner, "OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE", "");
            let (key_key, key) = env.value(owner, "OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_KEY_FILE", "");
            if cert.is_empty() != key.is_empty() {
                return Err(format!(
                    "{cert_key} and {key_key} must both be nonempty to enable OTLP HTTP TLS, or both empty"
                ));
            }
            !cert.is_empty()
        } else {
            false
        };
        let default = if platform != Platform::LinuxContainer && matches!(listener.value, ValueKind::Path) {
            ""
        } else {
            listener.default
        };
        let (key, value) = env.value(owner, listener.key, default);
        let invalid = |reason: &str| format!("{key}='{value}' {reason}");
        let endpoint = match listener.value {
            ValueKind::Pipe => {
                if value.is_empty() {
                    continue;
                }
                if value.contains(['\\', '/', '\0']) {
                    return Err(invalid("must be a pipe name without separators or NUL"));
                }
                Endpoint::named_pipe(value)
            }
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
            ValueKind::Port(deliver)
            | ValueKind::PositivePort(deliver)
            | ValueKind::Address(deliver)
            | ValueKind::HostPort(deliver) => {
                let port = if platform == Platform::MacOsHost && !env.values.contains_key(&key) {
                    Port::Assigned { deliver }
                } else {
                    let port = match listener.value {
                        ValueKind::HostPort(_) => {
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
                            if platform != Platform::MacOsHost && loopback_host(&host.to_string()) {
                                continue;
                            }
                            port
                        }
                        ValueKind::Address(_) => {
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
                            if platform != Platform::MacOsHost && loopback_host(address.host_str().unwrap()) {
                                return Err(invalid(
                                    "must use a non-loopback host so other participants can reach it",
                                ));
                            }
                            address.port().ok_or_else(|| invalid("must specify a port"))?
                        }
                        ValueKind::PositivePort(_) => {
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
                    Port::Fixed(port)
                };
                if platform != Platform::MacOsHost
                    && (listener.kind == EndpointKind::Udp || matches!(listener.owner, Owner::AdpDogStatsD))
                    && !env.enabled(owner, "DOGSTATSD_NON_LOCAL_TRAFFIC", "false")?
                {
                    let (_, host) = env.value(owner, "BIND_HOST", "");
                    if host.is_empty() || loopback_host(host) {
                        continue;
                    }
                }
                match listener.kind {
                    EndpointKind::Tcp => Endpoint::tcp(port).with_ready(Evidence::tcp_accepts(listener.name)),
                    EndpointKind::Http => Endpoint::http(port).with_ready(Evidence::tcp_accepts(listener.name)),
                    EndpointKind::Grpc => Endpoint::grpc(port).with_ready(Evidence::tcp_accepts(listener.name)),
                    EndpointKind::Udp => {
                        let endpoint = Endpoint::udp(port);
                        match (owner, port) {
                            (Owner::Adp, Port::Fixed(port)) => endpoint.with_ready(Evidence::output_matches(
                                "adp",
                                format!(r#"listen_addr:"udp://[^"]+:{port}" \| DogStatsD listener started\."#),
                            )),
                            // Core re-derives the endpoint after assigning its port.
                            (Owner::Adp, Port::Assigned { .. }) => endpoint,
                            // The Agent's DogStatsD log line varies by release; readiness follows process startup.
                            _ => endpoint,
                        }
                    }
                    _ => unreachable!("network listeners use ports"),
                }
            }
        };
        // The proxy replaces the Agent's normal gRPC listener, but its configured address still needs validation.
        if proxy && matches!(listener.owner, Owner::AgentOtlpGrpc) {
            continue;
        }
        let endpoint = if tls { endpoint.with_tls() } else { endpoint };
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

#[cfg(test)]
mod tests {
    use panoramic_core::{
        endpoint::test_support::{
            assert_network_endpoint, assert_pipe_endpoint, assert_socket_endpoint, assigned_delivery,
        },
        participant::{CaseValues, EffectiveConfig, ParticipantType},
    };

    use super::*;
    use crate::AgentAdp;

    #[test]
    fn only_windows_declares_dogstatsd_pipe_and_validates_its_name() {
        for &(os, kind) in AgentAdp.supports() {
            let realization = Realization { os, kind };
            for name in ["", "metrics-pipe", "bad/name", r"bad\name", "bad\0name"] {
                let values = CaseValues {
                    env: BTreeMap::from([("DD_DOGSTATSD_PIPE_NAME".into(), name.into())]),
                    ..Default::default()
                };
                let config = EffectiveConfig {
                    env: values.env.clone(),
                    ..Default::default()
                };
                let result = AgentAdp.endpoints(realization, &config);
                if os == Os::Windows && name.contains(['\\', '/', '\0']) {
                    let error = result.unwrap_err();
                    assert!(error.contains("DD_DOGSTATSD_PIPE_NAME"), "{error}");
                    assert_eq!(AgentAdp.validate_config(realization, &values).unwrap_err(), error);
                } else {
                    let endpoints = result.unwrap();
                    if os == Os::Windows && !name.is_empty() {
                        assert_pipe_endpoint(&endpoints["dogstatsd_pipe"], name, None);
                    } else {
                        assert!(!endpoints.contains_key("dogstatsd_pipe"));
                    }
                }
            }
            let config = EffectiveConfig {
                env: BTreeMap::from([
                    ("DD_DOGSTATSD_PIPE_NAME".into(), "metrics".into()),
                    ("DD_USE_DOGSTATSD".into(), "false".into()),
                ]),
                ..Default::default()
            };
            assert!(!AgentAdp
                .endpoints(realization, &config)
                .unwrap()
                .contains_key("dogstatsd_pipe"));
        }
    }

    const MACOS: Realization = Realization {
        os: Os::MacOs,
        kind: RealizationKind::HostProcessGroup,
    };
    const NETWORK: &[(&str, EndpointKind, &str, &str)] = &[
        ("agent_cmd", EndpointKind::Tcp, "DD_CMD_PORT", "20001"),
        ("agent_ipc", EndpointKind::Tcp, "DD_AGENT_IPC_PORT", "20001"),
        ("agent_expvar", EndpointKind::Tcp, "DD_EXPVAR_PORT", "20001"),
        (
            "api",
            EndpointKind::Http,
            "DD_DATA_PLANE_API_LISTEN_ADDRESS",
            "tcp://0.0.0.0:20001",
        ),
        (
            "secure_api",
            EndpointKind::Tcp,
            "DD_DATA_PLANE_SECURE_API_LISTEN_ADDRESS",
            "tcp://0.0.0.0:20001",
        ),
        ("dogstatsd_udp", EndpointKind::Udp, "DD_DOGSTATSD_PORT", "20001"),
        (
            "otlp_grpc",
            EndpointKind::Grpc,
            "DD_DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY",
            "127.0.0.1:20001",
        ),
        (
            "otlp_http",
            EndpointKind::Http,
            "DD_DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY",
            "127.0.0.1:20001",
        ),
        (
            "agent_otlp_grpc",
            EndpointKind::Grpc,
            "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT",
            "0.0.0.0:20001",
        ),
        (
            "agent_otlp_http",
            EndpointKind::Http,
            "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT",
            "0.0.0.0:20001",
        ),
        (
            "agent_otlp_proxy_grpc",
            EndpointKind::Grpc,
            "DD_DATA_PLANE_OTLP_PROXY_RECEIVER_PROTOCOLS_GRPC_ENDPOINT",
            "127.0.0.1:20001",
        ),
    ];

    fn config() -> EffectiveConfig {
        EffectiveConfig {
            env: BTreeMap::from([
                ("DD_DATA_PLANE_ENABLED".into(), "true".into()),
                ("DD_DATA_PLANE_DOGSTATSD_ENABLED".into(), "true".into()),
                ("DD_DATA_PLANE_OTLP_ENABLED".into(), "true".into()),
                ("DD_DATA_PLANE_OTLP_PROXY_ENABLED".into(), "true".into()),
            ]),
            ..Default::default()
        }
    }

    const WINDOWS: Realization = Realization {
        os: Os::Windows,
        kind: RealizationKind::WindowsContainer,
    };

    #[test]
    fn windows_declares_fixed_container_listeners_without_unix_sockets_or_host_only_listeners() {
        let case = CaseValues {
            env: BTreeMap::from([
                ("DD_DATA_PLANE_ENABLED".into(), "true".into()),
                ("DD_DOGSTATSD_NON_LOCAL_TRAFFIC".into(), "true".into()),
                ("DD_DOGSTATSD_TCP_PORT".into(), "58127".into()),
                ("DD_DATA_PLANE_OTLP_ENABLED".into(), "true".into()),
                (
                    "DD_DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY".into(),
                    "0.0.0.0:56317".into(),
                ),
                (
                    "DD_DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY".into(),
                    "0.0.0.0:56318".into(),
                ),
            ]),
            ..Default::default()
        };
        AgentAdp.validate_config(WINDOWS, &case).unwrap();
        let mut env = AgentAdp.default_env(WINDOWS, &case);
        env.extend(case.env);
        let endpoints = AgentAdp
            .endpoints(
                WINDOWS,
                &EffectiveConfig {
                    env,
                    ..Default::default()
                },
            )
            .unwrap();
        let expected = [
            ("agent_cmd", EndpointKind::Tcp, 55001),
            ("agent_ipc", EndpointKind::Tcp, 55004),
            ("api", EndpointKind::Http, 55100),
            ("secure_api", EndpointKind::Tcp, 55101),
            ("dogstatsd_udp", EndpointKind::Udp, 58125),
            ("dogstatsd_tcp", EndpointKind::Tcp, 58127),
            ("otlp_grpc", EndpointKind::Grpc, 56317),
            ("otlp_http", EndpointKind::Http, 56318),
        ];
        assert_eq!(endpoints.len(), expected.len());
        for (name, kind, port) in expected {
            let ready = if kind == EndpointKind::Udp {
                Evidence::output_matches(
                    "adp",
                    r#"listen_addr:"udp://[^"]+:58125" \| DogStatsD listener started\."#,
                )
            } else {
                Evidence::tcp_accepts(name)
            };
            assert_network_endpoint(&endpoints[name], kind, port, Some(ready));
        }
    }

    #[test]
    fn windows_rejects_unix_socket_paths_even_when_dogstatsd_is_disabled() {
        for key in ["DD_DOGSTATSD_SOCKET", "DD_DOGSTATSD_STREAM_SOCKET"] {
            for enabled in ["false", "true"] {
                let case = CaseValues {
                    env: BTreeMap::from([
                        (key.into(), "/run/dsd.sock".into()),
                        ("DD_USE_DOGSTATSD".into(), enabled.into()),
                    ]),
                    ..Default::default()
                };
                assert_eq!(AgentAdp.validate_config(WINDOWS, &case).unwrap_err(), format!(
                    "{key}='/run/dsd.sock' is unsupported on windows-container; unix socket listeners need a Linux container or macOS host"
                ));
            }
        }
    }

    #[test]
    fn windows_udp_listeners_need_non_local_traffic_or_a_non_loopback_bind_host() {
        for adp in ["false", "true"] {
            for (non_local, host, serves) in [
                ("false", "", false),
                ("false", "127.0.0.1", false),
                ("false", "localhost", false),
                ("false", "0.0.0.0", true),
                ("true", "", true),
                ("true", "127.0.0.1", true),
            ] {
                let mut env = AgentAdp.default_env(WINDOWS, &CaseValues::default());
                env.extend([
                    ("DD_DATA_PLANE_ENABLED".into(), adp.into()),
                    ("DD_DOGSTATSD_NON_LOCAL_TRAFFIC".into(), non_local.into()),
                    ("DD_BIND_HOST".into(), host.into()),
                ]);
                let endpoints = AgentAdp
                    .endpoints(
                        WINDOWS,
                        &EffectiveConfig {
                            env,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                assert_eq!(
                    endpoints.contains_key("dogstatsd_udp"),
                    serves,
                    "{adp} {non_local} {host}"
                );
            }
        }
    }

    #[test]
    fn listener_failure_configurations_serve_the_unusable_socket_and_no_udp_listener_when_its_port_is_zero() {
        let linux = Realization {
            os: Os::Linux,
            kind: RealizationKind::LinuxContainer,
        };
        for realization in [linux, MACOS] {
            for (name, required, disabled) in [
                ("no-listener", "false", true),
                ("require-listener", "true", true),
                ("skip-failed-listener", "true", false),
            ] {
                let socket = format!("/dev/null/saluki-dogstatsd-{name}.sock");
                let mut case = CaseValues {
                    env: BTreeMap::from([
                        ("DD_DATA_PLANE_ENABLED".into(), "true".into()),
                        ("DD_DATA_PLANE_STANDALONE_MODE".into(), "false".into()),
                        ("DD_DATA_PLANE_DOGSTATSD_ENABLED".into(), "true".into()),
                        ("DD_DOGSTATSD_SOCKET".into(), socket.clone()),
                        ("DD_DOGSTATSD_REQUIRE_LISTENER".into(), required.into()),
                    ]),
                    ..Default::default()
                };
                if disabled {
                    case.env.insert("DD_DOGSTATSD_PORT".into(), "0".into());
                }
                AgentAdp.validate_config(realization, &case).unwrap();
                let mut env = AgentAdp.default_env(realization, &case);
                env.extend(case.env);
                let mut config = EffectiveConfig {
                    env,
                    ..Default::default()
                };
                let endpoints = AgentAdp.endpoints(realization, &config).unwrap();
                assert_socket_endpoint(
                    &endpoints["dogstatsd_uds"],
                    EndpointKind::UnixDatagram,
                    &socket,
                    Some(Evidence::socket_exists("dogstatsd_uds")),
                );
                if disabled || realization == linux {
                    assert!(!endpoints.contains_key("dogstatsd_udp"), "{realization:?}: {name}");
                } else {
                    assert_eq!(
                        assigned_delivery(&endpoints["dogstatsd_udp"], EndpointKind::Udp)(20001),
                        BTreeMap::from([("DD_DOGSTATSD_PORT".into(), "20001".into())])
                    );
                }
                if !disabled && realization == linux {
                    // Loopback UDP stays private to the container; non-local traffic exposes its fixed port.
                    config
                        .env
                        .insert("DD_DOGSTATSD_NON_LOCAL_TRAFFIC".into(), "true".into());
                    let endpoints = AgentAdp.endpoints(realization, &config).unwrap();
                    assert_network_endpoint(
                        &endpoints["dogstatsd_udp"],
                        EndpointKind::Udp,
                        58125,
                        Some(Evidence::output_matches(
                            "adp",
                            r#"listen_addr:"udp://[^"]+:58125" \| DogStatsD listener started\."#,
                        )),
                    );
                }
            }
        }
    }

    #[test]
    fn macos_listeners_report_assigned_ports_when_their_keys_are_unset() {
        for &(name, kind, key, value) in NETWORK {
            let mut config = config();
            config.env.insert(
                "DD_DATA_PLANE_OTLP_PROXY_ENABLED".into(),
                (name == "agent_otlp_proxy_grpc").to_string(),
            );
            let endpoints = AgentAdp.endpoints(MACOS, &config).unwrap();
            assert_eq!(endpoints.len(), NETWORK.len() - 1);
            assert_eq!(
                assigned_delivery(&endpoints[name], kind)(20001),
                BTreeMap::from([(key.into(), value.into())])
            );
        }
        let endpoints = AgentAdp.endpoints(MACOS, &EffectiveConfig::default()).unwrap();
        assert_eq!(
            endpoints.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "agent_cmd",
                "agent_expvar",
                "agent_ipc",
                "agent_otlp_grpc",
                "agent_otlp_http",
                "dogstatsd_udp"
            ]
        );
    }

    #[test]
    fn macos_assigned_ports_rederive_as_fixed_ports_at_the_delivered_values() {
        for proxy in [false, true] {
            let mut config = config();
            config
                .env
                .insert("DD_DATA_PLANE_OTLP_PROXY_ENABLED".into(), proxy.to_string());
            let expected = NETWORK
                .iter()
                .filter(|(name, ..)| {
                    *name
                        != if proxy {
                            "agent_otlp_grpc"
                        } else {
                            "agent_otlp_proxy_grpc"
                        }
                })
                .copied()
                .collect::<Vec<_>>();
            let assigned = AgentAdp.endpoints(MACOS, &config).unwrap();
            for (i, &(name, kind, _, _)) in expected.iter().enumerate() {
                config
                    .env
                    .extend(assigned_delivery(&assigned[name], kind)(20001 + i as u16));
            }
            let fixed = AgentAdp.endpoints(MACOS, &config).unwrap();
            assert_eq!(fixed.keys().collect::<Vec<_>>(), assigned.keys().collect::<Vec<_>>());
            for (i, &(name, kind, _, _)) in expected.iter().enumerate() {
                let port = 20001 + i as u16;
                let ready = if kind == EndpointKind::Udp {
                    Evidence::output_matches(
                        "adp",
                        format!(r#"listen_addr:"udp://[^"]+:{port}" \| DogStatsD listener started\."#),
                    )
                } else {
                    Evidence::tcp_accepts(name)
                };
                assert_network_endpoint(&fixed[name], kind, port, Some(ready));
            }
        }
    }

    #[test]
    fn macos_listeners_keep_fixed_ports_the_case_pins() {
        let mut config = config();
        for &(name, kind, key, value) in NETWORK {
            config.env.insert(
                "DD_DATA_PLANE_OTLP_PROXY_ENABLED".into(),
                (name == "agent_otlp_proxy_grpc").to_string(),
            );
            config.env.insert(key.into(), value.replace("20001", "29001"));
            let endpoints = AgentAdp.endpoints(MACOS, &config).unwrap();
            let ready = if kind == EndpointKind::Udp {
                Evidence::output_matches(
                    "adp",
                    r#"listen_addr:"udp://[^"]+:29001" \| DogStatsD listener started\."#,
                )
            } else {
                Evidence::tcp_accepts(name)
            };
            assert_network_endpoint(&endpoints[name], kind, 29001, Some(ready));
        }
        for (name, key, value) in [
            ("agent_ipc", "DD_AGENT_IPC_PORT", "-1"),
            ("dogstatsd_udp", "DD_DOGSTATSD_PORT", "0"),
        ] {
            config.env.insert(key.into(), value.into());
            assert!(!AgentAdp.endpoints(MACOS, &config).unwrap().contains_key(name));
        }
    }

    #[test]
    fn macos_serves_loopback_listeners_and_loopback_dogstatsd_udp() {
        for enabled in ["true", "false"] {
            let mut config = config();
            config.env.extend([
                ("DD_DATA_PLANE_ENABLED".into(), enabled.into()),
                ("DD_DOGSTATSD_PORT".into(), "29001".into()),
                ("DD_BIND_HOST".into(), "127.0.0.1".into()),
                ("DD_DOGSTATSD_SOCKET".into(), "/run/dsd.sock".into()),
                ("DD_DOGSTATSD_STREAM_SOCKET".into(), "/run/dsd-stream.sock".into()),
            ]);
            for &(name, kind, key, value) in NETWORK {
                config.env.insert(
                    "DD_DATA_PLANE_OTLP_PROXY_ENABLED".into(),
                    (name == "agent_otlp_proxy_grpc").to_string(),
                );
                config.env.insert(key.into(), value.replace("0.0.0.0", "127.0.0.1"));
                let endpoints = AgentAdp.endpoints(MACOS, &config).unwrap();
                if enabled == "true" || name.starts_with("agent_") && name != "agent_otlp_proxy_grpc" {
                    let ready = if kind == EndpointKind::Udp {
                        Evidence::output_matches(
                            "adp",
                            r#"listen_addr:"udp://[^"]+:20001" \| DogStatsD listener started\."#,
                        )
                    } else {
                        Evidence::tcp_accepts(name)
                    };
                    assert_network_endpoint(&endpoints[name], kind, 20001, Some(ready));
                }
            }
            let endpoints = AgentAdp.endpoints(MACOS, &config).unwrap();
            assert!(endpoints.contains_key("dogstatsd_udp"));
            assert_socket_endpoint(
                &endpoints["dogstatsd_uds"],
                EndpointKind::UnixDatagram,
                "/run/dsd.sock",
                Some(Evidence::socket_exists("dogstatsd_uds")),
            );
            assert_socket_endpoint(
                &endpoints["dogstatsd_uds_stream"],
                EndpointKind::UnixStream,
                "/run/dsd-stream.sock",
                Some(Evidence::socket_exists("dogstatsd_uds_stream")),
            );
        }
    }

    #[test]
    fn macos_dogstatsd_tcp_requires_an_enabled_adp_pipeline_and_a_nonzero_case_port() {
        for adp in [false, true] {
            for dogstatsd in [false, true] {
                for adp_dogstatsd in [false, true] {
                    let enabled = adp && dogstatsd && adp_dogstatsd;
                    for port in [None, Some("0"), Some("29001"), Some("invalid")] {
                        let mut config = config();
                        config.env.extend([
                            ("DD_DATA_PLANE_ENABLED".into(), adp.to_string()),
                            ("DD_USE_DOGSTATSD".into(), dogstatsd.to_string()),
                            ("DD_DATA_PLANE_DOGSTATSD_ENABLED".into(), adp_dogstatsd.to_string()),
                        ]);
                        if let Some(port) = port {
                            config.env.insert("DD_DOGSTATSD_TCP_PORT".into(), port.into());
                        }
                        let result = AgentAdp.endpoints(MACOS, &config);
                        if enabled && port == Some("invalid") {
                            assert!(result.unwrap_err().contains("DD_DOGSTATSD_TCP_PORT"));
                        } else {
                            let endpoints = result.unwrap();
                            assert_eq!(
                                endpoints.contains_key("dogstatsd_tcp"),
                                enabled && port == Some("29001")
                            );
                            if enabled && port == Some("29001") {
                                assert_network_endpoint(
                                    &endpoints["dogstatsd_tcp"],
                                    EndpointKind::Tcp,
                                    29001,
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
    fn macos_serves_the_otlp_proxy_listener_only_when_the_adp_otlp_proxy_is_enabled() {
        for adp in ["true", "false"] {
            for otlp in ["true", "false"] {
                for proxy in [None, Some("true"), Some("false")] {
                    let mut config = config();
                    config.env.insert("DD_DATA_PLANE_ENABLED".into(), adp.into());
                    config.env.insert("DD_DATA_PLANE_OTLP_ENABLED".into(), otlp.into());
                    config.env.remove("DD_DATA_PLANE_OTLP_PROXY_ENABLED");
                    if let Some(proxy) = proxy {
                        config
                            .env
                            .insert("DD_DATA_PLANE_OTLP_PROXY_ENABLED".into(), proxy.into());
                    }
                    let endpoints = AgentAdp.endpoints(MACOS, &config).unwrap();
                    assert_eq!(
                        endpoints.contains_key("agent_otlp_grpc"),
                        !(adp == "true" && otlp == "true" && proxy == Some("true"))
                    );
                    assert_eq!(
                        endpoints.contains_key("agent_otlp_proxy_grpc"),
                        adp == "true" && otlp == "true" && proxy == Some("true")
                    );
                }
            }
        }
    }
}
