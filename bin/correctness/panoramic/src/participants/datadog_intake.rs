//! `datadog-intake`: the mock Datadog intake, which records what the Agent and ADP send it.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::core::participant::{Enclosure, Os, ParticipantType, SpecContext};
use crate::core::spec::{Address, EndpointSpec, Evidence, ParticipantSpec, Protocol};
use crate::core::template::Template;

/// The tools image the `Makefile` builds, and the intake's path in it.
const IMAGE: &str = "saluki-images/correctness-tools:latest";
const PROGRAM: &str = "/usr/local/bin/datadog-intake";

/// The intake takes no `with` parameters.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct With {}

pub(crate) struct DatadogIntake;

impl ParticipantType for DatadogIntake {
    fn name(&self) -> &'static str {
        "datadog-intake"
    }

    fn supports(&self) -> &'static [(Os, Enclosure)] {
        &[(Os::Linux, Enclosure::LinuxContainer)]
    }

    fn spec(&self, cx: &SpecContext<'_>) -> Result<ParticipantSpec, String> {
        let With {} = cx.with()?;
        let endpoint = |protocol, port| EndpointSpec {
            protocol,
            address: Address::Port(port),
            tls: false,
        };
        Ok(ParticipantSpec {
            image: Some(IMAGE.to_string()),
            command: Some(vec![Template::parse(PROGRAM)?]),
            env: BTreeMap::from([
                (
                    "INTAKE_HTTP_LISTEN_ADDR".to_string(),
                    Template::parse("0.0.0.0:{{ self.endpoints.api.port }}")?,
                ),
                (
                    "INTAKE_DOGSTATSD_LISTEN_ADDR".to_string(),
                    Template::parse("0.0.0.0:{{ self.endpoints.dogstatsd_forward.port }}")?,
                ),
            ]),
            endpoints: BTreeMap::from([
                ("api".to_string(), endpoint(Protocol::Http, 2049)),
                ("dogstatsd_forward".to_string(), endpoint(Protocol::Udp, 9125)),
            ]),
            ready: BTreeMap::from([(
                "api".to_string(),
                Evidence::Http {
                    path: "/ready".to_string(),
                },
            )]),
            ..ParticipantSpec::default()
        })
    }
}
