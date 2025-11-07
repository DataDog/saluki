use std::{
    collections::{BTreeMap, HashSet},
    fmt,
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use stele::{Metric, MetricContext, MetricValue};

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
#[derive(Clone, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct NormalizedMetricContext {
    name: String,
    tags: Vec<String>,
}

impl NormalizedMetricContext {
    fn from_stele_context(context: MetricContext) -> Self {
        let (name, mut tags) = context.into_parts();
        tags.sort_unstable();
        tags.dedup();

        Self { name, tags }
    }

    /// Returns the name of the metric.
    pub fn name(&self) -> &str {
        &self.name
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
    /// If the raw values are empty, or if the values cannot be normalized, an error is returned.
    pub fn try_from_values(
        context: MetricContext, mut raw_values: Vec<(u64, MetricValue)>,
    ) -> Result<Self, GenericError> {
        // We need to first sort the raw values by timestamp, to ensure we have proper ordering semantics.
        raw_values.sort_by(|a, b| a.0.cmp(&b.0));
        let value = try_normalize_values(&raw_values)
            .with_error_context(|| format!("Failed to normalize values for metric '{}'", context.name()))?;

        Ok(Self {
            context: NormalizedMetricContext::from_stele_context(context),
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

/// A metric context and type pair, used for comparing metrics between different sets.
type ContextTypePair<'a> = (&'a NormalizedMetricContext, MetricType);

/// A set of normalized metrics.
///
/// # Normalization behavior
///
/// - Metrics are sorted lexicographically by context (name, and then tags) in ascending order.
/// - Context tags are sorted lexicographically, in ascending order, and deduplicated.
/// - Tag sorting/deduplication is case-sensitive.
/// - Raw values for the same context, split across multiple metric entries, are combined. Duplicate values with the
///   same timestamp are not allowed.
/// - Raw values are sorted by timestamp in ascending order.
/// - Normalized value is the latest seen value for gauges, the accumulated sum of values for counters, rates, and
///   sketches.
pub struct NormalizedMetrics {
    metrics: Vec<NormalizedMetric>,
}

impl NormalizedMetrics {
    /// Attempts to create `NormalizedMetrics` from the given set of metrics.
    ///
    /// # Errors
    ///
    /// If the set of metrics is empty, or if any of the metrics cannot be normalized, an error is returned.
    pub fn try_from_stele_metrics(metrics: Vec<Metric>) -> Result<Self, GenericError> {
        if metrics.is_empty() {
            return Err(generic_error!("Cannot normalize an empty set of metrics."));
        }

        // Aggregate metric values by (context, type), using a `BTreeMap` so that we can get sorted metrics for ~free.
        // We group by type because metrics with the same context but different types (e.g., Count vs Rate)
        // cannot be merged together during normalization.
        let mut aggregated_context_values = BTreeMap::new();

        for metric in metrics {
            let context = metric.context();
            let metric_type = metric_value_type(metric.values());
            let key = (context.clone(), metric_type);
            let context_values = aggregated_context_values.entry(key).or_insert_with(Vec::new);
            context_values.extend_from_slice(metric.values());
        }

        let metrics = aggregated_context_values
            .into_iter()
            .map(|((context, _type), values)| NormalizedMetric::try_from_values(context, values))
            .try_fold(Vec::new(), |mut metrics, maybe_metric| {
                metrics.push(maybe_metric?);
                Ok::<_, GenericError>(metrics)
            })
            .with_error_context(|| "Failed to normalize metrics.")?;

        Ok(Self { metrics })
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

    /// Returns the differences, if any, between two sets of normalized metrics.
    ///
    /// Compares both context and type, so metrics with the same name but different types
    /// (e.g., Count vs Rate) are treated as different metrics.
    ///
    /// Returns two vectors, the first containing (context, type) pairs that are only present in the left set,
    /// and the second containing (context, type) pairs that are only present in the right set.
    /// If there are no differences, both vectors will be empty.
    pub fn context_differences<'a>(
        left: &'a Self, right: &'a Self,
    ) -> (Vec<ContextTypePair<'a>>, Vec<ContextTypePair<'a>>) {
        let left_pairs = left
            .metrics
            .iter()
            .map(|m| (m.context(), m.value_type()))
            .collect::<HashSet<_>>();
        let right_pairs = right
            .metrics
            .iter()
            .map(|m| (m.context(), m.value_type()))
            .collect::<HashSet<_>>();

        let left_only = left_pairs.difference(&right_pairs).copied().collect();
        let right_only = right_pairs.difference(&left_pairs).copied().collect();

        (left_only, right_only)
    }
}

fn metric_value_type(values: &[(u64, MetricValue)]) -> MetricType {
    match values.first() {
        Some((_, MetricValue::Count { .. })) => MetricType::Count,
        Some((_, MetricValue::Rate { .. })) => MetricType::Rate,
        Some((_, MetricValue::Gauge { .. })) => MetricType::Gauge,
        Some((_, MetricValue::Sketch { .. })) => MetricType::Sketch,
        // This should never happen since we validate non-empty values during normalization,
        // but we need to handle it for completeness. Default to Count as a fallback.
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
