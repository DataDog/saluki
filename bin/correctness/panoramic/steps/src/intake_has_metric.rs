//! Polls an intake participant for a metric context matching every configured criterion.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::Duration;

use panoramic_core::{
    case::CaseDuration,
    probe::{POLL_INTERVAL, PROBE_TIMEOUT},
    record::{AssertionStatus, StepReport},
    step::{decode_fields, Need, Step, StepContext, StepFactory, StepFields},
};
use saluki_error::{ErrorContext as _, GenericError};
use serde::Deserialize;
use serde_json::json;
use stele::{Metric, MetricValue};
use tokio::time::{sleep_until, Instant};

use crate::metrics::{
    get_formatted_metric_value, MetricType, NormalizationFailure, NormalizedMetric, NormalizedMetricContext,
    NormalizedMetrics,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fields {
    /// Required intake participant; has no default.
    on: String,
    /// Required metric name; has no default.
    name: String,
    /// Expected type; omitted accepts every type.
    metric_type: Option<MetricType>,
    /// Expected normalized value; omitted accepts every value. Invalid with the sketch type.
    value: Option<f64>,
    /// Expected tags, sorted and deduplicated before matching. Defaults to empty.
    #[serde(default)]
    tags: Vec<String>,
    /// Whether extra tags are allowed. Defaults to subset.
    #[serde(default)]
    tags_match: TagMatch,
    /// Required, positive time to wait for a match; has no default.
    timeout: CaseDuration,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum TagMatch {
    #[default]
    Subset,
    Exact,
}

/// Most metric contexts with the expected name that a failure message lists.
const MAX_CANDIDATES: usize = 10;

/// Most other metric names a failure message lists when no metric with the expected name was observed.
const MAX_OTHER_NAMES: usize = 10;

/// Most bytes in a failure message, including [`TRUNCATION_NOTICE`].
const MAX_MESSAGE_BYTES: usize = 4096;

const TRUNCATION_NOTICE: &str = "\n[message truncated]";

/// Assertion that polls an intake participant for a metric matching the configured criteria.
///
/// A metric context is a metric name plus its normalized tag set, with the host as a `host:` tag; see
/// [`NormalizedMetricContext`]. Each poll normalizes the intake's metrics dump per (metric context, type), so a value
/// split across flushes is compared as its total. The assertion passes once one metric context meets every criterion by
/// itself; metric contexts are never combined.
#[derive(Debug)]
pub(crate) struct IntakeHasMetric {
    on: String,
    /// Expected name and tags, normalized like an observed context.
    expected: NormalizedMetricContext,
    tags_match: TagMatch,
    metric_type: Option<MetricType>,
    value: Option<f64>,
    timeout: Duration,
}

/// The first criterion an observed context with the expected name fails.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mismatch {
    Tags,
    Type,
    Value,
}

/// One successful poll of the intake's metrics, normalized.
struct Snapshot {
    metrics: NormalizedMetrics,
    failures: Vec<NormalizationFailure>,
}

impl Snapshot {
    fn from_stele_metrics(metrics: &[Metric]) -> Self {
        let (metrics, failures) = NormalizedMetrics::from_stele_metrics(metrics);
        Self { metrics, failures }
    }
}

/// The most recent failed poll.
struct LastError {
    error: GenericError,

    /// Whether a later poll succeeded.
    before_snapshot: bool,
}

impl IntakeHasMetric {
    /// Registry name used in case procedures.
    pub(crate) const NAME: &'static str = "intake_has_metric";

    fn new(
        on: String, name: String, metric_type: Option<MetricType>, value: Option<f64>, tags: Vec<String>,
        tags_match: TagMatch, timeout: Duration,
    ) -> Result<Self, String> {
        if value.is_some() && metric_type == Some(MetricType::Sketch) {
            return Err(format!(
                "intake_has_metric `{}` can't combine `value` with `metric_type: sketch`: a sketch has no single \
                 value to compare",
                name
            ));
        }

        Ok(Self {
            on,
            expected: NormalizedMetricContext::new(name, tags),
            tags_match,
            metric_type,
            value,
            timeout,
        })
    }

    /// Returns whether one context in `metrics` meets every criterion.
    fn is_met_by(&self, metrics: &NormalizedMetrics) -> bool {
        metrics
            .metrics()
            .iter()
            .any(|metric| metric.context().name() == self.expected.name() && self.mismatch(metric).is_none())
    }

    /// Returns the first criterion `metric` fails, checking tags, then type, then value. The name isn't checked.
    fn mismatch(&self, metric: &NormalizedMetric) -> Option<Mismatch> {
        let observed_tags = metric.context().tags();
        let tags_hold = match self.tags_match {
            TagMatch::Subset => self.expected.tags().iter().all(|tag| observed_tags.contains(tag)),
            TagMatch::Exact => self.expected.tags() == observed_tags,
        };
        if !tags_hold {
            return Some(Mismatch::Tags);
        }

        if self.metric_type.is_some_and(|expected| expected != metric.value_type()) {
            return Some(Mismatch::Type);
        }

        if self
            .value
            .is_some_and(|expected| !value_matches(metric.normalized_value(), expected))
        {
            return Some(Mismatch::Value);
        }

        None
    }

    /// Explains how `metric` fails `mismatch`.
    fn describe_mismatch(&self, mismatch: Mismatch, metric: &NormalizedMetric) -> String {
        match mismatch {
            Mismatch::Tags => {
                let observed_tags = metric.context().tags();
                let missing = self
                    .expected
                    .tags()
                    .iter()
                    .filter(|tag| !observed_tags.contains(tag))
                    .cloned()
                    .collect::<Vec<_>>();
                let mut parts = Vec::new();
                if !missing.is_empty() {
                    parts.push(format!("missing [{}]", missing.join(", ")));
                }
                if self.tags_match == TagMatch::Exact {
                    let unexpected = observed_tags
                        .iter()
                        .filter(|tag| !self.expected.tags().contains(tag))
                        .cloned()
                        .collect::<Vec<_>>();
                    if !unexpected.is_empty() {
                        parts.push(format!("unexpected [{}]", unexpected.join(", ")));
                    }
                }
                format!("tags: {}", parts.join(", "))
            }
            Mismatch::Type => format!(
                "type: expected {}",
                self.metric_type.map(|t| t.to_string()).unwrap_or_default()
            ),
            Mismatch::Value => format!("value: expected {}", self.value.unwrap_or_default()),
        }
    }

    /// Reports a step timeout, leading with the last collection error if every poll failed.
    fn expired_result(
        &self, started: Instant, snapshot: Option<&Snapshot>, last_error: Option<&LastError>,
    ) -> StepReport {
        let summary = format!(
            "No metric matching {} arrived at intake participant '{}' within {:?}.",
            self.criteria(),
            self.on,
            self.timeout
        );
        match (snapshot, last_error) {
            (None, Some(last_error)) => {
                let message = format!(
                    "{:#}\n{}\nNo successful metrics snapshot from intake participant '{}'.",
                    last_error.error, summary, self.on
                );
                StepReport::new(started, AssertionStatus::Errored, truncate_message(message))
            }
            _ => StepReport::new(
                started,
                if snapshot.is_some() {
                    AssertionStatus::Failed
                } else {
                    AssertionStatus::Errored
                },
                self.failure_message(summary, snapshot, last_error),
            ),
        }
    }

    /// Explains a failure, keeping collection errors ahead of potentially long candidate details.
    fn failure_message(&self, summary: String, snapshot: Option<&Snapshot>, last_error: Option<&LastError>) -> String {
        let mut message = summary;

        if let Some(last_error) = last_error {
            let when = if last_error.before_snapshot {
                ", before the last successful snapshot"
            } else if snapshot.is_some() {
                ", after the last successful snapshot"
            } else {
                ""
            };
            let _ = write!(message, "\nLast collection error{}: {:#}", when, last_error.error);
        }

        match snapshot {
            Some(snapshot) => self.describe_snapshot(snapshot, &mut message),
            None if last_error.is_none() => message.push_str("\nNo poll completed."),
            None => {
                let _ = write!(
                    message,
                    "\nNo successful metrics snapshot from intake participant '{}'.",
                    self.on
                );
            }
        }

        truncate_message(message)
    }

    /// Lists the snapshot's contexts with the expected name, and why each failed, into `out`.
    fn describe_snapshot(&self, snapshot: &Snapshot, out: &mut String) {
        let name = self.expected.name();

        let mut candidates = snapshot
            .metrics
            .metrics()
            .iter()
            .filter(|metric| metric.context().name() == name)
            .map(|metric| {
                let outcome = match self.mismatch(metric) {
                    Some(mismatch) => format!("fails {}", self.describe_mismatch(mismatch, metric)),
                    None => "meets every criterion".to_string(),
                };
                let detail = format!(
                    "{} from {} point(s); {}",
                    get_formatted_metric_value(metric.normalized_value()),
                    metric.raw_values().len(),
                    outcome
                );
                (metric.context(), metric.value_type(), detail)
            })
            .collect::<Vec<_>>();
        candidates.extend(
            snapshot
                .failures
                .iter()
                .filter(|failure| failure.context.name() == name)
                .map(|failure| {
                    let detail = format!("fails normalization: {:#}", failure.error);
                    (&failure.context, failure.metric_type, detail)
                }),
        );
        candidates.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));

        let ignored_failures = snapshot
            .failures
            .iter()
            .filter(|failure| failure.context.name() != name)
            .count();
        if ignored_failures > 0 {
            let _ = write!(
                out,
                "\n{} normalization failure(s) under other names ignored.",
                ignored_failures
            );
        }

        if candidates.is_empty() {
            let _ = write!(out, "\nNo context named '{}' observed.", name);

            let other_names = snapshot
                .metrics
                .metrics()
                .iter()
                .map(|metric| metric.context().name())
                .chain(snapshot.failures.iter().map(|failure| failure.context.name()))
                .collect::<BTreeSet<_>>();
            if !other_names.is_empty() {
                let listed = other_names.iter().take(MAX_OTHER_NAMES).copied().collect::<Vec<_>>();
                let _ = write!(
                    out,
                    " {} other metric name(s) observed: {}",
                    other_names.len(),
                    listed.join(", ")
                );
                if other_names.len() > listed.len() {
                    let _ = write!(out, " (+{} more)", other_names.len() - listed.len());
                }
                out.push('.');
            }
        } else {
            let _ = write!(out, "\nCandidates named '{}' ({}):", name, candidates.len());
            for (context, metric_type, detail) in candidates.iter().take(MAX_CANDIDATES) {
                let _ = write!(out, "\n  - {} ({}): {}", context, metric_type, detail);
            }
            if candidates.len() > MAX_CANDIDATES {
                let _ = write!(out, "\n  (+{} more)", candidates.len() - MAX_CANDIDATES);
            }
        }
    }

    fn criteria(&self) -> String {
        let mut parts = vec![format!("name={}", self.expected.name())];
        if let Some(metric_type) = self.metric_type {
            parts.push(format!("metric_type={}", metric_type));
        }
        if let Some(value) = self.value {
            parts.push(format!("value={}", value));
        }
        // An empty exact set still constrains the context, so it is always shown.
        if !self.expected.tags().is_empty() || self.tags_match == TagMatch::Exact {
            parts.push(format!("tags=[{}]", self.expected.tags().join(", ")));
        }
        parts.push(
            match self.tags_match {
                TagMatch::Subset => "tags_match=subset",
                TagMatch::Exact => "tags_match=exact",
            }
            .to_string(),
        );
        parts.join(", ")
    }
}

/// Returns whether `observed` equals `expected`, read as a value of the observed type.
///
/// A rate is compared at its own interval. A sketch has no single value, so it never matches.
fn value_matches(observed: &MetricValue, expected: f64) -> bool {
    // Compare through `MetricValue` so float comparison stays owned by stele.
    match observed {
        MetricValue::Count { .. } => *observed == MetricValue::Count { value: expected },
        MetricValue::Gauge { .. } => *observed == MetricValue::Gauge { value: expected },
        MetricValue::Rate { interval, .. } => {
            *observed
                == MetricValue::Rate {
                    interval: *interval,
                    value: expected,
                }
        }
        MetricValue::Sketch { .. } => false,
    }
}

/// Caps `message` at [`MAX_MESSAGE_BYTES`], cutting on a character boundary and ending with [`TRUNCATION_NOTICE`].
fn truncate_message(mut message: String) -> String {
    if message.len() > MAX_MESSAGE_BYTES {
        let cut = message.floor_char_boundary(MAX_MESSAGE_BYTES - TRUNCATION_NOTICE.len());
        message.truncate(cut);
        message.push_str(TRUNCATION_NOTICE);
    }
    message
}

#[async_trait::async_trait]
impl Step for IntakeHasMetric {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Operation(self.on.clone(), "dump".into())]
    }

    fn budget(&self) -> Duration {
        self.timeout
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let deadline = started + self.timeout;
        let Some(handle) = cx.handle(&self.on) else {
            return StepReport::new(
                started,
                AssertionStatus::Errored,
                format!("participant '{}' has not started", self.on),
            );
        };
        let mut snapshot = None;
        let mut last_error: Option<LastError> = None;
        let mut polled = false;

        loop {
            // Always poll once, using only the remaining budget.
            if polled && Instant::now() >= deadline {
                return self.expired_result(started, snapshot.as_ref(), last_error.as_ref());
            }
            polled = true;

            let limit = deadline.saturating_duration_since(Instant::now()).min(PROBE_TIMEOUT);
            let metrics = match handle.op("dump", json!({"kind": "metrics"}), limit).await {
                Ok(value) => serde_json::from_value::<Vec<Metric>>(value).with_error_context(|| {
                    format!("Failed to decode metrics dump from intake participant '{}'", self.on)
                }),
                Err(error) => Err(error.into()),
            };
            match metrics {
                Ok(metrics) => {
                    let current = Snapshot::from_stele_metrics(&metrics);
                    if self.is_met_by(&current.metrics) {
                        return StepReport::new(
                            started,
                            AssertionStatus::Passed,
                            format!(
                                "Intake participant '{}' received a metric matching {}.",
                                self.on,
                                self.criteria()
                            ),
                        );
                    }
                    snapshot = Some(current);
                    if let Some(last_error) = last_error.as_mut() {
                        last_error.before_snapshot = true;
                    }
                }
                Err(error) => {
                    last_error = Some(LastError {
                        error,
                        before_snapshot: false,
                    });
                }
            }
            sleep_until(deadline.min(Instant::now() + POLL_INTERVAL)).await;
        }
    }
}

/// Builds `intake_has_metric` from its case-file fields.
pub(crate) struct IntakeHasMetricFactory;

impl StepFactory for IntakeHasMetricFactory {
    fn name(&self) -> &'static str {
        IntakeHasMetric::NAME
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let fields = decode_fields::<Fields>(fields)?;
        Ok(Box::new(IntakeHasMetric::new(
            fields.on,
            fields.name,
            fields.metric_type,
            fields.value,
            fields.tags,
            fields.tags_match,
            fields.timeout.into(),
        )?))
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::step::{
        test_support::{run_step_with, FakeParticipant},
        ExecOutput, HandleError,
    };
    use saluki_error::generic_error;
    use serde_json::Value;

    use super::*;
    use crate::test_support::valid_fields;

    fn count(value: f64) -> Value {
        json!({"mtype": "Count", "value": value})
    }

    fn rate(value: f64) -> Value {
        json!({"mtype": "Rate", "interval": 10, "value": value})
    }

    fn gauge(value: f64) -> Value {
        json!({"mtype": "Gauge", "value": value})
    }

    /// Builds a dump entry as JSON, its values stamped in order starting at zero.
    fn metric_json(name: &str, tags: &[&str], values: &[Value]) -> Value {
        json!({
            "context": {
                "name": name,
                "tags": tags,
            },
            "values": values
                .iter()
                .enumerate()
                .map(|(idx, value)| json!([idx as u64, value]))
                .collect::<Vec<_>>(),
        })
    }

    fn metric(name: &str, tags: &[&str], values: &[Value]) -> Metric {
        serde_json::from_value(metric_json(name, tags, values)).expect("metric should deserialize")
    }

    fn snapshot(metrics: &[Metric]) -> Snapshot {
        Snapshot::from_stele_metrics(metrics)
    }

    fn assertion(
        name: &str, metric_type: Option<MetricType>, value: Option<f64>, tags: &[&str], tags_match: TagMatch,
    ) -> IntakeHasMetric {
        IntakeHasMetric::new(
            "subject".to_string(),
            name.to_string(),
            metric_type,
            value,
            tags.iter().map(|t| t.to_string()).collect(),
            tags_match,
            Duration::from_secs(1),
        )
        .expect("assertion should build")
    }

    fn subset(name: &str, metric_type: Option<MetricType>, value: Option<f64>, tags: &[&str]) -> IntakeHasMetric {
        assertion(name, metric_type, value, tags, TagMatch::Subset)
    }

    fn exact(name: &str, tags: &[&str]) -> IntakeHasMetric {
        assertion(name, None, None, tags, TagMatch::Exact)
    }

    fn failure_message(assertion: &IntakeHasMetric, metrics: &[Metric]) -> String {
        assertion.failure_message("Summary.".to_string(), Some(&snapshot(metrics)), None)
    }

    fn dump(metrics: Value) -> ExecOutput {
        ExecOutput {
            stdout: serde_json::to_vec(&metrics).unwrap(),
            stderr: Vec::new(),
            exit_code: 0,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_later_dump_with_a_matching_context_passes() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(dump(json!([]))));
        fake.push_exec(Ok(dump(json!([metric_json("other.metric", &[], &[count(1.0)])]))));
        fake.push_exec(Ok(dump(json!([metric_json("some.metric", &[], &[count(1.0)])]))));
        let started = Instant::now();
        let report = run_step_with(
            &IntakeHasMetricFactory,
            &valid_fields(IntakeHasMetric::NAME),
            Some(&fake),
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(started.elapsed(), POLL_INTERVAL * 2);
        assert_eq!(
            report.message,
            "Intake participant 'subject' received a metric matching name=some.metric, tags_match=subset."
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_with_a_snapshot_fails_and_lists_candidates() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(dump(json!([metric_json(
            "some.metric",
            &["host:custom"],
            &[gauge(4.0)]
        )]))));
        let mut fields = valid_fields(IntakeHasMetric::NAME);
        fields.insert("value".into(), json!(5));
        let started = Instant::now();
        let report = run_step_with(&IntakeHasMetricFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert_eq!(
            report.message,
            "No metric matching name=some.metric, value=5, tags_match=subset arrived at intake participant 'subject' within 30s.\nCandidates named 'some.metric' (1):\n  - some.metric[host:custom] (gauge): gauge(4) from 1 point(s); fails value: expected 5"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_without_a_snapshot_errors_and_leads_with_the_last_dump_error() {
        for error in [
            HandleError::Harness {
                message: "dump mechanism lost".into(),
            },
            HandleError::Product {
                message: "dump unavailable".into(),
            },
        ] {
            let diagnostic = error.to_string();
            let fake = FakeParticipant::default();
            fake.push_exec(Err(HandleError::Product {
                message: "first dump error".into(),
            }));
            fake.push_exec(Err(error));
            let started = Instant::now();
            let report = run_step_with(
                &IntakeHasMetricFactory,
                &valid_fields(IntakeHasMetric::NAME),
                Some(&fake),
            )
            .await;
            assert_eq!(report.status, AssertionStatus::Errored);
            assert_eq!(started.elapsed(), Duration::from_secs(30));
            assert_eq!(
                report.message,
                format!("{diagnostic}\nNo metric matching name=some.metric, tags_match=subset arrived at intake participant 'subject' within 30s.\nNo successful metrics snapshot from intake participant 'subject'.")
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_undecodable_dump_is_a_poll_error_and_can_be_retried() {
        for recovers in [false, true] {
            let fake = FakeParticipant::default();
            fake.push_exec(Ok(dump(json!({"unexpected": "shape"}))));
            if recovers {
                fake.push_exec(Ok(dump(json!([metric_json("some.metric", &[], &[count(1.0)])]))));
            }
            let started = Instant::now();
            let report = run_step_with(
                &IntakeHasMetricFactory,
                &valid_fields(IntakeHasMetric::NAME),
                Some(&fake),
            )
            .await;
            if recovers {
                assert_eq!(report.status, AssertionStatus::Passed);
                assert_eq!(started.elapsed(), POLL_INTERVAL);
            } else {
                assert_eq!(report.status, AssertionStatus::Errored);
                assert_eq!(started.elapsed(), Duration::from_secs(30));
                assert!(
                    report
                        .message
                        .starts_with("Failed to decode metrics dump from intake participant 'subject': "),
                    "{}",
                    report.message
                );
                assert!(
                    report
                        .message
                        .ends_with("No successful metrics snapshot from intake participant 'subject'."),
                    "{}",
                    report.message
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_snapshot_keeps_a_timeout_failed_and_dates_the_last_collection_error() {
        for error_first in [false, true] {
            let fake = FakeParticipant::default();
            let snapshot = Ok(dump(json!([metric_json("some.metric", &[], &[count(1.0)])])));
            let error = Err(HandleError::Harness {
                message: "dump mechanism lost".into(),
            });
            if error_first {
                fake.push_exec(error);
                fake.push_exec(snapshot);
            } else {
                fake.push_exec(snapshot);
                fake.push_exec(error);
            }
            let mut fields = valid_fields(IntakeHasMetric::NAME);
            fields.insert("value".into(), json!(2));
            let report = run_step_with(&IntakeHasMetricFactory, &fields, Some(&fake)).await;
            assert_eq!(report.status, AssertionStatus::Failed);
            let when = if error_first { "before" } else { "after" };
            assert!(
                report.message.contains(&format!(
                    "Last collection error, {when} the last successful snapshot: dump mechanism lost"
                )),
                "{}",
                report.message
            );
            assert!(
                report
                    .message
                    .contains("some.metric (count): count(1) from 1 point(s); fails value: expected 2"),
                "{}",
                report.message
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn polling_matches_a_rate_split_across_flushes_by_its_total() {
        for (value, status) in [(0.6, AssertionStatus::Passed), (0.3, AssertionStatus::Failed)] {
            let fake = FakeParticipant::default();
            fake.push_exec(Ok(dump(json!([
                metric_json("some.metric", &["source:test"], &[rate(0.3)]),
                metric_json("some.metric", &["source:test"], &[rate(0.3)]),
            ]))));
            let mut fields = valid_fields(IntakeHasMetric::NAME);
            fields.extend(
                serde_saphyr::from_str::<StepFields>(&format!(
                    "metric_type: rate\nvalue: {value}\ntags: ['source:test']"
                ))
                .unwrap(),
            );
            let report = run_step_with(&IntakeHasMetricFactory, &fields, Some(&fake)).await;
            assert_eq!(report.status, status);
            if status == AssertionStatus::Failed {
                assert!(report.message.contains("some.metric[source:test] (rate): rate(0.6 over 10s) from 2 point(s); fails value: expected 0.3"), "{}", report.message);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_yaml_exact_tags_match_reaches_the_built_step() {
        for (tags_match, status) in [
            ("tags_match: exact", AssertionStatus::Failed),
            ("", AssertionStatus::Passed),
        ] {
            let fake = FakeParticipant::default();
            fake.push_exec(Ok(dump(json!([metric_json(
                "hosttag.metric",
                &["host:custom", "extra:tag"],
                &[gauge(4.0)]
            )]))));
            let fields = serde_saphyr::from_str(&format!(
                "on: subject\nname: hosttag.metric\nmetric_type: gauge\nvalue: 4\ntags: ['host:custom']\n{tags_match}\ntimeout: 30s"
            )).unwrap();
            let report = run_step_with(&IntakeHasMetricFactory, &fields, Some(&fake)).await;
            assert_eq!(report.status, status);
            if status == AssertionStatus::Failed {
                assert!(
                    report.message.contains("fails tags: unexpected [extra:tag]"),
                    "{}",
                    report.message
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_participant_exit_does_not_end_polling_early() {
        let fake = FakeParticipant::default();
        fake.complete();
        fake.push_exec(Ok(dump(json!([]))));
        let started = Instant::now();
        let report = run_step_with(
            &IntakeHasMetricFactory,
            &valid_fields(IntakeHasMetric::NAME),
            Some(&fake),
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert!(
            report.message.ends_with("No context named 'some.metric' observed."),
            "{}",
            report.message
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_budget_shorter_than_the_poll_interval_still_polls() {
        let fake = FakeParticipant::default();
        fake.push_exec(Ok(dump(json!([metric_json("some.metric", &[], &[count(1.0)])]))));
        let mut fields = valid_fields(IntakeHasMetric::NAME);
        fields.insert("timeout".into(), json!("1ns"));
        let report = run_step_with(&IntakeHasMetricFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed);
        assert_eq!(report.duration, Duration::ZERO);
    }

    #[test]
    fn the_metric_step_requires_its_participant_name_and_timeout() {
        for field in ["on", "name", "timeout"] {
            let mut fields = valid_fields(IntakeHasMetric::NAME);
            fields.remove(field).unwrap();
            let message = IntakeHasMetricFactory.build(&fields).unwrap_err();
            assert!(message.contains(&format!("missing field `{field}`")), "{message}");
        }
        let mut fields = valid_fields(IntakeHasMetric::NAME);
        fields.insert("metric_type".into(), json!("sketch"));
        fields.insert("value".into(), json!(1));
        assert_eq!(
            IntakeHasMetricFactory.build(&fields).unwrap_err(),
            "intake_has_metric `some.metric` can't combine `value` with `metric_type: sketch`: a sketch has no single value to compare"
        );
        assert!(IntakeHasMetricFactory.template_fields().is_empty());
    }

    #[test]
    fn name_only_criteria_matches_on_name_alone() {
        let observed = snapshot(&[metric("some.counter", &["env:test"], &[count(3.0)])]);

        assert!(subset("some.counter", None, None, &[]).is_met_by(&observed.metrics));
        assert!(!subset("other.counter", None, None, &[]).is_met_by(&observed.metrics));
    }

    #[test]
    fn type_and_value_must_hold_for_the_same_normalized_context() {
        let observed = snapshot(&[
            metric("some.metric", &[], &[count(3.0)]),
            metric("some.metric", &[], &[gauge(7.0)]),
        ]);
        let met = |metric_type, value| {
            subset("some.metric", Some(metric_type), Some(value), &[]).is_met_by(&observed.metrics)
        };

        assert!(met(MetricType::Count, 3.0));
        assert!(met(MetricType::Gauge, 7.0));

        // Each criterion holds for one context, but no single context satisfies both.
        assert!(!met(MetricType::Count, 7.0));
        assert!(!met(MetricType::Gauge, 3.0));

        // The same tags with a different type are separate contexts, so their values are never summed.
        assert!(!subset("some.metric", None, Some(10.0), &[]).is_met_by(&observed.metrics));
    }

    #[test]
    fn a_value_split_across_flushes_matches_its_total_not_a_partial_sum() {
        let both_flushes = snapshot(&[
            metric("some.counter", &[], &[rate(0.3)]),
            metric("some.counter", &[], &[rate(0.3)]),
        ]);
        let first_flush = snapshot(&[metric("some.counter", &[], &[rate(0.3)])]);
        let total = subset("some.counter", Some(MetricType::Rate), Some(0.6), &[]);
        let partial = subset("some.counter", Some(MetricType::Rate), Some(0.3), &[]);

        assert!(total.is_met_by(&both_flushes.metrics));
        assert!(!partial.is_met_by(&both_flushes.metrics));
        assert!(!total.is_met_by(&first_flush.metrics));
    }

    #[test]
    fn contexts_that_each_fall_short_are_not_summed() {
        let observed = snapshot(&[
            metric("some.counter", &["shard:a"], &[rate(0.3)]),
            metric("some.counter", &["shard:b"], &[rate(0.3)]),
        ]);

        assert!(!subset("some.counter", None, Some(0.6), &[]).is_met_by(&observed.metrics));
        assert!(subset("some.counter", None, Some(0.3), &[]).is_met_by(&observed.metrics));
    }

    #[test]
    fn subset_and_exact_tag_matching() {
        let tagged = snapshot(&[metric("m", &["source:s", "host:h"], &[gauge(1.0)])]);
        let untagged = snapshot(&[metric("m", &[], &[gauge(1.0)])]);

        // Subset allows extra tags, including the host tag.
        assert!(subset("m", None, None, &["source:s"]).is_met_by(&tagged.metrics));
        assert!(subset("m", None, None, &["host:h"]).is_met_by(&tagged.metrics));
        assert!(!subset("m", None, None, &["source:other"]).is_met_by(&tagged.metrics));

        // Exact treats the host as a tag like any other.
        assert!(!exact("m", &["source:s"]).is_met_by(&tagged.metrics));
        assert!(exact("m", &["host:h", "source:s"]).is_met_by(&tagged.metrics));

        // Expected tags are sorted and deduplicated like observed ones.
        assert!(exact("m", &["source:s", "host:h", "source:s"]).is_met_by(&tagged.metrics));

        // An empty set matches any context under subset, but only an untagged one under exact.
        assert!(subset("m", None, None, &[]).is_met_by(&tagged.metrics));
        assert!(!exact("m", &[]).is_met_by(&tagged.metrics));
        assert!(exact("m", &[]).is_met_by(&untagged.metrics));
    }

    #[test]
    fn a_value_combined_with_a_sketch_type_is_rejected() {
        let result = IntakeHasMetric::new(
            "subject".to_string(),
            "some.sketch".to_string(),
            Some(MetricType::Sketch),
            Some(1.0),
            Vec::new(),
            TagMatch::Subset,
            Duration::from_secs(1),
        );
        let error = result.expect_err("value with a sketch type should be rejected");
        assert!(
            error
                .to_string()
                .contains("can't combine `value` with `metric_type: sketch`"),
            "unexpected error: {}",
            error
        );

        // Each one alone is valid.
        assert!(IntakeHasMetric::new(
            "subject".to_string(),
            "some.sketch".to_string(),
            Some(MetricType::Sketch),
            None,
            Vec::new(),
            TagMatch::Subset,
            Duration::from_secs(1),
        )
        .is_ok());
        assert!(IntakeHasMetric::new(
            "subject".to_string(),
            "some.sketch".to_string(),
            None,
            Some(1.0),
            Vec::new(),
            TagMatch::Subset,
            Duration::from_secs(1),
        )
        .is_ok());
    }

    #[test]
    fn a_wrong_value_or_wrong_tags_is_reported_as_a_candidate_not_as_missing() {
        let observed = [
            metric("hosttag.metric", &["host:custom"], &[gauge(4.0)]),
            metric("hosttag.metric", &["host:other"], &[gauge(5.0)]),
        ];

        let message = failure_message(
            &assertion(
                "hosttag.metric",
                Some(MetricType::Gauge),
                Some(5.0),
                &["host:custom"],
                TagMatch::Exact,
            ),
            &observed,
        );

        assert_eq!(
            message,
            "Summary.\nCandidates named 'hosttag.metric' (2):\n  - hosttag.metric[host:custom] (gauge): gauge(4) from 1 \
             point(s); fails value: expected 5\n  - hosttag.metric[host:other] (gauge): gauge(5) from 1 point(s); fails \
             tags: missing [host:custom], unexpected [host:other]"
        );
    }

    #[test]
    fn a_wrong_type_is_reported_as_a_candidate() {
        let message = failure_message(
            &subset("some.counter", Some(MetricType::Count), None, &[]),
            &[metric("some.counter", &[], &[rate(0.3)])],
        );

        assert!(
            message.contains("some.counter (rate): rate(0.3 over 10s) from 1 point(s); fails type: expected count"),
            "unexpected message: {}",
            message
        );
    }

    #[test]
    fn a_candidate_normalization_failure_is_its_reason_and_others_are_counted() {
        let mixed = [count(1.0), gauge(2.0)];
        let observed = [
            metric("some.metric", &[], &mixed),
            metric("other.metric", &[], &mixed),
            metric("some.metric", &["ok:yes"], &[count(1.0)]),
        ];

        let missing_value = subset("some.metric", None, Some(9.0), &[]);
        let message = failure_message(&missing_value, &observed);
        assert_eq!(
            message,
            "Summary.\n1 normalization failure(s) under other names ignored.\nCandidates named 'some.metric' (2):\n  - \
             some.metric (count): fails normalization: Failed to normalize values for metric 'some.metric': \
             Cannot normalize values of different types.\n  - some.metric[ok:yes] (count): count(1) from 1 point(s); \
             fails value: expected 9"
        );

        // A failure under another name doesn't stop a matching context from passing.
        assert!(subset("some.metric", None, Some(1.0), &["ok:yes"]).is_met_by(&snapshot(&observed).metrics));
    }

    #[test]
    fn no_candidate_lists_a_bounded_set_of_other_names() {
        let mut observed = (0..12)
            .map(|idx| metric(&format!("other.{:02}", idx), &[], &[count(1.0)]))
            .collect::<Vec<_>>();
        observed.push(metric("broken", &[], &[count(1.0), gauge(2.0)]));

        let message = failure_message(&subset("missing.metric", None, None, &[]), &observed);

        assert_eq!(
            message,
            "Summary.\n1 normalization failure(s) under other names ignored.\nNo context named 'missing.metric' \
             observed. 13 other metric name(s) observed: broken, other.00, other.01, other.02, other.03, other.04, \
             other.05, other.06, other.07, other.08 (+3 more)."
        );
    }

    #[test]
    fn candidates_are_bounded_in_count() {
        let observed = (0..50)
            .map(|idx| metric("some.metric", &[&format!("shard:{:02}", idx)], &[count(1.0)]))
            .collect::<Vec<_>>();

        let message = failure_message(&subset("some.metric", None, Some(2.0), &[]), &observed);

        assert!(message.contains("Candidates named 'some.metric' (50):"), "{}", message);
        assert_eq!(message.matches("\n  - ").count(), 10);
        assert!(message.contains("some.metric[shard:09]"), "{}", message);
        assert!(!message.contains("some.metric[shard:10]"), "{}", message);
        assert!(message.ends_with("\n  (+40 more)"), "{}", message);
    }

    #[test]
    fn a_long_message_is_capped_on_a_character_boundary() {
        let tag = format!("label:{}", "€".repeat(200));
        let observed = (0..50)
            .map(|idx| metric("some.metric", &[&format!("{}{:02}", tag, idx)], &[count(1.0)]))
            .collect::<Vec<_>>();

        let message = failure_message(&subset("some.metric", None, Some(2.0), &[]), &observed);

        assert!(message.len() <= MAX_MESSAGE_BYTES, "message is {} bytes", message.len());
        assert!(message.ends_with(TRUNCATION_NOTICE), "{}", message);
        assert!(message.starts_with("Summary.\nCandidates named 'some.metric' (50):"));

        // The cap falls inside a multibyte character here, so cutting at it directly would split one.
        let input = format!("a{}", "€".repeat(2000));
        assert!(!input.is_char_boundary(MAX_MESSAGE_BYTES - TRUNCATION_NOTICE.len()));
        let truncated = truncate_message(input.clone());
        assert!(truncated.len() <= MAX_MESSAGE_BYTES);
        assert!(input.starts_with(truncated.trim_end_matches(TRUNCATION_NOTICE)));
        assert_eq!(truncate_message("short".to_string()), "short");
    }

    #[test]
    fn collection_errors_survive_tag_heavy_candidate_truncation() {
        let mut metrics = (0..10)
            .map(|idx| {
                let tags = (0..20)
                    .map(|tag| format!("tag{tag}:shard{idx}{}", "€".repeat(20)))
                    .collect::<Vec<_>>();
                let tags = tags.iter().map(String::as_str).collect::<Vec<_>>();
                metric("some.metric", &tags, &[count(1.0)])
            })
            .collect::<Vec<_>>();
        metrics.push(metric("broken", &[], &[count(1.0), gauge(2.0)]));

        let message = exact("some.metric", &[]).failure_message(
            "Summary.".to_string(),
            Some(&snapshot(&metrics)),
            Some(&LastError {
                error: generic_error!("connection refused").context("Failed to call endpoint."),
                before_snapshot: false,
            }),
        );

        assert!(message.len() <= MAX_MESSAGE_BYTES, "{}", message.len());
        assert!(message.ends_with(TRUNCATION_NOTICE), "{}", message);
        assert!(
            message.contains("Last collection error, after the last successful snapshot: Failed to call endpoint.: connection refused"),
            "{}",
            message
        );
        assert!(
            message.contains("1 normalization failure(s) under other names ignored."),
            "{}",
            message
        );
    }

    #[test]
    fn the_last_collection_error_says_when_it_happened() {
        let assertion = subset("m", None, None, &[]);
        let observed = snapshot(&[metric("m", &["a:b"], &[count(1.0)])]);
        let chained = || -> GenericError { generic_error!("connection refused").context("Failed to call endpoint.") };

        let after = assertion.failure_message(
            String::new(),
            Some(&observed),
            Some(&LastError {
                error: chained(),
                before_snapshot: false,
            }),
        );
        assert!(
            after.starts_with("\nLast collection error, after the last successful snapshot: Failed to call endpoint.: connection refused"),
            "{}",
            after
        );

        let before = assertion.failure_message(
            String::new(),
            Some(&observed),
            Some(&LastError {
                error: chained(),
                before_snapshot: true,
            }),
        );
        assert!(
            before.starts_with("\nLast collection error, before the last successful snapshot: Failed to call endpoint.: connection refused"),
            "{}",
            before
        );
    }
}
