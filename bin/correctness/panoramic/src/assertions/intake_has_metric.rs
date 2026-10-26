use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::Duration;

use saluki_error::{generic_error, GenericError};
use stele::{Metric, MetricValue};
use tokio::time::Instant;
use tracing::trace;

use crate::assertions::{Assertion, AssertionContext, AssertionResult, AssertionStatus};
use crate::config::{MetricTypeMatcher, TagMatch};
use crate::intake::metrics::{
    get_formatted_metric_value, MetricType, NormalizationFailure, NormalizedMetric, NormalizedMetricContext,
    NormalizedMetrics,
};
use crate::intake::IntakeClient;
use crate::probe::{probe, ProbeError, PROBE_TIMEOUT};

/// Names the intake request in probe messages.
const METRICS_DUMP: &str = "the intake sidecar's metrics dump";

const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Most metric contexts with the expected name that a failure message lists.
const MAX_CANDIDATES: usize = 10;

/// Most other metric names a failure message lists when no metric with the expected name was observed.
const MAX_OTHER_NAMES: usize = 10;

/// Most bytes in a failure message, including [`TRUNCATION_NOTICE`].
const MAX_MESSAGE_BYTES: usize = 4096;

const TRUNCATION_NOTICE: &str = "\n[message truncated]";

/// Assertion that polls the intake sidecar for a metric matching the configured criteria.
///
/// A metric context is a metric name plus its normalized tag set, with the host as a `host:` tag; see
/// [`NormalizedMetricContext`]. Each poll normalizes the intake's metrics dump per (metric context, type), so a value
/// split across flushes is compared as its total. The assertion passes once one metric context meets every criterion by
/// itself; metric contexts are never combined.
pub struct IntakeHasMetricAssertion {
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

impl IntakeHasMetricAssertion {
    /// Creates the assertion. Expected tags are sorted and deduplicated like observed ones.
    ///
    /// # Errors
    ///
    /// If `value` is combined with a sketch `metric_type`, which no observed metric can match, an error is returned.
    pub fn new(
        name: String, metric_type: Option<MetricTypeMatcher>, value: Option<f64>, tags: Vec<String>,
        tags_match: TagMatch, timeout: Duration,
    ) -> Result<Self, GenericError> {
        let metric_type = metric_type.map(|metric_type| match metric_type {
            MetricTypeMatcher::Count => MetricType::Count,
            MetricTypeMatcher::Rate => MetricType::Rate,
            MetricTypeMatcher::Gauge => MetricType::Gauge,
            MetricTypeMatcher::Sketch => MetricType::Sketch,
        });
        if value.is_some() && metric_type == Some(MetricType::Sketch) {
            return Err(generic_error!(
                "intake_has_metric `{}` can't combine `value` with `metric_type: sketch`: a sketch has no single \
                 value to compare",
                name
            ));
        }

        Ok(Self {
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

    /// Reports an unmatched metric as `failed` if the target exited or a poll returned a snapshot, otherwise `errored`.
    fn unmatched_result(
        &self, started: Instant, summary: String, target_exited: bool, snapshot: Option<&Snapshot>,
        last_error: Option<&LastError>,
    ) -> AssertionResult {
        let status = if target_exited || snapshot.is_some() {
            AssertionStatus::Failed
        } else {
            AssertionStatus::Errored
        };
        AssertionResult::new(
            self.name(),
            started,
            status,
            self.failure_message(summary, snapshot, last_error),
        )
    }

    /// Reports a step timeout, leading with the last collection error if every poll failed.
    fn expired_result(
        &self, started: Instant, snapshot: Option<&Snapshot>, last_error: Option<&LastError>,
    ) -> AssertionResult {
        let summary = format!(
            "No metric matching {} arrived at the intake within {:?}.",
            self.criteria(),
            self.timeout
        );
        match (snapshot, last_error) {
            (None, Some(last_error)) => {
                let message = format!(
                    "{:#}\n{}\nNo successful snapshot of the intake's metrics.",
                    last_error.error, summary
                );
                AssertionResult::new(
                    self.name(),
                    started,
                    AssertionStatus::Errored,
                    truncate_message(message),
                )
            }
            _ => self.unmatched_result(started, summary, false, snapshot, last_error),
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
            None => message.push_str("\nNo successful snapshot of the intake's metrics."),
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
impl Assertion for IntakeHasMetricAssertion {
    fn name(&self) -> &'static str {
        "intake_has_metric"
    }

    fn description(&self) -> String {
        format!("Intake received a metric matching {}.", self.criteria())
    }

    async fn check(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let Some(intake_port) = ctx.intake_host_port else {
            return AssertionResult::new(
                self.name(),
                started,
                AssertionStatus::Errored,
                "No intake sidecar for this test; set `intake.enabled: true`.".to_string(),
            );
        };

        let client = IntakeClient::new(intake_port);
        let deadline = started + self.timeout;
        let mut snapshot = None;
        let mut last_error: Option<LastError> = None;
        let mut polled = false;

        loop {
            // Read the exit state before polling so a metric that reached the intake just before
            // the target exited still counts; see the post-poll check below.
            let exited = ctx.container_exit_token.is_cancelled();

            if ctx.cancel_token.is_cancelled() {
                let summary = format!(
                    "Assertion cancelled. No metric matching {} had arrived at the intake.",
                    self.criteria()
                );
                return self.unmatched_result(started, summary, false, snapshot.as_ref(), last_error.as_ref());
            }

            // Poll at least once, even with no time left.
            if polled && Instant::now() >= deadline {
                return self.expired_result(started, snapshot.as_ref(), last_error.as_ref());
            }
            polled = true;

            let limit = deadline.saturating_duration_since(Instant::now()).min(PROBE_TIMEOUT);
            let error = match probe(METRICS_DUMP, limit, &ctx.cancel_token, client.metrics()).await {
                Ok(Ok(metrics)) => {
                    let current = Snapshot::from_stele_metrics(&metrics);
                    if self.is_met_by(&current.metrics) {
                        return AssertionResult::new(self.name(), started, AssertionStatus::Passed, self.description());
                    }
                    snapshot = Some(current);
                    if let Some(last_error) = last_error.as_mut() {
                        last_error.before_snapshot = true;
                    }
                    None
                }
                Ok(Err(error)) => Some(error),
                Err(error @ ProbeError::TimedOut { .. }) => Some(error.into()),
                Err(error @ ProbeError::Cancelled { .. }) => {
                    let summary = format!(
                        "{}\nNo metric matching {} had arrived at the intake.",
                        error,
                        self.criteria()
                    );
                    return self.unmatched_result(started, summary, false, snapshot.as_ref(), last_error.as_ref());
                }
            };
            if let Some(error) = error {
                last_error = Some(LastError {
                    error,
                    before_snapshot: false,
                });
            }

            trace!(metric = self.expected.name(), "Intake metric not present yet.");

            // Nothing more can reach the intake once the target is gone, so report the crash
            // instead of polling until the timeout.
            if exited {
                let summary = format!(
                    "No metric matching {} arrived at the intake before the target exited.",
                    self.criteria()
                );
                return self.unmatched_result(started, summary, true, snapshot.as_ref(), last_error.as_ref());
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL.min(remaining)) => {}
                _ = ctx.cancel_token.cancelled() => {}
                _ = ctx.container_exit_token.cancelled() => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    use saluki_error::{generic_error, GenericError};
    use serde_json::{json, Value};
    use stele::Metric;
    use tokio_util::sync::CancellationToken;

    use super::{
        truncate_message, IntakeHasMetricAssertion, LastError, Snapshot, MAX_MESSAGE_BYTES, TRUNCATION_NOTICE,
    };
    use crate::assertions::{
        create_assertion, Assertion as _, AssertionContext, AssertionResult, AssertionStatus, LogBuffer, TargetCommand,
    };
    use crate::config::{AssertionConfig, AssertionStep, IntegrationConfig, MetricTypeMatcher, TagMatch};
    use crate::intake::test_server::{closed_port, install_crypto_provider, MockIntake};

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
        name: &str, metric_type: Option<MetricTypeMatcher>, value: Option<f64>, tags: &[&str], tags_match: TagMatch,
    ) -> IntakeHasMetricAssertion {
        IntakeHasMetricAssertion::new(
            name.to_string(),
            metric_type,
            value,
            tags.iter().map(|t| t.to_string()).collect(),
            tags_match,
            Duration::from_secs(1),
        )
        .expect("assertion should build")
    }

    fn subset(
        name: &str, metric_type: Option<MetricTypeMatcher>, value: Option<f64>, tags: &[&str],
    ) -> IntakeHasMetricAssertion {
        assertion(name, metric_type, value, tags, TagMatch::Subset)
    }

    fn exact(name: &str, tags: &[&str]) -> IntakeHasMetricAssertion {
        assertion(name, None, None, tags, TagMatch::Exact)
    }

    fn failure_message(assertion: &IntakeHasMetricAssertion, metrics: &[Metric]) -> String {
        assertion.failure_message("Summary.".to_string(), Some(&snapshot(metrics)), None)
    }

    /// Returns a context pointing at `intake_port`, or at a port with no listener when it is `None`.
    fn context_for(
        cancel_token: CancellationToken, container_exit_token: CancellationToken, intake_port: Option<u16>,
    ) -> AssertionContext {
        install_crypto_provider();

        AssertionContext {
            log_buffer: Arc::new(RwLock::new(LogBuffer::default())),
            container_exit_token,
            cancel_token,
            port_mappings: HashMap::new(),
            container_ip: None,
            target_os: None,
            container_name: "intake-has-metric-test".to_string(),
            is_host_process: false,
            host_process_exit_code: None,
            docker_container_exit_code: None,
            intake_host_port: Some(intake_port.unwrap_or_else(closed_port)),
            core_agent_auth_token_path: None,
            adp_cli_command: TargetCommand::new(vec!["panoramic-unused-cli-program".to_string()]),
            core_agent_cli_command: TargetCommand::new(vec!["panoramic-unused-cli-program".to_string()]),
        }
    }

    /// Returns a context pointing at a port with no listener, so every intake poll fails.
    fn context(cancel_token: CancellationToken, container_exit_token: CancellationToken) -> AssertionContext {
        context_for(cancel_token, container_exit_token, None)
    }

    fn polling_assertion(timeout: Duration) -> IntakeHasMetricAssertion {
        IntakeHasMetricAssertion::new(
            "some.counter".to_string(),
            None,
            None,
            Vec::new(),
            TagMatch::Subset,
            timeout,
        )
        .expect("assertion should build")
    }

    /// Runs the assertion with a timeout to catch hangs.
    async fn check(assertion: &IntakeHasMetricAssertion, ctx: &AssertionContext) -> AssertionResult {
        tokio::time::timeout(Duration::from_secs(10), assertion.check(ctx))
            .await
            .expect("the assertion should return within ten seconds")
    }

    #[tokio::test]
    async fn target_exit_fails_without_waiting_out_the_timeout() {
        let container_exit_token = CancellationToken::new();
        container_exit_token.cancel();
        let ctx = context(CancellationToken::new(), container_exit_token);

        let result = check(&polling_assertion(Duration::from_secs(600)), &ctx).await;

        assert_eq!(result.status, AssertionStatus::Failed, "{}", result.message);
        assert!(
            result.message.contains("before the target exited"),
            "unexpected message: {}",
            result.message
        );
    }

    #[tokio::test]
    async fn missing_intake_port_fails_immediately() {
        let mut ctx = context(CancellationToken::new(), CancellationToken::new());
        ctx.intake_host_port = None;

        let result = check(&polling_assertion(Duration::from_secs(600)), &ctx).await;

        assert_eq!(result.status, AssertionStatus::Errored, "{}", result.message);
        assert!(
            result.message.contains("intake.enabled"),
            "unexpected message: {}",
            result.message
        );
    }

    #[tokio::test]
    async fn cancellation_and_timeout_are_reported_distinctly() {
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();
        let cancelled = check(
            &polling_assertion(Duration::from_secs(600)),
            &context(cancel_token, CancellationToken::new()),
        )
        .await;

        assert_eq!(cancelled.status, AssertionStatus::Errored, "{}", cancelled.message);
        assert!(
            cancelled.message.starts_with("Assertion cancelled."),
            "unexpected message: {}",
            cancelled.message
        );
        assert!(
            cancelled.message.contains("No poll completed."),
            "{}",
            cancelled.message
        );

        let timed_out = check(
            &polling_assertion(Duration::from_millis(300)),
            &context(CancellationToken::new(), CancellationToken::new()),
        )
        .await;

        assert_eq!(timed_out.status, AssertionStatus::Errored, "{}", timed_out.message);
        assert!(
            timed_out.message.contains("arrived at the intake within"),
            "unexpected message: {}",
            timed_out.message
        );
    }

    #[tokio::test]
    async fn a_failed_poll_reports_its_full_error_chain() {
        let intake = MockIntake::hanging_up().await;
        let result = check(
            &polling_assertion(Duration::from_millis(300)),
            &context_for(CancellationToken::new(), CancellationToken::new(), Some(intake.port())),
        )
        .await;

        assert_eq!(result.status, AssertionStatus::Errored, "{}", result.message);
        assert!(
            result
                .message
                .contains("\nNo successful snapshot of the intake's metrics."),
            "unexpected message: {}",
            result.message
        );
        // The outer context alone would hide the cause, so the chain continues past it.
        let (first_line, _) = result
            .message
            .split_once('\n')
            .expect("the message should have several lines");
        assert!(
            first_line
                .strip_prefix("Failed to call metrics dump endpoint on datadog-intake server.: ")
                .is_some_and(|cause| !cause.is_empty()),
            "unexpected message: {}",
            result.message
        );
        assert!(!result.message.contains("Last collection error"), "{}", result.message);
    }

    #[tokio::test]
    async fn polling_matches_a_rate_split_across_flushes_by_its_total() {
        let intake = MockIntake::serving(json!([
            metric_json("split.counter", &["source:test"], &[rate(0.3)]),
            metric_json("split.counter", &["source:test"], &[rate(0.3)]),
        ]))
        .await;
        let ctx = context_for(CancellationToken::new(), CancellationToken::new(), Some(intake.port()));

        let total = check(
            &subset(
                "split.counter",
                Some(MetricTypeMatcher::Rate),
                Some(0.6),
                &["source:test"],
            ),
            &ctx,
        )
        .await;
        assert_eq!(total.status, AssertionStatus::Passed, "{}", total.message);

        let mut partial = subset(
            "split.counter",
            Some(MetricTypeMatcher::Rate),
            Some(0.3),
            &["source:test"],
        );
        partial.timeout = Duration::from_millis(300);
        let partial = check(&partial, &ctx).await;
        assert_eq!(partial.status, AssertionStatus::Failed, "{}", partial.message);
        assert!(
            partial.message.contains(
                "split.counter[source:test] (rate): rate(0.6 over 10s) from 2 point(s); fails value: expected 0.3"
            ),
            "unexpected message: {}",
            partial.message
        );
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

        assert!(met(MetricTypeMatcher::Count, 3.0));
        assert!(met(MetricTypeMatcher::Gauge, 7.0));

        // Each criterion holds for one context, but no single context satisfies both.
        assert!(!met(MetricTypeMatcher::Count, 7.0));
        assert!(!met(MetricTypeMatcher::Gauge, 3.0));

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
        let total = subset("some.counter", Some(MetricTypeMatcher::Rate), Some(0.6), &[]);
        let partial = subset("some.counter", Some(MetricTypeMatcher::Rate), Some(0.3), &[]);

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
        let result = IntakeHasMetricAssertion::new(
            "some.sketch".to_string(),
            Some(MetricTypeMatcher::Sketch),
            Some(1.0),
            Vec::new(),
            TagMatch::Subset,
            Duration::from_secs(1),
        );
        let error = result.err().expect("value with a sketch type should be rejected");
        assert!(
            error
                .to_string()
                .contains("can't combine `value` with `metric_type: sketch`"),
            "unexpected error: {}",
            error
        );

        // Each one alone is valid.
        assert!(IntakeHasMetricAssertion::new(
            "some.sketch".to_string(),
            Some(MetricTypeMatcher::Sketch),
            None,
            Vec::new(),
            TagMatch::Subset,
            Duration::from_secs(1),
        )
        .is_ok());
        assert!(IntakeHasMetricAssertion::new(
            "some.sketch".to_string(),
            None,
            Some(1.0),
            Vec::new(),
            TagMatch::Subset,
            Duration::from_secs(1),
        )
        .is_ok());
    }

    #[tokio::test]
    async fn a_yaml_exact_tags_match_reaches_the_built_assertion() {
        let intake = MockIntake::serving(json!([metric_json(
            "hosttag.metric",
            &["host:custom", "extra:tag"],
            &[gauge(4.0)]
        )]))
        .await;
        let ctx = context_for(CancellationToken::new(), CancellationToken::new(), Some(intake.port()));

        let build = |tags_match: &str| {
            let yaml = format!(
                r#"
type: integration
name: tags-match-case
timeout: 60s
procedure:
  - assertion: intake_has_metric
    name: "hosttag.metric"
    metric_type: gauge
    value: 4
    tags: ["host:custom"]
    {}
    timeout: 300ms
"#,
                tags_match
            );
            let config: IntegrationConfig = serde_saphyr::from_str(&yaml).expect("case should parse");
            let AssertionStep::Single(assertion_config) = &config.procedure[0] else {
                panic!("step should parse as a single assertion");
            };
            assert!(matches!(assertion_config, AssertionConfig::IntakeHasMetric { .. }));
            create_assertion(assertion_config).expect("assertion should build")
        };

        let exact = tokio::time::timeout(Duration::from_secs(10), build("tags_match: exact").check(&ctx))
            .await
            .expect("the assertion should return within ten seconds");
        assert_eq!(exact.status, AssertionStatus::Failed, "{}", exact.message);
        assert!(
            exact.message.contains("fails tags: unexpected [extra:tag]"),
            "unexpected message: {}",
            exact.message
        );

        let default = tokio::time::timeout(Duration::from_secs(10), build("").check(&ctx))
            .await
            .expect("the assertion should return within ten seconds");
        assert_eq!(default.status, AssertionStatus::Passed, "{}", default.message);
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
                Some(MetricTypeMatcher::Gauge),
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
            &subset("some.counter", Some(MetricTypeMatcher::Count), None, &[]),
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
