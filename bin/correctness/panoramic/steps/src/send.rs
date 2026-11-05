//! Sends a payload verbatim to a participant's named endpoint.

use std::{num::NonZeroUsize, time::Duration};

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
    /// Positive number of payload copies to send in one message; defaults to 1.
    #[serde(default = "default_repeat")]
    repeat: NonZeroUsize,
    /// Required, positive time to wait for an accepted send; has no default.
    timeout: CaseDuration,
}

fn default_repeat() -> NonZeroUsize {
    NonZeroUsize::MIN
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
        let payload = self.payload.repeat(self.repeat.get());
        let subject = format!("endpoint '{}' of participant '{}'", self.endpoint, self.on);
        let mut attempts = Attempts::default();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match handle
                .send(&self.endpoint, payload.as_bytes(), &AttemptLimits::new(remaining))
                .await
            {
                Ok(diagnostic) => {
                    let mut message = format!("sent {} bytes to {subject}", payload.len());
                    if !diagnostic.is_empty() {
                        message.push_str("; ");
                        message.push_str(&diagnostic);
                    }
                    return StepReport::new(started, AssertionStatus::Passed, message);
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
        fake.push_send(Ok(String::new()));
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
        fake.assert_sent(&["métric:1|c\n".as_bytes()]);
    }

    #[tokio::test(start_paused = true)]
    async fn send_reports_transport_diagnostics_on_success_and_failure() {
        // Keep the selected UDP destination visible even when a later assertion fails the case.
        let diagnostic = "udp://subject:8125: resolved addresses [[::1]:8125, 127.0.0.1:8125]; selected 127.0.0.1:8125";
        for (answer, status) in [
            (Ok(diagnostic.into()), AssertionStatus::Passed),
            (
                Err(HandleError::Product {
                    message: format!("{diagnostic}; send failed: message too long"),
                }),
                AssertionStatus::Failed,
            ),
        ] {
            let fake = FakeParticipant::default();
            fake.push_send(answer);
            let report = run_step_with(&SendFactory, &valid_fields(Send::NAME), Some(&fake)).await;
            assert_eq!(report.status, status);
            assert!(report.message.contains(diagnostic), "{}", report.message);
        }
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
        fake.push_send(Ok(String::new()));
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

    #[tokio::test(start_paused = true)]
    async fn yaml_frame_prefixes_below_0x80_nuls_and_newlines_reach_send_unchanged() {
        for (payload, expected) in [
            (
                r#""\x3c\0\0\0adp.forwarding.stream_one:1|c\nadp.forwarding.stream_two:2|c\n""#,
                b"\x3c\0\0\0adp.forwarding.stream_one:1|c\nadp.forwarding.stream_two:2|c\n".as_slice(),
            ),
            (
                r#""\x1e\0\0\0adp.forwarding.uds_stream:1|c\n""#,
                b"\x1e\0\0\0adp.forwarding.uds_stream:1|c\n".as_slice(),
            ),
        ] {
            let fields =
                serde_saphyr::from_str(&format!("on: subject\nendpoint: tcp\npayload: {payload}\ntimeout: 30s"))
                    .unwrap();
            let fake = FakeParticipant::default();
            fake.push_send(Ok(String::new()));
            let report = run_step_with(&SendFactory, &fields, Some(&fake)).await;
            assert_eq!(report.status, AssertionStatus::Passed);
            fake.assert_sent(&[expected]);
            assert_eq!(
                u32::from_le_bytes(expected[..4].try_into().unwrap()) as usize,
                expected.len() - 4
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn send_repeats_its_payload_in_one_message_and_reports_the_total_bytes() {
        for (payload, repeat) in [("métric:1|c\n", 3), ("x", 10000), ("", 2)] {
            let fake = FakeParticipant::default();
            fake.push_send(Ok(String::new()));
            let mut fields = valid_fields(Send::NAME);
            fields.insert("payload".into(), json!(payload));
            fields.insert("repeat".into(), json!(repeat));
            let report = run_step_with(&SendFactory, &fields, Some(&fake)).await;
            assert_eq!(report.status, AssertionStatus::Passed);
            let expected = payload.repeat(repeat);
            fake.assert_sent(&[expected.as_bytes()]);
            assert_eq!(
                report.message,
                format!(
                    "sent {} bytes to endpoint 'udp' of participant 'subject'",
                    expected.len()
                )
            );
        }
    }

    #[test]
    fn send_rejects_zero_negative_and_noninteger_repeat_counts() {
        for repeat in [json!(0), json!(-1), json!(1.5), json!("2")] {
            let mut fields = valid_fields(Send::NAME);
            fields.insert("repeat".into(), repeat);
            let error = SendFactory.build(&fields).unwrap_err();
            assert!(error.starts_with("repeat:"), "{error}");
        }
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
