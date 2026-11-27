//! What the participant types that run the Datadog Agent share: its default environment and the rules that withdraw
//! it, its configuration file, its connections to an intake and to a server it forwards DogStatsD packets to, the host
//! view, the address of its own container, and its s6 services.
//!
//! Both sides of a comparison run the Agent, so they take one default environment from here and cannot drift apart.
//! The default environment moves every listener of the software off its own default port, as the harness has always
//! done for integration cases. Entries that refer to an endpoint wire it; the rest are literals for listeners no case
//! talks to. Every other endpoint is opt-in: a case that wants the listener writes the reference under the key the
//! software reads.
//!
//! The Agent and ADP let environment mask `datadog.yaml`, so a default entry would override a key the case sets in its
//! file. Three rules withdraw defaults. All look only at which keys the case sets, never at a value:
//!
//! 1. An entry is withdrawn when the case's `datadog.yaml` sets the entry's file key in any spelling the software
//!    reads: in any letter case, and nested or dotted.
//! 2. An entry `DD_<K>` is withdrawn when the case's `env` sets `ADP_DD_<K>`. The image gives that value to ADP alone,
//!    and the Agent would stream the default to ADP over it. The entry is withdrawn for both processes, so the Agent
//!    falls back to its own default for the key.
//! 3. An entry is withdrawn when the case's `env` sets another name the software reads for the entry's setting, or the
//!    `ADP_` form of that name. The software takes the first of a setting's names that is set, and the entry comes
//!    first, so it would mask the case's value.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::core::backend::KUBELET;
use crate::core::participant::{Enclosure, Os, SpecContext};
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

/// Other names the software reads for a default entry's setting, after the entry's own: the entry, then the name.
const SECONDARY_NAMES: &[(&str, &str)] = &[
    ("DD_APM_RECEIVER_PORT", "DD_RECEIVER_PORT"),
    ("DD_PROCESS_CONFIG_CMD_PORT", "DD_PROCESS_AGENT_CMD_PORT"),
];

/// The host view: the host's `/proc` and cgroup tree, mounted read-only, and the environment that points the software
/// at them. A container also gets the Docker socket, which a kind node has none of: a pod gets the runtime's socket
/// with the realization option `cri_socket`.
const HOST_VIEW_MOUNTS: [(&str, &str); 2] = [("/proc", "/host/proc"), ("/sys/fs/cgroup", "/host/sys/fs/cgroup")];
const DOCKER_SOCKET_MOUNT: (&str, &str) = ("/var/run/docker.sock", "/var/run/docker.sock");
const HOST_VIEW_ENV: &[DefaultEnv] = &[
    env("DD_CONTAINER_PROC_ROOT", "container_proc_root", "/host/proc"),
    env(
        "DD_CONTAINER_CGROUP_ROOT",
        "container_cgroup_root",
        "/host/sys/fs/cgroup",
    ),
];

/// Kubelet access in a pod, as the Datadog Helm chart sets it up: the kubelet on the pod's node, reached by its address,
/// without verifying the certificate it serves, which a kind node signs itself. The pod's service account may read the
/// kubelet API with the realization option `kubelet`, which a pod has unless the case turns it off.
const KUBELET_HOST_ENV: &str = "DD_KUBERNETES_KUBELET_HOST";
const KUBELET_HOST_KEY: &str = "kubernetes_kubelet_host";
const KUBELET_HOST_FIELD: &str = "status.hostIP";
const KUBELET_ENV: &[DefaultEnv] = &[env("DD_KUBELET_TLS_VERIFY", "kubelet_tls_verify", "false")];

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
///
/// The `ADP_` forms carry the same value. Delivering them lets the guard on delivered keys reject a case that sets
/// one, which the image would give ADP in place of the delivered key.
pub(super) const INTAKE: Consumed = Consumed {
    accepts: &[Protocol::Http],
    required: false,
    env: &[
        ("DD_DD_URL", "{{ self.connections.intake.url }}"),
        ("DD_APM_DD_URL", "{{ self.connections.intake.url }}"),
        ("ADP_DD_DD_URL", "{{ self.connections.intake.url }}"),
        ("ADP_DD_APM_DD_URL", "{{ self.connections.intake.url }}"),
    ],
};

/// The connection to a DogStatsD server the Agent and ADP forward every packet they receive to, with the `ADP_` forms
/// for the reason [`INTAKE`] has them.
pub(super) const DOGSTATSD_FORWARD: Consumed = Consumed {
    accepts: &[Protocol::Udp],
    required: false,
    env: &[
        (
            "DD_STATSD_FORWARD_HOST",
            "{{ self.connections.dogstatsd_forward.host }}",
        ),
        (
            "DD_STATSD_FORWARD_PORT",
            "{{ self.connections.dogstatsd_forward.port }}",
        ),
        (
            "ADP_DD_STATSD_FORWARD_HOST",
            "{{ self.connections.dogstatsd_forward.host }}",
        ),
        (
            "ADP_DD_STATSD_FORWARD_PORT",
            "{{ self.connections.dogstatsd_forward.port }}",
        ),
    ],
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct With {
    /// Mounts the host's `/proc` and cgroup tree, and the Docker socket where there is one, and points the software at
    /// them.
    #[serde(default)]
    pub(super) host_view: bool,
    /// Gives the participant the address of its own container, in environment variables and in its hosts file.
    #[serde(default)]
    pub(super) own_ip: Option<OwnIp>,
}

/// What `with.own_ip` asks for: each variable in `env` is set to the participant's own address, and each name in
/// `hosts` resolves to it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OwnIp {
    #[serde(default)]
    pub(super) env: Vec<String>,
    #[serde(default)]
    pub(super) hosts: Vec<String>,
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

/// A process a supervisor in the participant reports: s6 in a Linux container or pod, the entrypoint in a Windows
/// container. It has an exit record and a log file.
pub(super) fn supervised_process(name: &str, record: &str, log: &str) -> Result<ProcessSpec, String> {
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

/// Sets the default environment, with or without the entries only ADP reads, the host view when `with` asks for it, and
/// kubelet access in a pod unless the case turns it off, withdrawing each entry the case sets itself.
pub(super) fn set_defaults(cx: &SpecContext<'_>, data_plane: bool, spec: &mut ParticipantSpec) -> Result<(), String> {
    let with: With = cx.with()?;
    let mut defaults: Vec<&DefaultEnv> = DEFAULT_ENV
        .iter()
        .filter(|entry| data_plane || !entry.key.starts_with(DATA_PLANE_PREFIX))
        .collect();
    if with.host_view {
        defaults.extend(HOST_VIEW_ENV);
        let docker_socket = (cx.realization.enclosure != Enclosure::KubernetesPod).then_some(DOCKER_SOCKET_MOUNT);
        spec.mounts = Some(
            HOST_VIEW_MOUNTS
                .iter()
                .chain(&docker_socket)
                .map(|(host, path)| HostMount {
                    host: host.to_string(),
                    path: path.to_string(),
                    read_only: true,
                })
                .collect(),
        );
    }
    if cx.realization.enclosure == Enclosure::KubernetesPod && cx.options.get(KUBELET) != Some(&false) {
        spec.options.insert(KUBELET, true);
        defaults.extend(KUBELET_ENV);
        if !case_sets(cx, KUBELET_HOST_ENV, KUBELET_HOST_KEY)? {
            spec.field_env.insert(KUBELET_HOST_ENV.to_string(), KUBELET_HOST_FIELD);
        }
    }
    withdraw_defaults(cx, &defaults, spec)
}

/// Sets the default environment from `defaults`, withdrawing each entry the case sets itself, and records how a case
/// wires each endpoint a withdrawn entry wired.
fn withdraw_defaults(cx: &SpecContext<'_>, defaults: &[&DefaultEnv], spec: &mut ParticipantSpec) -> Result<(), String> {
    let file = case_datadog_yaml(cx)?;
    for entry in defaults {
        let value = Template::parse(entry.value)?;
        let mut env_names = std::iter::once(format!("ADP_{}", entry.key)).chain(
            SECONDARY_NAMES
                .iter()
                .filter(|(key, _)| *key == entry.key)
                .flat_map(|(_, name)| [name.to_string(), format!("ADP_{}", name)]),
        );
        let file_key: Vec<&str> = entry.file_key.split('.').collect();
        let withdrawn = if sets(&file, &file_key) {
            let line = match entry.file_key.rsplit_once('.') {
                Some((section, key)) => format!("{}: \"{}\" under {}", key, entry.value, section),
                None => format!("{}: \"{}\"", entry.file_key, entry.value),
            };
            let write = format!("{} in {}, placed with template: true", line, DATADOG_YAML);
            Some((format!("{} sets '{}'", DATADOG_YAML, entry.file_key), write))
        } else if let Some(name) = env_names.find(|name| cx.case.env.contains_key(name)) {
            let write = format!("{}: \"{}\"", name, entry.value);
            Some((format!("env sets {}", name), write))
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

/// The case's `datadog.yaml`, or null if it supplies none. The file is parsed as the case wrote it, so a reference in
/// it sits inside a quoted scalar.
fn case_datadog_yaml(cx: &SpecContext<'_>) -> Result<Value, String> {
    match cx.file_text(DATADOG_YAML)? {
        Some(text) => {
            saluki_config::yaml::from_str(text).map_err(|e| format!("{} does not parse as YAML: {}", DATADOG_YAML, e))
        }
        None => Ok(Value::Null),
    }
}

/// Whether the case sets a setting: its `env` holds `env_key` or the `ADP_` form of it, or its `datadog.yaml` sets
/// `file_key` as [`sets`] reads it.
pub(super) fn case_sets(cx: &SpecContext<'_>, env_key: &str, file_key: &str) -> Result<bool, String> {
    let in_env = cx.case.env.contains_key(env_key) || cx.case.env.contains_key(&format!("ADP_{}", env_key));
    if in_env {
        return Ok(true);
    }
    let path: Vec<&str> = file_key.split('.').collect();
    Ok(sets(&case_datadog_yaml(cx)?, &path))
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
    use std::collections::BTreeMap;

    use super::*;
    use crate::core::case::CASE_FILE;
    use crate::core::plan::PlannedParticipant;

    /// Plans for Linux a case of one `agent` participant, realized as `realization`, with `extra` as more of its
    /// fields, and calls `check` with the plan of the participant.
    fn plan_agent(realization: &str, extra: &str, check: impl FnOnce(&PlannedParticipant)) {
        let config = format!(
            "timeout: 1m\nsubject_os: [linux]\nparticipants:\n  agent:\n    type: agent\n    subject: true\n    \
             realization: {}\n{}procedure: []\n",
            realization, extra
        );
        let suite = tempfile::tempdir().unwrap();
        let dir = suite.path().join("case");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join(CASE_FILE), config).unwrap();
        let mut plans = crate::load_and_plan(&dir, &BTreeMap::new()).unwrap_or_else(|errors| {
            panic!(
                "the case should plan: {:?}",
                errors.iter().map(ToString::to_string).collect::<Vec<_>>()
            )
        });
        let plan = plans.remove(&Os::Linux).unwrap();
        check(&plan.participants["agent"]);
    }

    #[test]
    fn an_agent_pod_reaches_the_kubelet_unless_the_case_turns_it_off() {
        plan_agent("{enclosure: kubernetes-pod}", "", |agent| {
            assert_eq!(agent.options.get(KUBELET), Some(&true));
            assert_eq!(
                agent.field_env.get("DD_KUBERNETES_KUBELET_HOST").copied(),
                Some("status.hostIP")
            );
            assert_eq!(
                agent
                    .env
                    .get("DD_KUBELET_TLS_VERIFY")
                    .map(ToString::to_string)
                    .as_deref(),
                Some("false")
            );
        });

        plan_agent("{enclosure: kubernetes-pod, options: {kubelet: false}}", "", |agent| {
            assert_eq!(agent.options.get(KUBELET), Some(&false));
            assert!(agent.field_env.is_empty());
            assert!(!agent.env.contains_key("DD_KUBELET_TLS_VERIFY"));
        });

        // A setting the case makes itself replaces the default.
        plan_agent(
            "{enclosure: kubernetes-pod}",
            "    env: {DD_KUBERNETES_KUBELET_HOST: \"10.0.0.1\", DD_KUBELET_TLS_VERIFY: \"true\"}\n",
            |agent| {
                assert!(agent.field_env.is_empty());
                assert_eq!(
                    agent
                        .env
                        .get("DD_KUBELET_TLS_VERIFY")
                        .map(ToString::to_string)
                        .as_deref(),
                    Some("true")
                );
            },
        );

        plan_agent("{enclosure: linux-container}", "", |agent| {
            assert!(!agent.options.contains_key(KUBELET));
            assert!(agent.field_env.is_empty());
            assert!(!agent.env.contains_key("DD_KUBELET_TLS_VERIFY"));
        });
    }

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
