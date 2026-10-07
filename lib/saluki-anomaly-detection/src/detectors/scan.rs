//! Shared machinery for the streaming scan detectors.
//!
//! [`crate::detectors::scanmw`] and [`crate::detectors::scanwelch`] share almost all of their structure and
//! differ only in how they pick and verify a candidate split point. This module holds everything that is
//! not split selection: the per-series streaming state, the discovery/visibility/gating loop that decides
//! whether a scan runs at all, the Mann-Whitney and robust-deviation statistics both detectors verify
//! against, and the anomaly assembly. The statistics themselves (median, MAD, ranks, tie correction, tail
//! probability, rank-biserial correlation) live in [`crate::detectors::numerics`], which stays the single
//! source of truth for those helpers; this module only composes them.
//!
//! # Ported from
//!
//! The loop and per-series state mirror the Go `ScanMWDetector.Detect` and `ScanWelchDetector.Detect`
//! bodies in `observer/impl/metrics_detector_scanmw.go` and `metrics_detector_scanwelch.go`, which are
//! themselves acknowledged near-duplicates. The shared statistics mirror the scan-specific parts of
//! `observer/impl/metrics_detector_util.go`.
//!
//! # Deliberate omissions and divergences
//!
//! * The Go `supportsSeriesAggregate` policy lets storage narrow the aggregation set for series whose
//!   stored representation only gives some aggregates useful semantics (for example log-derived series that
//!   support only `avg`). [`StorageView`] exposes no such method and cannot be downcast, so this port
//!   reproduces the Go **fallback** semantics: every configured aggregate is treated as supported.
//! * Likewise, the Go bulk-status, ref-only-listing, and bounded-tail storage optimization interfaces are
//!   not visible through [`StorageView`]; the loop issues one status query and one range read per
//!   series/aggregate pair. The store's range read is already a bounded tail slice, so this is an
//!   interface-level simplification rather than an extra scan of the full history.
//! * The Mann-Whitney split loops here subsume the Go indexing panic when a buffer holds fewer than
//!   `min_segment` points: such a buffer can never contain a valid split, so it returns "not found" instead
//!   of panicking. The Go scan-Welch detector already guards this case explicitly; scan-MW does not.

use std::collections::HashMap;

use crate::detectors::numerics::{normal_cdf_upper, rank_biserial_correlation, ScanDetectorWorkspace, SCAN_MAX_POINTS};
use crate::identity::{Aggregate, QueryHandle, SeriesDescriptor, SeriesRef};
use crate::model::{Anomaly, AnomalyEvidence, AnomalyType, Point};
use crate::storage::TELEMETRY_NAMESPACE;
use crate::traits::StorageView;

/// Default minimum number of points on each side of a candidate split (Go `MinSegment`).
pub(crate) const DEFAULT_MIN_SEGMENT: i32 = 12;

/// Default minimum number of visible points before a detector scans a series (Go `MinPoints`).
pub(crate) const DEFAULT_MIN_POINTS: i32 = 30;

/// Default largest p-value accepted as a changepoint (Go `SignificanceThreshold`).
pub(crate) const DEFAULT_SIGNIFICANCE_THRESHOLD: f64 = 1e-8;

/// Default smallest accepted `|rank-biserial correlation|` (Go `MinEffectSize`).
pub(crate) const DEFAULT_MIN_EFFECT_SIZE: f64 = 0.85;

/// Default smallest accepted `|post_median - pre_median| / MAD` (Go `MinDeviationMAD`).
pub(crate) const DEFAULT_MIN_DEVIATION_MAD: f64 = 3.0;

/// Default smallest accepted Welch `|t|` (Go `MinTStatistic`).
pub(crate) const DEFAULT_MIN_T_STATISTIC: f64 = 8.0;

/// The normalized scan window shared by both detectors.
///
/// Raw configuration keeps the Go signed integers so that `0` (and negative values) mean "use the
/// default"; normalized windows hold usable, non-zero sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScanWindow {
    /// Minimum number of points required on each side of a split. Always `>= 1`.
    pub(crate) min_segment: usize,
    /// Minimum number of visible points before a series is scanned.
    pub(crate) min_points: usize,
    /// Maximum number of newest points scanned per series, per aggregate.
    pub(crate) max_points: usize,
}

impl ScanWindow {
    /// Builds a window from raw configuration values, applying the Go `ensureDefaults` rules.
    pub(crate) fn normalized(min_segment: i32, min_points: i32, max_points: i32) -> Self {
        let mut min_segment = min_segment;
        let mut min_points = min_points;
        let mut max_points = max_points;
        ensure_window_defaults(&mut min_segment, &mut min_points, &mut max_points);
        Self {
            min_segment: min_segment as usize,
            min_points: min_points as usize,
            max_points: max_points as usize,
        }
    }
}

/// Fills zero/negative scan-window fields with their defaults, in place.
///
/// This is the shared part of the Go `ensureDefaults`: `min_segment <= 0` and `min_points <= 0` fall back
/// to [`DEFAULT_MIN_SEGMENT`] and [`DEFAULT_MIN_POINTS`], `max_points <= 0` falls back to
/// [`SCAN_MAX_POINTS`], and a `max_points` below `min_points` is raised to `min_points` (Go logs a warning;
/// this port clamps silently because the detector core has no logging surface).
pub(crate) fn ensure_window_defaults(min_segment: &mut i32, min_points: &mut i32, max_points: &mut i32) {
    if *min_segment <= 0 {
        *min_segment = DEFAULT_MIN_SEGMENT;
    }
    if *min_points <= 0 {
        *min_points = DEFAULT_MIN_POINTS;
    }
    if *max_points <= 0 {
        *max_points = SCAN_MAX_POINTS as i32;
    }
    if *max_points < *min_points {
        *max_points = *min_points;
    }
}

/// The three statistic thresholds shared by both detectors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScanThresholds {
    /// Largest p-value accepted as a changepoint.
    pub(crate) significance_threshold: f64,
    /// Smallest accepted `|rank-biserial correlation|`.
    pub(crate) min_effect_size: f64,
    /// Smallest accepted `|post_median - pre_median| / MAD`.
    pub(crate) min_deviation_mad: f64,
}

/// Per-series streaming state, keyed by `(series ref, aggregate)` inside [`ScanState`].
///
/// Both scan detectors track exactly the same three fields, mirroring the Go `scanmwSeriesState` /
/// `scanwelchSeriesState` structs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScanSeriesState {
    /// Write generation observed at the last scan of this series/aggregate.
    pub(crate) last_write_gen: u64,
    /// Visible point count (`point_count_up_to`) observed at the last scan.
    pub(crate) last_processed_count: usize,
    /// Earliest timestamp still scanned; `0` means "scan the whole retained history".
    ///
    /// After a changepoint fires this advances to `change_second - 1`, so later scans only examine
    /// post-change data.
    pub(crate) segment_start_time: i64,
}

/// Reports whether a series/aggregate is due for a scan.
///
/// A scan runs when at least `min_segment` new points have become visible, **or** when the write
/// generation moved even though the visible count did not (a same-bucket merge that changed stored
/// values). The generation-only branch is what keeps the offline replay path scanning as `data_time`
/// exposes pre-loaded history: storage may already hold future points, so the write generation reaches its
/// final value before those points become visible.
pub(crate) fn should_scan(
    state: &ScanSeriesState, point_count: usize, write_generation: u64, min_segment: usize,
) -> bool {
    point_count >= state.last_processed_count.saturating_add(min_segment) || write_generation != state.last_write_gen
}

/// The statistics a scan detector reports for an accepted changepoint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScanEvidence {
    /// Median of the pre-change segment.
    pub(crate) baseline_median: f64,
    /// Unscaled MAD of the pre-change segment.
    pub(crate) baseline_mad: f64,
    /// Median of the post-change segment.
    pub(crate) current_value: f64,
    /// `|post_median - pre_median| / MAD`, using the configured fallback denominator when MAD is zero.
    pub(crate) deviation_sigma: f64,
    /// The p-value that justified the changepoint (Mann-Whitney based for both detectors).
    pub(crate) p_value: f64,
    /// The rank-biserial correlation (effect size) at the chosen split.
    pub(crate) effect_size: f64,
    /// The Welch `|t|` of the chosen split, set only by the Welch detector.
    pub(crate) test_statistic: Option<f64>,
}

/// A detector-specific scan hit: where the change is and the evidence supporting it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScanDetection {
    /// Index into the scanned point buffer at which the post-change segment starts.
    pub(crate) change_index: usize,
    /// The statistics of the accepted split.
    pub(crate) evidence: ScanEvidence,
}

/// The Mann-Whitney statistics for a fixed split of a ranked sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MannWhitneySplit {
    /// The two-sided `min(U1, U2)` statistic.
    pub(crate) u: f64,
    /// The continuity-corrected, floored (`>= 0`) normal deviate.
    pub(crate) z: f64,
}

/// Computes `P(Z > z)` for the two-sided Mann-Whitney scan, capped at `1.0`.
pub(crate) fn mw_p_value(z: f64) -> f64 {
    (2.0 * normal_cdf_upper(z)).min(1.0)
}

/// Computes the Mann-Whitney statistics at `split` from pre-computed `ranks`.
///
/// `tie_correction` is `sum(t^3 - t)` over rank tie groups, as returned by
/// [`crate::detectors::numerics::ScanDetectorWorkspace::assign_ranks`]. Returns `None` when the corrected
/// variance is not positive, which is the Go detectors' "not a usable candidate" case.
pub(crate) fn mann_whitney_split(ranks: &[f64], tie_correction: f64, split: usize) -> Option<MannWhitneySplit> {
    let n = ranks.len();
    if n < 2 || split == 0 || split >= n {
        return None;
    }
    let f_split = split as f64;
    let f_rest = (n - split) as f64;
    let f_n = n as f64;

    let mut rank_sum = 0.0;
    for &rank in &ranks[..split] {
        rank_sum += rank;
    }
    let u1 = rank_sum - f_split * (f_split + 1.0) / 2.0;
    let u = u1.min(f_split * f_rest - u1);

    let mean_u = f_split * f_rest / 2.0;
    let variance_u = (f_split * f_rest / 12.0) * (f_n + 1.0 - tie_correction / (f_n * (f_n - 1.0)));
    if variance_u <= 0.0 {
        return None;
    }
    let std_u = variance_u.sqrt();
    let z = ((u - mean_u).abs() - 0.5) / std_u;
    Some(MannWhitneySplit {
        u,
        z: if z < 0.0 { 0.0 } else { z },
    })
}

/// Computes the rank-biserial correlation of a split, matching the Go `rankBiserialCorrelation` call.
pub(crate) fn mw_effect_size(u: f64, split: usize, n: usize) -> f64 {
    rank_biserial_correlation(u, split, n - split)
}

/// The robust deviation of a split: pre-change center/scale and the resulting MAD-relative change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RobustDeviation {
    /// Median of the pre-change segment.
    pub(crate) baseline_median: f64,
    /// Unscaled MAD of the pre-change segment.
    pub(crate) baseline_mad: f64,
    /// Median of the post-change segment.
    pub(crate) current_value: f64,
    /// `|current_value - baseline_median| / denominator`.
    pub(crate) deviation_sigma: f64,
}

/// Computes the pre/post median and MAD-relative deviation, applying the shared acceptance gate.
///
/// The denominator is the pre-change MAD, falling back to `max(|pre_median| * 0.01, 1e-6)` when the MAD is
/// below `1e-10` (Go's floor for near-constant baselines). Returns `None` when the deviation is below
/// `min_deviation_mad`, which is the detectors' "not a real change" rejection.
pub(crate) fn robust_deviation(
    workspace: &mut ScanDetectorWorkspace, values: &[f64], split: usize, min_deviation_mad: f64,
) -> Option<RobustDeviation> {
    if split == 0 || split >= values.len() {
        return None;
    }
    let pre = &values[..split];
    let post = &values[split..];
    let baseline_median = workspace.median(pre);
    let current_value = workspace.median(post);
    let baseline_mad = workspace.mad(pre, baseline_median);
    let denominator = if baseline_mad < 1e-10 {
        (baseline_median.abs() * 0.01).max(1e-6)
    } else {
        baseline_mad
    };
    let deviation_sigma = (current_value - baseline_median).abs() / denominator;
    if deviation_sigma < min_deviation_mad {
        return None;
    }
    Some(RobustDeviation {
        baseline_median,
        baseline_mad,
        current_value,
        deviation_sigma,
    })
}

/// Converts a p-value into the detector score `-log10(p)`, flooring a non-finite score at `300`.
///
/// A p-value that underflows to `0.0` makes `-log10(p)` positive infinity; Go replaces that score with
/// `300.0` and this port does the same.
pub(crate) fn scan_score(p_value: f64) -> f64 {
    let score = -p_value.log10();
    if score.is_infinite() && score > 0.0 {
        300.0
    } else {
        score
    }
}

/// One series' visible-point count and write generation, as read at the start of a pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SeriesStatus {
    point_count: usize,
    write_generation: u64,
}

/// The mutable per-detector state shared by both scan detectors.
///
/// This is the Rust counterpart of the Go detectors' `series` map, `scanBuf`, `workspace`, `cachedRefs`,
/// `cachedGen`, and `ready` fields. The detector configuration is deliberately *not* here: the two
/// detectors own differently typed configs, and only the normalized window and thresholds cross into this
/// module.
#[derive(Debug, Default)]
pub(crate) struct ScanState {
    /// Per-series state keyed by ref and aggregate.
    pub(crate) series: HashMap<(SeriesRef, Aggregate), ScanSeriesState>,
    /// Workload refs discovered at `cached_gen`, reused across passes with the same generation.
    pub(crate) cached_refs: Vec<SeriesRef>,
    /// The series generation `cached_refs` was built for; `None` means "not cached".
    pub(crate) cached_gen: Option<u64>,
    /// Scratch buffer holding the newest points of the series currently being scanned.
    pub(crate) scan_buf: Vec<Point>,
    /// Scratch space for ranks, medians, MADs, and intervals.
    pub(crate) workspace: ScanDetectorWorkspace,
    /// Whether any series has reached the scoring condition since the last [`ScanState::reset`].
    pub(crate) ready: bool,
}

impl ScanState {
    /// Creates an empty scan state.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Reports whether at least one series has reached the scoring condition.
    pub(crate) fn is_ready(&self) -> bool {
        self.ready
    }

    /// Clears all per-series and cached state, leaving the scratch buffers allocated.
    ///
    /// Matches the Go `Reset`, which keeps `scanBuf` and the workspace so replay does not re-allocate.
    pub(crate) fn reset(&mut self) {
        self.series.clear();
        self.cached_refs.clear();
        self.cached_gen = None;
        self.ready = false;
    }

    /// Drops state for the given refs, then invalidates the cached ref list.
    ///
    /// Matches the Go `RemoveSeries`: a no-op when either argument is empty, and the cached series list is
    /// discarded because the live set has changed outside a generation bump.
    pub(crate) fn remove_series(&mut self, refs: &[SeriesRef], aggregations: &[Aggregate]) {
        if refs.is_empty() || self.series.is_empty() {
            return;
        }
        for &series_ref in refs {
            for &aggregate in aggregations {
                self.series.remove(&(series_ref, aggregate));
            }
        }
        self.cached_refs.clear();
        self.cached_gen = None;
    }

    /// Runs one detection pass, delegating split selection to `scan` and evidence mapping to `make_evidence`.
    ///
    /// Both scan detectors call this with their own `scan` function; everything else (discovery, the
    /// activation/visibility gates, segment trimming, anomaly assembly, and the post-scan bookkeeping) is
    /// identical and lives here.
    pub(crate) fn detect<F, E>(
        &mut self, view: &dyn StorageView, data_time_sec: i64, window: ScanWindow, detector_name: &str,
        aggregations: &[Aggregate], mut scan: F, make_evidence: E,
    ) -> Vec<Anomaly>
    where
        F: FnMut(&mut ScanDetectorWorkspace, &[Point]) -> Option<ScanDetection>,
        E: Fn(ScanEvidence) -> AnomalyEvidence,
    {
        let min_segment = window.min_segment.max(1);
        let Self {
            series,
            cached_refs,
            cached_gen,
            scan_buf,
            workspace,
            ready,
        } = self;

        let generation = view.series_generation();
        if *cached_gen != Some(generation) {
            *cached_refs = workload_refs(view);
            *cached_gen = Some(generation);
        }

        // Move the ref list aside so the per-series map can be mutated while iterating; nothing inside the
        // loop touches the cache, so it is restored verbatim afterwards.
        let refs = std::mem::take(cached_refs);
        let mut anomalies = Vec::new();

        for &series_ref in &refs {
            let status = series_status(view, series_ref, data_time_sec);

            for &aggregate in aggregations {
                let key = (series_ref, aggregate);
                let exists = series.contains_key(&key);
                if !exists && status.point_count < window.min_points {
                    continue;
                }
                let activated = !exists;
                let state = series.entry(key).or_default();

                if !should_scan(state, status.point_count, status.write_generation, min_segment) {
                    continue;
                }

                collect_last_points(view, series_ref, data_time_sec, window.max_points, aggregate, scan_buf);
                if state.segment_start_time > 0 {
                    scan_buf.retain(|point| point.second > state.segment_start_time);
                }

                let meta = view.series_meta(series_ref);
                if meta.is_none() || scan_buf.len() < window.min_points {
                    state.last_processed_count = status.point_count;
                    state.last_write_gen = status.write_generation;
                    continue;
                }
                // Readiness is only reached when a scan actually runs, matching the Go detectors.
                *ready = true;

                if let Some(detection) = scan(&mut *workspace, scan_buf.as_slice()) {
                    let change_second = scan_buf[detection.change_index].second;
                    let p_value = detection.evidence.p_value;
                    let series_meta = meta.as_ref().expect("series metadata is present on the scan path");
                    anomalies.push(Anomaly {
                        anomaly_type: AnomalyType::Metric,
                        series: SeriesDescriptor::new(
                            series_meta.namespace.clone(),
                            series_meta.name.clone(),
                            series_meta.host.clone(),
                            series_meta.tags.clone(),
                            aggregate,
                        ),
                        series_ref: Some(QueryHandle::new(series_ref, aggregate)),
                        detector_name: detector_name.to_string(),
                        context: None,
                        timestamp_sec: change_second,
                        score: Some(scan_score(p_value)),
                        sampling_interval_sec: workspace.median_point_interval(scan_buf.as_slice()),
                        evidence: Some(make_evidence(detection.evidence)),
                    });
                    state.segment_start_time = change_second - 1;
                }

                state.last_processed_count = if activated {
                    // First activation only "consumes" whole segments of history, so the detector keeps
                    // scanning as more of a backfilled series becomes visible.
                    1 + status.point_count.saturating_sub(1) / min_segment * min_segment
                } else {
                    status.point_count
                };
                state.last_write_gen = status.write_generation;
            }
        }

        *cached_refs = refs;
        anomalies
    }
}

/// Returns the workload series refs: every live series except the telemetry namespace.
///
/// [`StorageView::list_series`] only filters by a single namespace, so the workload filter is applied here.
/// The store returns series in ascending ref order and this keeps that order, matching the Go
/// `workloadSeriesRefs` ordering that the detectors rely on for deterministic output.
fn workload_refs(view: &dyn StorageView) -> Vec<SeriesRef> {
    view.list_series(None)
        .into_iter()
        .filter(|meta| meta.namespace.as_str() != TELEMETRY_NAMESPACE)
        .map(|meta| meta.series_ref)
        .collect()
}

/// Reads a series' visible point count and write generation for `data_time_sec`.
fn series_status(view: &dyn StorageView, series_ref: SeriesRef, data_time_sec: i64) -> SeriesStatus {
    SeriesStatus {
        point_count: view.point_count_up_to(series_ref, data_time_sec),
        write_generation: view.write_generation(series_ref),
    }
}

/// Fills `buf` with the newest `max_points` visible points of a series, in time order.
///
/// Points at or before `data_time_sec` are read through [`StorageView::get_series_range`]; the tail slice
/// reproduces the Go `collectLastPoints` bounded-tail semantics on top of the trait's materialized range.
fn collect_last_points(
    view: &dyn StorageView, series_ref: SeriesRef, data_time_sec: i64, max_points: usize, aggregate: Aggregate,
    buf: &mut Vec<Point>,
) {
    buf.clear();
    let Some(series) = view.get_series_range(series_ref, i64::MIN, data_time_sec, aggregate) else {
        return;
    };
    let start = series.points.len().saturating_sub(max_points);
    buf.extend_from_slice(&series.points[start..]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageConfig;
    use crate::model::SeriesMeta;
    use crate::storage::TimeSeriesStorage;

    #[test]
    fn scan_window_applies_go_defaults() {
        assert_eq!(
            ScanWindow::normalized(0, 0, 0),
            ScanWindow {
                min_segment: 12,
                min_points: 30,
                max_points: 120,
            }
        );
        assert_eq!(
            ScanWindow::normalized(-4, -2, -9),
            ScanWindow {
                min_segment: 12,
                min_points: 30,
                max_points: 120,
            }
        );
        // max_points below min_points is raised to min_points (Go clamps here instead of warning).
        assert_eq!(
            ScanWindow::normalized(12, 40, 20),
            ScanWindow {
                min_segment: 12,
                min_points: 40,
                max_points: 40,
            }
        );
        // Explicit values are kept.
        assert_eq!(
            ScanWindow::normalized(4, 8, 16),
            ScanWindow {
                min_segment: 4,
                min_points: 8,
                max_points: 16,
            }
        );
    }

    #[test]
    fn should_scan_gates_on_visible_count_and_write_generation() {
        let state = ScanSeriesState {
            last_write_gen: 40,
            last_processed_count: 37,
            segment_start_time: 0,
        };
        // No new visible points and no writes: skip.
        assert!(!should_scan(&state, 40, 40, 12));
        // Fewer than min_segment new visible points, no writes: still skip.
        assert!(!should_scan(&state, 48, 40, 12));
        // Exactly min_segment new visible points: scan.
        assert!(should_scan(&state, 49, 40, 12));
        // Same visible count but a new write (a same-bucket merge moved stored values): scan.
        assert!(should_scan(&state, 40, 41, 12));

        let fresh = ScanSeriesState::default();
        assert!(!should_scan(&fresh, 11, 0, 12));
        assert!(should_scan(&fresh, 12, 0, 12));
        // A fresh state with a non-zero generation counts as an activation write.
        assert!(should_scan(&fresh, 0, 1, 12));
    }

    #[test]
    fn mw_p_value_is_two_sided_and_capped() {
        // A negative deviate is floored before it reaches here, so the cap only matters defensively.
        assert_eq!(mw_p_value(0.0), 2.0 * normal_cdf_upper(0.0));
        assert!(mw_p_value(0.0) <= 1.0);
    }

    #[test]
    fn mann_whitney_split_matches_go_fixture_with_ties() {
        // Go: ranks([10,10,20,30,30,30]) = [1.5, 1.5, 3, 5, 5, 5], tie correction 30.
        let mut workspace = ScanDetectorWorkspace::new();
        let (ranks, tie_correction) = workspace.assign_ranks(&[10.0, 10.0, 20.0, 30.0, 30.0, 30.0]);
        assert_eq!(tie_correction, 30.0);

        let split = mann_whitney_split(ranks, tie_correction, 3).expect("split is usable");
        assert_eq!(split.u, 0.0);
        // meanU = 4.5, varU = (9/12) * (7 - 30/30) = 4.5, stdU = sqrt(4.5), z = 4 / sqrt(4.5).
        let expected_z = 4.0 / 4.5_f64.sqrt();
        assert!(
            (split.z - expected_z).abs() < 1e-12,
            "z = {}, expected {expected_z}",
            split.z
        );

        // Degenerate splits are rejected rather than producing a division by zero.
        assert!(mann_whitney_split(ranks, tie_correction, 0).is_none());
        assert!(mann_whitney_split(ranks, tie_correction, 6).is_none());
        assert!(mann_whitney_split(&[], 0.0, 1).is_none());
    }

    #[test]
    fn robust_deviation_applies_gate_and_floor() {
        let mut workspace = ScanDetectorWorkspace::new();

        // pre = [10, 11] (median 10.5, MAD 0.5), post = [12, 13] (median 12.5) -> deviation 4.0.
        let values = [10.0, 11.0, 12.0, 13.0];
        let deviation = robust_deviation(&mut workspace, &values, 2, 3.0).expect("deviation passes");
        assert_eq!(deviation.baseline_median, 10.5);
        assert_eq!(deviation.baseline_mad, 0.5);
        assert_eq!(deviation.current_value, 12.5);
        assert_eq!(deviation.deviation_sigma, 4.0);
        // The gate rejects a deviation strictly below the minimum, and accepts it at the boundary.
        assert!(robust_deviation(&mut workspace, &values, 2, 4.0).is_some());
        assert!(robust_deviation(&mut workspace, &values, 2, 4.5).is_none());

        // A zero MAD with a large pre-median falls back to 1% of that median.
        let scaled = [100.0, 100.0, 101.0, 101.0];
        let deviation = robust_deviation(&mut workspace, &scaled, 2, 0.5).expect("deviation passes");
        assert_eq!(deviation.baseline_mad, 0.0);
        assert_eq!(deviation.deviation_sigma, 1.0);
        assert!(robust_deviation(&mut workspace, &scaled, 2, 3.0).is_none());

        // A zero MAD around zero falls back to the absolute 1e-6 floor.
        let flat = [0.0, 0.0, 1.0, 1.0];
        let deviation = robust_deviation(&mut workspace, &flat, 2, 3.0).expect("floor keeps it finite");
        assert_eq!(deviation.baseline_mad, 0.0);
        assert!(
            (deviation.deviation_sigma - 1e6).abs() < 1.0,
            "deviation = {}",
            deviation.deviation_sigma
        );

        // Degenerate splits are rejected.
        assert!(robust_deviation(&mut workspace, &values, 0, 0.0).is_none());
        assert!(robust_deviation(&mut workspace, &values, 4, 0.0).is_none());
    }

    #[test]
    fn scan_score_floors_infinite_scores_at_300() {
        assert!((scan_score(1e-8) - 8.0).abs() < 1e-12);
        // A p-value that underflows to zero yields +inf, which the Go detectors replace with 300.
        assert_eq!(scan_score(0.0), 300.0);
        assert_eq!(scan_score(1.0), 0.0);
    }

    #[test]
    fn workload_refs_excludes_the_telemetry_namespace() {
        let mut storage = TimeSeriesStorage::new(StorageConfig::default());
        let workload = storage
            .add("ns", "metric", None, 1.0, 1, &[])
            .series_ref
            .expect("admitted");
        storage
            .add(TELEMETRY_NAMESPACE, "chart", None, 1.0, 1, &[])
            .series_ref
            .expect("admitted");

        assert_eq!(workload_refs(&storage), vec![workload]);
    }

    #[test]
    fn collect_last_points_keeps_the_newest_bounded_tail() {
        let mut storage = TimeSeriesStorage::new(StorageConfig::default());
        let series_ref = storage
            .add("ns", "metric", None, 0.0, 1, &[])
            .series_ref
            .expect("admitted");
        for second in 2..=10 {
            storage.add("ns", "metric", None, second as f64, second, &[]);
        }

        let mut buf = vec![Point { second: 0, value: 0.0 }];
        collect_last_points(&storage, series_ref, 10, 3, Aggregate::Average, &mut buf);
        assert_eq!(
            buf,
            vec![
                Point { second: 8, value: 8.0 },
                Point { second: 9, value: 9.0 },
                Point {
                    second: 10,
                    value: 10.0
                },
            ]
        );

        // Rows beyond `data_time_sec` are invisible, and an unknown ref yields an empty buffer.
        collect_last_points(&storage, series_ref, 5, 3, Aggregate::Average, &mut buf);
        assert_eq!(buf.last().map(|point| point.second), Some(5));
        collect_last_points(&storage, SeriesRef::new(4_242), 10, 3, Aggregate::Average, &mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn series_meta_carries_the_expected_identity() {
        // Guards the assumption that the shared loop can build the anomaly source from series metadata.
        let mut storage = TimeSeriesStorage::new(StorageConfig::default());
        let series_ref = storage
            .add("ns", "metric", None, 1.0, 1, &[])
            .series_ref
            .expect("admitted");
        let meta: SeriesMeta = storage.series_meta(series_ref).expect("live");
        assert_eq!(meta.namespace.as_str(), "ns");
        assert_eq!(meta.name, "metric");
        assert_eq!(meta.host, None);
        assert!(meta.tags.is_empty());
    }
}
