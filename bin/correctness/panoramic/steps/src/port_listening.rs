//! Probes a named endpoint until it listens or the timeout expires.

use std::time::Duration;

use async_trait::async_trait;
use panoramic_core::{
    case::CaseDuration,
    endpoint::EndpointOperation,
    probe::{AttemptLimits, ProbeAnswer, POLL_INTERVAL},
    record::{AssertionStatus, StepReport},
    step::{decode_fields, HandleError, Need, Step, StepContext, StepFactory, StepFields},
};
use serde::Deserialize;
use tokio::time::{sleep_until, Instant};

use crate::attempts::Attempts;

/// Waits until an endpoint listens, or is addressable over UDP.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortListening {
    /// Required participant serving the endpoint.
    on: String,
    /// Required endpoint name declared by the participant.
    endpoint: String,
    /// Required, positive time to wait for the endpoint.
    timeout: CaseDuration,
}

impl PortListening {
    /// Registry name used in case procedures.
    pub(crate) const NAME: &'static str = "port_listening";
}

#[async_trait]
impl Step for PortListening {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Endpoint(
            self.on.clone(),
            self.endpoint.clone(),
            EndpointOperation::Probe,
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
            match handle.probe(&self.endpoint, &AttemptLimits::new(remaining)).await {
                Ok(ProbeAnswer::Listening) => {
                    return StepReport::new(started, AssertionStatus::Passed, format!("{subject} is listening"));
                }
                Ok(ProbeAnswer::Addressable) => {
                    return StepReport::new(
                        started,
                        AssertionStatus::Passed,
                        format!("{subject} is addressable over UDP; listener availability is unconfirmed"),
                    );
                }
                Ok(ProbeAnswer::NotListening { reason }) => attempts.observed(reason),
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
            attempts.explain_expired(&format!("{subject} did not listen within {:?}", self.budget())),
        )
    }
}

/// Builds `port_listening` from its case-file fields.
pub(crate) struct PortListeningFactory;

impl StepFactory for PortListeningFactory {
    fn name(&self) -> &'static str {
        PortListening::NAME
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(decode_fields::<PortListening>(fields)?))
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::step::test_support::{run_step_with, FakeParticipant};
    use serde_json::Value;

    use super::*;
    use crate::test_support::{exited, valid_fields};

    #[tokio::test(start_paused = true)]
    async fn a_listening_endpoint_and_an_addressable_udp_endpoint_pass() {
        for (endpoint, answer, expected) in [
            ("tcp", ProbeAnswer::Listening, "is listening"),
            (
                "udp",
                ProbeAnswer::Addressable,
                "is addressable over UDP; listener availability is unconfirmed",
            ),
        ] {
            let fake = FakeParticipant::default();
            fake.push_probe(Ok(answer));
            let mut fields = valid_fields(PortListening::NAME);
            fields.insert("endpoint".into(), Value::String(endpoint.into()));
            let started = Instant::now();
            let report = run_step_with(&PortListeningFactory, &fields, Some(&fake)).await;
            assert_eq!(report.status, AssertionStatus::Passed);
            assert_eq!(started.elapsed(), Duration::ZERO);
            assert_eq!(
                report.message,
                format!("endpoint '{endpoint}' of participant 'subject' {expected}")
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_unsuccessful_probe_is_retried_until_the_endpoint_listens() {
        let fake = FakeParticipant::default();
        fake.push_probe(Ok(ProbeAnswer::NotListening {
            reason: "connection refused".into(),
        }));
        fake.push_probe(Err(HandleError::Product {
            message: "connection timed out".into(),
        }));
        fake.push_probe(Err(HandleError::Harness {
            message: "probe connection lost".into(),
        }));
        fake.push_probe(Ok(ProbeAnswer::Listening));
        let started = Instant::now();
        let report = run_step_with(&PortListeningFactory, &valid_fields(PortListening::NAME), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(started.elapsed(), POLL_INTERVAL * 3);
        assert_eq!(report.message, "endpoint 'tcp' of participant 'subject' is listening");
    }

    #[tokio::test(start_paused = true)]
    async fn a_port_timeout_fails_after_product_answers_and_errors_after_only_harness_errors() {
        for (answer, status, expected) in [
            (
                Ok(ProbeAnswer::NotListening {
                    reason: "connection refused".into(),
                }),
                AssertionStatus::Failed,
                "connection refused",
            ),
            (
                Err(HandleError::Product {
                    message: "connection timed out".into(),
                }),
                AssertionStatus::Failed,
                "connection timed out",
            ),
            (
                Err(HandleError::Harness {
                    message: "probe connection lost".into(),
                }),
                AssertionStatus::Errored,
                "probe connection lost",
            ),
        ] {
            let fake = FakeParticipant::default();
            fake.push_probe(answer);
            let started = Instant::now();
            let report = run_step_with(&PortListeningFactory, &valid_fields(PortListening::NAME), Some(&fake)).await;
            assert_eq!(report.status, status);
            assert_eq!(started.elapsed(), Duration::from_secs(30));
            assert_eq!(
                report.message,
                format!("endpoint 'tcp' of participant 'subject' did not listen within 30s; last attempt: {expected}")
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_product_answer_keeps_the_timeout_failed_even_when_the_last_attempt_is_a_harness_error() {
        let fake = FakeParticipant::default();
        fake.push_probe(Ok(ProbeAnswer::NotListening {
            reason: "connection refused".into(),
        }));
        fake.push_probe(Err(HandleError::Harness {
            message: "probe connection lost".into(),
        }));
        let started = Instant::now();
        let report = run_step_with(&PortListeningFactory, &valid_fields(PortListening::NAME), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert!(
            report.message.ends_with("last attempt: probe connection lost"),
            "{}",
            report.message
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_process_exit_does_not_end_port_retries_early() {
        let fake = FakeParticipant::default();
        fake.set_exit("main", exited(Some(1), None));
        fake.set_exit("worker", exited(Some(1), None));
        fake.complete();
        fake.push_probe(Ok(ProbeAnswer::NotListening {
            reason: "connection refused".into(),
        }));
        let started = Instant::now();
        let report = run_step_with(&PortListeningFactory, &valid_fields(PortListening::NAME), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert!(report.message.ends_with("last attempt: connection refused"));
    }

    #[test]
    fn the_port_step_requires_its_participant_endpoint_and_timeout_and_needs_a_probe_route() {
        for field in ["on", "endpoint", "timeout"] {
            let mut fields = valid_fields(PortListening::NAME);
            fields.remove(field).unwrap();
            let message = PortListeningFactory.build(&fields).unwrap_err();
            assert!(message.contains(&format!("missing field `{field}`")), "{message}");
        }
        let step = PortListeningFactory.build(&valid_fields(PortListening::NAME)).unwrap();
        assert_eq!(
            step.needs(),
            vec![Need::Endpoint("subject".into(), "tcp".into(), EndpointOperation::Probe)]
        );
    }
}
