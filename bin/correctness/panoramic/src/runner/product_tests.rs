//! Real product images exercised through the native runner.

use std::{collections::BTreeMap, time::Duration};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use panoramic_core::{
    endpoint::EndpointOperation,
    probe::{AttemptLimits, POLL_INTERVAL},
    record::{AssertionStatus, StepReport},
    step::{test_support, Need, Step, StepContext, StepFactory, StepFields},
};
use panoramic_participants::{AgentAdp, TOOLS_LINUX_IMAGE};
use serde_json::json;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::test_support::run_native_case;
use crate::intake::IntakeClient;

const BUDGET: Duration = Duration::from_secs(30);
const PACKET: &[u8] = b"panoramic.test:1|c";

#[derive(Debug)]
struct Forward;

#[async_trait]
impl Step for Forward {
    fn name(&self) -> &'static str {
        "forward"
    }

    fn needs(&self) -> Vec<Need> {
        vec![
            Need::Endpoint("first".into(), "dogstatsd_forward".into(), EndpointOperation::Send),
            Need::Endpoint("first".into(), "api".into(), EndpointOperation::Request),
            Need::Endpoint("second".into(), "api".into(), EndpointOperation::Request),
        ]
    }

    fn budget(&self) -> Duration {
        BUDGET
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let result = tokio::time::timeout(BUDGET, async {
            let client = reqwest::Client::new();
            let mut intakes = Vec::new();
            for name in ["first", "second"] {
                let port = test_support::runner_host_port(cx, name, "api", EndpointOperation::Request)
                    .ok_or_else(|| format!("{name}.api has no runner-host port"))?;
                loop {
                    let limits = AttemptLimits::new(BUDGET.saturating_sub(started.elapsed()));
                    let response = client
                        .get(format!("http://127.0.0.1:{port}/ready"))
                        .timeout(limits.attempt)
                        .send()
                        .await;
                    if response.is_ok_and(|response| response.status() == reqwest::StatusCode::OK) {
                        break;
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                intakes.push(IntakeClient::new(port));
            }
            test_support::send(cx, "first", "dogstatsd_forward", PACKET).await?;
            loop {
                let limits = AttemptLimits::new(BUDGET.saturating_sub(started.elapsed()));
                let packets = tokio::time::timeout(limits.attempt, async {
                    let first = intakes[0].dogstatsd_forwarded_packets().await?;
                    let second = intakes[1].dogstatsd_forwarded_packets().await?;
                    Ok::<_, saluki_error::GenericError>((first, second))
                })
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())?;
                if !packets.1.is_empty() {
                    return Ok((
                        AssertionStatus::Failed,
                        format!("second intake received packets: {:?}", packets.1),
                    ));
                }
                if packets.0 == [STANDARD.encode(PACKET)] {
                    return Ok((
                        AssertionStatus::Passed,
                        "only the first intake received the packet".into(),
                    ));
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        })
        .await;
        let (status, message) = match result {
            Ok(Ok(result)) => result,
            Ok(Err(message)) => (AssertionStatus::Errored, message),
            Err(_) => (
                AssertionStatus::Failed,
                "intakes did not become ready and retain the expected packets".into(),
            ),
        };
        StepReport::new(started, status, message)
    }
}

struct ForwardFactory;

impl StepFactory for ForwardFactory {
    fn name(&self) -> &'static str {
        "forward"
    }

    fn build(&self, _: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(Forward))
    }
}

#[tokio::test]
#[ignore = "needs product images; run with make test-panoramic-products"]
async fn two_intakes_are_ready_independently_and_keep_their_own_forwarded_packets() {
    let _ = crate::default_crypto_provider().install_default();
    let bindings = [
        (AgentAdp::LINUX_IMAGE, "PANORAMIC_AGENT_ADP_LINUX_IMAGE"),
        (TOOLS_LINUX_IMAGE, "PANORAMIC_TOOLS_LINUX_IMAGE"),
    ]
    .into_iter()
    .map(|(role, key)| {
        (
            role.name.into(),
            std::env::var(key).unwrap_or_else(|_| role.local_default.into()),
        )
    })
    .collect::<BTreeMap<_, _>>();
    let (_logs, report) = run_native_case(
        panoramic_participants::REGISTERED_TYPES,
        &[&ForwardFactory],
        json!({
            "timeout": "90s", "subject_os": ["linux"],
            "participants": {
                "first": {"type": "datadog-intake", "subject": true},
                "second": {"type": "datadog-intake"},
            },
            "procedure": [{"step": "forward"}],
        }),
        bindings,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(report["outcome"], "passed", "{report:#}");
    assert_eq!(report["assertions"][0]["status"], "passed");
    let participants = report["participants"].as_array().unwrap();
    assert_eq!(participants.len(), 2);
    for (participant, name) in participants.iter().zip(["first", "second"]) {
        assert_eq!(participant["name"], name);
        assert_eq!(participant["type"], "datadog-intake");
        assert_eq!(participant["processes"].as_array().unwrap().len(), 1);
        let process = &participant["processes"][0];
        assert_eq!(process["name"], "intake");
        assert_eq!(process["exit"]["state"], "running");
        assert_eq!(process["exit"]["source"], "container");
    }
}
