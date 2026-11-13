//! `agent-adp`: the Datadog Agent and ADP in one participant, as the Agent image runs them under s6.
//!
//! The default environment moves every listener of the software off its own default port, as the harness has always
//! done for integration cases. Entries that refer to an endpoint wire it; the rest are literals for listeners no case
//! talks to. Every other endpoint is opt-in: a case that wants the listener writes the reference under the key the
//! software reads.
//!
//! The Agent and ADP let environment mask `datadog.yaml`, so a default entry would override a key the case sets in its
//! file. Two rules withdraw defaults. Both look only at which keys the case sets, never at a value:
//!
//! 1. An entry is withdrawn when the case's `datadog.yaml` sets the entry's file key in any spelling the software
//!    reads: in any letter case, and nested or dotted.
//! 2. An entry `DD_<K>` is withdrawn when the case's `env` sets `ADP_DD_<K>`. The image gives that value to ADP alone,
//!    and the Agent would stream the default to ADP over it. The entry is withdrawn for both processes, so the Agent
//!    falls back to its own default for the key.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::core::participant::{Enclosure, Os, ParticipantType, SpecContext};
use crate::core::spec::{
    Address, Consumed, EndpointSpec, Evidence, ExitChannel, FileSource, FileSpec, HostMount, OutputChannel,
    ParticipantSpec, ProcessSpec, Protocol, Wiring,
};
use crate::core::template::{Reference, Template};

/// The Agent image the `Makefile` builds with ADP in it.
const IMAGE: &str = "saluki-images/datadog-agent:testing-devel";

/// The name a case writes for the file ADP and the Agent read their configuration from, and its destination.
const DATADOG_YAML: &str = "datadog.yaml";
const DATADOG_YAML_PATH: &str = "/etc/datadog-agent/datadog.yaml";

/// One entry of the default environment: the variable, the `datadog.yaml` key of the same setting, and the value.
struct DefaultEnv {
    key: &'static str,
    file_key: &'static str,
    value: &'static str,
}

const fn env(key: &'static str, file_key: &'static str, value: &'static str) -> DefaultEnv {
    DefaultEnv { key, file_key, value }
}

const DEFAULT_ENV: &[DefaultEnv] = &[
    env(
        "DD_DOGSTATSD_PORT",
        "dogstatsd_port",
        "{{ self.endpoints.dogstatsd_udp.port }}",
    ),
    env(
        "DD_APM_RECEIVER_PORT",
        "apm_config.receiver_port",
        "{{ self.endpoints.apm_tcp.port }}",
    ),
    env(
        "DD_DATA_PLANE_API_LISTEN_ADDRESS",
        "data_plane.api_listen_address",
        "tcp://0.0.0.0:{{ self.endpoints.api.port }}",
    ),
    env(
        "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT",
        "otlp_config.receiver.protocols.grpc.endpoint",
        "0.0.0.0:{{ self.endpoints.agent_otlp_grpc.port }}",
    ),
    env(
        "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT",
        "otlp_config.receiver.protocols.http.endpoint",
        "0.0.0.0:{{ self.endpoints.agent_otlp_http.port }}",
    ),
    env("DD_CMD_PORT", "cmd_port", "45001"),
    env("DD_GUI_PORT", "GUI_port", "-1"),
    env("DD_EXPVAR_PORT", "expvar_port", "45000"),
    env("DD_PROCESS_CONFIG_CMD_PORT", "process_config.cmd_port", "46062"),
    env("DD_AGENT_IPC_PORT", "agent_ipc.port", "45004"),
    env(
        "DD_DATA_PLANE_SECURE_API_LISTEN_ADDRESS",
        "data_plane.secure_api_listen_address",
        "tcp://0.0.0.0:45101",
    ),
    env(
        "DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR",
        "data_plane.telemetry_listen_addr",
        "tcp://0.0.0.0:45102",
    ),
];

/// The host view: the host's `/proc`, cgroup tree and Docker socket, mounted read-only, and the environment that points
/// the software at them.
const HOST_VIEW_MOUNTS: [(&str, &str); 3] = [
    ("/proc", "/host/proc"),
    ("/sys/fs/cgroup", "/host/sys/fs/cgroup"),
    ("/var/run/docker.sock", "/var/run/docker.sock"),
];
const HOST_VIEW_ENV: &[DefaultEnv] = &[
    env("DD_CONTAINER_PROC_ROOT", "container_proc_root", "/host/proc"),
    env(
        "DD_CONTAINER_CGROUP_ROOT",
        "container_cgroup_root",
        "/host/sys/fs/cgroup",
    ),
];

/// One listener the software can open, as an endpoint.
struct Listener {
    name: &'static str,
    protocol: Protocol,
    address: ListenAt,
    on: &'static [Os],
    /// The variable a case sets to wire an opt-in listener, and the value that wires it.
    opt_in: Option<(&'static str, &'static str)>,
}

enum ListenAt {
    Port(u16),
    Socket(&'static str),
    Pipe(&'static str),
}

const EVERY_OS: &[Os] = &[Os::Linux, Os::Macos, Os::Windows];
const NOT_WINDOWS: &[Os] = &[Os::Linux, Os::Macos];

const LISTENERS: &[Listener] = &[
    Listener {
        name: "dogstatsd_udp",
        protocol: Protocol::Udp,
        address: ListenAt::Port(48125),
        on: EVERY_OS,
        opt_in: None,
    },
    Listener {
        name: "apm_tcp",
        protocol: Protocol::Http,
        address: ListenAt::Port(48126),
        on: EVERY_OS,
        opt_in: None,
    },
    Listener {
        name: "api",
        protocol: Protocol::Http,
        address: ListenAt::Port(45100),
        on: EVERY_OS,
        opt_in: None,
    },
    Listener {
        name: "agent_otlp_grpc",
        protocol: Protocol::Grpc,
        address: ListenAt::Port(44317),
        on: EVERY_OS,
        opt_in: None,
    },
    Listener {
        name: "agent_otlp_http",
        protocol: Protocol::Http,
        address: ListenAt::Port(44318),
        on: EVERY_OS,
        opt_in: None,
    },
    Listener {
        name: "otlp_grpc",
        protocol: Protocol::Grpc,
        address: ListenAt::Port(46317),
        on: EVERY_OS,
        opt_in: Some((
            "DD_DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY",
            "0.0.0.0:{{ self.endpoints.otlp_grpc.port }}",
        )),
    },
    Listener {
        name: "otlp_http",
        protocol: Protocol::Http,
        address: ListenAt::Port(46318),
        on: EVERY_OS,
        opt_in: Some((
            "DD_DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY",
            "0.0.0.0:{{ self.endpoints.otlp_http.port }}",
        )),
    },
    Listener {
        name: "dogstatsd_tcp",
        protocol: Protocol::Tcp,
        address: ListenAt::Port(9126),
        on: EVERY_OS,
        opt_in: Some(("DD_DOGSTATSD_TCP_PORT", "{{ self.endpoints.dogstatsd_tcp.port }}")),
    },
    Listener {
        name: "dogstatsd_uds",
        protocol: Protocol::UnixDatagram,
        address: ListenAt::Socket("{{ self.dir }}/dogstatsd_uds.sock"),
        on: NOT_WINDOWS,
        opt_in: Some(("DD_DOGSTATSD_SOCKET", "{{ self.endpoints.dogstatsd_uds.path }}")),
    },
    Listener {
        name: "dogstatsd_uds_stream",
        protocol: Protocol::UnixStream,
        address: ListenAt::Socket("{{ self.dir }}/dogstatsd_uds_stream.sock"),
        on: NOT_WINDOWS,
        opt_in: Some((
            "DD_DOGSTATSD_STREAM_SOCKET",
            "{{ self.endpoints.dogstatsd_uds_stream.path }}",
        )),
    },
    Listener {
        name: "apm_uds",
        protocol: Protocol::UnixStream,
        address: ListenAt::Socket("{{ self.dir }}/apm_uds.sock"),
        on: &[Os::Linux],
        opt_in: Some(("DD_APM_RECEIVER_SOCKET", "{{ self.endpoints.apm_uds.path }}")),
    },
    Listener {
        name: "dogstatsd_pipe",
        protocol: Protocol::NamedPipe,
        address: ListenAt::Pipe("panoramic-dogstatsd"),
        on: &[Os::Windows],
        opt_in: Some(("DD_DOGSTATSD_PIPE_NAME", "{{ self.endpoints.dogstatsd_pipe.name }}")),
    },
];

/// ADP's line once its DogStatsD UDP listener is bound, whatever interface it binds.
const DOGSTATSD_UDP_STARTED: &str = r#":{{ self.endpoints.dogstatsd_udp.port }}" | DogStatsD listener started."#;

/// The s6 overlay, placed over the image's own services: for each service, a `run` that merges standard error into the
/// log, a `log/run` that writes the log file the harness follows, and a `finish` that writes the exit record. Each is
/// supervising. The scripts sit beside the image's own in `docker/`, and no image is built with them.
const S6_OVERLAY: [(&str, &str, &[u8]); 6] = [
    (
        "/etc/services.d/data-plane/run",
        "s6 adp run",
        include_bytes!("../../../../../docker/s6-services-panoramic/data-plane/run"),
    ),
    (
        "/etc/services.d/data-plane/finish",
        "s6 adp finish",
        include_bytes!("../../../../../docker/s6-services-panoramic/data-plane/finish"),
    ),
    (
        "/etc/services.d/data-plane/log/run",
        "s6 adp log",
        include_bytes!("../../../../../docker/s6-services-panoramic/data-plane/log/run"),
    ),
    (
        "/etc/services.d/agent/run",
        "s6 agent run",
        include_bytes!("../../../../../docker/s6-services-panoramic/agent/run"),
    ),
    (
        "/etc/services.d/agent/finish",
        "s6 agent finish",
        include_bytes!("../../../../../docker/s6-services-panoramic/agent/finish"),
    ),
    (
        "/etc/services.d/agent/log/run",
        "s6 agent log",
        include_bytes!("../../../../../docker/s6-services-panoramic/agent/log/run"),
    ),
];

/// The connection to an intake, which both the Agent and ADP send to.
const INTAKE: Consumed = Consumed {
    accepts: &[Protocol::Http],
    required: false,
    env: &[
        ("DD_DD_URL", "{{ self.connections.intake.url }}"),
        ("DD_APM_DD_URL", "{{ self.connections.intake.url }}"),
    ],
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct With {
    /// Mounts the host's `/proc`, cgroup tree and Docker socket, and points the software at them.
    #[serde(default)]
    host_view: bool,
}

pub(crate) struct AgentAdp;

impl ParticipantType for AgentAdp {
    fn name(&self) -> &'static str {
        "agent-adp"
    }

    fn supports(&self) -> &'static [(Os, Enclosure)] {
        &[(Os::Linux, Enclosure::LinuxContainer)]
    }

    fn spec(&self, cx: &SpecContext<'_>) -> Result<ParticipantSpec, String> {
        let with: With = cx.with()?;
        let processes = vec![
            ProcessSpec::new(
                "adp",
                ExitChannel::Record {
                    file: "data-plane".to_string(),
                },
                OutputChannel::File {
                    file: "adp.log".to_string(),
                },
            )?,
            ProcessSpec::new(
                "agent",
                ExitChannel::Record {
                    file: "agent".to_string(),
                },
                OutputChannel::File {
                    file: "agent.log".to_string(),
                },
            )?,
        ];
        let ready = Evidence::Output {
            pattern: Template::parse(DOGSTATSD_UDP_STARTED)?,
            process: "adp".to_string(),
        };
        let mut spec = ParticipantSpec {
            image: Some(IMAGE.to_string()),
            // No command: the image's own entrypoint starts s6.
            files: S6_OVERLAY
                .iter()
                .map(|&(destination, label, bytes)| {
                    let file = FileSpec {
                        source: FileSource::Bytes { label, bytes },
                        mode: Some(0o755),
                        supervises: true,
                    };
                    (destination.to_string(), file)
                })
                .collect(),
            ready: BTreeMap::from([("dogstatsd_udp".to_string(), ready)]),
            processes: Some(processes),
            aliases: BTreeMap::from([(DATADOG_YAML, DATADOG_YAML_PATH)]),
            consumes: BTreeMap::from([("intake", INTAKE)]),
            ..ParticipantSpec::default()
        };

        for listener in LISTENERS
            .iter()
            .filter(|listener| listener.on.contains(&cx.realization.os))
        {
            let address = match listener.address {
                ListenAt::Port(port) => Address::Port(port),
                ListenAt::Socket(path) => Address::Path(Template::parse(path)?),
                ListenAt::Pipe(name) => Address::Pipe(name.to_string()),
            };
            let endpoint = EndpointSpec {
                protocol: listener.protocol,
                address,
                tls: false,
            };
            spec.endpoints.insert(listener.name.to_string(), endpoint);
            if let Some((key, value)) = listener.opt_in {
                let wiring = Wiring {
                    withdrawn: None,
                    write: format!("{}: \"{}\"", key, value),
                    overridden: None,
                };
                spec.wiring.insert(listener.name.to_string(), wiring);
            }
        }

        let mut defaults: Vec<&DefaultEnv> = DEFAULT_ENV.iter().collect();
        if with.host_view {
            defaults.extend(HOST_VIEW_ENV);
            spec.mounts = Some(
                HOST_VIEW_MOUNTS
                    .iter()
                    .map(|(host, path)| HostMount {
                        host: host.to_string(),
                        path: path.to_string(),
                        read_only: true,
                    })
                    .collect(),
            );
        }
        withdraw_defaults(cx, &defaults, &mut spec)?;
        let enabled = |key| cx.case.env.get(key).is_some_and(|value| value.text() == "true");
        if enabled("DD_DATA_PLANE_ENABLED") && enabled("DD_DATA_PLANE_OTLP_ENABLED") {
            // The image's cont-init script moves the Agent's OTLP listeners to loopback ports of its own.
            for endpoint in ["agent_otlp_grpc", "agent_otlp_http"] {
                let wiring = Wiring {
                    withdrawn: None,
                    write: "DD_DATA_PLANE_OTLP_ENABLED: \"false\"".to_string(),
                    overridden: Some(
                        "With DD_DATA_PLANE_OTLP_ENABLED set, the image moves the Agent's OTLP listeners to 127.0.0.1, \
                         ports 14317 and 14318, where no other participant reaches them."
                            .to_string(),
                    ),
                };
                spec.wiring.insert(endpoint.to_string(), wiring);
            }
        }
        Ok(spec)
    }
}

/// Sets the default environment from `defaults`, withdrawing each entry the case sets itself, and records how a case
/// wires each endpoint a withdrawn entry wired.
fn withdraw_defaults(cx: &SpecContext<'_>, defaults: &[&DefaultEnv], spec: &mut ParticipantSpec) -> Result<(), String> {
    // The file is parsed as the case wrote it, so a reference in it sits inside a quoted scalar.
    let file: Value = match cx.file_text(DATADOG_YAML)? {
        Some(text) => {
            serde_saphyr::from_str(text).map_err(|e| format!("{} does not parse as YAML: {}", DATADOG_YAML, e))?
        }
        None => Value::Null,
    };
    for entry in defaults {
        let value = Template::parse(entry.value)?;
        let adp_only = format!("ADP_{}", entry.key);
        let file_key: Vec<&str> = entry.file_key.split('.').collect();
        let withdrawn = if sets(&file, &file_key) {
            let line = match entry.file_key.rsplit_once('.') {
                Some((section, key)) => format!("{}: \"{}\" under {}", key, entry.value, section),
                None => format!("{}: \"{}\"", entry.file_key, entry.value),
            };
            let write = format!("{} in {}, placed with template: true", line, DATADOG_YAML);
            Some((format!("{} sets '{}'", DATADOG_YAML, entry.file_key), write))
        } else if cx.case.env.contains_key(&adp_only) {
            let write = format!("{}: \"{}\"", adp_only, entry.value);
            Some((format!("env sets {}", adp_only), write))
        } else {
            None
        };
        let Some((because, write)) = withdrawn else {
            spec.env.insert(entry.key.to_string(), value);
            continue;
        };
        for reference in value.references() {
            if let Reference::Endpoint { endpoint, .. } = reference {
                let wiring = Wiring {
                    withdrawn: Some(format!("{}, so the default {} was withdrawn", because, entry.key)),
                    write: write.clone(),
                    overridden: None,
                };
                spec.wiring.insert(endpoint.clone(), wiring);
            }
        }
    }
    Ok(())
}

/// Whether the YAML mapping `node` sets the setting at `path`, read as the Agent and ADP read `datadog.yaml`: each key
/// matches in any letter case and stands for the parts it splits into on `.`.
///
/// A key with no value sets nothing, because ADP drops null values from its file before it reads the environment.
fn sets(node: &Value, path: &[&str]) -> bool {
    let Some(map) = node.as_object() else {
        return false;
    };
    map.iter().any(|(key, value)| {
        let mut rest = path;
        for part in key.split('.') {
            match rest.split_first() {
                Some((first, tail)) if first.eq_ignore_ascii_case(part) => rest = tail,
                _ => return false,
            }
        }
        if rest.is_empty() {
            !value.is_null()
        } else {
            sets(value, rest)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s6_overlay_keeps_unix_line_endings() {
        for (destination, _, bytes) in S6_OVERLAY {
            assert!(bytes.starts_with(b"#!"), "{} has no interpreter line", destination);
            assert!(!bytes.contains(&b'\r'), "{} has a carriage return", destination);
        }
    }
}
