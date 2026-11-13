//! `datadog-intake`: the mock Datadog intake, which records what the Agent and ADP send it.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core::handle::{Handle, HandleError};
use crate::core::participant::{Enclosure, Os, ParticipantType, SpecContext};
use crate::core::spec::{Address, EndpointSpec, Evidence, ParticipantSpec, Protocol};
use crate::core::step::{Operation, Uses};
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
            operations: &[&Dump],
            ..ParticipantSpec::default()
        })
    }
}

/// `dump`: what the intake holds, as `{"metrics": [...]}`, each kind as the intake's own dump endpoint answers it.
// TODO: dump the other kinds the intake holds with the first steps that read them.
#[derive(Debug)]
struct Dump;

#[async_trait]
impl Operation for Dump {
    fn name(&self) -> &'static str {
        "dump"
    }

    fn uses(&self) -> &'static [Uses] {
        &[Uses::Request("api")]
    }

    async fn run(&self, on: &Handle<'_>, args: Value, limit: Duration) -> Result<Value, HandleError> {
        if !args.is_null() {
            return Err(HandleError::Harness(format!("dump takes no arguments, not {}", args)));
        }
        let path = "/metrics/dump";
        let response = on.http_get("api", path, limit).await?;
        if response.status != 200 {
            return Err(HandleError::Software(format!(
                "the intake answered {} with status {}: {}",
                path,
                response.status,
                String::from_utf8_lossy(&response.body)
            )));
        }
        let metrics: Value = serde_json::from_slice(&response.body)
            .map_err(|e| HandleError::Software(format!("the intake's answer to {} is not JSON: {}", path, e)))?;
        Ok(json!({ "metrics": metrics }))
    }
}
