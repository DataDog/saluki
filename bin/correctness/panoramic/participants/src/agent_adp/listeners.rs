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
}

struct Listener {
    name: &'static str,
    kind: EndpointKind,
    key: &'static str,
    default: &'static str,
    value: ValueKind,
    owner: Owner,
    macos_only: bool,
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
    },
    Listener {
        name: "dogstatsd_tcp",
        kind: EndpointKind::Tcp,
        key: "DOGSTATSD_TCP_PORT",
        default: "0",
        value: ValueKind::Port(|port| BTreeMap::from([("DD_DOGSTATSD_TCP_PORT".into(), port.to_string())])),
        owner: Owner::AdpDogStatsD,
        macos_only: false,
    },
    Listener {
        name: "dogstatsd_uds",
        kind: EndpointKind::UnixDatagram,
        key: "DOGSTATSD_SOCKET",
        default: "/var/run/datadog/dsd.socket",
        value: ValueKind::Path,
        owner: Owner::DogStatsD,
        macos_only: false,
    },
    Listener {
        name: "dogstatsd_uds_stream",
        kind: EndpointKind::UnixStream,
        key: "DOGSTATSD_STREAM_SOCKET",
        default: "",
        value: ValueKind::Path,
        owner: Owner::DogStatsD,
        macos_only: false,
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
    },
    Listener {
        name: "agent_cmd",
        kind: EndpointKind::Tcp,
        key: "CMD_PORT",
        default: "5001",
        value: ValueKind::Port(|port| BTreeMap::from([("DD_CMD_PORT".into(), port.to_string())])),
        owner: Owner::Agent,
        macos_only: false,
    },
    Listener {
        name: "agent_ipc",
        kind: EndpointKind::Tcp,
        key: "AGENT_IPC_PORT",
        default: "0",
        value: ValueKind::PositivePort(|port| BTreeMap::from([("DD_AGENT_IPC_PORT".into(), port.to_string())])),
        owner: Owner::Agent,
        macos_only: false,
    },
    Listener {
        name: "agent_expvar",
        kind: EndpointKind::Tcp,
        key: "EXPVAR_PORT",
        default: "5000",
        value: ValueKind::Port(|port| BTreeMap::from([("DD_EXPVAR_PORT".into(), port.to_string())])),
        owner: Owner::Agent,
        macos_only: true,
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
    },
];

struct Environment<'a> {
    values: &'a BTreeMap<String, String>,
    macos: bool,
}

impl<'a> Environment<'a> {
    fn value(&self, owner: Owner, key: &str, default: &'a str) -> (String, &'a str) {
        if !self.macos && matches!(owner, Owner::Adp) {
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
        macos: true,
    };
    Ok(env.enabled(Owner::Agent, "DATA_PLANE_ENABLED", "false")?
        && env.enabled(Owner::Agent, "DATA_PLANE_OTLP_ENABLED", "false")?
        && env.enabled(Owner::Agent, "DATA_PLANE_OTLP_PROXY_ENABLED", "false")?)
}

pub(super) fn endpoints(
    realization: Realization, env: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Endpoint>, String> {
    let macos = (realization.os, realization.kind) == (Os::MacOs, RealizationKind::HostProcessGroup);
    let env = Environment { values: env, macos };
    let adp = env.enabled(Owner::Agent, "DATA_PLANE_ENABLED", "false")?;
    let dogstatsd = env.enabled(Owner::Agent, "USE_DOGSTATSD", "true")?;
    let adp_dogstatsd = adp && env.enabled(Owner::Agent, "DATA_PLANE_DOGSTATSD_ENABLED", "true")?;
    let adp_otlp = adp && env.enabled(Owner::Adp, "DATA_PLANE_OTLP_ENABLED", "false")?;
    let proxy = macos && otlp_proxy_enabled(env.values)?;
    let mut endpoints = BTreeMap::new();
    for listener in LISTENERS {
        if listener.macos_only && !macos {
            continue;
        }
        let owner = match listener.owner {
            Owner::AgentOtlpGrpc => Owner::Agent,
            Owner::AgentOtlpProxy if !proxy => continue,
            Owner::AgentOtlpProxy => Owner::Agent,
            Owner::Adp if !adp => continue,
            Owner::AdpOtlp if !adp_otlp => continue,
            Owner::AdpDogStatsD if !(dogstatsd && adp_dogstatsd) => continue,
            Owner::AdpDogStatsD if macos && !env.values.contains_key(&format!("DD_{}", listener.key)) => {
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
        let default = if macos && matches!(listener.value, ValueKind::Path) {
            ""
        } else {
            listener.default
        };
        let (key, value) = env.value(owner, listener.key, default);
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
            ValueKind::Port(deliver)
            | ValueKind::PositivePort(deliver)
            | ValueKind::Address(deliver)
            | ValueKind::HostPort(deliver) => {
                let port = if macos && !env.values.contains_key(&key) {
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
                            if !macos && loopback_host(&host.to_string()) {
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
                            if !macos && loopback_host(address.host_str().unwrap()) {
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
                if !macos
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
        endpoint::test_support::{assert_network_endpoint, assert_socket_endpoint, assigned_delivery},
        participant::{EffectiveConfig, ParticipantType},
    };

    use super::*;
    use crate::AgentAdp;

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
