//! `send`: bytes written to a participant's endpoint, as a peer would write them.

use std::num::NonZeroU32;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::Deserialize;
use tokio::time::Instant;

use super::stopped;
use crate::core::case::Timeout;
use crate::core::handle::HandleError;
use crate::core::record::StepStatus;
use crate::core::step::{EndpointOperation, Need, Step, StepContext, StepFactory, StepFields, StepPurpose, StepReport};

pub(super) struct SendFactory;

impl StepFactory for SendFactory {
    fn name(&self) -> &'static str {
        "send"
    }

    fn purpose(&self) -> StepPurpose {
        StepPurpose::Action
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(fields.decode::<SendBytes>()?))
    }
}

/// Sends `payload` to the endpoint `repeat` times, one datagram or one connection each, within `timeout`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendBytes {
    on: String,
    endpoint: String,
    payload: String,
    #[serde(default = "once")]
    repeat: NonZeroU32,
    timeout: Timeout,
}

fn once() -> NonZeroU32 {
    NonZeroU32::MIN
}

#[async_trait]
impl Step for SendBytes {
    fn name(&self) -> &'static str {
        "send"
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Endpoint(
            self.on.clone(),
            self.endpoint.clone(),
            EndpointOperation::Send,
        )]
    }

    fn budget(&self) -> Duration {
        self.timeout.duration
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let sent = async {
            let handle = cx.handle(&self.on)?;
            let payload = Bytes::from(self.payload.clone());
            for _ in 0..self.repeat.get() {
                let limit = cx.deadline.saturating_duration_since(Instant::now());
                handle.send(&self.endpoint, payload.clone(), limit).await?;
            }
            let message = format!(
                "Sent {} bytes to endpoint '{}' of '{}' {} time(s).",
                payload.len(),
                self.endpoint,
                self.on,
                self.repeat
            );
            Ok::<_, HandleError>(StepReport {
                status: StepStatus::Passed,
                message,
            })
        };
        sent.await.unwrap_or_else(stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::super::scripted::{observation, run, Scripted};
    use super::*;

    #[tokio::test]
    async fn a_send_errors_when_the_courier_fails() {
        let fields = "{on: app, endpoint: statsd, payload: 'app.count:1|c', timeout: 5s}";
        let failing = Scripted {
            exit: Some((2, "send: cannot resolve 'app'")),
            ..Scripted::default()
        };
        let report = run(&SendFactory, fields, &failing, &observation()).await;
        assert_eq!(report.status, StepStatus::Errored, "{}", report.message);
        assert!(report.message.contains("exited with code 2"), "{}", report.message);
    }
}
