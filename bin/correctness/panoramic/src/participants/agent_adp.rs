//! `agent-adp`: the Datadog Agent and ADP in one participant, as the Agent image runs them under s6.
//!
//! Its default environment, the rules that withdraw it, and the host view are the Agent's, in [`super::datadog_agent`],
//! with the entries that only ADP reads.

use std::collections::BTreeMap;

use super::datadog_agent::{
    self, ListenAt, Listener, DATADOG_YAML, DATADOG_YAML_PATH, EVERY_OS, IMAGE, INTAKE, NOT_WINDOWS,
};
use crate::core::participant::{Enclosure, Os, ParticipantType, SpecContext};
use crate::core::spec::{Evidence, ParticipantSpec, Protocol, Wiring};
use crate::core::template::Template;

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

pub(crate) struct AgentAdp;

impl ParticipantType for AgentAdp {
    fn name(&self) -> &'static str {
        "agent-adp"
    }

    fn supports(&self) -> &'static [(Os, Enclosure)] {
        &[(Os::Linux, Enclosure::LinuxContainer)]
    }

    fn spec(&self, cx: &SpecContext<'_>) -> Result<ParticipantSpec, String> {
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
            consumes: BTreeMap::from([("intake", INTAKE)]),
            ..ParticipantSpec::default()
        };
        datadog_agent::add_endpoints(cx, LISTENERS, &mut spec)?;
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
