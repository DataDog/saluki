//! `agent-adp`: the Datadog Agent and ADP in one participant. In a container or pod they run as the Agent image runs
//! them under s6. On macOS they are two host programs.
//!
//! Its default environment, the rules that withdraw it, and the host view are the Agent's, in [`super::datadog_agent`],
//! with the entries that only ADP reads.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use self::dogstatsd::{Capture, Replay, Stats};
use super::datadog_agent::{
    self, ListenAt, Listener, With, DATADOG_YAML, DATADOG_YAML_PATH, DOGSTATSD_FORWARD, EVERY_OS, IMAGE, INTAKE,
    NOT_WINDOWS,
};
use crate::core::handle::{Handle, HandleError};
use crate::core::participant::{Enclosure, Os, ParticipantType, SpecContext};
use crate::core::spec::{CommandSpec, Evidence, FileSource, FileSpec, ParticipantSpec, Program, Protocol, Wiring};
use crate::core::step::{Operation, Uses};
use crate::core::template::Template;

mod dogstatsd;

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

/// ADP's command line, the same in a container and in a pod.
const ADP_COMMAND: &str = "/opt/datadog-agent/embedded/bin/agent-data-plane";

/// ADP as `make build-adp-host` builds it. A run replaces it with `--set`.
const ADP_PROGRAM: &str = "target/release/agent-data-plane";

/// The Agent as `make provision-macos-test-env` installs it. A run replaces it with `--set`.
const AGENT_PROGRAM: &str = "/tmp/saluki-dda/datadog-agent/bin/agent/agent";

/// ADP's loader needs a `datadog.yaml` to exist. A case configures ADP through `env`, so it is empty.
const EMPTY_DATADOG_YAML: FileSource = FileSource::Bytes {
    label: "empty datadog.yaml",
    bytes: b"",
};

/// What every realization of `agent-adp` offers.
const OPERATIONS: [&dyn Operation; 4] = [&ConfigDump, &Capture, &Replay, &Stats];

pub(crate) struct AgentAdp;

impl ParticipantType for AgentAdp {
    fn name(&self) -> &'static str {
        "agent-adp"
    }

    fn supports(&self) -> &'static [(Os, Enclosure)] {
        &[
            (Os::Linux, Enclosure::LinuxContainer),
            (Os::Linux, Enclosure::KubernetesPod),
            (Os::Macos, Enclosure::HostProcessGroup),
        ]
    }

    fn spec(&self, cx: &SpecContext<'_>) -> Result<ParticipantSpec, String> {
        if cx.realization.enclosure == Enclosure::HostProcessGroup {
            return host_processes(cx);
        }
        let processes = vec![
            datadog_agent::s6_process("adp", "data-plane", "adp.log")?,
            datadog_agent::s6_process("agent", "agent", "agent.log")?,
        ];
        let ready = Evidence::Output {
            pattern: Template::parse(DOGSTATSD_UDP_STARTED)?,
            process: "adp".to_string(),
        };
        let mut spec = ParticipantSpec {
            image: Some(IMAGE.to_string()),
            // No command: the image's own entrypoint starts s6.
            files: datadog_agent::s6_overlay(&["data-plane", "agent"]),
            ready: BTreeMap::from([("dogstatsd_udp".to_string(), ready)]),
            processes: Some(processes),
            aliases: BTreeMap::from([(DATADOG_YAML, DATADOG_YAML_PATH)]),
            consumes: BTreeMap::from([("intake", INTAKE), ("dogstatsd_forward", DOGSTATSD_FORWARD)]),
            commands: BTreeMap::from([(
                "adp".to_string(),
                CommandSpec {
                    program: None,
                    argv: vec![Template::parse(ADP_COMMAND)?],
                    env: BTreeMap::new(),
                },
            )]),
            operations: &OPERATIONS,
            ..ParticipantSpec::default()
        };
        add_endpoints(cx, &mut spec)?;
        datadog_agent::set_defaults(cx, true, &mut spec)?;
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

/// Adds the endpoints the realization's OS has. A case that gives the HTTP receiver a certificate makes it serve TLS.
fn add_endpoints(cx: &SpecContext<'_>, spec: &mut ParticipantSpec) -> Result<(), String> {
    datadog_agent::add_endpoints(cx, LISTENERS, spec)?;
    let certificate = datadog_agent::case_sets(
        cx,
        "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE",
        "otlp_config.receiver.protocols.http.tls.cert_file",
    )?;
    if let Some(endpoint) = spec.endpoints.get_mut("otlp_http").filter(|_| certificate) {
        endpoint.tls = true;
    }
    Ok(())
}

/// The spec on macOS: the Agent, then ADP, as two programs in the participant's directory. ADP does not wait for the
/// Agent's IPC credentials.
fn host_processes(cx: &SpecContext<'_>) -> Result<ParticipantSpec, String> {
    let with: With = cx.with()?;
    if with.host_view {
        return Err(
            "with.host_view mounts host paths into a container, and host processes already see the host".to_string(),
        );
    }
    if let Some(key) = cx.case.env.keys().find(|key| key.starts_with("ADP_DD_")) {
        return Err(format!(
            "{} is set. On macOS ADP and the Agent read the same environment: write the DD_ form instead",
            key
        ));
    }
    let own_dir = Template::parse("{{ self.dir }}")?;
    let mut agent_env = BTreeMap::from([("DD_RUN_PATH".to_string(), own_dir.clone())]);
    // The Agent's darwin guard ignores `data_plane.enabled`, so only the force flag hands over to ADP.
    if cx
        .case
        .env
        .get("DD_DATA_PLANE_ENABLED")
        .is_some_and(|value| value.text() == "true")
    {
        agent_env.insert("DD_DATA_PLANE_FORCE_ENABLE".to_string(), Template::parse("true")?);
    }
    let programs = vec![
        Program {
            name: "agent".to_string(),
            path: AGENT_PROGRAM.to_string(),
            args: vec![Template::parse("run")?, Template::parse("-c")?, own_dir],
            env: agent_env,
        },
        Program {
            name: "adp".to_string(),
            path: ADP_PROGRAM.to_string(),
            args: vec![
                Template::parse("-c")?,
                Template::parse("{{ self.dir }}/datadog.yaml")?,
                Template::parse("run")?,
            ],
            env: BTreeMap::new(),
        },
    ];
    let ready = Evidence::Output {
        pattern: Template::parse(DOGSTATSD_UDP_STARTED)?,
        process: "adp".to_string(),
    };
    let mut spec = ParticipantSpec {
        programs: Some(programs),
        // The Agent streams its own token path to ADP, so both must name the one the harness owns. Otherwise ADP follows
        // the platform default, whose certificate is not the one this Agent serves.
        env: BTreeMap::from([(
            "DD_AUTH_TOKEN_FILE_PATH".to_string(),
            Template::parse("{{ self.dir }}/auth_token")?,
        )]),
        files: BTreeMap::from([(
            "./datadog.yaml".to_string(),
            FileSpec {
                source: EMPTY_DATADOG_YAML,
                mode: None,
                supervises: false,
            },
        )]),
        ready: BTreeMap::from([("dogstatsd_udp".to_string(), ready)]),
        commands: BTreeMap::from([(
            "adp".to_string(),
            CommandSpec {
                program: Some("adp".to_string()),
                argv: vec![Template::parse("-c")?, Template::parse("{{ self.dir }}/datadog.yaml")?],
                env: BTreeMap::new(),
            },
        )]),
        operations: &OPERATIONS,
        ..ParticipantSpec::default()
    };
    add_endpoints(cx, &mut spec)?;
    datadog_agent::set_defaults(cx, true, &mut spec)?;
    Ok(spec)
}

/// Which of ADP's two configuration views `config_dump` answers with.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum View {
    /// The configuration as ADP read it from its sources.
    Source,
    /// The configuration ADP runs with, after Remote Configuration changed it.
    Runtime,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigDumpArgs {
    view: View,
}

/// `config_dump`: ADP's configuration, as JSON, from its `config --json` command. Its argument is `{view: source |
/// runtime}`.
#[derive(Debug)]
struct ConfigDump;

#[async_trait]
impl Operation for ConfigDump {
    fn name(&self) -> &'static str {
        "config_dump"
    }

    fn uses(&self) -> &'static [Uses] {
        &[Uses::Command("adp")]
    }

    async fn run(&self, on: &Handle<'_>, args: Value, limit: Duration) -> Result<Value, HandleError> {
        let ConfigDumpArgs { view } = serde_json::from_value(args)
            .map_err(|e| HandleError::Harness(format!("config_dump takes {{view: source | runtime}}: {}", e)))?;
        let mut argv = vec!["config".to_string(), "--json".to_string()];
        if matches!(view, View::Runtime) {
            argv.push("--runtime".to_string());
        }
        let exited = on.command("adp", &argv, &BTreeMap::new(), limit).await?;
        // The configuration holds the API key, so no message here quotes the command's output.
        if exited.code != 0 {
            return Err(HandleError::Software(format!(
                "agent-data-plane config exited with code {}",
                exited.code
            )));
        }
        serde_json::from_slice(&exited.stdout)
            .map_err(|e| HandleError::Software(format!("the output of agent-data-plane config is not JSON: {}", e)))
    }
}
