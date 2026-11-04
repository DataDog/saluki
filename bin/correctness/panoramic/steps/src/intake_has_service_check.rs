//! Polls an intake participant for one service check matching every configured criterion.

use std::{collections::BTreeSet, fmt::Write as _, time::Duration};

use async_trait::async_trait;
use panoramic_core::{
    case::CaseDuration,
    probe::{POLL_INTERVAL, PROBE_TIMEOUT},
    record::{AssertionStatus, StepReport},
    step::{decode_fields, Need, Step, StepContext, StepFactory, StepFields},
};
use serde::Deserialize;
use serde_json::json;
use stele::ServiceCheck;
use tokio::time::{sleep_until, Instant};

use crate::message::truncate_message;

const MAX_DIAGNOSTIC_ENTRIES: usize = 10;

/// Waits for one service check to meet every criterion.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IntakeHasServiceCheck {
    /// Required intake participant; has no default.
    on: String,
    /// Required exact service check name; has no default.
    name: String,
    /// Expected status; omitted accepts every status.
    status: Option<u8>,
    /// Expected exact hostname; omitted accepts every hostname.
    hostname: Option<String>,
    /// Tags the check must include, allowing extras. Defaults to empty.
    #[serde(default)]
    tags: Vec<String>,
    /// Required, positive time to wait for a match; has no default.
    timeout: CaseDuration,
}

impl IntakeHasServiceCheck {
    /// Registry name used in case procedures.
    pub(crate) const NAME: &'static str = "intake_has_service_check";

    /// Checks status, hostname, then tags for a check with the expected name.
    fn mismatch(&self, check: &ServiceCheck) -> Option<String> {
        if let Some(status) = self.status.filter(|&status| status != check.status()) {
            return Some(format!("status: expected {status}, observed {}", check.status()));
        }
        if let Some(hostname) = self.hostname.as_ref().filter(|hostname| *hostname != check.hostname()) {
            return Some(format!(
                "hostname: expected {hostname:?}, observed {:?}",
                check.hostname()
            ));
        }
        let missing = self
            .tags
            .iter()
            .filter(|tag| !check.tags().contains(tag))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Some(format!("tags: missing {missing:?}"));
        }
        None
    }

    fn expired_result(
        &self, started: Instant, snapshot: Option<&[ServiceCheck]>, last_error: Option<&str>,
    ) -> StepReport {
        let summary = format!(
            "No service check named {:?} matched at intake participant '{}' within {:?}.",
            self.name,
            self.on,
            self.budget(),
        );
        let Some(checks) = snapshot else {
            let message = match last_error {
                Some(error) => format!("{error}\n{summary}\nNo successful service checks snapshot."),
                None => format!("{summary}\nNo poll completed."),
            };
            return StepReport::new(started, AssertionStatus::Errored, truncate_message(message));
        };
        let mut message = summary;
        if let Some(error) = last_error {
            let _ = write!(message, "\nLast collection error: {error}");
        }
        let candidates = checks
            .iter()
            .filter(|check| check.name() == self.name)
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            let _ = write!(message, "\nNo check named {:?} observed.", self.name);
            let names = checks.iter().map(ServiceCheck::name).collect::<BTreeSet<_>>();
            if !names.is_empty() {
                let _ = write!(message, "\nOther service check names ({}):", names.len());
                for name in names.iter().take(MAX_DIAGNOSTIC_ENTRIES) {
                    let _ = write!(message, "\n  - {name:?}");
                }
                if names.len() > MAX_DIAGNOSTIC_ENTRIES {
                    let _ = write!(message, "\n  (+{} more)", names.len() - MAX_DIAGNOSTIC_ENTRIES);
                }
            }
        } else {
            let _ = write!(message, "\nCandidates named {:?} ({}):", self.name, candidates.len());
            for check in candidates.iter().take(MAX_DIAGNOSTIC_ENTRIES) {
                if let Some(reason) = self.mismatch(check) {
                    let _ = write!(
                        message,
                        "\n  - status={}, hostname={:?}, tags={:?}; fails {reason}",
                        check.status(),
                        check.hostname(),
                        check.tags(),
                    );
                }
            }
            if candidates.len() > MAX_DIAGNOSTIC_ENTRIES {
                let _ = write!(message, "\n  (+{} more)", candidates.len() - MAX_DIAGNOSTIC_ENTRIES);
            }
        }
        StepReport::new(started, AssertionStatus::Failed, truncate_message(message))
    }
}

#[async_trait]
impl Step for IntakeHasServiceCheck {
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
                truncate_message(format!("participant '{}' has not started", self.on)),
            );
        };
        let mut snapshot = None;
        let mut last_error = None;
        let mut polled = false;
        loop {
            if polled && Instant::now() >= deadline {
                return self.expired_result(started, snapshot.as_deref(), last_error.as_deref());
            }
            polled = true;
            let limit = deadline.saturating_duration_since(Instant::now()).min(PROBE_TIMEOUT);
            let checks = match handle.op("dump", json!({"kind": "service_checks"}), limit).await {
                Ok(value) => serde_json::from_value::<Vec<ServiceCheck>>(value).map_err(|error| {
                    format!(
                        "Failed to decode service checks from intake participant '{}': {error}",
                        self.on
                    )
                }),
                Err(error) => Err(error.to_string()),
            };
            match checks {
                Ok(checks) => {
                    if checks
                        .iter()
                        .any(|check| check.name() == self.name && self.mismatch(check).is_none())
                    {
                        return StepReport::new(
                            started,
                            AssertionStatus::Passed,
                            truncate_message(format!(
                                "intake participant '{}' received a service check named {:?} matching every criterion",
                                self.on, self.name,
                            )),
                        );
                    }
                    snapshot = Some(checks);
                }
                Err(error) => last_error = Some(error),
            }
            sleep_until(deadline.min(Instant::now() + POLL_INTERVAL)).await;
        }
    }
}

/// Builds `intake_has_service_check` from its case-file fields.
pub(crate) struct IntakeHasServiceCheckFactory;

impl StepFactory for IntakeHasServiceCheckFactory {
    fn name(&self) -> &'static str {
        IntakeHasServiceCheck::NAME
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(decode_fields::<IntakeHasServiceCheck>(fields)?))
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::step::{
        test_support::{run_step_with, FakeParticipant},
        ExecCommand, ExecOutput, HandleError,
    };
    use serde_json::Value;

    use super::*;
    use crate::{
        message::{MAX_MESSAGE_BYTES, TRUNCATION_NOTICE},
        test_support::valid_fields,
    };

    fn check(name: &str, status: u8, hostname: &str, tags: &[&str]) -> ServiceCheck {
        ServiceCheck::from_check_run(
            name.into(),
            status,
            hostname.into(),
            "check message".into(),
            tags.iter().map(|tag| (*tag).into()).collect(),
            Some(1),
        )
    }

    fn dump(value: Value) -> ExecOutput {
        ExecOutput {
            stdout: serde_json::to_vec(&value).unwrap(),
            stderr: Vec::new(),
            exit_code: 0,
        }
    }

    fn dump_call(limit: Duration) -> (ExecCommand, Duration) {
        (
            ExecCommand {
                argv: vec!["dump".into(), "service_checks".into()],
                env: Vec::new(),
                stdin: None,
            },
            limit,
        )
    }

    fn all_criteria() -> StepFields {
        let mut fields = valid_fields(IntakeHasServiceCheck::NAME);
        fields.extend(
            serde_saphyr::from_str::<StepFields>(
                "status: 0\nhostname: expected-host\ntags: ['env:test', 'source:fixture']",
            )
            .unwrap(),
        );
        fields
    }

    #[tokio::test(start_paused = true)]
    async fn intake_has_service_check_passes_when_one_check_meets_every_criterion() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(dump(json!([]))));
        fake.push_exec(Ok(dump(json!([
            check("other.check", 0, "expected-host", &["env:test", "source:fixture"]),
            check("some.check", 1, "expected-host", &["env:test", "source:fixture"]),
            check(
                "some.check",
                0,
                "expected-host",
                &["env:test", "extra:tag", "source:fixture"]
            ),
        ]))));
        let report = run_step_with(&IntakeHasServiceCheckFactory, &all_criteria(), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(report.duration, POLL_INTERVAL);
        assert_eq!(
            report.message,
            "intake participant 'subject' received a service check named \"some.check\" matching every criterion"
        );
        fake.assert_executed(&[dump_call(PROBE_TIMEOUT), dump_call(PROBE_TIMEOUT)]);
    }

    #[tokio::test(start_paused = true)]
    async fn intake_has_service_check_does_not_combine_checks() {
        for checks in [
            vec![
                check("some.check", 0, "wrong-host", &["env:test", "source:fixture"]),
                check("some.check", 1, "expected-host", &["env:test", "source:fixture"]),
                check("other.check", 0, "expected-host", &["env:test", "source:fixture"]),
            ],
            vec![
                check("some.check", 0, "expected-host", &["env:test"]),
                check("some.check", 0, "expected-host", &["source:fixture"]),
            ],
        ] {
            let fake = FakeParticipant::default();
            fake.push_exec(Ok(dump(json!(checks))));
            let report = run_step_with(&IntakeHasServiceCheckFactory, &all_criteria(), Some(&fake)).await;
            assert_eq!(report.status, AssertionStatus::Failed);
            assert_eq!(report.duration, Duration::from_secs(30));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn omitted_criteria_allow_any_status_hostname_and_tags() {
        for status in [0, 255] {
            let fake = FakeParticipant::default();
            fake.push_exec(Ok(dump(json!([check(
                "some.check",
                status,
                "any-host",
                &["extra:tag"]
            )]))));
            let mut fields = valid_fields(IntakeHasServiceCheck::NAME);
            let report = run_step_with(&IntakeHasServiceCheckFactory, &fields, Some(&fake)).await;
            assert_eq!(report.status, AssertionStatus::Passed);
            fields.insert("status".into(), json!(status));
            fields.insert("tags".into(), json!(["extra:tag", "extra:tag"]));
            let report = run_step_with(&IntakeHasServiceCheckFactory, &fields, Some(&fake)).await;
            assert_eq!(report.status, AssertionStatus::Passed);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn names_hostnames_and_tags_match_exactly() {
        for (name, hostname, tags, status) in [
            (
                "some.check.extra",
                "expected-host",
                vec!["env:test", "source:fixture"],
                AssertionStatus::Failed,
            ),
            (
                "some.chec",
                "expected-host",
                vec!["env:test", "source:fixture"],
                AssertionStatus::Failed,
            ),
            (
                "some.check",
                "expected-host.extra",
                vec!["env:test", "source:fixture"],
                AssertionStatus::Failed,
            ),
            (
                "some.check",
                "wrong-host",
                vec!["env:test", "source:fixture", "host:expected-host"],
                AssertionStatus::Failed,
            ),
            (
                "some.check",
                "expected-host",
                vec!["env:testing", "source:fixture"],
                AssertionStatus::Failed,
            ),
            (
                "some.check",
                "expected-host",
                vec!["env:test", "source:fixture"],
                AssertionStatus::Passed,
            ),
        ] {
            let fake = FakeParticipant::default();
            fake.push_exec(Ok(dump(json!([check(name, 0, hostname, &tags)]))));
            let report = run_step_with(&IntakeHasServiceCheckFactory, &all_criteria(), Some(&fake)).await;
            assert_eq!(report.status, status, "{name}, {hostname}, {tags:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn intake_has_service_check_fails_listing_candidates_after_a_snapshot() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(dump(json!([
            check("some.check", 1, "wrong-host", &[]),
            check("some.check", 0, "wrong-host", &[]),
            check("some.check", 0, "expected-host", &["env:test"]),
            check("other.check", 0, "expected-host", &["env:test", "source:fixture"]),
        ]))));
        let report = run_step_with(&IntakeHasServiceCheckFactory, &all_criteria(), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(report.duration, Duration::from_secs(30));
        assert_eq!(report.message, concat!(
            "No service check named \"some.check\" matched at intake participant 'subject' within 30s.\n",
            "Candidates named \"some.check\" (3):\n",
            "  - status=1, hostname=\"wrong-host\", tags=[]; fails status: expected 0, observed 1\n",
            "  - status=0, hostname=\"wrong-host\", tags=[]; fails hostname: expected \"expected-host\", observed \"wrong-host\"\n",
            "  - status=0, hostname=\"expected-host\", tags=[\"env:test\"]; fails tags: missing [\"source:fixture\"]",
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn intake_has_service_check_errors_without_any_snapshot() {
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
                &IntakeHasServiceCheckFactory,
                &valid_fields(IntakeHasServiceCheck::NAME),
                Some(&fake),
            )
            .await;
            assert_eq!(report.status, AssertionStatus::Errored);
            assert_eq!(report.duration, Duration::from_secs(30));
            assert_eq!(report.message.lines().next(), Some(diagnostic.as_str()));
            assert!(!report.message.contains("first error"));
            assert!(report.message.ends_with("No successful service checks snapshot."));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn malformed_dumps_are_retried_and_a_later_snapshot_can_pass() {
        let valid = serde_json::to_value(check("some.check", 0, "", &[])).unwrap();
        let mut missing_status = valid.clone();
        missing_status.as_object_mut().unwrap().remove("status");
        let mut invalid_status = valid.clone();
        invalid_status["status"] = json!(256);
        for invalid in [
            json!({}),
            json!(null),
            json!([missing_status]),
            json!([invalid_status]),
            json!([valid.clone(), {}]),
        ] {
            for recovers in [false, true] {
                let fake = FakeParticipant::default();
                fake.push_exec(Ok(dump(invalid.clone())));
                if recovers {
                    fake.push_exec(Ok(dump(json!([valid]))));
                }
                let report = run_step_with(
                    &IntakeHasServiceCheckFactory,
                    &valid_fields(IntakeHasServiceCheck::NAME),
                    Some(&fake),
                )
                .await;
                if recovers {
                    assert_eq!(report.status, AssertionStatus::Passed);
                    assert_eq!(report.duration, POLL_INTERVAL);
                } else {
                    assert_eq!(report.status, AssertionStatus::Errored);
                    assert!(report
                        .message
                        .starts_with("Failed to decode service checks from intake participant 'subject':"));
                    assert_eq!(report.duration, Duration::from_secs(30));
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_snapshot_fails_even_after_exit_and_later_dump_errors() {
        let fake = FakeParticipant::default();
        fake.complete();
        fake.push_exec(Ok(dump(json!([]))));
        fake.push_exec(Err(HandleError::Harness {
            message: "dump mechanism failed".into(),
        }));
        let report = run_step_with(
            &IntakeHasServiceCheckFactory,
            &valid_fields(IntakeHasServiceCheck::NAME),
            Some(&fake),
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(report.duration, Duration::from_secs(30));
        assert!(report.message.contains("Last collection error: dump mechanism failed"));
        assert!(report.message.ends_with("No check named \"some.check\" observed."));
    }

    #[tokio::test(start_paused = true)]
    async fn polling_after_exit_can_find_a_later_match() {
        let fake = FakeParticipant::default();
        fake.complete();
        fake.push_exec(Err(HandleError::Product {
            message: "dump unavailable".into(),
        }));
        fake.push_exec(Ok(dump(json!([check("some.check", 0, "", &[])]))));
        let report = run_step_with(
            &IntakeHasServiceCheckFactory,
            &valid_fields(IntakeHasServiceCheck::NAME),
            Some(&fake),
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(report.duration, POLL_INTERVAL);
    }

    #[tokio::test(start_paused = true)]
    async fn later_errors_preserve_only_the_last_decoded_snapshot() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(dump(json!([check("first.check", 0, "", &[])]))));
        fake.push_exec(Ok(dump(json!([check("last.check", 0, "", &[])]))));
        fake.push_exec(Ok(dump(json!({}))));
        let report = run_step_with(
            &IntakeHasServiceCheckFactory,
            &valid_fields(IntakeHasServiceCheck::NAME),
            Some(&fake),
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert!(report
            .message
            .contains("Last collection error: Failed to decode service checks"));
        assert!(report.message.contains("last.check"));
        assert!(!report.message.contains("first.check"));
    }

    #[tokio::test(start_paused = true)]
    async fn candidates_and_distinct_other_names_are_limited_to_ten() {
        for candidates in [false, true] {
            let checks = (0..12)
                .map(|index| {
                    let name = if candidates {
                        "some.check".into()
                    } else {
                        format!("other.{index:02}")
                    };
                    check(&name, 1, &format!("host-{index:02}"), &[])
                })
                .collect::<Vec<_>>();
            let fake = FakeParticipant::default();
            fake.push_exec(Ok(dump(json!([checks.clone(), checks].concat()))));
            let report = run_step_with(&IntakeHasServiceCheckFactory, &all_criteria(), Some(&fake)).await;
            assert_eq!(report.status, AssertionStatus::Failed);
            assert_eq!(report.message.matches("\n  - ").count(), 10);
            if candidates {
                assert!(report.message.contains("Candidates named \"some.check\" (24):"));
                assert!(report.message.contains("host-09"));
                assert!(!report.message.contains("host-10"));
                assert!(report.message.ends_with("(+14 more)"));
            } else {
                assert!(report.message.contains("Other service check names (12):"));
                assert!(report.message.contains("other.09"));
                assert!(!report.message.contains("other.10"));
                assert!(report.message.ends_with("(+2 more)"));
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn long_success_failure_and_error_messages_are_truncated_on_character_boundaries() {
        let long = "€".repeat(2000);
        for (fields, answer, status) in [
            (
                all_criteria(),
                Ok(dump(json!([check("some.check", 1, &long, &[])]))),
                AssertionStatus::Failed,
            ),
            (
                valid_fields(IntakeHasServiceCheck::NAME),
                Ok(dump(json!([check(&long, 0, "", &[])]))),
                AssertionStatus::Failed,
            ),
            (
                valid_fields(IntakeHasServiceCheck::NAME),
                Err(HandleError::Harness { message: long.clone() }),
                AssertionStatus::Errored,
            ),
            (
                {
                    let mut fields = valid_fields(IntakeHasServiceCheck::NAME);
                    fields.insert("name".into(), json!(long));
                    fields
                },
                Ok(dump(json!([check(&long, 0, "", &[])]))),
                AssertionStatus::Passed,
            ),
        ] {
            let fake = FakeParticipant::default();
            fake.push_exec(answer);
            let report = run_step_with(&IntakeHasServiceCheckFactory, &fields, Some(&fake)).await;
            assert_eq!(report.status, status);
            assert!(report.message.len() <= MAX_MESSAGE_BYTES);
            assert!(report.message.ends_with(TRUNCATION_NOTICE));
            assert!(report.message.contains('€'));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn each_dump_limit_is_capped_by_the_probe_timeout_and_remaining_budget() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(dump(json!([]))));
        let mut fields = valid_fields(IntakeHasServiceCheck::NAME);
        fields.insert("timeout".into(), json!("11s"));
        let report = run_step_with(&IntakeHasServiceCheckFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(report.duration, Duration::from_secs(11));
        let calls = (0..22)
            .map(|poll| dump_call((Duration::from_secs(11) - POLL_INTERVAL * poll).min(PROBE_TIMEOUT)))
            .collect::<Vec<_>>();
        fake.assert_executed(&calls);
    }

    #[tokio::test(start_paused = true)]
    async fn a_sub_poll_interval_budget_still_polls_once_and_never_oversleeps() {
        for matches in [false, true] {
            let fake = FakeParticipant::default();
            fake.push_exec(Ok(dump(if matches {
                json!([check("some.check", 0, "", &[])])
            } else {
                json!([])
            })));
            let mut fields = valid_fields(IntakeHasServiceCheck::NAME);
            fields.insert("timeout".into(), json!("1ms"));
            let report = run_step_with(&IntakeHasServiceCheckFactory, &fields, Some(&fake)).await;
            assert_eq!(
                report.status,
                if matches {
                    AssertionStatus::Passed
                } else {
                    AssertionStatus::Failed
                }
            );
            assert_eq!(
                report.duration,
                if matches {
                    Duration::ZERO
                } else {
                    Duration::from_millis(1)
                }
            );
            fake.assert_executed(&[dump_call(Duration::from_millis(1))]);
        }
    }

    #[test]
    fn service_check_fields_require_a_participant_name_and_positive_timeout() {
        for field in ["on", "name", "timeout"] {
            let mut fields = valid_fields(IntakeHasServiceCheck::NAME);
            fields.remove(field).unwrap();
            let message = IntakeHasServiceCheckFactory.build(&fields).unwrap_err();
            assert!(message.contains(&format!("missing field `{field}`")), "{message}");
        }
        for (field, invalid) in [
            ("on", vec![json!(null), json!([]), json!({})]),
            ("name", vec![json!(null), json!([]), json!({})]),
            ("hostname", vec![json!([]), json!({})]),
            (
                "status",
                vec![
                    json!(-1),
                    json!(256),
                    json!(1.5),
                    json!("0"),
                    json!(true),
                    json!([]),
                    json!({}),
                ],
            ),
            (
                "tags",
                vec![
                    json!(null),
                    json!("env:test"),
                    json!({}),
                    json!([true]),
                    json!([3]),
                    json!([null]),
                ],
            ),
            (
                "timeout",
                vec![
                    json!(null),
                    json!(""),
                    json!("0s"),
                    json!("-1s"),
                    json!("30"),
                    json!("invalid"),
                ],
            ),
        ] {
            for value in invalid {
                let mut fields = valid_fields(IntakeHasServiceCheck::NAME);
                fields.insert(field.into(), value.clone());
                let message = IntakeHasServiceCheckFactory.build(&fields).unwrap_err();
                assert!(message.starts_with(field), "{field}={value}: {message}");
            }
        }
        assert!(IntakeHasServiceCheckFactory.template_fields().is_empty());
    }
}
