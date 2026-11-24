//! Splitting metrics into timestamped passthrough and non-timestamped aggregation streams.

use std::time::Duration;

use saluki_core::data_model::event::metric::{Metric, MetricValues};

/// Fixed interval used to normalize timestamped counters into rates on the no-aggregation path.
pub const PASSTHROUGH_COUNTER_NORMALIZATION_INTERVAL: Duration = Duration::from_secs(10);

/// Result of splitting a metric into timestamped passthrough and non-timestamped aggregate portions.
#[derive(Debug, PartialEq)]
pub struct TimestampSplitOutcome {
    /// Timestamped portion converted for passthrough, if present and supported.
    pub passthrough: Option<Metric>,

    /// Non-timestamped portion destined for aggregation, if present.
    pub aggregate: Option<Metric>,

    /// Dropped timestamped portion when its value type cannot pass through without aggregation.
    pub dropped_unsupported: Option<Metric>,
}

/// Splits `metric` into `(timestamped, non_timestamped)` without changing value types.
pub fn try_split_timestamped_values(mut metric: Metric) -> (Option<Metric>, Option<Metric>) {
    if metric.values().all_timestamped() {
        (Some(metric), None)
    } else if metric.values().any_timestamped() {
        let new_metric_values = metric.values_mut().split_timestamped();
        let new_metric = Metric::from_parts(metric.context().clone(), new_metric_values, metric.metadata().clone());

        (Some(new_metric), Some(metric))
    } else {
        (None, Some(metric))
    }
}

/// Returns `true` if `values` can pass through the no-aggregation pipeline without aggregation.
///
/// Only counters, rates, and gauges are supported; histograms, distributions, and sets require aggregation.
pub fn is_supported_passthrough_type(values: &MetricValues) -> bool {
    matches!(
        values,
        MetricValues::Counter(_) | MetricValues::Rate(..) | MetricValues::Gauge(_)
    )
}

/// Converts a counter into a rate using [`PASSTHROUGH_COUNTER_NORMALIZATION_INTERVAL`].
///
/// Non-counter values are returned unchanged.
pub fn counter_values_to_rate(values: MetricValues) -> MetricValues {
    match values {
        MetricValues::Counter(points) => MetricValues::rate(points, PASSTHROUGH_COUNTER_NORMALIZATION_INTERVAL),
        values => values,
    }
}

/// Splits `metric` by timestamp state, validates supported passthrough types, and normalizes passthrough counters.
pub fn split_for_no_aggregation(metric: Metric) -> TimestampSplitOutcome {
    let (timestamped, aggregate) = try_split_timestamped_values(metric);
    let (passthrough, dropped_unsupported) = match timestamped {
        Some(ts_metric) if is_supported_passthrough_type(ts_metric.values()) => {
            let (context, values, metadata) = ts_metric.into_parts();
            let adjusted = counter_values_to_rate(values);
            (Some(Metric::from_parts(context, adjusted, metadata)), None)
        }
        Some(unsupported) => (None, Some(unsupported)),
        None => (None, None),
    };

    TimestampSplitOutcome {
        passthrough,
        aggregate,
        dropped_unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_timestamped_metric_routes_to_aggregate() {
        let metric = Metric::counter("requests", 5.0);
        let outcome = split_for_no_aggregation(metric.clone());

        assert_eq!(outcome.passthrough, None);
        assert_eq!(outcome.aggregate, Some(metric));
        assert_eq!(outcome.dropped_unsupported, None);
    }

    #[test]
    fn timestamped_counter_routes_to_passthrough_as_rate() {
        let metric = Metric::counter("requests", (1000, 5.0));
        let outcome = split_for_no_aggregation(metric);

        assert_eq!(outcome.aggregate, None);
        assert_eq!(outcome.dropped_unsupported, None);
        let passthrough = outcome.passthrough.expect("should route to passthrough");
        assert!(matches!(
            passthrough.values(),
            MetricValues::Rate(_, interval) if *interval == PASSTHROUGH_COUNTER_NORMALIZATION_INTERVAL
        ));
    }

    #[test]
    fn timestamped_distribution_is_dropped_as_unsupported() {
        let metric = Metric::distribution("latency", (1000, 1.5));
        let outcome = split_for_no_aggregation(metric);

        assert_eq!(outcome.passthrough, None);
        assert_eq!(outcome.aggregate, None);
        assert!(outcome.dropped_unsupported.is_some());
    }

    #[test]
    fn mixed_metric_splits_both_ways() {
        let mut metric = Metric::gauge("cpu", 10.0);
        metric.values_mut().merge(MetricValues::gauge((2000, 20.0)));

        let outcome = split_for_no_aggregation(metric);
        assert!(outcome.passthrough.is_some());
        assert!(outcome.aggregate.is_some());
        assert!(outcome.dropped_unsupported.is_none());
    }
}
