//! `intake_has_metric`: an intake holds a metric that meets every criterion.
//!
//! A metric context is a metric name plus its normalized tag set, with the host as a `host:` tag. Each poll
//! normalizes what the intake's `dump` holds per metric context and type, so a value split across flushes is compared
//! as its total. The step passes once one metric context meets every criterion by itself; contexts are never combined.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::time::{sleep_until, Instant};

use super::intake::metrics::{get_formatted_metric_value, MetricType, NormalizationFailure, NormalizedMetrics};
use super::intake::{CollectedData, MetricCriteria, TagMatch};
use super::stopped;
use crate::core::case::Timeout;
use crate::core::handle::HandleError;
use crate::core::probe::{POLL_INTERVAL, PROBE_TIMEOUT};
use crate::core::record::StepStatus;
use crate::core::step::{Need, Step, StepContext, StepFactory, StepFields, StepReport};

/// The operation the step calls on its participant.
const DUMP: &str = "dump";

/// Most metric contexts with the expected name, and most other metric names, a failure message lists.
const MAX_LISTED: usize = 10;

pub(super) struct IntakeHasMetricFactory;

impl StepFactory for IntakeHasMetricFactory {
    fn name(&self) -> &'static str {
        "intake_has_metric"
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let fields: Fields = fields.decode()?;
        let criteria = MetricCriteria::new(
            fields.name,
            fields.tags,
            fields.tags_match,
            fields.metric_type,
            fields.value,
        )?;
        Ok(Box::new(IntakeHasMetric {
            on: fields.on,
            criteria,
            timeout: fields.timeout,
        }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fields {
    on: String,
    name: String,
    #[serde(default)]
    metric_type: Option<MetricType>,
    #[serde(default)]
    value: Option<f64>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    tags_match: TagMatch,
    timeout: Timeout,
}

/// Passes once a metric context the intake holds meets every criterion within `timeout`.
#[derive(Debug)]
struct IntakeHasMetric {
    on: String,
    criteria: MetricCriteria,
    timeout: Timeout,
}

#[async_trait]
impl Step for IntakeHasMetric {
    fn name(&self) -> &'static str {
        "intake_has_metric"
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Operation(self.on.clone(), DUMP.to_string())]
    }

    fn budget(&self) -> Duration {
        self.timeout.duration
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let polled = async {
            let handle = cx.handle(&self.on)?;
            // The last dump that succeeded, normalized, and the error of the last dump if that one failed.
            let mut snapshot = None;
            let mut failed;
            loop {
                let limit = PROBE_TIMEOUT.min(cx.deadline.saturating_duration_since(Instant::now()));
                let dumped = handle.op(DUMP, json!(["metrics"]), limit).await.and_then(|dump| {
                    serde_json::from_value::<CollectedData>(dump)
                        .map_err(|e| HandleError::Harness(format!("the dump of '{}' does not decode: {}", self.on, e)))
                });
                match dumped {
                    Ok(collected) => {
                        let (metrics, failures) = NormalizedMetrics::from_stele_metrics(collected.metrics());
                        if self.criteria.is_met_by(&metrics) {
                            let message =
                                format!("'{}' holds a metric matching {}.", self.on, self.criteria.describe());
                            return Ok(StepReport {
                                status: StepStatus::Passed,
                                message,
                            });
                        }
                        snapshot = Some((metrics, failures));
                        failed = None;
                    }
                    Err(e) => failed = Some(e),
                }
                // A dump started just before the deadline would only time out.
                let next = Instant::now() + POLL_INTERVAL;
                if next >= cx.deadline {
                    break;
                }
                sleep_until(next).await;
            }
            let summary = format!(
                "No metric matching {} reached '{}' within {}.",
                self.criteria.describe(),
                self.on,
                self.timeout.text
            );
            match (snapshot, failed) {
                (Some((metrics, failures)), failed) => {
                    let mut message = summary;
                    if let Some(e) = failed {
                        let _ = write!(message, "\nThe last dump failed: {}", e);
                    }
                    self.describe(&metrics, &failures, &mut message);
                    Ok(StepReport {
                        status: StepStatus::Failed,
                        message,
                    })
                }
                (None, Some(HandleError::Harness(e))) => {
                    Err(HandleError::Harness(format!("{} No dump succeeded: {}", summary, e)))
                }
                (None, Some(HandleError::Software(e))) => {
                    Err(HandleError::Software(format!("{} No dump succeeded: {}", summary, e)))
                }
                (None, None) => Err(HandleError::Harness(format!("{} No dump completed.", summary))),
            }
        };
        polled.await.unwrap_or_else(stopped)
    }
}

impl IntakeHasMetric {
    /// Lists, into `out`, the contexts with the expected name and why each fails, or else the other names observed.
    fn describe(&self, metrics: &NormalizedMetrics, failures: &[NormalizationFailure], out: &mut String) {
        let name = self.criteria.name();
        let mut candidates: Vec<String> = metrics
            .metrics()
            .iter()
            .filter(|metric| metric.context().name() == name)
            .map(|metric| {
                format!(
                    "{} ({}): {} from {} point(s); fails {}",
                    metric.context(),
                    metric.value_type(),
                    get_formatted_metric_value(metric.normalized_value()),
                    metric.raw_values().len(),
                    self.criteria
                        .mismatch(metric)
                        .map(|mismatch| self.criteria.describe_mismatch(mismatch, metric))
                        .unwrap_or_default()
                )
            })
            .chain(
                failures
                    .iter()
                    .filter(|failure| failure.context.name() == name)
                    .map(|failure| {
                        format!(
                            "{} ({}): fails normalization: {:#}",
                            failure.context, failure.metric_type, failure.error
                        )
                    }),
            )
            .collect();
        candidates.sort();
        if candidates.is_empty() {
            let others: BTreeSet<&str> = metrics
                .metrics()
                .iter()
                .map(|metric| metric.context().name())
                .chain(failures.iter().map(|failure| failure.context.name()))
                .collect();
            let listed: Vec<&str> = others.iter().take(MAX_LISTED).copied().collect();
            let _ = write!(
                out,
                "\nNo context named '{}'. Other names ({}): {}",
                name,
                others.len(),
                listed.join(", ")
            );
            return;
        }
        let _ = write!(out, "\nContexts named '{}' ({}):", name, candidates.len());
        for candidate in candidates.iter().take(MAX_LISTED) {
            let _ = write!(out, "\n  - {}", candidate);
        }
        if candidates.len() > MAX_LISTED {
            let _ = write!(out, "\n  (+{} more)", candidates.len() - MAX_LISTED);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::scripted::{observation, run, Scripted, DUMP_FILE};
    use super::*;

    const FIELDS: &str = "{on: app, name: app.count, metric_type: count, value: 3, timeout: 2s}";

    #[tokio::test(start_paused = true)]
    async fn no_matching_metric_fails_and_a_dump_that_fails_errors() {
        let other = json!({"metrics": [{
            "context": {"name": "app.count", "tags": []},
            "values": [[1, {"mtype": "Count", "value": 2.0}]],
        }]})
        .to_string();
        let holds_another = Scripted {
            files: [(DUMP_FILE, Ok(other.as_bytes().to_vec()))].into(),
            ..Scripted::default()
        };
        let report = run(&IntakeHasMetricFactory, FIELDS, &holds_another, &observation()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(report.message.contains("fails value"), "{}", report.message);

        let unreachable = Scripted {
            files: [(
                DUMP_FILE,
                Err(HandleError::Harness("Docker refused the exec".to_string())),
            )]
            .into(),
            ..Scripted::default()
        };
        let report = run(&IntakeHasMetricFactory, FIELDS, &unreachable, &observation()).await;
        assert_eq!(report.status, StepStatus::Errored, "{}", report.message);
        assert!(report.message.contains("Docker refused the exec"), "{}", report.message);
    }
}
