//! Detector implementations and the shared numerical utilities they build on.
//!
//! Anomaly detectors are ported one at a time from the Agent's Go `observer/impl` package. Everything that
//! more than one detector needs lives in [`numerics`] so the individual detector modules stay focused on
//! their algorithm instead of re-deriving medians, ranks, or tail probabilities.
//!
//! This module also holds the two pieces of storage plumbing that the Go detectors share at the detector
//! layer rather than in the numerics file:
//!
//! * [`workload_series_refs`] — the Go `WorkloadSeriesFilter` series listing. The [`StorageView`] trait
//!   deliberately exposes only namespace-scoped or all-namespace listings, so the telemetry exclusion lives
//!   here instead of in the trait.
//! * [`collect_last_points`] — the bounded tail read the Go `collectLastPoints` /
//!   `ForEachLastPoints` fast path performs, implemented over the trait's range and count methods.

pub mod bocpd;
pub mod numerics;
pub mod tukey_biweight;

use crate::identity::{Aggregate, SeriesRef};
use crate::model::Series;
use crate::storage::TELEMETRY_NAMESPACE;
use crate::traits::StorageView;

/// Returns the refs of every live workload series, in ascending ref order.
///
/// This is the Rust counterpart of the Go `workloadSeriesRefs`: the `StorageView` trait has no
/// workload-filter listing, so the telemetry namespace is excluded here, exactly as
/// `WorkloadSeriesFilter` does. Series refs are returned rather than full metadata because the Go BOCPD
/// detector holds refs to avoid retaining every series' tags.
pub(crate) fn workload_series_refs(view: &dyn StorageView) -> Vec<SeriesRef> {
    view.list_series(None)
        .into_iter()
        .filter(|meta| meta.namespace.as_str() != TELEMETRY_NAMESPACE)
        .map(|meta| meta.series_ref)
        .collect()
}

/// Returns the newest `max_points` points of a series with timestamp `<= end_sec`, in time order.
///
/// This ports the bounded tail read of the Go `collectLastPoints` fast path (storage's
/// `ForEachLastPoints`): instead of materialising the whole retained range, it binary-searches the second
/// that leaves exactly the wanted tail using [`StorageView::point_count_up_to`], then reads that single
/// range. The search is exact because the store keeps at most one bucket per second, so the point count is
/// strictly increasing across stored seconds.
///
/// Returns `None` when `max_points` is zero, when the series is not live, or when the tail is empty — the
/// Go version leaves its series metadata nil in every one of those cases. The returned [`Series`] carries
/// the series metadata of the matched points, which is what detectors use to build an anomaly's source.
pub(crate) fn collect_last_points(
    view: &dyn StorageView, series_ref: SeriesRef, end_sec: i64, max_points: usize, aggregate: Aggregate,
) -> Option<Series> {
    if max_points == 0 {
        return None;
    }
    let count = view.point_count_up_to(series_ref, end_sec);
    if count == 0 {
        return None;
    }
    let start_sec = if count <= max_points {
        // `i64::MIN` reads from the beginning: the start of the range is exclusive, and no real bucket can
        // sit at the minimum representable second.
        i64::MIN
    } else {
        first_second_with_count(view, series_ref, end_sec, count - max_points)
    };

    let mut series = view.get_series_range(series_ref, start_sec, end_sec, aggregate)?;
    // Defensive trim: the range read is exact by construction, but tolerating a store that returns extra
    // points keeps the helper safe if a future view coalesces reads.
    if series.points.len() > max_points {
        let excess = series.points.len() - max_points;
        series.points.drain(..excess);
    }
    if series.points.is_empty() {
        return None;
    }
    Some(series)
}

/// Returns the smallest second `s` in `(i64::MIN, end_sec]` with at least `target` stored buckets at or
/// before it.
///
/// `target` must be positive and at most the bucket count at `end_sec`; the result then has exactly
/// `target` buckets at or before it, so reading `(s, end_sec]` yields the requested tail.
fn first_second_with_count(view: &dyn StorageView, series_ref: SeriesRef, end_sec: i64, target: usize) -> i64 {
    let mut low = i64::MIN;
    let mut high = end_sec;
    while low < high {
        // Compute the midpoint in `i128` so the full `i64` range is searchable without overflow.
        let mid = ((low as i128 + high as i128) / 2) as i64;
        if view.point_count_up_to(series_ref, mid) >= target {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    low
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageConfig;
    use crate::model::Point;
    use crate::storage::TimeSeriesStorage;

    /// Builds a store with point retention disabled, mirroring the Go `newDetectorTestStorage` helper.
    fn detector_storage() -> TimeSeriesStorage {
        TimeSeriesStorage::new(StorageConfig {
            point_retention_secs: 0,
            ..StorageConfig::default()
        })
    }

    fn add(storage: &mut TimeSeriesStorage, name: &str, value: f64, second: i64) -> SeriesRef {
        storage
            .add("ns", name, None, value, second, &[])
            .series_ref
            .expect("finite values are admitted")
    }

    fn timestamps(series: &Series) -> Vec<i64> {
        series.points.iter().map(|point| point.second).collect()
    }

    /// Ports the Go `TestStorageBackedTailMatchesFullRangeWindow` tail-parity fixture, including the
    /// same-bucket merge at second 10 (values 10 and 30 average to 20).
    #[test]
    fn collect_last_points_matches_full_range_window_with_merge() {
        let mut storage = detector_storage();
        let mut series_ref = SeriesRef::new(0);
        for second in 1..=10 {
            series_ref = add(&mut storage, "metric", second as f64, second);
        }
        add(&mut storage, "metric", 30.0, 10);

        let cases: [(&str, i64, usize, i64, Vec<i64>); 3] = [
            ("before end", 6, 3, 0, vec![4, 5, 6]),
            ("exact max", 10, 4, 0, vec![7, 8, 9, 10]),
            ("segment boundary", 10, 6, 7, vec![8, 9, 10]),
        ];
        for (name, end, max_points, segment_start, want) in cases {
            let series = collect_last_points(&storage, series_ref, end, max_points, Aggregate::Average)
                .unwrap_or_else(|| panic!("{name}: expected a bounded tail"));
            let kept: Vec<i64> = series
                .points
                .iter()
                .filter(|point| point.second > segment_start)
                .map(|point| point.second)
                .collect();
            assert_eq!(kept, want, "{name}");
            if end == 10 {
                assert_eq!(
                    series.points.last().map(|point| point.value),
                    Some(20.0),
                    "the merged bucket must expose the averaged value"
                );
            }
        }
    }

    /// Ports the Go `TestTimeSeriesStorage_ForEachLastPoints` fixture.
    #[test]
    fn collect_last_points_returns_the_newest_points() {
        let mut storage = detector_storage();
        let mut series_ref = SeriesRef::new(0);
        for second in 1..=5 {
            series_ref = add(&mut storage, "my.metric", second as f64, second);
        }

        let series = collect_last_points(&storage, series_ref, 4, 3, Aggregate::Average).expect("tail exists");
        assert_eq!(series.name, "my.metric");
        assert_eq!(
            series.points,
            vec![
                Point { second: 2, value: 2.0 },
                Point { second: 3, value: 3.0 },
                Point { second: 4, value: 4.0 },
            ]
        );
    }

    /// Ports the Go `TestTimeSeriesStorage_ForEachLastPoints_Boundaries` fixture.
    #[test]
    fn collect_last_points_boundaries() {
        let mut storage = detector_storage();
        let series_ref = add(&mut storage, "my.metric", 1.0, 10);
        add(&mut storage, "my.metric", 2.0, 20);

        // A zero-length window reads nothing, matching Go's `n <= 0` guard.
        assert!(collect_last_points(&storage, series_ref, 20, 0, Aggregate::Average).is_none());

        // A window wider than the stored history returns everything up to `end`.
        let series = collect_last_points(&storage, series_ref, 100, 10, Aggregate::Average).expect("tail exists");
        assert_eq!(timestamps(&series), vec![10, 20]);

        // An unknown ref reads nothing.
        assert!(collect_last_points(&storage, SeriesRef::new(999), 100, 10, Aggregate::Average).is_none());
    }

    #[test]
    fn workload_series_refs_exclude_telemetry() {
        let mut storage = detector_storage();
        let workload = add(&mut storage, "workload", 1.0, 1);
        storage.add(TELEMETRY_NAMESPACE, "chart", None, 1.0, 1, &[]);

        assert_eq!(workload_series_refs(&storage), vec![workload]);
    }
}
