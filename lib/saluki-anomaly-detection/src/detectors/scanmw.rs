//! The Mann-Whitney scan detector (`scanmw`).
//!
//! Ported from `observer/impl/metrics_detector_scanmw.go`. The detector scans every split of the retained
//! window with the Mann-Whitney U test and reports the split with the most significant result (the smallest
//! p-value), then verifies it with an effect-size and a robust-deviation gate. Ranks are assigned once and
//! the left-hand rank sum is advanced incrementally as the split moves, so the scan is `O(n log n)`.
//!
//! The streaming machinery — per-series state, the discovery/visibility gates, the post-change segment
//! truncation, and the shared verification statistics — lives in [`crate::detectors::scan`]; this module
//! owns only the split-selection loop and the configuration.
//!
//! # Design
//!
//! Each series/aggregate pair keeps a segment start. On a first scan the whole retained history is
//! examined; after a changepoint fires, the segment start moves to just before the change so later passes
//! only examine post-change data. Readiness turns on the first time a series has enough visible points to
//! be scanned, not merely enough to exist.

use crate::detectors::numerics::ScanDetectorWorkspace;
use crate::detectors::scan::{
    ensure_window_defaults, mann_whitney_split, mw_effect_size, mw_p_value, robust_deviation, ScanDetection,
    ScanEvidence, ScanState, ScanThresholds, ScanWindow, DEFAULT_MIN_DEVIATION_MAD, DEFAULT_MIN_EFFECT_SIZE,
    DEFAULT_MIN_POINTS, DEFAULT_MIN_SEGMENT, DEFAULT_SIGNIFICANCE_THRESHOLD,
};
use crate::identity::{Aggregate, SeriesRef};
use crate::model::{Anomaly, AnomalyEvidence, Point};
use crate::traits::{Detector, StorageView};

/// The detector name reported in anomalies and used by the scorer's per-detector thresholds.
pub const NAME: &str = "scanmw";

/// Configuration for the ScanMW detector.
///
/// The defaults match the Go `NewScanMWDetector`. Every field is normalized on the first detection pass:
/// a non-positive value (or an empty aggregation list) falls back to its default, and a `max_points` below
/// `min_points` is raised to `min_points`.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanMwConfig {
    /// Minimum number of points required on each side of a candidate split. Default: `12`.
    ///
    /// Smaller segments react faster but produce noisier changepoints. Values `<= 0` fall back to the
    /// default.
    pub min_segment: i32,

    /// Minimum number of visible points before a series is scanned. Default: `30`.
    ///
    /// A series with fewer visible points is left untouched, including its stored state. Values `<= 0` fall
    /// back to the default.
    pub min_points: i32,

    /// Maximum number of points scanned per series, keeping the newest. Default: `120`.
    ///
    /// This bounds the work per series and the memory the detector touches. Values `<= 0` fall back to the
    /// default; a value below `min_points` is raised to `min_points`.
    pub max_points: i32,

    /// Largest p-value a candidate split may have and still count as a changepoint. Default: `1e-8`.
    ///
    /// Lower values make the detector more selective. Values `<= 0` fall back to the default.
    pub significance_threshold: f64,

    /// Smallest `|rank-biserial correlation|` accepted. Default: `0.85`.
    ///
    /// The effect-size floor rejects statistically significant but practically small shifts. Values `<= 0`
    /// fall back to the default.
    pub min_effect_size: f64,

    /// Smallest `|post_median - pre_median| / MAD` accepted. Default: `3.0`.
    ///
    /// The robust-deviation floor rejects significant splits that do not move the median far relative to the
    /// pre-change spread. Values `<= 0` fall back to the default.
    pub min_deviation_mad: f64,

    /// Aggregations to scan. Default: `[Average, Count]`.
    ///
    /// An empty list falls back to the default pair. Every aggregate yields its own state and its own
    /// anomalies.
    pub aggregations: Vec<Aggregate>,
}

impl Default for ScanMwConfig {
    fn default() -> Self {
        Self {
            min_segment: DEFAULT_MIN_SEGMENT,
            min_points: DEFAULT_MIN_POINTS,
            max_points: crate::detectors::numerics::SCAN_MAX_POINTS as i32,
            significance_threshold: DEFAULT_SIGNIFICANCE_THRESHOLD,
            min_effect_size: DEFAULT_MIN_EFFECT_SIZE,
            min_deviation_mad: DEFAULT_MIN_DEVIATION_MAD,
            aggregations: vec![Aggregate::Average, Aggregate::Count],
        }
    }
}

impl ScanMwConfig {
    /// Fills in zero/negative fields with their defaults, in place (the Go `ensureDefaults`).
    fn ensure_defaults(&mut self) {
        ensure_window_defaults(&mut self.min_segment, &mut self.min_points, &mut self.max_points);
        if self.significance_threshold <= 0.0 {
            self.significance_threshold = DEFAULT_SIGNIFICANCE_THRESHOLD;
        }
        if self.min_effect_size <= 0.0 {
            self.min_effect_size = DEFAULT_MIN_EFFECT_SIZE;
        }
        if self.min_deviation_mad <= 0.0 {
            self.min_deviation_mad = DEFAULT_MIN_DEVIATION_MAD;
        }
        if self.aggregations.is_empty() {
            self.aggregations = vec![Aggregate::Average, Aggregate::Count];
        }
    }

    /// Returns the normalized scan window.
    fn window(&self) -> ScanWindow {
        ScanWindow::normalized(self.min_segment, self.min_points, self.max_points)
    }

    /// Returns the normalized statistic thresholds.
    fn thresholds(&self) -> ScanThresholds {
        ScanThresholds {
            significance_threshold: self.significance_threshold,
            min_effect_size: self.min_effect_size,
            min_deviation_mad: self.min_deviation_mad,
        }
    }
}

/// The Mann-Whitney scan changepoint detector.
#[derive(Debug)]
pub struct ScanMwDetector {
    config: ScanMwConfig,
    state: ScanState,
}

impl ScanMwDetector {
    /// Creates a detector with the production defaults.
    pub fn new() -> Self {
        Self {
            config: ScanMwConfig::default(),
            state: ScanState::new(),
        }
    }

    /// Creates a detector with an explicit configuration.
    ///
    /// The configuration is normalized in place on the first pass, so zero/negative fields behave like the
    /// Go `ensureDefaults` even when set here.
    pub fn with_config(config: ScanMwConfig) -> Self {
        Self {
            config,
            state: ScanState::new(),
        }
    }
}

impl Default for ScanMwDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector for ScanMwDetector {
    type Config = ScanMwConfig;

    fn name(&self) -> &str {
        NAME
    }

    fn config(&self) -> &Self::Config {
        &self.config
    }

    fn is_ready(&self) -> bool {
        self.state.is_ready()
    }

    fn detect(&mut self, view: &dyn StorageView, data_time_sec: i64) -> Vec<Anomaly> {
        self.config.ensure_defaults();
        let window = self.config.window();
        let thresholds = self.config.thresholds();
        let min_segment = window.min_segment;
        self.state.detect(
            view,
            data_time_sec,
            window,
            NAME,
            &self.config.aggregations,
            |workspace, points| scan_mann_whitney(workspace, points, min_segment, thresholds),
            |evidence| AnomalyEvidence::ScanMw {
                baseline_median: evidence.baseline_median,
                baseline_mad: evidence.baseline_mad,
                current_value: evidence.current_value,
                deviation_sigma: evidence.deviation_sigma,
                p_value: evidence.p_value,
                effect_size: evidence.effect_size,
            },
        )
    }

    fn reset(&mut self) {
        self.state.reset();
    }

    fn remove_series(&mut self, series: &[SeriesRef]) {
        self.config.ensure_defaults();
        self.state.remove_series(series, &self.config.aggregations);
    }
}

/// Scans one point buffer for the most significant Mann-Whitney split.
///
/// Returns `None` when no split passes the p-value, effect-size, and robust-deviation gates, or when the
/// buffer is too short to contain a split (`n < 2 * min_segment`). The Go implementation would index out of
/// range for `n < min_segment`; returning "not found" reproduces the behavior of its valid input domain.
pub(crate) fn scan_mann_whitney(
    workspace: &mut ScanDetectorWorkspace, points: &[Point], min_segment: usize, thresholds: ScanThresholds,
) -> Option<ScanDetection> {
    let n = points.len();
    let min_segment = min_segment.max(1);
    if n < 2 * min_segment {
        return None;
    }

    // Copy the values out: the workspace borrows them back for ranking and statistics, and the deviation
    // gate needs the workspace mutably afterwards.
    let values = workspace.values_from_points(points).to_vec();
    let (ranks, tie_correction) = workspace.assign_ranks(&values);

    let f_n = n as f64;
    let mut rank_sum = 0.0;
    for &rank in &ranks[..min_segment] {
        rank_sum += rank;
    }

    let mut best_z = 0.0;
    let mut best_split: Option<usize> = None;
    for split in min_segment..=(n - min_segment) {
        if split > min_segment {
            rank_sum += ranks[split - 1];
        }

        let f_split = split as f64;
        let f_rest = (n - split) as f64;
        let u1 = rank_sum - f_split * (f_split + 1.0) / 2.0;
        let u = u1.min(f_split * f_rest - u1);

        let mean_u = f_split * f_rest / 2.0;
        let variance_u = (f_split * f_rest / 12.0) * (f_n + 1.0 - tie_correction / (f_n * (f_n - 1.0)));
        if variance_u <= 0.0 {
            continue;
        }

        let z = ((u - mean_u).abs() - 0.5) / variance_u.sqrt();
        let z = if z < 0.0 { 0.0 } else { z };
        // Strictly greater, so the earliest split wins a tie (matching the Go `z > bestZAbs`).
        if z > best_z {
            best_z = z;
            best_split = Some(split);
        }
    }

    let best_split = best_split?;
    let p_value = mw_p_value(best_z);
    if p_value >= thresholds.significance_threshold {
        return None;
    }

    // Recompute the statistics at the winning split; the scan above only tracked the rank sum.
    let split = mann_whitney_split(ranks, tie_correction, best_split)?;
    let effect_size = mw_effect_size(split.u, best_split, n);
    if effect_size.abs() < thresholds.min_effect_size {
        return None;
    }

    let deviation = robust_deviation(workspace, &values, best_split, thresholds.min_deviation_mad)?;

    Some(ScanDetection {
        change_index: best_split,
        evidence: ScanEvidence {
            baseline_median: deviation.baseline_median,
            baseline_mad: deviation.baseline_mad,
            current_value: deviation.current_value,
            deviation_sigma: deviation.deviation_sigma,
            p_value,
            effect_size,
            test_statistic: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageConfig;
    use crate::identity::QueryHandle;
    use crate::storage::TimeSeriesStorage;

    /// The Go `testScanMWDetector`: the default detector restricted to the average aggregate.
    fn average_only() -> ScanMwConfig {
        ScanMwConfig {
            aggregations: vec![Aggregate::Average],
            ..ScanMwConfig::default()
        }
    }

    fn points(values: &[f64]) -> Vec<Point> {
        values
            .iter()
            .enumerate()
            .map(|(index, &value)| Point {
                second: index as i64 + 1,
                value,
            })
            .collect()
    }

    /// Fixture reference numbers are produced by the verbatim Go scan bodies; arithmetic-only values are
    /// asserted exactly, `exp`/`sqrt`-dependent ones with a documented tolerance.
    #[test]
    fn scan_detects_step_change_at_the_transition() {
        let mut workspace = ScanDetectorWorkspace::new();
        let mut values = vec![50.0; 20];
        values.extend(vec![200.0; 20]);

        let detection = scan_mann_whitney(
            &mut workspace,
            &points(&values),
            12,
            ScanMwConfig::default().thresholds(),
        )
        .expect("the step change is detected");

        assert_eq!(detection.change_index, 20, "the split lands on the transition");
        // Go: p = 2 * normalCDFUpper(6.229385503402403) = 4.702215998070091e-10
        assert!(
            (detection.evidence.p_value - 4.702_215_998_070_091e-10).abs() < 1e-12,
            "p = {}",
            detection.evidence.p_value
        );
        assert_eq!(detection.evidence.effect_size, 1.0);
        assert_eq!(detection.evidence.baseline_median, 50.0);
        assert_eq!(detection.evidence.baseline_mad, 0.0);
        assert_eq!(detection.evidence.current_value, 200.0);
        assert_eq!(detection.evidence.deviation_sigma, 300.0);
        assert_eq!(detection.evidence.test_statistic, None);
    }

    #[test]
    fn scan_selects_the_split_at_the_change_index() {
        let mut workspace = ScanDetectorWorkspace::new();
        let mut values = vec![50.0; 16];
        values.extend(vec![200.0; 24]);

        let detection = scan_mann_whitney(
            &mut workspace,
            &points(&values),
            12,
            ScanMwConfig::default().thresholds(),
        )
        .expect("the step change is detected");

        assert_eq!(detection.change_index, 16);
    }

    #[test]
    fn scan_handles_tied_ranks() {
        // Both segments are constant, so every value is tied within its segment and the tie correction
        // dominates the variance. Go: p = 4.706949313954042e-10 at split 22.
        let mut workspace = ScanDetectorWorkspace::new();
        let mut values = vec![10.0; 22];
        values.extend(vec![20.0; 18]);

        let detection = scan_mann_whitney(
            &mut workspace,
            &points(&values),
            12,
            ScanMwConfig::default().thresholds(),
        )
        .expect("tied values still produce a split");

        assert_eq!(detection.change_index, 22);
        assert!(
            (detection.evidence.p_value - 4.706_949_313_954_042e-10).abs() < 1e-12,
            "p = {}",
            detection.evidence.p_value
        );
        assert_eq!(detection.evidence.effect_size, 1.0);
    }

    #[test]
    fn scan_rejects_when_the_p_value_gate_fails() {
        let mut workspace = ScanDetectorWorkspace::new();
        let mut values = vec![50.0; 20];
        values.extend(vec![200.0; 20]);
        let thresholds = ScanThresholds {
            significance_threshold: 1e-30,
            ..ScanMwConfig::default().thresholds()
        };

        assert!(scan_mann_whitney(&mut workspace, &points(&values), 12, thresholds).is_none());
    }

    #[test]
    fn scan_rejects_when_the_effect_size_gate_fails() {
        let mut workspace = ScanDetectorWorkspace::new();
        let mut values = vec![50.0; 20];
        values.extend(vec![200.0; 20]);
        let thresholds = ScanThresholds {
            // The fixture's effect size is exactly 1.0, so a floor above it rejects.
            min_effect_size: 1.5,
            ..ScanMwConfig::default().thresholds()
        };

        assert!(scan_mann_whitney(&mut workspace, &points(&values), 12, thresholds).is_none());
    }

    #[test]
    fn scan_rejects_when_the_deviation_gate_fails() {
        let mut workspace = ScanDetectorWorkspace::new();
        let mut values = vec![50.0; 20];
        values.extend(vec![200.0; 20]);
        let thresholds = ScanThresholds {
            min_deviation_mad: 1e9,
            ..ScanMwConfig::default().thresholds()
        };

        assert!(scan_mann_whitney(&mut workspace, &points(&values), 12, thresholds).is_none());
    }

    #[test]
    fn scan_ignores_a_constant_series() {
        let mut workspace = ScanDetectorWorkspace::new();
        let values = vec![50.0; 40];

        assert!(scan_mann_whitney(
            &mut workspace,
            &points(&values),
            12,
            ScanMwConfig::default().thresholds()
        )
        .is_none());
    }

    #[test]
    fn scan_returns_none_when_no_split_fits() {
        let mut workspace = ScanDetectorWorkspace::new();
        // n < 2 * min_segment: Go's scan-MW would index out of range here; the port reports "not found".
        let values = vec![50.0; 12];
        assert!(scan_mann_whitney(
            &mut workspace,
            &points(&values),
            12,
            ScanMwConfig::default().thresholds()
        )
        .is_none());

        // min_segment <= n < 2 * min_segment: Go's k-range is empty and nothing is found.
        let values = vec![50.0; 20];
        assert!(scan_mann_whitney(
            &mut workspace,
            &points(&values),
            12,
            ScanMwConfig::default().thresholds()
        )
        .is_none());
    }

    fn storage(values: &[(i64, f64)]) -> (TimeSeriesStorage, SeriesRef) {
        let mut storage = TimeSeriesStorage::new(StorageConfig::default());
        let mut series_ref = None;
        for &(second, value) in values {
            let result = storage.add("ns", "metric", None, value, second, &[]);
            if result.series_ref.is_some() {
                series_ref = result.series_ref;
            }
        }
        (storage, series_ref.expect("the fixture writes at least one point"))
    }

    fn step_data(baseline: usize, baseline_value: f64, changed: usize, changed_value: f64) -> Vec<(i64, f64)> {
        let mut data: Vec<(i64, f64)> = (1..=baseline).map(|second| (second as i64, baseline_value)).collect();
        data.extend((baseline + 1..=baseline + changed).map(|second| (second as i64, changed_value)));
        data
    }

    #[test]
    fn not_enough_points_does_not_fire() {
        let mut detector = ScanMwDetector::with_config(average_only());
        let (storage, _) = storage(&(1..=10).map(|second| (second as i64, 100.0)).collect::<Vec<_>>());

        assert!(detector.detect(&storage, 10).is_empty());
        assert!(!detector.is_ready());
    }

    #[test]
    fn detects_step_change() {
        let mut detector = ScanMwDetector::with_config(average_only());
        let (storage, series_ref) = storage(&step_data(20, 50.0, 20, 200.0));

        let anomalies = detector.detect(&storage, 40);

        assert_eq!(anomalies.len(), 1, "should detect step change");
        let anomaly = &anomalies[0];
        assert_eq!(anomaly.detector_name, "scanmw");
        assert_eq!(
            anomaly.series_ref,
            Some(QueryHandle::new(series_ref, Aggregate::Average))
        );
        assert_eq!(anomaly.series.name, "metric");
        assert_eq!(anomaly.series.aggregate, Aggregate::Average);
        assert_eq!(anomaly.timestamp_sec, 21, "changepoint near the transition at index 20");
        assert_eq!(anomaly.sampling_interval_sec, 1);
        assert!(detector.is_ready());
        match anomaly.evidence.as_ref().expect("scan evidence is present") {
            AnomalyEvidence::ScanMw {
                baseline_median,
                deviation_sigma,
                p_value,
                effect_size,
                ..
            } => {
                assert_eq!(*baseline_median, 50.0);
                assert_eq!(*deviation_sigma, 300.0);
                assert!(*p_value > 0.0);
                assert_eq!(*effect_size, 1.0);
            }
            other => panic!("unexpected evidence: {other:?}"),
        }
        // -log10(4.702215998070091e-10) from the Go fixture.
        assert!((anomaly.score.expect("scored") - 9.327_697_425_271_61).abs() < 1e-9);
    }

    #[test]
    fn detects_downward_step_change() {
        let mut detector = ScanMwDetector::with_config(average_only());
        let (storage, _) = storage(&step_data(20, 200.0, 20, 50.0));

        let anomalies = detector.detect(&storage, 40);

        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].timestamp_sec, 21);
    }

    #[test]
    fn detects_step_change_with_historical_timestamps() {
        // The scan must not depend on small or monotonic-from-one timestamps: the same step shifted to a
        // current-era Unix timestamp detects at the same relative index.
        const BASE: i64 = 1_700_000_000;
        let mut detector = ScanMwDetector::with_config(average_only());
        let data: Vec<(i64, f64)> = step_data(20, 50.0, 20, 200.0)
            .into_iter()
            .map(|(second, value)| (BASE + second, value))
            .collect();
        let (storage, _) = storage(&data);

        let anomalies = detector.detect(&storage, BASE + 40);

        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].timestamp_sec, BASE + 21);
    }

    #[test]
    fn incremental_advance_only_scans_new_data() {
        let mut detector = ScanMwDetector::with_config(average_only());
        let (mut storage, _) = storage(&(1..=20).map(|second| (second as i64, 50.0)).collect::<Vec<_>>());

        // First advance: stable data with enough points to scan, but no changepoint.
        assert!(detector.detect(&storage, 20).is_empty(), "no anomaly in stable data");

        // Second advance: add shifted data, now 40 visible points.
        for second in 21..=40 {
            storage.add("ns", "metric", None, 200.0, second, &[]);
        }
        assert_eq!(
            detector.detect(&storage, 40).len(),
            1,
            "should detect the step on the second advance"
        );

        // Third advance: no new data, so the count/write-generation gate skips the scan.
        assert!(
            detector.detect(&storage, 40).is_empty(),
            "no new data produces no anomalies"
        );
    }

    #[test]
    fn segment_advancement_does_not_refire_on_stable_data() {
        let mut detector = ScanMwDetector::with_config(average_only());
        let (mut storage, _) = storage(&step_data(20, 50.0, 30, 200.0));

        assert_eq!(
            detector.detect(&storage, 50).len(),
            1,
            "should detect the first changepoint"
        );

        // Stable post-change data: the segment start has advanced past the change, so no re-fire.
        for second in 51..=90 {
            storage.add("ns", "metric", None, 200.0, second, &[]);
        }
        assert!(
            detector.detect(&storage, 90).is_empty(),
            "stable post-change data does not re-fire"
        );
    }

    #[test]
    fn two_sequential_changes_are_both_detected() {
        let mut detector = ScanMwDetector::with_config(average_only());
        let (mut storage, _) = storage(&step_data(20, 50.0, 30, 200.0));

        assert_eq!(
            detector.detect(&storage, 50).len(),
            1,
            "should detect the first changepoint"
        );

        for second in 51..=80 {
            storage.add("ns", "metric", None, 500.0, second, &[]);
        }
        let anomalies = detector.detect(&storage, 80);

        assert_eq!(
            anomalies.len(),
            1,
            "should detect the second changepoint after segment advancement"
        );
        assert_eq!(anomalies[0].timestamp_sec, 51);
    }

    #[test]
    fn deterministic_replay_produces_identical_output() {
        let (storage, _) = storage(&step_data(20, 50.0, 20, 200.0));

        let mut first = ScanMwDetector::with_config(average_only());
        let first_result = first.detect(&storage, 40);
        let mut second = ScanMwDetector::with_config(average_only());
        let second_result = second.detect(&storage, 40);

        assert_eq!(first_result.len(), second_result.len());
        for (left, right) in first_result.iter().zip(second_result.iter()) {
            assert_eq!(left.timestamp_sec, right.timestamp_sec);
            assert_eq!(left.series_ref, right.series_ref);
            assert_eq!(left.score, right.score);
            assert_eq!(left.series, right.series);
        }
    }

    #[test]
    fn preloaded_replay_fires_as_data_time_advances() {
        // The offline replay path: every point is written before the first pass, so the write generation
        // reaches its final value immediately and only the visible point count grows. A write-generation-only
        // skip gate would suppress every scan after the first; the count-based gate keeps scanning.
        let mut detector = ScanMwDetector::with_config(average_only());
        let (storage, _) = storage(&step_data(20, 50.0, 20, 200.0));

        let mut fired = false;
        for data_time in 1..=40 {
            if !detector.detect(&storage, data_time).is_empty() {
                fired = true;
            }
        }

        assert!(
            fired,
            "preloaded replay should detect the step change as dataTime advances"
        );
    }

    #[test]
    fn reset_clears_per_series_and_cached_state() {
        let mut detector = ScanMwDetector::with_config(average_only());
        let (storage, _) = storage(&step_data(20, 50.0, 20, 200.0));

        detector.detect(&storage, 40);
        assert!(!detector.state.series.is_empty(), "should have state after detection");
        assert!(
            !detector.state.cached_refs.is_empty(),
            "should have cached refs after detection"
        );

        detector.reset();

        assert!(detector.state.series.is_empty(), "reset clears all per-series state");
        assert!(detector.state.cached_refs.is_empty(), "reset clears the cached refs");
        assert_eq!(detector.state.cached_gen, None);
        assert!(!detector.is_ready());
    }

    #[test]
    fn remove_series_drops_tracked_state_and_tolerates_unknown_refs() {
        let mut detector = ScanMwDetector::with_config(average_only());
        let (storage, series_ref) = storage(&step_data(20, 50.0, 20, 200.0));

        detector.detect(&storage, 40);
        assert!(!detector.state.series.is_empty());

        // An unknown ref is a no-op rather than a panic.
        detector.remove_series(&[SeriesRef::new(9_999)]);
        assert!(!detector.state.series.is_empty());

        detector.remove_series(&[series_ref]);
        assert!(
            detector.state.series.is_empty(),
            "remove_series drops the tracked series"
        );
        assert!(detector.state.cached_refs.is_empty());
        assert_eq!(detector.state.cached_gen, None);
    }
}
