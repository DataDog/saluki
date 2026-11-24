//! `dogstatsd_replay`: captures DogStatsD traffic, replays the capture, and checks the statistics.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::{sleep, Instant};

use super::stopped;
use crate::core::case::Timeout;
use crate::core::handle::HandleError;
use crate::core::record::StepStatus;
use crate::core::step::{EndpointOperation, Need, Step, StepContext, StepFactory, StepFields, StepPurpose, StepReport};

/// The operations the step runs.
const CAPTURE: &str = "dogstatsd_capture";
const REPLAY: &str = "dogstatsd_replay";
const STATS: &str = "dogstatsd_stats";

/// How long to wait between rounds of replay and statistics.
const ROUND_PAUSE: Duration = Duration::from_millis(50);

pub(super) struct DogstatsdReplayFactory;

impl StepFactory for DogstatsdReplayFactory {
    fn name(&self) -> &'static str {
        "dogstatsd_replay"
    }

    fn purpose(&self) -> StepPurpose {
        StepPurpose::Action
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step = fields.decode::<DogstatsdReplay>()?;
        if step.payloads.is_empty() {
            return Err("payloads must hold at least one payload".to_string());
        }
        if step.stats_duration.duration.subsec_nanos() != 0 {
            return Err(format!(
                "stats_duration '{}' must be a whole number of seconds",
                step.stats_duration.text
            ));
        }
        Ok(Box::new(step))
    }
}

/// Starts a capture on `on`, and sends each payload to its endpoint `endpoint` while the capture runs. It waits
/// `payload_interval` between payloads if that is set. The capture lasts `capture_duration`. Then, until `timeout`, it
/// replays the capture and collects statistics together, and the statistics take `stats_duration`. Passes once the
/// statistics hold every one of `expected_metrics`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DogstatsdReplay {
    on: String,
    endpoint: String,
    payloads: Vec<String>,
    payload_interval: Option<Timeout>,
    capture_duration: Timeout,
    stats_duration: Timeout,
    expected_metrics: Vec<String>,
    timeout: Timeout,
}

impl DogstatsdReplay {
    /// The expected metrics that the answer of `dogstatsd_stats` lacks.
    fn missing(&self, answer: &Value) -> Result<Vec<String>, HandleError> {
        let metrics = answer
            .get("metrics")
            .and_then(Value::as_array)
            .ok_or_else(|| HandleError::Harness(format!("{} answered without a list of metrics", STATS)))?;
        Ok(self
            .expected_metrics
            .iter()
            .filter(|expected| !metrics.iter().any(|metric| metric.as_str() == Some(expected)))
            .cloned()
            .collect())
    }

    /// The report of a statistics run that failed. An earlier replay that reached ADP keeps the step `failed`, even
    /// when the error is the harness's.
    fn stats_failed(&self, error: HandleError, replay_ran: bool, missing: &[String]) -> StepReport {
        let mut report = match (error, replay_ran) {
            (HandleError::Software(message), _) | (HandleError::Harness(message), true) => StepReport {
                status: StepStatus::Failed,
                message,
            },
            (error, false) => stopped(error),
        };
        report.message = format!("Failed to collect DogStatsD replay statistics: {}", report.message);
        if !missing.is_empty() {
            report.message.push_str(&format!(
                "\nAn earlier replay's statistics did not contain expected metric(s): {}",
                missing.join(", ")
            ));
        }
        report
    }
}

/// `error` with `prefix` before its message.
fn prefixed(prefix: &str, error: HandleError) -> HandleError {
    match error {
        HandleError::Harness(message) => HandleError::Harness(format!("{}{}", prefix, message)),
        HandleError::Software(message) => HandleError::Software(format!("{}{}", prefix, message)),
    }
}

#[async_trait]
impl Step for DogstatsdReplay {
    fn name(&self) -> &'static str {
        "dogstatsd_replay"
    }

    fn needs(&self) -> Vec<Need> {
        let on = |operation: &str| Need::Operation(self.on.clone(), operation.to_string());
        vec![
            on(CAPTURE),
            on(REPLAY),
            on(STATS),
            Need::Endpoint(self.on.clone(), self.endpoint.clone(), EndpointOperation::Send),
        ]
    }

    /// One timeout for the capture, one for each payload's send, and two for the replay and statistics rounds, because
    /// the last round can start just before the deadline. Then the capture's duration, the waits between payloads and
    /// the pause between rounds.
    fn budget(&self) -> Duration {
        let sends = u32::try_from(self.payloads.len()).unwrap_or(u32::MAX);
        let waits = sends.saturating_sub(1);
        let interval = self
            .payload_interval
            .as_ref()
            .map_or(Duration::ZERO, |interval| interval.duration);
        self.timeout.duration.saturating_mul(sends.saturating_add(3))
            + interval.saturating_mul(waits)
            + self.capture_duration.duration
            + ROUND_PAUSE
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let limit = self.timeout.duration;
        let ran = async {
            let handle = cx.handle(&self.on)?;
            let duration_ms = self.capture_duration.duration.as_millis() as u64;
            let captured = handle
                .op(CAPTURE, json!({ "duration_ms": duration_ms }), limit)
                .await
                .map_err(|e| prefixed("Failed to start DogStatsD capture: ", e))?;
            let path = captured
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| HandleError::Harness(format!("{} answered without a path", CAPTURE)))?;

            for (index, payload) in self.payloads.iter().enumerate() {
                if let (Some(interval), true) = (&self.payload_interval, index > 0) {
                    sleep(interval.duration).await;
                }
                handle
                    .send(&self.endpoint, Bytes::from(payload.clone()), limit)
                    .await
                    .map_err(|e| prefixed("Failed to send DogStatsD traffic for capture: ", e))?;
            }

            // The capture writer finishes the file only after the capture's duration.
            sleep(self.capture_duration.duration).await;

            let stats_args = json!({ "duration_secs": self.stats_duration.duration.as_secs() });
            let replay_args = json!({ "path": path });
            let deadline = Instant::now() + limit;
            let mut last_replay_error = None;
            let mut last_missing = Vec::new();
            // At the deadline, a replay that reached ADP makes the step `failed` and not `errored`.
            let mut replay_ran = false;
            loop {
                let (stats, replay) = tokio::join!(
                    handle.op(STATS, stats_args.clone(), limit),
                    handle.op(REPLAY, replay_args.clone(), limit)
                );
                match replay {
                    Ok(_) => match stats {
                        Ok(answer) => {
                            replay_ran = true;
                            last_missing = self.missing(&answer)?;
                            if last_missing.is_empty() {
                                let message = format!(
                                    "Replayed a DogStatsD capture of '{}', and its statistics hold every expected metric.",
                                    self.on
                                );
                                return Ok(StepReport {
                                    status: StepStatus::Passed,
                                    message,
                                });
                            }
                        }
                        Err(error) => return Ok(self.stats_failed(error, replay_ran, &last_missing)),
                    },
                    Err(error) => {
                        replay_ran |= matches!(error, HandleError::Software(_));
                        last_replay_error = Some(error);
                    }
                }

                if Instant::now() >= deadline {
                    let status = if replay_ran {
                        StepStatus::Failed
                    } else {
                        StepStatus::Errored
                    };
                    let message = match last_replay_error {
                        Some(HandleError::Harness(message) | HandleError::Software(message)) => {
                            format!("Failed to replay DogStatsD capture: {}", message)
                        }
                        None => format!(
                            "DogStatsD replay statistics did not contain expected metric(s): {}",
                            last_missing.join(", ")
                        ),
                    };
                    return Ok(StepReport { status, message });
                }
                sleep(ROUND_PAUSE).await;
            }
        };
        ran.await.unwrap_or_else(stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::super::scripted::{observation, run, Scripted, CAPTURE_FILE, REPLAY_FILE, STATS_FILE};
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn missing_metrics_after_a_replay_fail_the_step() {
        let fields = "{on: app, endpoint: statsd, payloads: ['b:1|c'], capture_duration: 1s, stats_duration: 1s, \
                      expected_metrics: [b], timeout: 1s}";
        let scripted = Scripted {
            files: [
                (CAPTURE_FILE, Ok(br#"{"path": "/c"}"#.to_vec())),
                (REPLAY_FILE, Ok(b"{}".to_vec())),
                (STATS_FILE, Ok(br#"{"metrics": ["a"]}"#.to_vec())),
            ]
            .into(),
            exit: Some((0, "")),
        };
        let report = run(&DogstatsdReplayFactory, fields, &scripted, &observation()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report
                .message
                .ends_with("DogStatsD replay statistics did not contain expected metric(s): b"),
            "{}",
            report.message
        );
    }

    #[test]
    fn a_statistics_harness_error_after_a_replay_keeps_the_step_failed() {
        // A harness error from the statistics run does not override the missing metrics of an earlier replay, so the
        // step stays `failed`.
        let step: DogstatsdReplay = serde_saphyr::from_str(
            "{on: app, endpoint: statsd, payloads: ['b:1|c'], capture_duration: 1s, stats_duration: 1s, \
             expected_metrics: [replay.requests], timeout: 5s}",
        )
        .unwrap();
        let harness_error = || HandleError::Harness("Docker API failed.".to_string());

        let after_replay = step.stats_failed(harness_error(), true, &["replay.requests".to_string()]);
        assert_eq!(after_replay.status, StepStatus::Failed, "{}", after_replay.message);
        assert_eq!(
            after_replay.message,
            "Failed to collect DogStatsD replay statistics: Docker API failed.\n\
             An earlier replay's statistics did not contain expected metric(s): replay.requests"
        );

        let before_replay = step.stats_failed(harness_error(), false, &[]);
        assert_eq!(before_replay.status, StepStatus::Errored, "{}", before_replay.message);
        assert_eq!(
            before_replay.message,
            "Failed to collect DogStatsD replay statistics: Docker API failed."
        );
    }
}
