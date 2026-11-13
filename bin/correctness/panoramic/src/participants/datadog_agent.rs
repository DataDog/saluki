//! What the participant types that run the Datadog Agent share: its default environment and the rules that withdraw
//! it, its configuration file, its connection to an intake, the host view, and its s6 services.
//!
//! Both sides of a comparison run the Agent, so they take one default environment from here and cannot drift apart.
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

use crate::core::participant::{Os, SpecContext};
use crate::core::spec::{
    Address, Consumed, EndpointSpec, ExitChannel, FileSource, FileSpec, HostMount, OutputChannel, ParticipantSpec,
    ProcessSpec, Protocol, Wiring,
};
use crate::core::template::{Reference, Template};

/// The Agent image the `Makefile` builds with ADP in it.
pub(super) const IMAGE: &str = "saluki-images/datadog-agent:testing-devel";

/// The name a case writes for the file ADP and the Agent read their configuration from, and its destination.
pub(super) const DATADOG_YAML: &str = "datadog.yaml";
pub(super) const DATADOG_YAML_PATH: &str = "/etc/datadog-agent/datadog.yaml";

/// The prefix of the default environment entries that only ADP reads.
pub(super) const DATA_PLANE_PREFIX: &str = "DD_DATA_PLANE_";

/// One entry of the default environment: the variable, the `datadog.yaml` key of the same setting, and the value.
pub(super) struct DefaultEnv {
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
pub(super) struct Listener {
    pub(super) name: &'static str,
    pub(super) protocol: Protocol,
    pub(super) address: ListenAt,
    pub(super) on: &'static [Os],
    /// The line a case writes to wire an opt-in listener.
    pub(super) opt_in: Option<(&'static str, &'static str)>,
}

pub(super) enum ListenAt {
    Port(u16),
    Socket(&'static str),
    Pipe(&'static str),
}

pub(super) const EVERY_OS: &[Os] = &[Os::Linux, Os::Macos, Os::Windows];
pub(super) const NOT_WINDOWS: &[Os] = &[Os::Linux, Os::Macos];

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
pub(super) const INTAKE: Consumed = Consumed {
    accepts: &[Protocol::Http],
    required: false,
    env: &[
        ("DD_DD_URL", "{{ self.connections.intake.url }}"),
        ("DD_APM_DD_URL", "{{ self.connections.intake.url }}"),
    ],
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct With {
    /// Mounts the host's `/proc`, cgroup tree and Docker socket, and points the software at them.
    #[serde(default)]
    host_view: bool,
}

/// The files of the s6 services under `/etc/services.d/<service>/`, for each of `services`.
pub(super) fn s6_overlay(services: &[&str]) -> BTreeMap<String, FileSpec> {
    S6_OVERLAY
        .iter()
        .filter(|(destination, _, _)| {
            services
                .iter()
                .any(|service| destination.starts_with(&format!("/etc/services.d/{}/", service)))
        })
        .map(|&(destination, label, bytes)| {
            let file = FileSpec {
                source: FileSource::Bytes { label, bytes },
                mode: Some(0o755),
                supervises: true,
            };
            (destination.to_string(), file)
        })
        .collect()
}

/// A process of an s6 service, with its exit record and its log file.
pub(super) fn s6_process(name: &str, record: &str, log: &str) -> Result<ProcessSpec, String> {
    ProcessSpec::new(
        name,
        ExitChannel::Record {
            file: record.to_string(),
        },
        OutputChannel::File { file: log.to_string() },
    )
}

/// Adds each of `listeners` that the realization's OS has to `spec`'s endpoints, with how a case wires an opt-in one.
pub(super) fn add_endpoints(
    cx: &SpecContext<'_>, listeners: &[Listener], spec: &mut ParticipantSpec,
) -> Result<(), String> {
    for listener in listeners
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
    Ok(())
}

/// Sets the default environment, with or without the entries only ADP reads, and the host view when `with` asks for it,
/// withdrawing each entry the case sets itself.
pub(super) fn set_defaults(cx: &SpecContext<'_>, data_plane: bool, spec: &mut ParticipantSpec) -> Result<(), String> {
    let with: With = cx.with()?;
    let mut defaults: Vec<&DefaultEnv> = DEFAULT_ENV
        .iter()
        .filter(|entry| data_plane || !entry.key.starts_with(DATA_PLANE_PREFIX))
        .collect();
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
    withdraw_defaults(cx, &defaults, spec)
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
    fn container_scripts_keep_unix_line_endings() {
        let scripts = S6_OVERLAY
            .iter()
            .map(|(destination, _, bytes)| (*destination, *bytes))
            .chain([("senders.sh", super::super::millstone::SENDERS_SCRIPT)]);
        for (destination, bytes) in scripts {
            assert!(bytes.starts_with(b"#!"), "{} has no interpreter line", destination);
            assert!(!bytes.contains(&b'\r'), "{} has a carriage return", destination);
        }
    }
}
