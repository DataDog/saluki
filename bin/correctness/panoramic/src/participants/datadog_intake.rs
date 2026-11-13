//! `datadog-intake`: the mock Datadog intake, which records what the Agent and ADP send it.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::time::Instant;

use crate::core::handle::{Handle, HandleError};
use crate::core::participant::{Enclosure, Os, ParticipantType, SpecContext};
use crate::core::spec::{Address, EndpointSpec, Evidence, ParticipantSpec, Protocol, DEFAULT_PROCESS};
use crate::core::step::{Operation, Uses};
use crate::core::template::Template;

/// The tools image the `Makefile` builds, and the intake's path in it.
const IMAGE: &str = "saluki-images/correctness-tools:latest";
const PROGRAM: &str = "/usr/local/bin/datadog-intake";

/// The intake's line once its socket for forwarded DogStatsD packets is bound.
const FORWARD_CAPTURE_STARTED: &str = "DogStatsD forwarding capture started: listening on";

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
            ready: BTreeMap::from([
                (
                    "api".to_string(),
                    Evidence::Http {
                        path: "/ready".to_string(),
                    },
                ),
                (
                    "dogstatsd_forward".to_string(),
                    Evidence::Output {
                        pattern: Template::parse(FORWARD_CAPTURE_STARTED)?,
                        process: DEFAULT_PROCESS.to_string(),
                    },
                ),
            ]),
            operations: &[&Dump],
            ..ParticipantSpec::default()
        })
    }
}

/// The intake's own dump endpoint for each key of `dump`'s answer, named as `CollectedData` names its fields.
const DUMP_PATHS: [(&str, &str); 7] = [
    ("events", "/events/dump"),
    ("log_requests", "/logs/dump"),
    ("metrics", "/metrics/dump"),
    ("service_checks", "/service_checks/dump"),
    ("spans", "/traces/dump_spans"),
    ("trace_stats", "/traces/dump_stats"),
    ("dogstatsd_forwarded_packets", "/dogstatsd-forwarding/dump"),
];

/// `dump`: what the intake holds, as an object with a key per [`DUMP_PATHS`] row, each as the intake's own dump
/// endpoint answers it. Its argument is the list of keys to answer, and without one it answers them all.
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
        let keys: Option<Vec<String>> = serde_json::from_value(args.clone())
            .map_err(|_| HandleError::Harness(format!("dump takes a list of keys, not {}", args)))?;
        if let Some(unknown) = keys
            .iter()
            .flatten()
            .find(|key| !DUMP_PATHS.iter().any(|(known, _)| known == key))
        {
            return Err(HandleError::Harness(format!("dump has no key '{}'", unknown)));
        }
        let asked = |key: &str| keys.as_ref().is_none_or(|keys| keys.iter().any(|asked| asked == key));
        let deadline = Instant::now() + limit;
        let mut dump = Map::new();
        for (key, path) in DUMP_PATHS.into_iter().filter(|(key, _)| asked(key)) {
            let limit = deadline.saturating_duration_since(Instant::now());
            let response = on.http_get("api", path, limit).await?;
            if response.status != 200 {
                return Err(HandleError::Software(format!(
                    "the intake answered {} with status {}: {}",
                    path,
                    response.status,
                    String::from_utf8_lossy(&response.body)
                )));
            }
            let value: Value = serde_json::from_slice(&response.body)
                .map_err(|e| HandleError::Software(format!("the intake's answer to {} is not JSON: {}", path, e)))?;
            dump.insert(key.to_string(), value);
        }
        Ok(Value::Object(dump))
    }
}
