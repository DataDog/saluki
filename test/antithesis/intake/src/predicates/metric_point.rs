//! Per-point predicates from the `invariant-jig` `README.md`
//! §Properties.Payloads for the `MetricPoint` category.

use std::num::NonZeroUsize;

use datadog_protos::metrics::metric_payload::MetricSeries;

/// W20 -- `MetricPoint`, Value Not-NaN. Specification §Properties.Payloads W20:
/// a point's `value` is not NaN. The W20 asymmetry note records intake
/// admitting `+/-Inf` and rejecting only NaN, so the predicate checks `is_nan`
/// alone. Returns the index of the first NaN point paired with the count of NaN
/// points across the series, or `None` when the series carries none.
#[must_use]
pub fn w20_nan_violations(series: &MetricSeries) -> Option<(usize, NonZeroUsize)> {
    let first = series.points.iter().position(|p| p.value.is_nan())?;
    let count = series.points.iter().filter(|p| p.value.is_nan()).count();
    NonZeroUsize::new(count).map(|count| (first, count))
}

/// W21 -- `MetricPoint`, Timestamp Future Bound. Specification
/// §Properties.Payloads W21: a point's `timestamp` is at most `intake_now +
/// MaxSecondsInFuture`. Intake drops points strictly past that bound, so the
/// predicate flags `timestamp > intake_now + max_future`. `intake_now_secs` is
/// the intake's wall clock at request receipt in epoch seconds.
/// `max_future_secs` is the bound. Both are epoch-second quantities far below
/// `i64::MAX`, so the sum does not overflow. Returns the index of the first
/// over-bound point paired with the count across the series, or `None` when
/// every point is within the bound.
#[must_use]
pub fn w21_future_violations(
    series: &MetricSeries, intake_now_secs: i64, max_future_secs: i64,
) -> Option<(usize, NonZeroUsize)> {
    let bound = intake_now_secs + max_future_secs;
    let first = series.points.iter().position(|p| p.timestamp > bound)?;
    let count = series.points.iter().filter(|p| p.timestamp > bound).count();
    NonZeroUsize::new(count).map(|count| (first, count))
}

#[cfg(test)]
mod tests {
    use datadog_protos::metrics::metric_payload::{MetricPoint, MetricSeries};

    use super::*;

    fn series_with(values: &[(i64, f64)]) -> MetricSeries {
        let mut series = MetricSeries::new();
        for &(timestamp, value) in values {
            let mut point = MetricPoint::new();
            point.timestamp = timestamp;
            point.value = value;
            series.points.push(point);
        }
        series
    }

    #[test]
    fn w20_admits_infinities_flags_nan() {
        let series = series_with(&[(0, f64::INFINITY), (1, f64::NEG_INFINITY), (2, 1.0)]);
        assert!(w20_nan_violations(&series).is_none());
    }

    #[test]
    fn w20_reports_first_index_and_count() {
        let series = series_with(&[(0, 1.0), (1, f64::NAN), (2, 2.0), (3, f64::NAN)]);
        assert_eq!(
            w20_nan_violations(&series).map(|(first, count)| (first, count.get())),
            Some((1, 2)),
        );
    }

    #[test]
    fn w21_is_strict_at_the_future_bound() {
        // bound = 1000 + 600 = 1600. A point at exactly 1600 holds; 1601 fires.
        let at_bound = series_with(&[(1600, 1.0)]);
        assert!(w21_future_violations(&at_bound, 1000, 600).is_none());
        let past_bound = series_with(&[(1601, 1.0)]);
        assert_eq!(
            w21_future_violations(&past_bound, 1000, 600).map(|(first, count)| (first, count.get())),
            Some((0, 1)),
        );
    }
}
