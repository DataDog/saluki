//! The Welch-t scan detector (`scanwelch`).
//!
//! Ported from `observer/impl/metrics_detector_scanwelch.go`. The detector is a hybrid: a parametric phase
//! finds the split with the largest Welch `|t|` using running sums, then a non-parametric phase verifies the
//! candidate with a Mann-Whitney p-value and effect size, and a final phase requires the median to move far
//! relative to the pre-change MAD.
//!
//! The streaming machinery — per-series state, the discovery/visibility gates, the post-change segment
//! truncation, and the shared verification statistics — lives in [`crate::detectors::scan`]; this module
//! owns only the split-selection loop and the configuration.
//!
//! # Design
//!
//! The two-phase split is deliberate: the t-statistic is cheap to update incrementally and reacts to mean
//! shifts, while the Mann-Whitney verification is robust to distribution shape and to outliers that inflate
//! the variance. A single extreme value can win the t-phase without being a real distributional change, and
//! the rank-based verification rejects exactly that case.

use crate::detectors::numerics::ScanDetectorWorkspace;
use crate::detectors::scan::{
    ensure_window_defaults, mann_whitney_split, mw_effect_size, mw_p_value, robust_deviation, ScanDetection,
    ScanEvidence, ScanState, ScanThresholds, ScanWindow, DEFAULT_MIN_DEVIATION_MAD, DEFAULT_MIN_EFFECT_SIZE,
    DEFAULT_MIN_POINTS, DEFAULT_MIN_SEGMENT, DEFAULT_MIN_T_STATISTIC, DEFAULT_SIGNIFICANCE_THRESHOLD,
};
use crate::identity::{Aggregate, SeriesRef};
use crate::model::{Anomaly, AnomalyEvidence, Point};
use crate::traits::{Detector, StorageView};

/// The detector name reported in anomalies and used by the scorer's per-detector thresholds.
pub const NAME: &str = "scanwelch";

/// Configuration for the ScanWelch detector.
///
/// The defaults match the Go `NewScanWelchDetector`. Every field is normalized on the first detection pass:
/// a non-positive value (or an empty aggregation list) falls back to its default, and a `max_points` below
/// `min_points` is raised to `min_points`.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanWelchConfig {
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

    /// Smallest Welch `|t|` accepted by the candidate-selection phase. Default: `8.0`.
    ///
    /// Lower values let more candidates through to the Mann-Whitney verification. Values `<= 0` fall back to
    /// the default.
    pub min_t_statistic: f64,

    /// Largest Mann-Whitney p-value the verification phase may report. Default: `1e-8`.
    ///
    /// Lower values make the verification stricter. Values `<= 0` fall back to the default.
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

impl Default for ScanWelchConfig {
    fn default() -> Self {
        Self {
            min_segment: DEFAULT_MIN_SEGMENT,
            min_points: DEFAULT_MIN_POINTS,
            max_points: crate::detectors::numerics::SCAN_MAX_POINTS as i32,
            min_t_statistic: DEFAULT_MIN_T_STATISTIC,
            significance_threshold: DEFAULT_SIGNIFICANCE_THRESHOLD,
            min_effect_size: DEFAULT_MIN_EFFECT_SIZE,
            min_deviation_mad: DEFAULT_MIN_DEVIATION_MAD,
            aggregations: vec![Aggregate::Average, Aggregate::Count],
        }
    }
}

impl ScanWelchConfig {
    /// Fills in zero/negative fields with their defaults, in place (the Go `ensureDefaults`).
    fn ensure_defaults(&mut self) {
        ensure_window_defaults(&mut self.min_segment, &mut self.min_points, &mut self.max_points);
        if self.min_t_statistic <= 0.0 {
            self.min_t_statistic = DEFAULT_MIN_T_STATISTIC;
        }
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

/// The hybrid Welch/Mann-Whitney scan changepoint detector.
#[derive(Debug)]
pub struct ScanWelchDetector {
    config: ScanWelchConfig,
    state: ScanState,
}

impl ScanWelchDetector {
    /// Creates a detector with the production defaults.
    pub fn new() -> Self {
        Self {
            config: ScanWelchConfig::default(),
            state: ScanState::new(),
        }
    }

    /// Creates a detector with an explicit configuration.
    ///
    /// The configuration is normalized in place on the first pass, so zero/negative fields behave like the
    /// Go `ensureDefaults` even when set here.
    pub fn with_config(config: ScanWelchConfig) -> Self {
        Self {
            config,
            state: ScanState::new(),
        }
    }
}

impl Default for ScanWelchDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector for ScanWelchDetector {
    type Config = ScanWelchConfig;

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
        let min_t_statistic = self.config.min_t_statistic;
        self.state.detect(
            view,
            data_time_sec,
            window,
            NAME,
            &self.config.aggregations,
            |workspace, points| scan_welch(workspace, points, min_segment, min_t_statistic, thresholds),
            |evidence| AnomalyEvidence::ScanWelch {
                baseline_median: evidence.baseline_median,
                baseline_mad: evidence.baseline_mad,
                current_value: evidence.current_value,
                deviation_sigma: evidence.deviation_sigma,
                p_value: evidence.p_value,
                effect_size: evidence.effect_size,
                test_statistic: evidence.test_statistic.unwrap_or(0.0),
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

/// Scans one point buffer for the best Welch split, verified by Mann-Whitney and a robust deviation.
///
/// Returns `None` when the buffer is too short for a split (`n < 2 * min_segment`), when no candidate beats
/// `min_t_statistic`, or when the Mann-Whitney p-value, effect size, or robust deviation gates reject the
/// winning candidate.
pub(crate) fn scan_welch(
    workspace: &mut ScanDetectorWorkspace, points: &[Point], min_segment: usize, min_t_statistic: f64,
    thresholds: ScanThresholds,
) -> Option<ScanDetection> {
    let n = points.len();
    let min_segment = min_segment.max(1);
    // A valid split needs min_segment points on each side. min_points is independently configurable, so a
    // newly activated series can legitimately reach this method before a candidate split exists.
    if n < 2 * min_segment {
        return None;
    }

    // Copy the values out: the workspace borrows them back for ranking and statistics, and the deviation
    // gate needs the workspace mutably afterwards.
    let values = workspace.values_from_points(points).to_vec();

    // Phase 1: scan with Welch's t-statistic, keeping total and left-side running moments rather than a
    // prefix array for every candidate split.
    let mut total_sum = 0.0;
    let mut total_sum_sq = 0.0;
    for &value in &values {
        total_sum += value;
        total_sum_sq += value * value;
    }
    let mut left_sum = 0.0;
    let mut left_sum_sq = 0.0;
    for &value in &values[..min_segment] {
        left_sum += value;
        left_sum_sq += value * value;
    }

    let mut best_t = 0.0;
    let mut best_split: Option<usize> = None;
    // The value that joins the left side when the split advances past it, in Go's `values[k]` order.
    let mut upcoming = values[min_segment..].iter();
    for split in min_segment..=(n - min_segment) {
        let f_split = split as f64;
        let f_rest = (n - split) as f64;

        let left_mean = left_sum / f_split;
        let right_mean = (total_sum - left_sum) / f_rest;
        let mut left_var = left_sum_sq / f_split - left_mean * left_mean;
        let mut right_var = (total_sum_sq - left_sum_sq) / f_rest - right_mean * right_mean;
        if left_var < 1e-12 {
            left_var = 1e-12;
        }
        if right_var < 1e-12 {
            right_var = 1e-12;
        }

        let se = (left_var / f_split + right_var / f_rest).sqrt();
        // The Go loop `continue`s here, skipping the left-moment advance below. With the variance floored at
        // 1e-12 the branch is unreachable for any real buffer, so the port keeps the same shape.
        if se < 1e-15 {
            continue;
        }
        let t = (left_mean - right_mean).abs() / se;
        // Strictly greater, so the earliest split wins a tie (matching the Go `t > bestTAbs`).
        if t > best_t {
            best_t = t;
            best_split = Some(split);
        }

        if split < n - min_segment {
            let value = *upcoming.next().expect("one upcoming value per advancing split");
            left_sum += value;
            left_sum_sq += value * value;
        }
    }

    let best_split = best_split?;
    if best_t < min_t_statistic {
        return None;
    }

    // Phase 2: verify the candidate with Mann-Whitney at the same split.
    let (ranks, tie_correction) = workspace.assign_ranks(&values);
    let split = mann_whitney_split(ranks, tie_correction, best_split)?;
    let p_value = mw_p_value(split.z);
    if p_value >= thresholds.significance_threshold {
        return None;
    }
    let effect_size = mw_effect_size(split.u, best_split, n);
    if effect_size.abs() < thresholds.min_effect_size {
        return None;
    }

    // Phase 3: require the median to move far relative to the pre-change spread.
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
            test_statistic: Some(best_t),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageConfig;
    use crate::identity::QueryHandle;
    use crate::storage::TimeSeriesStorage;

    /// The Go `testScanWelchDetector`: the default detector restricted to the average aggregate.
    fn average_only() -> ScanWelchConfig {
        ScanWelchConfig {
            aggregations: vec![Aggregate::Average],
            ..ScanWelchConfig::default()
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

        let detection = scan_welch(
            &mut workspace,
            &points(&values),
            12,
            DEFAULT_MIN_T_STATISTIC,
            ScanWelchConfig::default().thresholds(),
        )
        .expect("the step change is detected");

        assert_eq!(detection.change_index, 20, "the split lands on the transition");
        // Go: bestT = 474341649.02525693 (two constant segments leave a near-zero standard error).
        let test_statistic = detection.evidence.test_statistic.expect("welch sets the t statistic");
        assert!(
            (test_statistic - 474_341_649.025_256_93).abs() / 474_341_649.025_256_93 < 1e-12,
            "t = {test_statistic}"
        );
        assert!(
            (detection.evidence.p_value - 4.702_215_998_070_091e-10).abs() < 1e-12,
            "p = {}",
            detection.evidence.p_value
        );
        assert_eq!(detection.evidence.effect_size, 1.0);
        assert_eq!(detection.evidence.baseline_median, 50.0);
        assert_eq!(detection.evidence.current_value, 200.0);
        assert_eq!(detection.evidence.deviation_sigma, 300.0);
    }

    #[test]
    fn scan_selects_the_split_at_the_change_index() {
        let mut workspace = ScanDetectorWorkspace::new();
        let mut values = vec![50.0; 16];
        values.extend(vec![200.0; 24]);

        let detection = scan_welch(
            &mut workspace,
            &points(&values),
            12,
            DEFAULT_MIN_T_STATISTIC,
            ScanWelchConfig::default().thresholds(),
        )
        .expect("the step change is detected");

        assert_eq!(detection.change_index, 16);
    }

    #[test]
    fn scan_returns_none_when_no_split_fits() {
        let mut workspace = ScanDetectorWorkspace::new();
        let values = vec![100.0; 20];
        assert!(scan_welch(
            &mut workspace,
            &points(&values),
            12,
            DEFAULT_MIN_T_STATISTIC,
            ScanWelchConfig::default().thresholds(),
        )
        .is_none());
    }

    #[test]
    fn scan_rejects_when_the_t_statistic_gate_fails() {
        let mut workspace = ScanDetectorWorkspace::new();
        let mut values = vec![50.0; 20];
        values.extend(vec![200.0; 20]);

        assert!(scan_welch(
            &mut workspace,
            &points(&values),
            12,
            1e18,
            ScanWelchConfig::default().thresholds(),
        )
        .is_none());
    }

    #[test]
    fn scan_rejects_when_the_mann_whitney_p_value_gate_fails() {
        let mut workspace = ScanDetectorWorkspace::new();
        let mut values = vec![50.0; 20];
        values.extend(vec![200.0; 20]);
        let thresholds = ScanThresholds {
            significance_threshold: 1e-30,
            ..ScanWelchConfig::default().thresholds()
        };

        assert!(scan_welch(
            &mut workspace,
            &points(&values),
            12,
            DEFAULT_MIN_T_STATISTIC,
            thresholds
        )
        .is_none());
    }

    #[test]
    fn scan_rejects_when_the_effect_size_gate_fails() {
        let mut workspace = ScanDetectorWorkspace::new();
        let mut values = vec![50.0; 20];
        values.extend(vec![200.0; 20]);
        let thresholds = ScanThresholds {
            // The fixture's effect size is exactly 1.0, so a floor above it rejects.
            min_effect_size: 1.5,
            ..ScanWelchConfig::default().thresholds()
        };

        assert!(scan_welch(
            &mut workspace,
            &points(&values),
            12,
            DEFAULT_MIN_T_STATISTIC,
            thresholds
        )
        .is_none());
    }

    #[test]
    fn scan_rejects_when_the_deviation_gate_fails() {
        let mut workspace = ScanDetectorWorkspace::new();
        let mut values = vec![50.0; 20];
        values.extend(vec![200.0; 20]);
        let thresholds = ScanThresholds {
            min_deviation_mad: 1e9,
            ..ScanWelchConfig::default().thresholds()
        };

        assert!(scan_welch(
            &mut workspace,
            &points(&values),
            12,
            DEFAULT_MIN_T_STATISTIC,
            thresholds
        )
        .is_none());
    }

    #[test]
    fn mann_whitney_verification_rejects_an_outlier_driven_candidate() {
        // One extreme post-change value wins the t-phase (t = 1.044 at split 28) without changing the
        // distribution shape: the post-change median is still 100. With the Mann-Whitney and deviation gates
        // relaxed, that candidate is accepted; with the production p-value or effect-size gate it is not,
        // because the rank-based verification sees p = 0.141 and an effect size of only 0.083.
        let mut workspace = ScanDetectorWorkspace::new();
        let mut values = vec![100.0; 20];
        values.extend(vec![100.0; 19]);
        values.push(1e6);
        let points = points(&values);

        let relaxed = ScanThresholds {
            significance_threshold: 1.0,
            min_effect_size: 0.01,
            min_deviation_mad: 0.0,
        };
        let detection =
            scan_welch(&mut workspace, &points, 12, 0.5, relaxed).expect("the relaxed candidate passes the t-phase");
        assert_eq!(detection.change_index, 28);
        assert!(
            (detection.evidence.p_value - 0.140_758_990_289_240_08).abs() < 1e-12,
            "p = {}",
            detection.evidence.p_value
        );
        assert!((detection.evidence.effect_size - 0.083_333_333_333_333_37).abs() < 1e-12);

        // The Mann-Whitney p-value gate rejects the candidate.
        let p_value_gate = ScanThresholds {
            significance_threshold: DEFAULT_SIGNIFICANCE_THRESHOLD,
            ..relaxed
        };
        assert!(
            scan_welch(&mut workspace, &points, 12, 0.5, p_value_gate).is_none(),
            "the rank-based p-value verification rejects the outlier"
        );

        // So does the effect-size gate.
        let effect_gate = ScanThresholds {
            min_effect_size: DEFAULT_MIN_EFFECT_SIZE,
            ..relaxed
        };
        assert!(
            scan_welch(&mut workspace, &points, 12, 0.5, effect_gate).is_none(),
            "the effect-size gate rejects the outlier"
        );

        // And the production t-gate never even considers it.
        assert!(scan_welch(
            &mut workspace,
            &points,
            12,
            DEFAULT_MIN_T_STATISTIC,
            ScanWelchConfig::default().thresholds(),
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
        let mut detector = ScanWelchDetector::with_config(average_only());
        let (storage, _) = storage(&(1..=10).map(|second| (second as i64, 100.0)).collect::<Vec<_>>());

        assert!(detector.detect(&storage, 10).is_empty());
        assert!(!detector.is_ready());
    }

    #[test]
    fn min_points_below_min_segment_does_not_panic() {
        // Go `TestScanWelch_MinPointsBelowMinSegmentDoesNotPanic`: the buffer can satisfy min_points without
        // containing a split, which must be a no-op rather than an index-out-of-range.
        let config = ScanWelchConfig {
            min_points: 4,
            min_segment: 12,
            max_points: 24,
            aggregations: vec![Aggregate::Average],
            ..ScanWelchConfig::default()
        };
        let mut detector = ScanWelchDetector::with_config(config);
        let (storage, _) = storage(&(1..=12).map(|second| (second as i64, 100.0)).collect::<Vec<_>>());

        assert!(detector.detect(&storage, 12).is_empty());
    }

    #[test]
    fn detects_step_change() {
        let mut detector = ScanWelchDetector::with_config(average_only());
        let (storage, series_ref) = storage(&step_data(20, 50.0, 20, 200.0));

        let anomalies = detector.detect(&storage, 40);

        assert_eq!(anomalies.len(), 1, "should detect step change");
        let anomaly = &anomalies[0];
        assert_eq!(anomaly.detector_name, "scanwelch");
        assert_eq!(
            anomaly.series_ref,
            Some(QueryHandle::new(series_ref, Aggregate::Average))
        );
        assert_eq!(anomaly.series.name, "metric");
        assert_eq!(anomaly.timestamp_sec, 21, "changepoint near the transition at index 20");
        assert_eq!(anomaly.sampling_interval_sec, 1);
        assert!(detector.is_ready());
        match anomaly.evidence.as_ref().expect("scan evidence is present") {
            AnomalyEvidence::ScanWelch {
                baseline_median,
                deviation_sigma,
                p_value,
                effect_size,
                test_statistic,
                ..
            } => {
                assert_eq!(*baseline_median, 50.0);
                assert_eq!(*deviation_sigma, 300.0);
                assert!(*p_value > 0.0);
                assert_eq!(*effect_size, 1.0);
                assert!(*test_statistic > 0.0);
            }
            other => panic!("unexpected evidence: {other:?}"),
        }
        assert!((anomaly.score.expect("scored") - 9.327_697_425_271_61).abs() < 1e-9);
    }

    #[test]
    fn detects_downward_step_change() {
        let mut detector = ScanWelchDetector::with_config(average_only());
        let (storage, _) = storage(&step_data(20, 200.0, 20, 50.0));

        let anomalies = detector.detect(&storage, 40);

        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].timestamp_sec, 21);
    }

    #[test]
    fn detects_step_change_with_historical_timestamps() {
        const BASE: i64 = 1_700_000_000;
        let mut detector = ScanWelchDetector::with_config(average_only());
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
        let mut detector = ScanWelchDetector::with_config(average_only());
        let (mut storage, _) = storage(&(1..=20).map(|second| (second as i64, 50.0)).collect::<Vec<_>>());

        assert!(detector.detect(&storage, 20).is_empty(), "no anomaly in stable data");

        for second in 21..=40 {
            storage.add("ns", "metric", None, 200.0, second, &[]);
        }
        assert_eq!(
            detector.detect(&storage, 40).len(),
            1,
            "should detect the step on the second advance"
        );

        assert!(
            detector.detect(&storage, 40).is_empty(),
            "no new data produces no anomalies"
        );
    }

    #[test]
    fn segment_advancement_does_not_refire_on_stable_data() {
        let mut detector = ScanWelchDetector::with_config(average_only());
        let (mut storage, _) = storage(&step_data(20, 50.0, 30, 200.0));

        assert_eq!(
            detector.detect(&storage, 50).len(),
            1,
            "should detect the first changepoint"
        );

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
        let mut detector = ScanWelchDetector::with_config(average_only());
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

        let mut first = ScanWelchDetector::with_config(average_only());
        let first_result = first.detect(&storage, 40);
        let mut second = ScanWelchDetector::with_config(average_only());
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
        let mut detector = ScanWelchDetector::with_config(average_only());
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
        let mut detector = ScanWelchDetector::with_config(average_only());
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
        let mut detector = ScanWelchDetector::with_config(average_only());
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
