//! `agent`: the Datadog Agent alone, as the Agent image runs it under s6, with no data plane. A comparison case runs
//! it as the baseline.
//!
//! It takes the Agent's default environment and the rules that withdraw it from [`super::datadog_agent`], without the
//! entries only ADP reads, so both sides of a comparison set the same Agent keys. It keeps the Agent's own ports for
//! DogStatsD and APM, because it runs only in a container of its own.

use std::collections::BTreeMap;

use super::datadog_agent::{
    self, ListenAt, Listener, DATADOG_YAML, DATADOG_YAML_PATH, EVERY_OS, IMAGE, INTAKE, NOT_WINDOWS,
};
use crate::core::participant::{Enclosure, Os, ParticipantType, SpecContext};
use crate::core::spec::{Evidence, ParticipantSpec, Protocol, Wiring};
use crate::core::template::Template;

/// The endpoint of the Agent's embedded collector, which its own `otel-config.yaml` sets.
const OTEL_OTLP_GRPC: &str = "otel_otlp_grpc";

const LISTENERS: &[Listener] = &[
    Listener {
        name: "dogstatsd_udp",
        protocol: Protocol::Udp,
        address: ListenAt::Port(8125),
        on: EVERY_OS,
        opt_in: None,
    },
    Listener {
        name: "apm_tcp",
        protocol: Protocol::Http,
        address: ListenAt::Port(8126),
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
        name: OTEL_OTLP_GRPC,
        protocol: Protocol::Grpc,
        address: ListenAt::Port(4317),
        on: EVERY_OS,
        opt_in: None,
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
];

/// The Agent's line once its DogStatsD UDP listener is bound. It names the address it binds, whose interface depends
/// on the Agent's configuration, so the line is matched without it. The Agent opens one UDP listener.
const DOGSTATSD_UDP_STARTED: &str = "dogstatsd-udp: starting to listen on";

pub(crate) struct Agent;

impl ParticipantType for Agent {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn supports(&self) -> &'static [(Os, Enclosure)] {
        &[(Os::Linux, Enclosure::LinuxContainer)]
    }

    fn spec(&self, cx: &SpecContext<'_>) -> Result<ParticipantSpec, String> {
        let ready = Evidence::Output {
            pattern: Template::parse(DOGSTATSD_UDP_STARTED)?,
            process: "agent".to_string(),
        };
        let mut spec = ParticipantSpec {
            image: Some(IMAGE.to_string()),
            // No command: the image's own entrypoint starts s6.
            files: datadog_agent::s6_overlay(&["agent"]),
            ready: BTreeMap::from([("dogstatsd_udp".to_string(), ready)]),
            processes: Some(vec![datadog_agent::s6_process("agent", "agent", "agent.log")?]),
            aliases: BTreeMap::from([(DATADOG_YAML, DATADOG_YAML_PATH)]),
            consumes: BTreeMap::from([("intake", INTAKE)]),
            ..ParticipantSpec::default()
        };
        datadog_agent::add_endpoints(cx, LISTENERS, &mut spec)?;
        let wiring = Wiring {
            withdrawn: None,
            write: "endpoint: \"0.0.0.0:{{ self.endpoints.otel_otlp_grpc.port }}\" under \
                    receivers.otlp.protocols.grpc in /etc/datadog-agent/otel-config.yaml, placed with template: true"
                .to_string(),
            overridden: None,
        };
        spec.wiring.insert(OTEL_OTLP_GRPC.to_string(), wiring);
        datadog_agent::set_defaults(cx, false, &mut spec)?;
        Ok(spec)
    }
}
