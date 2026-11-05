//! Captures endpoint traffic and checks its replay through participant operations.

use std::time::Duration;

use async_trait::async_trait;
use panoramic_core::{
    case::CaseDuration,
    endpoint::EndpointOperation,
    probe::AttemptLimits,
    record::{AssertionStatus, StepReport},
    step::{decode_fields, HandleError, Need, Step, StepContext, StepFactory, StepFields, StepKind},
};
use serde::Deserialize;
use serde_json::json;
use tokio::time::{sleep, Instant};

const REPLAY_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    data: String,
    after: Option<CaseDuration>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DogstatsdReplay {
    on: String,
    endpoint: String,
    payloads: Vec<Payload>,
    capture_duration: CaseDuration,
    stats_duration: CaseDuration,
    expected_metrics: Vec<String>,
    timeout: CaseDuration,
    #[serde(skip)]
    budget: Duration,
}

#[derive(Deserialize)]
struct Statistics {
    metrics: Vec<String>,
}

impl DogstatsdReplay {
    pub(crate) const NAME: &'static str = "dogstatsd_replay";

    fn stats_failed(
        &self, started: Instant, error: HandleError, replay_ran: bool, last_missing_metrics: &[&str],
    ) -> StepReport {
        let mut report = operation_failure(started, "Failed to collect DogStatsD replay statistics", error);
        if replay_ran {
            report.status = AssertionStatus::Failed;
        }
        if !last_missing_metrics.is_empty() {
            report.message.push_str(&format!(
                "\nAn earlier replay's statistics did not contain expected metric(s): {}",
                last_missing_metrics.join(", ")
            ));
        }
        report
    }
}

fn operation_failure(started: Instant, summary: &str, error: HandleError) -> StepReport {
    let status = match error {
        HandleError::Product { .. } => AssertionStatus::Failed,
        HandleError::Harness { .. } => AssertionStatus::Errored,
    };
    StepReport::new(started, status, format!("{summary}: {error}"))
}

#[async_trait]
impl Step for DogstatsdReplay {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn needs(&self) -> Vec<Need> {
        ["dogstatsd_capture", "dogstatsd_replay", "dogstatsd_stats"]
            .into_iter()
            .map(|operation| Need::Operation(self.on.clone(), operation.into()))
            .chain(std::iter::once(Need::Endpoint(
                self.on.clone(),
                self.endpoint.clone(),
                EndpointOperation::Send,
            )))
            .collect()
    }

    fn budget(&self) -> Duration {
        self.budget
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let Some(handle) = cx.handle(&self.on) else {
            return StepReport::new(
                started,
                AssertionStatus::Errored,
                format!("participant '{}' has not started", self.on),
            );
        };
        let timeout: Duration = self.timeout.into();
        let capture_duration: Duration = self.capture_duration.into();
        let capture = handle
            .op(
                "dogstatsd_capture",
                json!({"duration_ms": capture_duration.as_millis() as u64}),
                timeout,
            )
            .await;
        let capture = match capture {
            Ok(capture) => capture,
            Err(error) => return operation_failure(started, "Failed to start DogStatsD capture", error),
        };
        let Some(path) = capture
            .get("path")
            .and_then(|path| path.as_str())
            .filter(|path| !path.is_empty())
        else {
            return StepReport::new(
                started,
                AssertionStatus::Errored,
                "dogstatsd_capture returned no nonempty path",
            );
        };
        for payload in &self.payloads {
            if let Some(after) = payload.after {
                sleep(after.into()).await;
            }
            if let Err(error) = handle
                .send(&self.endpoint, payload.data.as_bytes(), &AttemptLimits::new(timeout))
                .await
            {
                return operation_failure(started, "Failed to send DogStatsD traffic for capture", error);
            }
        }
        // The capture writer appends its state trailer only after the requested duration.
        sleep(capture_duration).await;
        let stats_args = json!({"seconds": Duration::from(self.stats_duration).as_secs()});
        let replay_args = json!({"path": path});
        let deadline = Instant::now() + timeout;
        let mut last_replay_error = None;
        let mut last_missing_metrics = Vec::new();
        let mut replay_ran = false;
        loop {
            let (stats, replay) = tokio::join!(
                handle.op("dogstatsd_stats", stats_args.clone(), timeout),
                handle.op("dogstatsd_replay", replay_args.clone(), timeout),
            );
            match replay {
                Ok(_) => match stats.and_then(|value| {
                    serde_json::from_value::<Statistics>(value).map_err(|_| HandleError::Harness {
                        message: "dogstatsd_stats returned no list of metric names".into(),
                    })
                }) {
                    Ok(stats) => {
                        replay_ran = true;
                        last_missing_metrics = self
                            .expected_metrics
                            .iter()
                            .map(String::as_str)
                            .filter(|metric| !stats.metrics.iter().any(|observed| observed == metric))
                            .collect();
                        if last_missing_metrics.is_empty() {
                            return StepReport::new(
                                started,
                                AssertionStatus::Passed,
                                "Captured and replayed DogStatsD traffic with every expected metric",
                            );
                        }
                    }
                    Err(error) => return self.stats_failed(started, error, replay_ran, &last_missing_metrics),
                },
                Err(error) => {
                    replay_ran |= matches!(error, HandleError::Product { .. });
                    last_replay_error = Some(error);
                }
            }
            if Instant::now() >= deadline {
                let status = if replay_ran {
                    AssertionStatus::Failed
                } else {
                    AssertionStatus::Errored
                };
                let message = match last_replay_error {
                    Some(error) => format!("Failed to replay DogStatsD capture: {error}"),
                    None => format!(
                        "DogStatsD replay statistics did not contain expected metric(s): {}",
                        last_missing_metrics.join(", ")
                    ),
                };
                return StepReport::new(started, status, message);
            }
            sleep(REPLAY_INTERVAL).await;
        }
    }
}

pub(crate) struct DogstatsdReplayFactory;

impl StepFactory for DogstatsdReplayFactory {
    fn name(&self) -> &'static str {
        DogstatsdReplay::NAME
    }

    fn kind(&self) -> StepKind {
        StepKind::Action
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let mut step: DogstatsdReplay = decode_fields(fields)?;
        if step.payloads.is_empty() {
            return Err("payloads must not be empty".into());
        }
        if step.expected_metrics.is_empty() {
            return Err("expected_metrics must not be empty".into());
        }
        let stats: Duration = step.stats_duration.into();
        if stats.as_secs() < 1 || stats.subsec_nanos() != 0 {
            return Err("stats_duration must be a whole number of seconds, at least 1s".into());
        }
        let capture: Duration = step.capture_duration.into();
        if capture.as_millis() > u64::MAX as u128 {
            return Err("capture_duration is too large in milliseconds".into());
        }
        let budget_error = "replay budget is too large";
        let commands = u32::try_from(step.payloads.len())
            .ok()
            .and_then(|count| count.checked_add(3))
            .ok_or(budget_error)?;
        step.budget = Duration::from(step.timeout)
            .checked_mul(commands)
            .and_then(|budget| budget.checked_add(capture))
            .and_then(|budget| budget.checked_add(REPLAY_INTERVAL))
            .ok_or(budget_error)?;
        for payload in &step.payloads {
            if let Some(after) = payload.after {
                step.budget = step.budget.checked_add(after.into()).ok_or(budget_error)?;
            }
        }
        Ok(Box::new(step))
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::step::{
        test_support::{run_step_with, FakeParticipant},
        ExecCommand,
    };
    use serde_json::Value;

    use super::*;
    use crate::test_support::{output, valid_fields, OPERATIONS};

    fn command(name: &str, args: Value, limit: Duration) -> (ExecCommand, Duration) {
        (
            ExecCommand {
                argv: vec![name.into(), args.to_string()],
                env: vec![],
                stdin: None,
            },
            limit,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn replay_captures_sends_after_each_delay_and_retries_until_every_metric_appears() {
        let fake = FakeParticipant::default().with_operations(OPERATIONS);
        for value in [
            json!({"path": "/capture"}),
            json!({"metrics": ["one"]}),
            json!({}),
            json!({"metrics": ["one", "two"]}),
            json!({}),
        ] {
            fake.push_exec(Ok(output(value)));
        }
        fake.push_send(Ok(()));
        let mut fields = valid_fields(DogstatsdReplay::NAME);
        fields.insert(
            "payloads".into(),
            json!([{"data": "one:1|c", "after": "200ms"}, {"data": "two:1|c", "after": "1s"}]),
        );
        fields.insert("capture_duration".into(), "3s".into());
        fields.insert("stats_duration".into(), "3s".into());
        fields.insert("expected_metrics".into(), json!(["one", "two"]));
        let started = Instant::now();
        let (report, ()) = tokio::join!(run_step_with(&DogstatsdReplayFactory, &fields, Some(&fake)), async {
            sleep(Duration::from_millis(199)).await;
            fake.assert_sent(&[]);
            sleep(Duration::from_millis(2)).await;
            fake.assert_sent(&[b"one:1|c"]);
            sleep(Duration::from_secs(1)).await;
            fake.assert_sent(&[b"one:1|c", b"two:1|c"]);
            fake.assert_executed(&[command(
                "dogstatsd_capture",
                json!({"duration_ms": 3000}),
                Duration::from_secs(30),
            )]);
        });
        assert_eq!(report.status, AssertionStatus::Passed, "{report:?}");
        assert_eq!(started.elapsed(), Duration::from_millis(4250));
        let limit = Duration::from_secs(30);
        fake.assert_executed(&[
            command("dogstatsd_capture", json!({"duration_ms": 3000}), limit),
            command("dogstatsd_stats", json!({"seconds": 3}), limit),
            command("dogstatsd_replay", json!({"path": "/capture"}), limit),
            command("dogstatsd_stats", json!({"seconds": 3}), limit),
            command("dogstatsd_replay", json!({"path": "/capture"}), limit),
        ]);
    }

    #[tokio::test(start_paused = true)]
    async fn capture_and_send_product_errors_fail_and_harness_errors_error_without_retrying() {
        for capture in [true, false] {
            for (error, status) in [
                (
                    HandleError::Product {
                        message: "refused".into(),
                    },
                    AssertionStatus::Failed,
                ),
                (
                    HandleError::Harness {
                        message: "unavailable".into(),
                    },
                    AssertionStatus::Errored,
                ),
            ] {
                let fake = FakeParticipant::default().with_operations(OPERATIONS);
                if capture {
                    fake.push_exec(Err(error.clone()));
                } else {
                    fake.push_exec(Ok(output(json!({"path": "/capture"}))));
                    fake.push_send(Err(error.clone()));
                }
                let report = run_step_with(
                    &DogstatsdReplayFactory,
                    &valid_fields(DogstatsdReplay::NAME),
                    Some(&fake),
                )
                .await;
                assert_eq!(report.status, status);
                assert!(report.message.ends_with(&error.to_string()), "{report:?}");
                fake.assert_executed(&[command(
                    "dogstatsd_capture",
                    json!({"duration_ms": 1000}),
                    Duration::from_secs(30),
                )]);
                if capture {
                    fake.assert_sent(&[]);
                } else {
                    fake.assert_sent(&[b"packet"]);
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn statistics_errors_keep_an_earlier_replays_failure() {
        // A later statistics mechanism error must not erase metrics missing from an earlier completed replay.
        for earlier_replay in [false, true] {
            for (error, status) in [
                (
                    HandleError::Harness {
                        message: "statistics unavailable".into(),
                    },
                    if earlier_replay {
                        AssertionStatus::Failed
                    } else {
                        AssertionStatus::Errored
                    },
                ),
                (
                    HandleError::Product {
                        message: "statistics refused".into(),
                    },
                    AssertionStatus::Failed,
                ),
            ] {
                let fake = FakeParticipant::default().with_operations(OPERATIONS);
                fake.push_exec(Ok(output(json!({"path": "/capture"}))));
                if earlier_replay {
                    fake.push_exec(Ok(output(json!({"metrics": []}))));
                    fake.push_exec(Ok(output(json!({}))));
                }
                fake.push_exec(Err(error.clone()));
                fake.push_exec(Ok(output(json!({}))));
                fake.push_send(Ok(()));
                let report = run_step_with(
                    &DogstatsdReplayFactory,
                    &valid_fields(DogstatsdReplay::NAME),
                    Some(&fake),
                )
                .await;
                assert_eq!(report.status, status, "{report:?}");
                assert!(report
                    .message
                    .contains(&format!("Failed to collect DogStatsD replay statistics: {error}")));
                assert_eq!(
                    report
                        .message
                        .contains("An earlier replay's statistics did not contain expected metric(s): metric"),
                    earlier_replay
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn replay_reports_only_missing_metrics_at_its_deadline() {
        let fake = FakeParticipant::default().with_operations(OPERATIONS);
        fake.push_exec(Ok(output(json!({"path": "/capture"}))));
        for _ in 0..3 {
            fake.push_exec(Ok(output(json!({"metrics": ["metric"]}))));
            fake.push_exec(Ok(output(json!({}))));
        }
        fake.push_send(Ok(()));
        let mut fields = valid_fields(DogstatsdReplay::NAME);
        fields.insert("timeout".into(), "100ms".into());
        fields.insert("expected_metrics".into(), json!(["metric", "missing"]));
        let started = Instant::now();
        let report = run_step_with(&DogstatsdReplayFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(
            report.message,
            "DogStatsD replay statistics did not contain expected metric(s): missing"
        );
        assert_eq!(started.elapsed(), Duration::from_millis(1100));
    }

    #[tokio::test(start_paused = true)]
    async fn replay_errors_only_when_no_replay_attempt_observed_the_product() {
        for product in [false, true] {
            let fake = FakeParticipant::default().with_operations(OPERATIONS);
            fake.push_exec(Ok(output(json!({"path": "/capture"}))));
            for attempt in 0..3 {
                fake.push_exec(Err(HandleError::Harness {
                    message: "statistics unavailable".into(),
                }));
                fake.push_exec(Err(if product && attempt == 0 {
                    HandleError::Product {
                        message: "replay refused".into(),
                    }
                } else {
                    HandleError::Harness {
                        message: "replay unavailable".into(),
                    }
                }));
            }
            fake.push_send(Ok(()));
            let mut fields = valid_fields(DogstatsdReplay::NAME);
            fields.insert("timeout".into(), "100ms".into());
            let report = run_step_with(&DogstatsdReplayFactory, &fields, Some(&fake)).await;
            assert_eq!(
                report.status,
                if product {
                    AssertionStatus::Failed
                } else {
                    AssertionStatus::Errored
                }
            );
            assert_eq!(report.message, "Failed to replay DogStatsD capture: replay unavailable");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_replay_during_the_capture_wait_stops_it_promptly() {
        let fake = FakeParticipant::default().with_operations(OPERATIONS);
        fake.push_exec(Ok(output(json!({"path": "/capture"}))));
        fake.push_send(Ok(()));
        let mut fields = valid_fields(DogstatsdReplay::NAME);
        fields.insert("capture_duration".into(), "600s".into());
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            run_step_with(&DogstatsdReplayFactory, &fields, Some(&fake)),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(started.elapsed(), Duration::from_secs(1));
        sleep(Duration::from_secs(601)).await;
        fake.assert_executed(&[command(
            "dogstatsd_capture",
            json!({"duration_ms": 600_000}),
            Duration::from_secs(30),
        )]);
        fake.assert_sent(&[b"packet"]);
    }

    #[tokio::test(start_paused = true)]
    async fn malformed_capture_and_statistics_answers_error_the_step() {
        for (capture, value) in [
            (true, json!({})),
            (true, json!({"path": ""})),
            (true, json!({"path": 1})),
            (false, json!({})),
            (false, json!({"metrics": "metric"})),
            (false, json!({"metrics": [1]})),
        ] {
            let fake = FakeParticipant::default().with_operations(OPERATIONS);
            if !capture {
                fake.push_exec(Ok(output(json!({"path": "/capture"}))));
            }
            fake.push_exec(Ok(output(value)));
            if !capture {
                fake.push_exec(Ok(output(json!({}))));
            }
            fake.push_send(Ok(()));
            let report = run_step_with(
                &DogstatsdReplayFactory,
                &valid_fields(DogstatsdReplay::NAME),
                Some(&fake),
            )
            .await;
            assert_eq!(report.status, AssertionStatus::Errored, "{report:?}");
            assert!(
                report.message.contains(if capture {
                    "dogstatsd_capture returned no nonempty path"
                } else {
                    "dogstatsd_stats returned no list of metric names"
                }),
                "{report:?}"
            );
        }
    }

    #[test]
    fn replay_rejects_fractional_statistics_durations_empty_lists_and_invalid_payloads() {
        for (field, value, message) in [
            ("stats_duration", json!("1500ms"), "whole number of seconds"),
            ("stats_duration", json!("999ms"), "whole number of seconds"),
            ("stats_duration", json!("1s1ns"), "whole number of seconds"),
            ("payloads", json!([]), "payloads must not be empty"),
            ("expected_metrics", json!([]), "expected_metrics must not be empty"),
            (
                "payloads",
                json!([{"data": "x", "extra": true}]),
                "unknown field `extra`",
            ),
            ("payloads", json!([{"data": 1}]), "expected a string"),
            ("payloads", json!([{"data": "x", "after": "0s"}]), "greater than zero"),
            ("expected_metrics", json!([1]), "expected a string"),
            ("timeout", json!("18446744073709551615s"), "replay budget is too large"),
            (
                "capture_duration",
                json!("18446744073709551615s"),
                "too large in milliseconds",
            ),
        ] {
            let mut fields = valid_fields(DogstatsdReplay::NAME);
            fields.insert(field.into(), value);
            let error = DogstatsdReplayFactory.build(&fields).unwrap_err();
            assert!(error.contains(message), "{field}: {error}");
        }
        for field in [
            "on",
            "endpoint",
            "payloads",
            "capture_duration",
            "stats_duration",
            "expected_metrics",
            "timeout",
        ] {
            let mut fields = valid_fields(DogstatsdReplay::NAME);
            fields.remove(field);
            assert!(DogstatsdReplayFactory
                .build(&fields)
                .unwrap_err()
                .contains(&format!("missing field `{field}`")));
        }
    }

    #[test]
    fn replay_budget_covers_every_command_payload_delay_and_capture() {
        let mut fields = valid_fields(DogstatsdReplay::NAME);
        fields.insert(
            "payloads".into(),
            json!([{"data": "a", "after": "200ms"}, {"data": "b", "after": "1s"}]),
        );
        fields.insert("capture_duration".into(), "3s".into());
        assert_eq!(
            DogstatsdReplayFactory.build(&fields).unwrap().budget(),
            Duration::from_millis(154_250)
        );
        assert_eq!(DogstatsdReplayFactory.kind(), StepKind::Action);
        assert!(DogstatsdReplayFactory.template_fields().is_empty());
    }
}
