use std::{collections::BTreeMap, fmt};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use stele::{Metric, MetricValue};

/// The type of a metric value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MetricType {
    Count,
    Rate,
    Gauge,
    Sketch,
}

impl fmt::Display for MetricType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetricType::Count => write!(f, "count"),
            MetricType::Rate => write!(f, "rate"),
            MetricType::Gauge => write!(f, "gauge"),
            MetricType::Sketch => write!(f, "sketch"),
        }
    }
}

/// A normalized metric context.
///
/// # Normalization behavior
///
/// - Tags are sorted and deduplicated in a case-sensitive fashion.
/// - The host appears as a `host:<value>` tag (the stele parsers fold it in from each wire format's host field),
///   so it participates in identity comparisons like any other tag.
#[derive(Clone, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct NormalizedMetricContext {
    name: String,
    tags: Vec<String>,
}

impl NormalizedMetricContext {
    /// Creates a normalized context from a name and tags, sorting and deduplicating the tags.
    pub fn new(name: String, mut tags: Vec<String>) -> Self {
        tags.sort_unstable();
        tags.dedup();

        Self { name, tags }
    }

    /// Returns the name of the metric.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the tags, sorted and deduplicated.
    pub fn tags(&self) -> &[String] {
        &self.tags
    }
}

impl fmt::Display for NormalizedMetricContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;

        if !self.tags.is_empty() {
            write!(f, "[{}]", self.tags.join(", "))?;
        }

        Ok(())
    }
}

/// A normalized metric.
///
/// # Normalization behavior
///
/// - Context tags are sorted lexicographically, in ascending order, and deduplicated.
/// - Tag sorting/deduplication is case-sensitive.
/// - Raw values are sorted by timestamp in ascending order.
/// - Normalized value is the latest seen value for gauges, the accumulated sum of values for counters, rates, and
///   sketches.
///
/// # Raw values vs normalized value
///
/// During a test run, we may sometimes see a single update to a metric, or multiple updates. This is due to well-known
/// behavior, such as aggregation working on a fixed interval, where some of the input packets to the system under test
/// (SUT) were processed during one aggregation window, and the remaining packets were processed during the next window.
/// While this is normal and expected, it introduces challenges when trying to compare the outputs of one SUT to
/// another.
///
/// When we "normalize" raw metrics during the creation of `NormalizedMetric`, we effectively resolve the sum total of
/// all of the relevant metric updates by processing those updates in a similar fashion to how they would be processed
/// when sent directly to the Datadog backend. For example, gauges have "last-write-wins" semantics, so comparing a
/// gauge between two SUTs boils down to comparing the last value seen for that gauge by both SUTs. We don't need to
/// care about any prior values, as they would have been overwritten by the latest value.
///
/// We apply a similar approach for counters/rates/sketches (they're aggregations, so we just aggregate further) and
/// this leaves us with a single value, regardless of metric type, that represents the latest view of the metric after a
/// test has concluded, and is stable for the purposes of comparing the output of one SUT to another.
#[derive(Clone)]
pub struct NormalizedMetric {
    context: NormalizedMetricContext,
    raw_values: Vec<(u64, MetricValue)>,
    value: MetricValue,
}

impl NormalizedMetric {
    /// Attempts to create `NormalizedMetric` from the given context and raw metric values.
    ///
    /// # Errors
    ///
    /// If the raw values are empty, or if the values can't be normalized, an error is returned.
    pub fn try_from_values(
        context: NormalizedMetricContext, mut raw_values: Vec<(u64, MetricValue)>,
    ) -> Result<Self, GenericError> {
        // We need to first sort the raw values by timestamp, to ensure we have proper ordering semantics.
        raw_values.sort_by_key(|a| a.0);
        let value = try_normalize_values(&raw_values)
            .with_error_context(|| format!("Failed to normalize values for metric '{}'", context.name()))?;

        Ok(Self {
            context,
            raw_values,
            value,
        })
    }

    /// Returns the context of the metric.
    pub fn context(&self) -> &NormalizedMetricContext {
        &self.context
    }

    /// Returns the type of the metric value.
    pub fn value_type(&self) -> MetricType {
        metric_value_type(&self.raw_values)
    }

    /// Returns the raw values of the metric.
    pub fn raw_values(&self) -> &[(u64, MetricValue)] {
        &self.raw_values
    }

    /// Returns the normalized value of the metric.
    ///
    /// This represents the latest view of the metric after all raw values are processed, similar to what might be
    /// returned if the metrics storage backends were queried for the sum of all updates to a counter, or the latest
    /// value seen for a gauge, in a given time window.
    ///
    /// More information about normalization can be found in the comments for [`NormalizedMetric`] itself.
    pub fn normalized_value(&self) -> &MetricValue {
        &self.value
    }
}

/// A set of normalized metrics.
///
/// # Normalization behavior
///
/// - Metrics are sorted lexicographically by context (name, and then tags), and then type, in ascending order.
/// - Context tags are sorted lexicographically, in ascending order, and deduplicated.
/// - Tag sorting/deduplication is case-sensitive.
/// - Metrics are grouped by context and by the type of each entry's first value. An entry with no values falls into the
///   count group.
/// - Raw values for the same group, split across multiple metric entries, are combined. Values with the same timestamp
///   are all kept.
/// - Raw values are sorted by timestamp in ascending order. The sort is stable, so values with the same timestamp stay
///   in dump order, and a gauge resolves them by taking the one dumped last.
/// - Normalized value is the latest seen value for gauges, the accumulated sum of values for counters, rates, and
///   sketches.
/// - A group fails to normalize if it mixes value types (including within one entry) or holds rates with different
///   intervals.
pub struct NormalizedMetrics {
    metrics: Vec<NormalizedMetric>,
}

/// A group of metric entries that could not be normalized.
pub struct NormalizationFailure {
    /// The group's context.
    pub context: NormalizedMetricContext,

    /// The group's type, taken from the first value of its first entry.
    pub metric_type: MetricType,

    /// Why the group could not be normalized.
    pub error: GenericError,
}

impl NormalizedMetrics {
    /// Creates `NormalizedMetrics` from the given set of metrics, normalizing each (context, type) group independently.
    ///
    /// A group that fails to normalize is left out of the set and reported as a [`NormalizationFailure`] instead, so
    /// one malformed group does not hide the others. Failures are sorted like the set. An empty input yields an empty
    /// set.
    pub fn from_stele_metrics(metrics: &[Metric]) -> (Self, Vec<NormalizationFailure>) {
        let mut normalized = Vec::new();
        let mut failures = Vec::new();

        for ((context, metric_type), values) in group_by_context_and_type(metrics) {
            match NormalizedMetric::try_from_values(context.clone(), values) {
                Ok(metric) => normalized.push(metric),
                Err(error) => failures.push(NormalizationFailure {
                    context,
                    metric_type,
                    error,
                }),
            }
        }

        (Self { metrics: normalized }, failures)
    }

    /// Attempts to create `NormalizedMetrics` from the given set of metrics.
    ///
    /// # Errors
    ///
    /// If the set of metrics is empty, or if any of the metrics can't be normalized, an error is returned. For a
    /// normalization failure, the error is that of the first failed group in sorted order.
    pub fn try_from_stele_metrics(metrics: &[Metric]) -> Result<Self, GenericError> {
        if metrics.is_empty() {
            return Err(generic_error!("Cannot normalize an empty set of metrics."));
        }

        let (normalized, failures) = Self::from_stele_metrics(metrics);
        match failures.into_iter().next() {
            Some(failure) => Err(failure.error).error_context("Failed to normalize metrics."),
            None => Ok(normalized),
        }
    }

    /// Returns the number of metrics in the set.
    pub fn len(&self) -> usize {
        self.metrics.len()
    }

    /// Returns the metrics in the set.
    pub fn metrics(&self) -> &[NormalizedMetric] {
        &self.metrics
    }

    /// Removes metrics that match the given predicate.
    ///
    /// If `match` returns `true` for a given metric, it will be removed from the provided `metrics` vector.
    ///
    /// Returns all of the metrics that matched the predicate.
    pub fn remove_matching<F>(&mut self, predicate: F) -> Vec<NormalizedMetric>
    where
        F: Fn(&NormalizedMetric) -> bool,
    {
        let mut filtered = self.metrics.clone();
        filtered.retain(|metric| !predicate(metric));
        self.metrics.retain(|metric| predicate(metric));

        std::mem::replace(&mut self.metrics, filtered)
    }
}

/// Groups metric values by normalized (context, type), in sorted order.
///
/// Metrics with the same context but different types (for example, Count vs Rate) cannot be merged together during
/// normalization, so they form separate groups.
fn group_by_context_and_type(
    metrics: &[Metric],
) -> BTreeMap<(NormalizedMetricContext, MetricType), Vec<(u64, MetricValue)>> {
    let mut groups = BTreeMap::new();

    for metric in metrics {
        let (name, tags) = metric.context().clone().into_parts();
        let key = (
            NormalizedMetricContext::new(name, tags),
            metric_value_type(metric.values()),
        );
        let values = groups.entry(key).or_insert_with(Vec::new);
        values.extend_from_slice(metric.values());
    }

    groups
}

fn metric_value_type(values: &[(u64, MetricValue)]) -> MetricType {
    match values.first() {
        Some((_, MetricValue::Count { .. })) => MetricType::Count,
        Some((_, MetricValue::Rate { .. })) => MetricType::Rate,
        Some((_, MetricValue::Gauge { .. })) => MetricType::Gauge,
        Some((_, MetricValue::Sketch { .. })) => MetricType::Sketch,
        // Empty entries join the Count group; normalization validates the combined values.
        None => MetricType::Count,
    }
}

fn try_normalize_values(raw_values: &[(u64, MetricValue)]) -> Result<MetricValue, GenericError> {
    // PRECONDITION: raw_values is sorted by timestamp in ascending order.

    // Nothing to normalize if there are no values.
    if raw_values.is_empty() {
        return Err(generic_error!("Cannot normalize an empty set of values."));
    }

    let mut values = raw_values.iter();

    // We take the first value as the initial value, and then keep merging subsequent values into it.
    let (_, first_value) = values.next().unwrap();
    let mut current_value = first_value.clone();

    for (_, new_value) in values {
        match (&mut current_value, new_value) {
            (MetricValue::Count { value: value_a }, MetricValue::Count { value: value_b }) => {
                *value_a += value_b;
            }
            (
                MetricValue::Rate {
                    interval: interval_a,
                    value: value_a,
                },
                MetricValue::Rate {
                    interval: interval_b,
                    value: value_b,
                },
            ) => {
                if interval_a != interval_b {
                    return Err(generic_error!("Cannot normalize rate values with different intervals."));
                }

                *value_a += value_b;
            }
            (MetricValue::Gauge { value: value_a }, MetricValue::Gauge { value: value_b }) => {
                *value_a = *value_b;
            }
            (MetricValue::Sketch { sketch: sketch_a }, MetricValue::Sketch { sketch: sketch_b }) => {
                sketch_a.merge(sketch_b);
            }
            _ => return Err(generic_error!("Cannot normalize values of different types.")),
        }
    }

    Ok(current_value)
}

/// Formats a metric value for diagnostics, showing a sketch as summary statistics.
pub(crate) fn get_formatted_metric_value(value: &MetricValue) -> String {
    match value {
        MetricValue::Count { value } => format!("count({})", value),
        MetricValue::Rate { interval, value } => format!("rate({} over {}s)", value, interval),
        MetricValue::Gauge { value } => format!("gauge({})", value),
        MetricValue::Sketch { sketch } => format!(
            "sketch(min={} max={} avg={} sum={} cnt={} bins_n={})",
            sketch.min().unwrap_or(0.0),
            sketch.max().unwrap_or(0.0),
            sketch.avg().unwrap_or(0.0),
            sketch.sum().unwrap_or(0.0),
            sketch.count(),
            sketch.bin_count(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use stele::{Metric, MetricValue};

    use super::{MetricType, NormalizedMetricContext, NormalizedMetrics};

    fn metric(name: &str, tags: &[&str], points: &[(u64, f64)]) -> Metric {
        let values = points
            .iter()
            .map(|(ts, value)| (*ts, json!({"mtype": "Count", "value": value})))
            .collect::<Vec<_>>();
        metric_with_values(name, tags, &values)
    }

    fn metric_with_values(name: &str, tags: &[&str], values: &[(u64, Value)]) -> Metric {
        serde_json::from_value(json!({
            "context": {
                "name": name,
                "tags": tags,
            },
            "values": values.iter().map(|(ts, value)| json!([ts, value])).collect::<Vec<_>>(),
        }))
        .expect("metric should deserialize")
    }

    fn rate(ts: u64, interval: u64, value: f64) -> (u64, Value) {
        (ts, json!({"mtype": "Rate", "interval": interval, "value": value}))
    }

    fn gauge(ts: u64, value: f64) -> (u64, Value) {
        (ts, json!({"mtype": "Gauge", "value": value}))
    }

    fn count(ts: u64, value: f64) -> (u64, Value) {
        (ts, json!({"mtype": "Count", "value": value}))
    }

    fn normalize_all(metrics: &[Metric]) -> NormalizedMetrics {
        let (normalized, failures) = NormalizedMetrics::from_stele_metrics(metrics);
        assert!(failures.is_empty(), "unexpected normalization failures");
        normalized
    }

    #[test]
    fn normalizes_context_order_before_grouping_metrics() {
        let metrics = vec![
            metric("tagfilter.miss", &["pod:pod-a", "env:prod"], &[(10, 2.0)]),
            metric("tagfilter.miss", &["env:prod", "pod:pod-a"], &[(20, 3.0)]),
        ];

        let normalized = NormalizedMetrics::try_from_stele_metrics(&metrics).expect("metrics should normalize");

        assert_eq!(normalized.len(), 1);
        assert_eq!(
            normalized.metrics()[0].context().to_string(),
            "tagfilter.miss[env:prod, pod:pod-a]"
        );
        assert_eq!(normalized.metrics()[0].raw_values().len(), 2);
    }

    #[test]
    fn context_tags_are_sorted_and_deduplicated() {
        let context = NormalizedMetricContext::new(
            "some.metric".to_string(),
            vec!["b:2".to_string(), "a:1".to_string(), "b:2".to_string()],
        );

        assert_eq!(context.tags(), ["a:1".to_string(), "b:2".to_string()]);
    }

    #[test]
    fn a_rate_split_across_flushes_normalizes_to_its_total() {
        let one_flush = normalize_all(&[metric_with_values("r", &[], &[rate(10, 10, 0.6)])]);
        let two_flushes = normalize_all(&[
            metric_with_values("r", &[], &[rate(10, 10, 0.3)]),
            metric_with_values("r", &[], &[rate(20, 10, 0.3)]),
        ]);

        let expected = MetricValue::Rate {
            interval: 10,
            value: 0.6,
        };
        assert_eq!(one_flush.metrics()[0].normalized_value(), &expected);
        assert_eq!(two_flushes.len(), 1);
        assert_eq!(two_flushes.metrics()[0].normalized_value(), &expected);
        assert_eq!(two_flushes.metrics()[0].raw_values().len(), 2);
    }

    #[test]
    fn unequal_rate_intervals_fail_only_their_group() {
        let (normalized, failures) = NormalizedMetrics::from_stele_metrics(&[
            metric_with_values("bad.rate", &[], &[rate(10, 10, 1.0)]),
            metric_with_values("bad.rate", &[], &[rate(20, 20, 1.0)]),
            metric_with_values("good.rate", &[], &[rate(10, 10, 1.0)]),
        ]);

        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized.metrics()[0].context().name(), "good.rate");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].context.name(), "bad.rate");
        assert_eq!(failures[0].metric_type, MetricType::Rate);
        assert_eq!(
            format!("{:#}", failures[0].error),
            "Failed to normalize values for metric 'bad.rate': Cannot normalize rate values with different intervals."
        );
    }

    #[test]
    fn count_and_rate_on_one_context_form_two_groups() {
        let normalized = normalize_all(&[
            metric_with_values("m", &["env:test"], &[count(10, 3.0)]),
            metric_with_values("m", &["env:test"], &[rate(10, 10, 0.3)]),
        ]);

        let groups = normalized
            .metrics()
            .iter()
            .map(|m| (m.context().to_string(), m.value_type(), m.normalized_value().clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            groups,
            vec![
                (
                    "m[env:test]".to_string(),
                    MetricType::Count,
                    MetricValue::Count { value: 3.0 }
                ),
                (
                    "m[env:test]".to_string(),
                    MetricType::Rate,
                    MetricValue::Rate {
                        interval: 10,
                        value: 0.3
                    }
                ),
            ]
        );
    }

    #[test]
    fn a_gauge_takes_its_latest_timestamp_and_resolves_ties_by_dump_order() {
        let normalized_value = |entries: &[(u64, f64)]| {
            let metrics = entries
                .iter()
                .map(|(ts, value)| metric_with_values("g", &[], &[gauge(*ts, *value)]))
                .collect::<Vec<_>>();
            normalize_all(&metrics).metrics()[0].normalized_value().clone()
        };

        // The latest timestamp wins, wherever it sits in the dump.
        assert_eq!(
            normalized_value(&[(20, 2.0), (10, 1.0)]),
            MetricValue::Gauge { value: 2.0 }
        );
        assert_eq!(
            normalized_value(&[(10, 1.0), (20, 2.0)]),
            MetricValue::Gauge { value: 2.0 }
        );

        // Equal timestamps are all kept, and the one dumped last wins.
        assert_eq!(
            normalized_value(&[(10, 1.0), (10, 2.0)]),
            MetricValue::Gauge { value: 2.0 }
        );
        assert_eq!(
            normalized_value(&[(10, 2.0), (10, 1.0)]),
            MetricValue::Gauge { value: 1.0 }
        );
    }

    #[test]
    fn an_entry_with_no_values_falls_into_the_count_group() {
        // Alone, its count group has nothing to normalize.
        let (normalized, failures) = NormalizedMetrics::from_stele_metrics(&[metric_with_values("e", &[], &[])]);
        assert_eq!(normalized.len(), 0);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].metric_type, MetricType::Count);
        assert_eq!(
            format!("{:#}", failures[0].error),
            "Failed to normalize values for metric 'e': Cannot normalize an empty set of values."
        );

        // Beside a count on the same context, it joins that group and contributes nothing.
        let normalized = normalize_all(&[
            metric_with_values("e", &[], &[]),
            metric_with_values("e", &[], &[count(10, 3.0)]),
        ]);
        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized.metrics()[0].value_type(), MetricType::Count);
        assert_eq!(
            normalized.metrics()[0].normalized_value(),
            &MetricValue::Count { value: 3.0 }
        );
    }

    #[test]
    fn a_mixed_type_entry_invalidates_its_well_formed_siblings() {
        let (normalized, failures) = NormalizedMetrics::from_stele_metrics(&[
            metric_with_values("mixed", &[], &[count(10, 1.0), gauge(20, 2.0)]),
            metric_with_values("mixed", &[], &[count(30, 3.0)]),
            metric_with_values("other", &[], &[count(10, 1.0)]),
        ]);

        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized.metrics()[0].context().name(), "other");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].context.name(), "mixed");
        assert_eq!(failures[0].metric_type, MetricType::Count);
        assert_eq!(
            format!("{:#}", failures[0].error),
            "Failed to normalize values for metric 'mixed': Cannot normalize values of different types."
        );
    }

    #[test]
    fn empty_input_is_an_error_only_for_the_strict_constructor() {
        let error = NormalizedMetrics::try_from_stele_metrics(&[])
            .err()
            .expect("empty input should be rejected");
        assert_eq!(format!("{:#}", error), "Cannot normalize an empty set of metrics.");

        let (normalized, failures) = NormalizedMetrics::from_stele_metrics(&[]);
        assert_eq!(normalized.len(), 0);
        assert!(failures.is_empty());
    }

    #[test]
    fn the_strict_constructor_reports_the_first_failed_group_in_sorted_order() {
        // `b.metric` is dumped first, but `a.metric` sorts first.
        let metrics = [
            metric_with_values("b.metric", &[], &[count(10, 1.0), gauge(20, 2.0)]),
            metric_with_values("a.metric", &[], &[rate(10, 10, 1.0), rate(20, 20, 1.0)]),
            metric_with_values("c.metric", &[], &[count(10, 1.0)]),
        ];

        let error = NormalizedMetrics::try_from_stele_metrics(&metrics)
            .err()
            .expect("a failed group should fail the whole set");

        assert_eq!(
            format!("{:#}", error),
            "Failed to normalize metrics.: Failed to normalize values for metric 'a.metric': Cannot normalize rate \
             values with different intervals."
        );
    }
}
