//! Sends a payload verbatim to a participant's named endpoint.

use std::time::Duration;

use async_trait::async_trait;
use panoramic_core::{
    case::CaseDuration,
    endpoint::EndpointOperation,
    probe::{AttemptLimits, POLL_INTERVAL},
    record::{AssertionStatus, StepReport},
    step::{decode_fields, HandleError, Need, Step, StepContext, StepFactory, StepFields, StepKind},
};
use serde::Deserialize;
use tokio::time::{sleep_until, Instant};

use crate::attempts::Attempts;

/// Sends a payload, retrying until an attempt is accepted or the timeout expires.
/// A retry after a harness error may deliver the payload twice.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Send {
    /// Required participant serving the endpoint; has no default.
    on: String,
    /// Required endpoint name; has no default.
    endpoint: String,
    /// Required payload, sent as UTF-8 bytes verbatim, including framing; has no default.
    /// YAML escapes above `\x7f` encode as UTF-8, so frame-length bytes must be below 0x80.
    payload: String,
    /// Required, positive time to wait for an accepted send; has no default.
    timeout: CaseDuration,
}

impl Send {
    /// Registry name used in case procedures.
    pub(crate) const NAME: &'static str = "send";
}

#[async_trait]
impl Step for Send {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Endpoint(
            self.on.clone(),
            self.endpoint.clone(),
            EndpointOperation::Send,
        )]
    }

    fn budget(&self) -> Duration {
        self.timeout.into()
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let deadline = started + self.budget();
        let Some(handle) = cx.handle(&self.on) else {
            return StepReport::new(
                started,
                AssertionStatus::Errored,
                format!("participant '{}' has not started", self.on),
            );
        };
        let subject = format!("endpoint '{}' of participant '{}'", self.endpoint, self.on);
        let mut attempts = Attempts::default();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match handle
                .send(&self.endpoint, self.payload.as_bytes(), &AttemptLimits::new(remaining))
                .await
            {
                Ok(()) => {
                    return StepReport::new(
                        started,
                        AssertionStatus::Passed,
                        format!("sent {} bytes to {subject}", self.payload.len()),
                    );
                }
                Err(HandleError::Product { message }) => attempts.observed(message),
                Err(HandleError::Harness { message }) => attempts.errored(message),
            }
            if Instant::now() >= deadline {
                break;
            }
            sleep_until(deadline.min(Instant::now() + POLL_INTERVAL)).await;
        }
        StepReport::new(
            started,
            attempts.status(),
            attempts.explain_expired(&format!(
                "send to {subject} was not accepted within {:?}",
                self.budget()
            )),
        )
    }
}

/// Builds `send` from its case-file fields.
pub(crate) struct SendFactory;

impl StepFactory for SendFactory {
    fn name(&self) -> &'static str {
        Send::NAME
    }

    fn kind(&self) -> StepKind {
        StepKind::Action
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(decode_fields::<Send>(fields)?))
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::step::test_support::{run_step_with, FakeParticipant};
    use serde_json::json;

    use super::*;
    use crate::test_support::valid_fields;

    #[tokio::test(start_paused = true)]
    async fn the_first_accepted_send_passes_without_retrying() {
        let fake = FakeParticipant::default();
        fake.push_send(Ok(()));
        fake.push_send(Err(HandleError::Harness {
            message: "unexpected second send".into(),
        }));
        let mut fields = valid_fields(Send::NAME);
        fields.insert("payload".into(), json!("métric:1|c\n"));
        let started = Instant::now();
        let report = run_step_with(&SendFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(
            report.message,
            "sent 12 bytes to endpoint 'udp' of participant 'subject'"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn refused_sends_and_mechanism_errors_are_retried_until_one_is_accepted() {
        let fake = FakeParticipant::default();
        fake.push_send(Err(HandleError::Product {
            message: "connection refused".into(),
        }));
        fake.push_send(Err(HandleError::Harness {
            message: "send connection lost".into(),
        }));
        fake.push_send(Ok(()));
        let started = Instant::now();
        let report = run_step_with(&SendFactory, &valid_fields(Send::NAME), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(started.elapsed(), POLL_INTERVAL * 2);
        assert_eq!(
            report.message,
            "sent 10 bytes to endpoint 'udp' of participant 'subject'"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_send_timeout_fails_after_product_refusals_and_errors_after_only_harness_errors() {
        for (answer, status, diagnostic) in [
            (
                HandleError::Product {
                    message: "connection refused".into(),
                },
                AssertionStatus::Failed,
                "connection refused",
            ),
            (
                HandleError::Harness {
                    message: "send connection lost".into(),
                },
                AssertionStatus::Errored,
                "send connection lost",
            ),
        ] {
            let fake = FakeParticipant::default();
            fake.push_send(Err(answer));
            let started = Instant::now();
            let report = run_step_with(&SendFactory, &valid_fields(Send::NAME), Some(&fake)).await;
            assert_eq!(report.status, status);
            assert_eq!(started.elapsed(), Duration::from_secs(30));
            assert_eq!(
                report.message,
                format!("send to endpoint 'udp' of participant 'subject' was not accepted within 30s; last attempt: {diagnostic}")
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_product_refusal_keeps_the_timeout_failed_after_a_later_harness_error() {
        let fake = FakeParticipant::default();
        fake.push_send(Err(HandleError::Product {
            message: "connection refused".into(),
        }));
        fake.push_send(Err(HandleError::Harness {
            message: "send connection lost".into(),
        }));
        let report = run_step_with(&SendFactory, &valid_fields(Send::NAME), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert!(
            report.message.ends_with("last attempt: send connection lost"),
            "{}",
            report.message
        );
    }

    #[test]
    fn the_send_action_requires_its_participant_endpoint_payload_and_timeout() {
        for field in ["on", "endpoint", "payload", "timeout"] {
            let mut fields = valid_fields(Send::NAME);
            fields.remove(field).unwrap();
            let message = SendFactory.build(&fields).unwrap_err();
            assert!(message.contains(&format!("missing field `{field}`")), "{message}");
        }
        assert!(SendFactory.template_fields().is_empty());
    }
}
