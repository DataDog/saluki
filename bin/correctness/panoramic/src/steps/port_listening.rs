//! `port_listening`: an endpoint of a participant accepts connections.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::time::{sleep_until, Instant};

use super::{stopped, ATTEMPT, POLL};
use crate::core::case::Timeout;
use crate::core::handle::HandleError;
use crate::core::record::StepStatus;
use crate::core::step::{EndpointOperation, Need, Step, StepContext, StepFactory, StepFields, StepReport};

pub(super) struct PortListeningFactory;

impl StepFactory for PortListeningFactory {
    fn name(&self) -> &'static str {
        "port_listening"
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(fields.decode::<PortListening>()?))
    }
}

/// Passes once the endpoint accepts a connection, or its socket exists, within `timeout`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortListening {
    on: String,
    endpoint: String,
    timeout: Timeout,
}

#[async_trait]
impl Step for PortListening {
    fn name(&self) -> &'static str {
        "port_listening"
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Endpoint(
            self.on.clone(),
            self.endpoint.clone(),
            EndpointOperation::Probe,
        )]
    }

    fn budget(&self) -> Duration {
        self.timeout.duration
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let probed = async {
            let handle = cx.handle(&self.on)?;
            let last = loop {
                let limit = ATTEMPT.min(cx.deadline.saturating_duration_since(Instant::now()));
                let refused = match handle.probe(&self.endpoint, limit).await {
                    Ok(()) => {
                        let message = format!("Endpoint '{}' of '{}' accepts connections.", self.endpoint, self.on);
                        return Ok(StepReport {
                            status: StepStatus::Passed,
                            message,
                        });
                    }
                    Err(HandleError::Software(message)) => message,
                    Err(e) => return Err(e),
                };
                if Instant::now() >= cx.deadline {
                    break refused;
                }
                sleep_until((Instant::now() + POLL).min(cx.deadline)).await;
            };
            let message = format!(
                "Endpoint '{}' of '{}' did not accept a connection within {}: {}",
                self.endpoint, self.on, self.timeout.text, last
            );
            Ok::<_, HandleError>(StepReport {
                status: StepStatus::Failed,
                message,
            })
        };
        probed.await.unwrap_or_else(stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::super::scripted::{observation, run, Scripted};
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_probe_that_is_refused_fails() {
        let fields = "{on: app, endpoint: api, timeout: 1s}";
        let refused = Scripted {
            exit: Some((1, "probe: connection refused")),
            ..Scripted::default()
        };
        let report = run(&PortListeningFactory, fields, &refused, &observation()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(report.message.contains("connection refused"), "{}", report.message);
    }
}
