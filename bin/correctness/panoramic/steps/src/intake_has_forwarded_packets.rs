//! Checks that an intake participant received every expected forwarded packet, byte for byte.

use std::{fmt::Write as _, time::Duration};

use async_trait::async_trait;
use panoramic_core::{
    case::CaseDuration,
    probe::{POLL_INTERVAL, PROBE_TIMEOUT},
    record::{AssertionStatus, StepReport},
    step::{decode_fields, Need, Step, StepContext, StepFactory, StepFields},
};
use serde::Deserialize;
use serde_json::json;
use tokio::time::{sleep_until, Instant};

use crate::message::truncate_message;

const MAX_OBSERVED_PACKETS: usize = 10;

/// Waits for all expected packets to appear in one intake dump.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IntakeHasForwardedPackets {
    /// Required intake participant; has no default.
    on: String,
    /// Required, nonempty list of complete packets to match as UTF-8 bytes; has no default.
    contains: Vec<String>,
    /// Required, positive time to wait for a match; has no default.
    timeout: CaseDuration,
}

impl IntakeHasForwardedPackets {
    /// Registry name used in case procedures.
    pub(crate) const NAME: &'static str = "intake_has_forwarded_packets";

    fn expired_result(&self, started: Instant, snapshot: Option<&[Vec<u8>]>, last_error: Option<&str>) -> StepReport {
        let summary = format!(
            "Not every expected forwarded packet arrived at intake participant '{}' within {:?}.",
            self.on,
            self.budget(),
        );
        let Some(packets) = snapshot else {
            let message = match last_error {
                Some(error) => format!("{error}\n{summary}\nNo successful snapshot of forwarded packets."),
                None => format!("{summary}\nNo poll completed."),
            };
            return StepReport::new(started, AssertionStatus::Errored, truncate_message(message));
        };
        let mut message = summary;
        if let Some(error) = last_error {
            let _ = write!(message, "\nLast collection error: {error}");
        }
        message.push_str("\nMissing packets:");
        for expected in self
            .contains
            .iter()
            .filter(|expected| !packets.iter().any(|packet| packet == expected.as_bytes()))
        {
            let _ = write!(message, "\n  - {expected:?}");
        }
        let _ = write!(message, "\nObserved packets ({}):", packets.len());
        for packet in packets.iter().take(MAX_OBSERVED_PACKETS) {
            let _ = write!(message, "\n  - {:?}", String::from_utf8_lossy(packet));
        }
        if packets.len() > MAX_OBSERVED_PACKETS {
            let _ = write!(message, "\n  (+{} more)", packets.len() - MAX_OBSERVED_PACKETS);
        }
        StepReport::new(started, AssertionStatus::Failed, truncate_message(message))
    }
}

#[async_trait]
impl Step for IntakeHasForwardedPackets {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Operation(self.on.clone(), "dump".into())]
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
        let mut snapshot: Option<Vec<Vec<u8>>> = None;
        let mut last_error = None;
        let mut polled = false;
        loop {
            if polled && Instant::now() >= deadline {
                return self.expired_result(started, snapshot.as_deref(), last_error.as_deref());
            }
            polled = true;
            let remaining = deadline.saturating_duration_since(Instant::now()).min(PROBE_TIMEOUT);
            let packets = match handle
                .op("dump", json!({"kind": "dogstatsd_forwarded"}), remaining)
                .await
            {
                Ok(value) => serde_json::from_value::<Vec<Vec<u8>>>(value).map_err(|error| {
                    format!(
                        "Failed to decode forwarded packets from intake participant '{}': {error}",
                        self.on
                    )
                }),
                Err(error) => Err(error.to_string()),
            };
            match packets {
                Ok(packets) => {
                    if self
                        .contains
                        .iter()
                        .all(|expected| packets.iter().any(|packet| packet == expected.as_bytes()))
                    {
                        return StepReport::new(
                            started,
                            AssertionStatus::Passed,
                            format!(
                                "intake participant '{}' received every expected forwarded packet",
                                self.on
                            ),
                        );
                    }
                    snapshot = Some(packets);
                }
                Err(error) => last_error = Some(error),
            }
            sleep_until(deadline.min(Instant::now() + POLL_INTERVAL)).await;
        }
    }
}

/// Builds `intake_has_forwarded_packets` from its case-file fields.
pub(crate) struct IntakeHasForwardedPacketsFactory;

impl StepFactory for IntakeHasForwardedPacketsFactory {
    fn name(&self) -> &'static str {
        IntakeHasForwardedPackets::NAME
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step = decode_fields::<IntakeHasForwardedPackets>(fields)?;
        if step.contains.is_empty() {
            return Err("contains must not be empty".into());
        }
        Ok(Box::new(step))
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::step::{
        test_support::{run_step_with, FakeParticipant},
        ExecOutput, HandleError,
    };
    use serde_json::Value;

    use super::*;
    use crate::{
        message::{MAX_MESSAGE_BYTES, TRUNCATION_NOTICE},
        test_support::valid_fields,
    };

    fn dump(value: Value) -> ExecOutput {
        ExecOutput {
            stdout: serde_json::to_vec(&value).unwrap(),
            stderr: Vec::new(),
            exit_code: 0,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn every_expected_packet_must_match_a_complete_packet_in_one_dump() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(dump(json!([b"first"]))));
        fake.push_exec(Ok(dump(json!([b"second"]))));
        fake.push_exec(Ok(dump(json!([b"second".as_slice(), b"first".as_slice()]))));
        let mut fields = valid_fields(IntakeHasForwardedPackets::NAME);
        fields.insert("contains".into(), json!(["first", "second"]));
        let report = run_step_with(&IntakeHasForwardedPacketsFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(report.duration, POLL_INTERVAL * 2);
        assert_eq!(
            report.message,
            "intake participant 'subject' received every expected forwarded packet"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn prefixes_suffixes_and_substrings_are_not_packet_matches() {
        for packet in [
            b"pack".as_slice(),
            b"packet-extra",
            b"extra-packet",
            b"extra-packet-extra",
        ] {
            let fake = FakeParticipant::default();
            fake.push_exec(Ok(dump(json!([packet]))));
            let report = run_step_with(
                &IntakeHasForwardedPacketsFactory,
                &valid_fields(IntakeHasForwardedPackets::NAME),
                Some(&fake),
            )
            .await;
            assert_eq!(report.status, AssertionStatus::Failed);
            assert_eq!(report.duration, Duration::from_secs(30));
            assert!(
                report.message.contains("Missing packets:\n  - \"packet\""),
                "{}",
                report.message
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_lists_only_missing_entries_and_escapes_lossy_observed_packets() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(dump(json!([b"found".as_slice(), b"line\n\0\xff\"\\".as_slice()]))));
        let mut fields = valid_fields(IntakeHasForwardedPackets::NAME);
        fields.insert("contains".into(), json!(["found", "missing\npacket"]));
        let report = run_step_with(&IntakeHasForwardedPacketsFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(
            report.message,
            concat!(
                "Not every expected forwarded packet arrived at intake participant 'subject' within 30s.\n",
                "Missing packets:\n  - \"missing\\npacket\"\n",
                "Observed packets (2):\n  - \"found\"\n  - \"line\\n\\0\u{fffd}\\\"\\\\\"",
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn observed_packets_are_bounded_in_count_and_the_message_in_bytes() {
        for long in [false, true] {
            let packets = (0..12)
                .map(|index| {
                    if long {
                        format!("packet-{index:02}:{}", "€".repeat(2000))
                    } else {
                        format!("packet-{index:02}")
                    }
                })
                .map(String::into_bytes)
                .collect::<Vec<_>>();
            let fake = FakeParticipant::default();
            fake.push_exec(Ok(dump(json!(packets))));
            let report = run_step_with(
                &IntakeHasForwardedPacketsFactory,
                &valid_fields(IntakeHasForwardedPackets::NAME),
                Some(&fake),
            )
            .await;
            assert_eq!(report.status, AssertionStatus::Failed);
            assert!(report.message.len() <= MAX_MESSAGE_BYTES);
            if long {
                assert!(report.message.ends_with(TRUNCATION_NOTICE));
            } else {
                assert_eq!(report.message.matches("\n  - ").count(), 11);
                assert!(report.message.contains("\"packet-09\""));
                assert!(!report.message.contains("\"packet-10\""));
                assert!(report.message.ends_with("(+2 more)"));
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_without_a_snapshot_errors_with_the_last_dump_error() {
        for error in [
            HandleError::Harness {
                message: "dump mechanism failed".into(),
            },
            HandleError::Product {
                message: "dump unavailable".into(),
            },
        ] {
            let diagnostic = error.to_string();
            let fake = FakeParticipant::default();
            fake.push_exec(Err(HandleError::Product {
                message: "first error".into(),
            }));
            fake.push_exec(Err(error));
            let report = run_step_with(
                &IntakeHasForwardedPacketsFactory,
                &valid_fields(IntakeHasForwardedPackets::NAME),
                Some(&fake),
            )
            .await;
            assert_eq!(report.status, AssertionStatus::Errored);
            assert_eq!(report.duration, Duration::from_secs(30));
            assert_eq!(report.message.lines().next(), Some(diagnostic.as_str()));
            assert!(report.message.ends_with("No successful snapshot of forwarded packets."));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_packet_shapes_are_poll_errors_and_later_valid_dumps_can_match() {
        for invalid in [json!({}), json!(["packet"]), json!([[256]]), json!([[-1]])] {
            for recovers in [false, true] {
                let fake = FakeParticipant::default();
                fake.push_exec(Ok(dump(invalid.clone())));
                if recovers {
                    fake.push_exec(Ok(dump(json!([b"packet"]))));
                }
                let report = run_step_with(
                    &IntakeHasForwardedPacketsFactory,
                    &valid_fields(IntakeHasForwardedPackets::NAME),
                    Some(&fake),
                )
                .await;
                if recovers {
                    assert_eq!(report.status, AssertionStatus::Passed);
                    assert_eq!(report.duration, POLL_INTERVAL);
                } else {
                    assert_eq!(report.status, AssertionStatus::Errored);
                    assert!(
                        report
                            .message
                            .starts_with("Failed to decode forwarded packets from intake participant 'subject':"),
                        "{}",
                        report.message
                    );
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn later_errors_keep_the_last_snapshot_and_failed_status() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(dump(json!([b"first"]))));
        fake.push_exec(Ok(dump(json!([b"last"]))));
        fake.push_exec(Err(HandleError::Harness {
            message: "dump mechanism failed".into(),
        }));
        let report = run_step_with(
            &IntakeHasForwardedPacketsFactory,
            &valid_fields(IntakeHasForwardedPackets::NAME),
            Some(&fake),
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert!(report.message.contains("Last collection error: dump mechanism failed"));
        assert!(report.message.contains("\"last\""));
        assert!(!report.message.contains("\"first\""));
    }

    #[test]
    fn the_step_requires_a_participant_nonempty_string_list_and_timeout() {
        for field in ["on", "contains", "timeout"] {
            let mut fields = valid_fields(IntakeHasForwardedPackets::NAME);
            fields.remove(field).unwrap();
            let message = IntakeHasForwardedPacketsFactory.build(&fields).unwrap_err();
            assert!(message.contains(&format!("missing field `{field}`")), "{message}");
        }
        let mut fields = valid_fields(IntakeHasForwardedPackets::NAME);
        fields.insert("contains".into(), json!([]));
        assert_eq!(
            IntakeHasForwardedPacketsFactory.build(&fields).unwrap_err(),
            "contains must not be empty"
        );
        for contains in [json!("packet"), json!([true]), json!([3])] {
            fields.insert("contains".into(), contains);
            assert!(IntakeHasForwardedPacketsFactory
                .build(&fields)
                .unwrap_err()
                .starts_with("contains"));
        }
        assert!(IntakeHasForwardedPacketsFactory.template_fields().is_empty());
    }
}
