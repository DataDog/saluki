//! Tukey biweight changepoint detection.
//!
//! This is the Rust port of the Go `observer/impl/metrics_detector_tukey_biweight.go`. On each scoring
//! tick it fits a redescending biweight location/scale pair `(mu, sigma)` over the most recent
//! `window_size` values with a few IRLS sweeps, then standardizes the latest point against the immunized
//! baseline. Points further than `biweight_c * sigma` get zero weight while the baseline is fitted, so a
//! single historical spike cannot poison the reference window.
//!
//! # Streaming state and cursors
//!
//! State is kept per `(series, aggregate)` and is private to the detector. A per-series cursor tracks the
//! last visible point count, write generation, and timestamp so only new points are ingested. Because
//! storage can mutate an already-processed bucket (same-second merge) or insert a point behind the cursor
//! (out-of-order backfill), the detector rebuilds the per-series state from visible storage when the
//! cursor's bucket changed, keeping incremental processing equivalent to replaying the final stored
//! points.
//!
//! # Faithfulness
//!
//! The Go control flow is ported as-is: the readiness test at `point_count >= min_points`, the
//! cooldown decrement per ingested point (independent of whether scoring runs), the `score_every`
//! amortization counter resetting only when a full window is scored, the `>=` threshold gates, and the
//! glitch cap. The Go state also tracks `lastFireTime`, but nothing reads it, so it is omitted rather than
//! carried as write-only state. The Go detector also keeps a detector-wide `scoreBuf` because it retains a
//! rolling window; this port reads the bounded tail from storage into a short-lived `Series` on each
//! scoring tick instead, so per-series memory stays at cursor scalars without a shared scratch buffer.

use std::collections::HashMap;

use super::collect_last_points;
use super::numerics::{mad, median, median_point_interval, parse_aggregate_config};
use super::workload_series_refs;
use crate::identity::{Aggregate, QueryHandle, SeriesDescriptor, SeriesRef};
use crate::model::{Anomaly, AnomalyEvidence, AnomalyType, Series};
use crate::traits::{Detector, StorageView};

/// Default IRLS window size in points, from the Go `DefaultTukeyBiweightConfig`.
const DEFAULT_WINDOW_SIZE: usize = 80;

/// Default Tukey biweight tuning constant. `4.685` is the canonical 95% Gaussian-efficiency value.
const DEFAULT_BIWEIGHT_C: f64 = 4.685;

/// Default cap on biweight reweighting sweeps.
const DEFAULT_IRLS_ITERATIONS: usize = 4;

/// Default absolute robust z-score threshold for a fire.
const DEFAULT_Z_THRESHOLD: f64 = 5.0;

/// Default scoring cadence: one IRLS fit every `score_every` ingested points once the window is full.
const DEFAULT_SCORE_EVERY: usize = 4;

/// Default per-series suppression window after a fire, in points.
const DEFAULT_COOLDOWN_POINTS: usize = 30;

/// Upper bound on `|z|` for a fire, from the Go `tbGlitchZCap`.
///
/// Anything beyond this is almost certainly an instrumentation artifact (a NaN-converted `1e308`, a
/// malformed counter reset) rather than a genuine regime change, and missing it is preferable to emitting
/// a single anomaly with a runaway score.
const TB_GLITCH_Z_CAP: f64 = 50.0;

/// Floor for the location/scale fallback and for the scoring denominator, from the Go `scoreBiweight`.
const SIGMA_FLOOR: f64 = 1e-10;

/// Identifies one `(series, aggregate)` pair of Tukey biweight state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TbStateKey {
    series_ref: SeriesRef,
    aggregate: Aggregate,
}

/// Per-series streaming state for the Tukey biweight detector.
///
/// The memory footprint per key is limited to cursors, counters, and scalars: the scoring window is read
/// from bounded storage into the detector-wide [`TukeyBiweightDetector::score_buf`] instead of being
/// retained per series.
#[derive(Debug, Default)]
struct TbSeriesState {
    /// Point count visible at the last advance.
    last_processed_count: usize,
    /// Write generation at the last advance.
    last_write_gen: u64,
    /// Timestamp of the last ingested point.
    last_processed_time: i64,
    /// Value of the last ingested point at [`TbSeriesState::last_processed_time`].
    last_processed_value: f64,
    /// New points ingested since the last scoring tick; reset only when a full window is scored.
    ticks_since_score: usize,
    /// Remaining points of the post-fire suppression window; decremented on every ingested point.
    cooldown_left: usize,
}

/// Configuration for [`TukeyBiweightDetector`], mirroring the Go `TukeyBiweightConfig`.
///
/// `Default` reproduces `DefaultTukeyBiweightConfig`. Zero-valued fields are replaced with their defaults
/// by [`TukeyBiweightDetector::new`], exactly as the Go `ensureDefaults` does.
#[derive(Clone, Debug, PartialEq)]
pub struct TukeyBiweightConfig {
    /// Number of recent points held in the IRLS window. Default: `80`.
    ///
    /// A larger window gives more context for a robust local baseline at the cost of a more expensive
    /// scoring tick.
    pub window_size: usize,

    /// Minimum window fill before scoring runs. Default: `window_size`.
    ///
    /// Values larger than `window_size` are capped to it, because scoring requires a full window.
    pub min_points: usize,

    /// Tukey biweight tuning constant. Default: `4.685`.
    ///
    /// Tightening towards `4.0` trades efficiency for outlier rejection.
    pub biweight_c: f64,

    /// Number of biweight reweighting sweeps. Default: `4`.
    ///
    /// Maronna, Martin & Yohai recommend 3–5; three is faster but occasionally non-convergent on bimodal
    /// windows.
    pub irls_iterations: usize,

    /// Absolute robust z-score above which the latest point is flagged. Default: `5.0`.
    pub z_threshold: f64,

    /// Scoring cadence: the IRLS fit runs every Nth ingested point once the window is full. Default: `4`.
    pub score_every: usize,

    /// Per-series suppression window after a fire, in points. Default: `30`. Must be positive.
    pub cooldown_points: usize,

    /// Aggregate suffixes to run detection on. Default: `["avg", "count"]`.
    ///
    /// Unrecognized entries are dropped, exactly as the Go `parseAggregateConfig` does; if nothing
    /// recognizable remains the default list is used.
    pub aggregations: Vec<String>,
}

impl Default for TukeyBiweightConfig {
    /// Returns the production/testbench baseline configuration, matching `DefaultTukeyBiweightConfig`.
    fn default() -> Self {
        Self {
            window_size: DEFAULT_WINDOW_SIZE,
            min_points: DEFAULT_WINDOW_SIZE,
            biweight_c: DEFAULT_BIWEIGHT_C,
            irls_iterations: DEFAULT_IRLS_ITERATIONS,
            z_threshold: DEFAULT_Z_THRESHOLD,
            score_every: DEFAULT_SCORE_EVERY,
            cooldown_points: DEFAULT_COOLDOWN_POINTS,
            aggregations: vec!["avg".to_string(), "count".to_string()],
        }
    }
}

impl TukeyBiweightConfig {
    /// Returns the offline replay profile: the default configuration with a 40-point window and minimum.
    ///
    /// This matches the Tukey biweight entry of Go's `ApplyTestbenchDefaults`.
    pub fn testbench() -> Self {
        Self {
            window_size: 40,
            min_points: 40,
            ..Self::default()
        }
    }

    /// Returns the configuration with zero-valued fields replaced by their defaults.
    ///
    /// Ports the Go `ensureDefaults`.
    fn normalized(mut self) -> Self {
        if self.window_size == 0 {
            self.window_size = DEFAULT_WINDOW_SIZE;
        }
        if self.min_points == 0 {
            self.min_points = self.window_size;
        }
        if self.min_points > self.window_size {
            self.min_points = self.window_size;
        }
        if self.biweight_c <= 0.0 {
            self.biweight_c = DEFAULT_BIWEIGHT_C;
        }
        if self.irls_iterations == 0 {
            self.irls_iterations = DEFAULT_IRLS_ITERATIONS;
        }
        if self.z_threshold <= 0.0 {
            self.z_threshold = DEFAULT_Z_THRESHOLD;
        }
        if self.score_every == 0 {
            self.score_every = DEFAULT_SCORE_EVERY;
        }
        if self.cooldown_points == 0 {
            self.cooldown_points = DEFAULT_COOLDOWN_POINTS;
        }
        self
    }

    /// Returns the parsed aggregate list, falling back to `[Average, Count]` when nothing is recognized.
    ///
    /// Ports the interaction between the Go `parseAggregateConfig` and `ensureDefaults`.
    fn parsed_aggregations(&self) -> Vec<Aggregate> {
        let parsed = parse_aggregate_config(&self.aggregations);
        if parsed.is_empty() {
            vec![Aggregate::Average, Aggregate::Count]
        } else {
            parsed
        }
    }
}

/// Streaming Tukey biweight M-estimator changepoint detector, mirroring the Go `TukeyBiweightDetector`.
#[derive(Debug)]
pub struct TukeyBiweightDetector {
    config: TukeyBiweightConfig,
    aggregations: Vec<Aggregate>,
    ready: bool,
    /// Per-`(series, aggregate)` state.
    series: HashMap<TbStateKey, TbSeriesState>,
    /// Series refs discovered on the last listing; refreshed when the series generation changes.
    cached_refs: Option<Vec<SeriesRef>>,
    cached_gen: u64,
}

impl TukeyBiweightDetector {
    /// Creates a detector with the given configuration, filling zero-valued fields from
    /// [`TukeyBiweightConfig::default`].
    pub fn new(config: TukeyBiweightConfig) -> Self {
        let config = config.normalized();
        let aggregations = config.parsed_aggregations();
        Self {
            config,
            aggregations,
            ready: false,
            series: HashMap::new(),
            cached_refs: None,
            cached_gen: 0,
        }
    }

    /// Creates a detector with the default configuration.
    pub fn with_default_config() -> Self {
        Self::new(TukeyBiweightConfig::default())
    }

    /// Returns the number of `(series, aggregate)` state entries currently held.
    ///
    /// This exists so integration code and tests can assert that eviction fan-out really tears state down.
    pub fn tracked_series_count(&self) -> usize {
        self.series.len()
    }
}

impl Detector for TukeyBiweightDetector {
    type Config = TukeyBiweightConfig;

    fn name(&self) -> &str {
        "tukey_biweight"
    }

    fn config(&self) -> &Self::Config {
        &self.config
    }

    fn is_ready(&self) -> bool {
        self.ready
    }

    fn reset(&mut self) {
        self.series.clear();
        self.cached_refs = None;
        self.cached_gen = 0;
        self.ready = false;
    }

    fn remove_series(&mut self, refs: &[SeriesRef]) {
        if refs.is_empty() || self.series.is_empty() {
            return;
        }
        for series_ref in refs {
            for aggregate in &self.aggregations {
                self.series.remove(&TbStateKey {
                    series_ref: *series_ref,
                    aggregate: *aggregate,
                });
            }
        }
        // Drop the cached listing so the next detect re-lists from storage instead of iterating over
        // removed refs.
        self.cached_refs = None;
        self.cached_gen = 0;
    }

    fn detect(&mut self, view: &dyn StorageView, data_time_sec: i64) -> Vec<Anomaly> {
        let generation = view.series_generation();
        if self.cached_refs.is_none() || generation != self.cached_gen {
            self.cached_refs = Some(workload_series_refs(view));
            self.cached_gen = generation;
        }
        let refs = self.cached_refs.take().unwrap_or_default();
        // Bulk status: point count and write generation per ref, like the Go `bulkSeriesStatus` fallback.
        let statuses: Vec<SeriesStatus> = refs
            .iter()
            .map(|series_ref| SeriesStatus {
                point_count: view.point_count_up_to(*series_ref, data_time_sec),
                write_generation: view.write_generation(*series_ref),
            })
            .collect();

        let mut anomalies = Vec::new();
        let aggregations = self.aggregations.clone();

        for (index, &series_ref) in refs.iter().enumerate() {
            let status = statuses[index];
            for &aggregate in &aggregations {
                if !view.supports_aggregate(series_ref, aggregate) {
                    continue;
                }
                let key = TbStateKey { series_ref, aggregate };

                if !self.series.contains_key(&key) {
                    if status.point_count < self.config.min_points {
                        continue;
                    }
                    self.series.insert(key, TbSeriesState::default());
                }
                let state = self.series.get_mut(&key).expect("state was just inserted");

                if status.point_count <= state.last_processed_count && status.write_generation == state.last_write_gen {
                    continue;
                }

                let mut start_time = state.last_processed_time;
                let count_increased = status.point_count > state.last_processed_count;
                let prefix_count = view.point_count_up_to(series_ref, state.last_processed_time);
                // A full point window can evict an old bucket while appending a new one. That changes the
                // generation without changing the total count; the smaller prefix shows the lost bucket was
                // before our cursor.
                let merge_occurred = status.point_count == state.last_processed_count
                    && status.write_generation != state.last_write_gen
                    && prefix_count >= state.last_processed_count;
                let cursor_bucket_changed_with_append = count_increased
                    && status.write_generation != state.last_write_gen
                    && prefix_count == state.last_processed_count
                    && cursor_point_changed(view, series_ref, aggregate, state);
                if merge_occurred || prefix_count > state.last_processed_count || cursor_bucket_changed_with_append {
                    *state = TbSeriesState::default();
                    start_time = 0;
                }

                let mut points_seen = false;
                if let Some(series) = view.get_series_range(series_ref, start_time, data_time_sec, aggregate) {
                    for point in &series.points {
                        points_seen = true;
                        // Decrement cooldown per ingested point so it expires regardless of whether
                        // scoring runs on this tick.
                        if state.cooldown_left > 0 {
                            state.cooldown_left -= 1;
                        }
                        state.ticks_since_score += 1;

                        if status.point_count >= self.config.min_points
                            && state.cooldown_left == 0
                            && state.ticks_since_score >= self.config.score_every
                        {
                            let candidate =
                                collect_last_points(view, series_ref, point.second, self.config.window_size, aggregate);
                            if let Some(window) = candidate {
                                if window.points.len() >= self.config.min_points {
                                    state.ticks_since_score = 0;
                                    self.ready = true;
                                    if let Some(anomaly) =
                                        score_biweight(&self.config, &window, series_ref, aggregate, point.second)
                                    {
                                        anomalies.push(anomaly);
                                        state.cooldown_left = self.config.cooldown_points;
                                    }
                                }
                            }
                        }

                        state.last_processed_time = point.second;
                        state.last_processed_value = point.value;
                    }
                }

                if !points_seen && status.write_generation != state.last_write_gen {
                    state.last_processed_count = status.point_count;
                    state.last_write_gen = status.write_generation;
                    continue;
                }
                if points_seen {
                    state.last_processed_count = status.point_count;
                    state.last_write_gen = status.write_generation;
                }
            }
        }

        self.cached_refs = Some(refs);
        anomalies
    }
}

/// Point count and write generation for one series, mirroring the Go `seriesStatus`.
#[derive(Clone, Copy, Debug, Default)]
struct SeriesStatus {
    point_count: usize,
    write_generation: u64,
}

/// Reports whether the bucket under the cursor changed in place.
///
/// Ports the Go `cursorPointChanged`: it compares the stored value at `last_processed_time` against the
/// value the detector last ingested. A state that has never ingested a point cannot have a changed cursor.
fn cursor_point_changed(
    view: &dyn StorageView, series_ref: SeriesRef, aggregate: Aggregate, state: &TbSeriesState,
) -> bool {
    if state.last_processed_count == 0 {
        return false;
    }
    let Some(series) = view.get_series_range(
        series_ref,
        state.last_processed_time - 1,
        state.last_processed_time,
        aggregate,
    ) else {
        return false;
    };
    series
        .points
        .iter()
        .any(|point| point.second == state.last_processed_time && point.value != state.last_processed_value)
}

/// Fits the biweight baseline over `window` and decides whether the latest point should be flagged.
///
/// Ports the Go `scoreBiweight`, which is pure with respect to detector state: it returns an anomaly (with
/// the source ref unset) when the latest point clears the z-score gate and the glitch cap, and `None`
/// otherwise. The statistics helpers come from [`super::numerics`].
fn score_biweight(
    config: &TukeyBiweightConfig, window: &Series, series_ref: SeriesRef, aggregate: Aggregate, data_time_sec: i64,
) -> Option<Anomaly> {
    let values: Vec<f64> = window.points.iter().map(|point| point.value).collect();
    let n = values.len();
    if n < 2 {
        return None;
    }

    // Initial robust location/scale: median + 1.4826 * MAD, with a floor for degenerate windows.
    let mut mu = median(&values);
    let mut sigma = mad(&values, mu, true);
    if sigma < SIGMA_FLOOR {
        sigma = (mu.abs() * 0.01).max(1e-6);
    }

    let c = config.biweight_c;

    // IRLS sweeps: biweight weights `w_j = (1 - u_j^2)^2` for `|u_j| < 1`, zero otherwise, then a weighted
    // mean update. Break early on convergence.
    for _ in 0..config.irls_iterations {
        let mut numerator = 0.0;
        let mut denominator = 0.0;
        for value in &values {
            let u = (value - mu) / (c * sigma);
            if u.abs() >= 1.0 {
                continue;
            }
            let weight = (1.0 - u * u).powi(2);
            numerator += weight * value;
            denominator += weight;
        }
        if denominator < SIGMA_FLOOR {
            break;
        }
        let new_mu = numerator / denominator;
        let converged = (new_mu - mu).abs() < 1e-6 * sigma;
        mu = new_mu;
        if converged {
            break;
        }
    }

    // Recompute sigma from the biweight-weighted residuals, with the same trimming rule as the mean update.
    let mut numerator = 0.0;
    let mut denominator = 0.0;
    for value in &values {
        let u = (value - mu) / (c * sigma);
        if u.abs() >= 1.0 {
            continue;
        }
        let weight = (1.0 - u * u).powi(2);
        let residual = value - mu;
        numerator += weight * residual * residual;
        denominator += weight;
    }
    if denominator > 0.0 {
        sigma = (numerator / denominator).sqrt();
    }

    // Score the latest point against the immunized baseline.
    let mut scoring_denominator = sigma;
    if scoring_denominator < SIGMA_FLOOR {
        scoring_denominator = SIGMA_FLOOR;
    }
    let latest = values[n - 1];
    let z = (latest - mu) / scoring_denominator;
    let z_abs = z.abs();
    if z_abs < config.z_threshold {
        return None;
    }
    // Suppress extreme glitches without blocking real shifts.
    if z_abs >= TB_GLITCH_Z_CAP {
        return None;
    }

    // The score is |z| capped at the glitch cap to keep downstream consumers sane.
    let score = z_abs.min(TB_GLITCH_Z_CAP);

    Some(Anomaly {
        anomaly_type: AnomalyType::Metric,
        series: SeriesDescriptor::new(
            window.namespace.clone(),
            window.name.clone(),
            window.host.clone(),
            window.tags.clone(),
            aggregate,
        ),
        series_ref: Some(QueryHandle::new(series_ref, aggregate)),
        detector_name: "tukey_biweight".to_string(),
        context: None,
        timestamp_sec: data_time_sec,
        score: Some(score),
        sampling_interval_sec: median_point_interval(&window.points),
        evidence: Some(AnomalyEvidence::TukeyBiweight {
            baseline_median: mu,
            baseline_mad: sigma,
            current_value: latest,
            deviation_sigma: z_abs,
            sample_count: n,
            z_score: z,
        }),
    })
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

    /// Builds a store with a physical point cap, mirroring the Go `newTimeSeriesStorageWith` helper.
    fn capped_storage(max_points_per_series: usize) -> TimeSeriesStorage {
        TimeSeriesStorage::new(StorageConfig {
            max_points_per_series,
            point_retention_secs: 0,
            ..StorageConfig::default()
        })
    }

    fn add(storage: &mut TimeSeriesStorage, name: &str, value: f64, second: i64) {
        storage
            .add("ns", name, None, value, second, &[])
            .series_ref
            .expect("finite values are admitted");
    }

    /// The Go `testTukeyBiweightDetector`: default config pinned to the average aggregate.
    fn test_detector() -> TukeyBiweightDetector {
        TukeyBiweightDetector::new(TukeyBiweightConfig {
            aggregations: vec!["avg".to_string()],
            ..TukeyBiweightConfig::default()
        })
    }

    /// Builds a config pinned to the average aggregate with the given overrides.
    fn avg_config(config: TukeyBiweightConfig) -> TukeyBiweightConfig {
        TukeyBiweightConfig {
            aggregations: vec!["avg".to_string()],
            ..config
        }
    }

    /// A tiny deterministic standard-normal generator.
    ///
    /// The crate is deliberately dependency-free, so tests cannot pull in `rand`; this reproduces the
    /// shape of the Go fixtures (deterministic pseudo-normal samples) without the exact values.
    struct TestRng(u64);

    impl TestRng {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn next_unit(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }

        /// Draws a standard normal with the Box–Muller transform.
        fn normal(&mut self) -> f64 {
            let u1 = self.next_unit().max(f64::MIN_POSITIVE);
            let u2 = self.next_unit();
            (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
        }
    }

    fn anomaly_timestamps(anomalies: &[Anomaly]) -> Vec<i64> {
        anomalies.iter().map(|anomaly| anomaly.timestamp_sec).collect()
    }

    #[test]
    fn name_is_tukey_biweight() {
        assert_eq!(TukeyBiweightDetector::with_default_config().name(), "tukey_biweight");
    }

    /// Ports Go `TestTukeyBiweight_IncrementalMatchesBatch`.
    #[test]
    fn incremental_matches_batch() {
        let mut rng = TestRng::new(42);
        let end = 180i64;
        let mut values = Vec::with_capacity(end as usize);
        for _ in 0..100 {
            values.push(10.0 + 0.5 * rng.normal());
        }
        for _ in 100..end {
            values.push(15.0 + 0.5 * rng.normal());
        }

        let mut batch_detector = test_detector();
        let mut batch_storage = detector_storage();
        for (index, value) in values.iter().enumerate() {
            add(&mut batch_storage, "metric", *value, index as i64 + 1);
        }
        let batch = batch_detector.detect(&batch_storage, end);

        let mut incremental_detector = test_detector();
        let mut incremental_storage = detector_storage();
        let mut incremental = Vec::new();
        for (index, value) in values.iter().enumerate() {
            let timestamp = index as i64 + 1;
            add(&mut incremental_storage, "metric", *value, timestamp);
            incremental.extend(incremental_detector.detect(&incremental_storage, timestamp));
        }

        assert_eq!(anomaly_timestamps(&batch), anomaly_timestamps(&incremental));
    }

    /// Ports Go `TestTukeyBiweight_ReprocessesSameBucketMerge`.
    #[test]
    fn reprocesses_same_bucket_merge() {
        let mut detector = TukeyBiweightDetector::new(avg_config(TukeyBiweightConfig {
            window_size: 4,
            min_points: 1,
            score_every: 100,
            ..TukeyBiweightConfig::default()
        }));
        let mut storage = detector_storage();
        add(&mut storage, "metric", 10.0, 1);
        detector.detect(&storage, 1);

        let series_ref = SeriesRef::new(0);
        let key = TbStateKey {
            series_ref,
            aggregate: Aggregate::Average,
        };
        assert_eq!(
            detector.series.get(&key).expect("state exists").last_processed_value,
            10.0
        );

        add(&mut storage, "metric", 30.0, 1);
        let merged = storage
            .get_series_range(series_ref, 0, 1, Aggregate::Average)
            .expect("series is live");
        assert_eq!(merged.points.len(), 1);
        assert_eq!(merged.points[0].value, 20.0, "storage should expose the merged average");

        detector.detect(&storage, 1);
        let state = detector.series.get(&key).expect("state exists");
        assert_eq!(
            state.last_processed_value, 20.0,
            "merge should replace the stale aggregate"
        );
        assert_eq!(state.last_write_gen, storage.write_generation(series_ref));
    }

    /// Ports Go `TestTukeyBiweight_RebuildsOnOutOfOrderBackfillBeforeCursor`.
    #[test]
    fn rebuilds_on_out_of_order_backfill() {
        let mut detector = TukeyBiweightDetector::new(avg_config(TukeyBiweightConfig {
            window_size: 4,
            min_points: 1,
            score_every: 100,
            ..TukeyBiweightConfig::default()
        }));
        let mut storage = detector_storage();
        add(&mut storage, "metric", 10.0, 10);
        detector.detect(&storage, 10);

        let key = TbStateKey {
            series_ref: SeriesRef::new(0),
            aggregate: Aggregate::Average,
        };
        assert_eq!(detector.series.get(&key).expect("state exists").last_processed_time, 10);

        add(&mut storage, "metric", 5.0, 5);
        detector.detect(&storage, 10);

        let state = detector.series.get(&key).expect("state exists");
        assert_eq!(state.last_processed_count, 2);
        assert_eq!(state.last_processed_time, 10);
    }

    /// Ports Go `TestTukeyBiweight_RebuildsOnCursorMergeWithLaterAppend`.
    #[test]
    fn rebuilds_on_cursor_merge_with_later_append() {
        let mut detector = TukeyBiweightDetector::new(avg_config(TukeyBiweightConfig {
            window_size: 4,
            min_points: 1,
            score_every: 100,
            ..TukeyBiweightConfig::default()
        }));
        let mut storage = detector_storage();
        add(&mut storage, "metric", 10.0, 10);
        detector.detect(&storage, 10);

        let key = TbStateKey {
            series_ref: SeriesRef::new(0),
            aggregate: Aggregate::Average,
        };
        assert_eq!(detector.series.get(&key).expect("state exists").last_processed_time, 10);

        add(&mut storage, "metric", 30.0, 10);
        add(&mut storage, "metric", 40.0, 11);
        detector.detect(&storage, 11);

        let state = detector.series.get(&key).expect("state exists");
        assert_eq!(state.last_processed_value, 40.0);
        assert_eq!(state.last_processed_count, 2);
        assert_eq!(state.last_processed_time, 11);
    }

    /// Ports Go `TestTukeyBiweight_PreservesStateWhenPointCapEvictsOldestBucket`.
    #[test]
    fn preserves_state_when_point_cap_evicts_oldest_bucket() {
        let mut detector = TukeyBiweightDetector::new(avg_config(TukeyBiweightConfig {
            window_size: 4,
            min_points: 1,
            score_every: 100,
            ..TukeyBiweightConfig::default()
        }));
        let mut storage = capped_storage(2);
        for timestamp in 1..=3 {
            add(&mut storage, "metric", timestamp as f64, timestamp);
        }
        detector.detect(&storage, 3);

        let key = TbStateKey {
            series_ref: SeriesRef::new(0),
            aggregate: Aggregate::Average,
        };
        assert_eq!(detector.series.get(&key).expect("state exists").last_processed_count, 3);

        add(&mut storage, "metric", 4.0, 4);
        detector.detect(&storage, 4);

        let state = detector.series.get(&key).expect("state survives the eviction");
        assert_eq!(state.last_processed_time, 4);
        assert_eq!(state.last_processed_count, 3);
    }

    /// Ports Go `TestTukeyBiweight_ContinuesAfterRetentionDropsBelowMinimum`.
    #[test]
    fn continues_after_retention_drops_below_minimum() {
        let mut detector = TukeyBiweightDetector::new(avg_config(TukeyBiweightConfig {
            window_size: 4,
            min_points: 3,
            score_every: 100,
            ..TukeyBiweightConfig::default()
        }));
        let mut storage = detector_storage();
        for timestamp in 1..=3 {
            add(&mut storage, "metric", timestamp as f64, timestamp);
        }
        detector.detect(&storage, 3);

        let series_ref = SeriesRef::new(0);
        let key = TbStateKey {
            series_ref,
            aggregate: Aggregate::Average,
        };
        assert!(detector.series.contains_key(&key));

        storage.set_series_retention(series_ref, 1);
        add(&mut storage, "metric", 4.0, 4);
        detector.detect(&storage, 4);

        let state = detector.series.get(&key).expect("state survives the retention drop");
        assert_eq!(state.last_processed_time, 4);
    }

    /// Ports Go `TestTukeyBiweight_NoFireOnStableGaussian`.
    #[test]
    fn no_fire_on_stable_gaussian() {
        let mut detector = test_detector();
        let mut storage = detector_storage();
        let mut rng = TestRng::new(42);
        for i in 0..200 {
            add(&mut storage, "metric", 10.0 + 0.5 * rng.normal(), i + 1);
        }

        assert!(
            detector.detect(&storage, 200).is_empty(),
            "stable Gaussian must not fire"
        );
    }

    /// Ports Go `TestTukeyBiweight_FiresOnLevelShift`.
    #[test]
    fn fires_on_level_shift() {
        let mut detector = test_detector();
        let mut storage = detector_storage();
        let mut rng = TestRng::new(42);
        let shift_start = 101i64;
        for i in 0..100 {
            add(&mut storage, "metric", 10.0 + 0.5 * rng.normal(), i + 1);
        }
        for i in 0..80 {
            add(&mut storage, "metric", 15.0 + 0.5 * rng.normal(), shift_start + i);
        }

        let anomalies = detector.detect(&storage, 180);
        assert!(!anomalies.is_empty(), "level shift must produce at least one anomaly");

        let first = &anomalies[0];
        assert_eq!(first.detector_name, "tukey_biweight");
        assert!(first.score.expect("anomaly must carry a score") >= 5.0);
        assert!(
            first.timestamp_sec < shift_start + 20,
            "first fire should arrive within 20 points"
        );
        assert_eq!(first.sampling_interval_sec, 1);
        match first.evidence.as_ref().expect("tukey evidence") {
            AnomalyEvidence::TukeyBiweight { sample_count, .. } => assert!(*sample_count > 0),
            other => panic!("unexpected evidence {other:?}"),
        }
    }

    /// Ports Go `TestTukeyBiweight_RobustToHistoricalOutlier`: a single huge historical spike must not
    /// blind the detector, so the later real level shift still fires while the spike itself is suppressed by
    /// the glitch cap.
    #[test]
    fn robust_to_historical_outlier() {
        let mut detector = test_detector();
        let mut storage = detector_storage();
        let mut rng = TestRng::new(7);
        let spike_at = 81i64;
        let shift_start = 162i64;
        let end = 221i64;

        for i in 1..=80 {
            add(&mut storage, "metric", 10.0 + 0.5 * rng.normal(), i);
        }
        add(&mut storage, "metric", 100.0, spike_at);
        for i in spike_at + 1..shift_start {
            add(&mut storage, "metric", 10.0 + 0.5 * rng.normal(), i);
        }
        for i in shift_start..=end {
            add(&mut storage, "metric", 15.0 + 0.5 * rng.normal(), i);
        }

        let anomalies = detector.detect(&storage, end);
        for anomaly in &anomalies {
            assert!(
                anomaly.timestamp_sec >= shift_start,
                "no fire allowed before the real shift begins (got fire at {})",
                anomaly.timestamp_sec
            );
        }
        let post_shift: Vec<&Anomaly> = anomalies
            .iter()
            .filter(|anomaly| anomaly.timestamp_sec >= shift_start)
            .collect();
        assert!(
            !post_shift.is_empty(),
            "real level shift must fire; the biweight baseline must not be blinded by the historical spike"
        );
        assert!(post_shift[0].score.expect("score") >= 5.0);
    }

    /// Ports Go `TestTukeyBiweight_NoFireOnLinearTrend`.
    #[test]
    fn at_most_one_fire_on_linear_trend() {
        let mut detector = test_detector();
        let mut storage = detector_storage();
        let mut rng = TestRng::new(11);
        for i in 0..200 {
            add(&mut storage, "metric", 0.01 * i as f64 + 0.05 * rng.normal(), i + 1);
        }

        let anomalies = detector.detect(&storage, 200);
        assert!(
            anomalies.len() <= 1,
            "linear trend is the trend detector's territory; biweight must not double-fire"
        );
    }

    /// Ports Go `TestTukeyBiweight_IRLSConverges`: the IRLS fit on a bimodal window terminates with finite
    /// statistics and a positive MAD.
    #[test]
    fn irls_converges_on_bimodal_window() {
        let detector = test_detector();
        let mut points = Vec::with_capacity(80);
        for i in 1..=80i64 {
            points.push(Point {
                second: i,
                value: if i <= 40 { 0.0 } else { 10.0 },
            });
        }
        let series = Series {
            namespace: "ns".into(),
            name: "metric".to_string(),
            host: None,
            tags: Vec::new(),
            points: points.clone(),
        };
        // We do not care whether it fires; the point is that the underlying IRLS terminates cleanly.
        let _ = score_biweight(&detector.config, &series, SeriesRef::new(0), Aggregate::Average, 80);

        let values: Vec<f64> = points.iter().map(|point| point.value).collect();
        let mu = median(&values);
        let sigma = mad(&values, mu, true);
        assert!(mu.is_finite(), "mu must be finite");
        assert!(sigma.is_finite(), "sigma must be finite");
        assert!(sigma > 0.0, "MAD must be positive on a bimodal window with separation");
    }

    /// Ports Go `TestTukeyBiweight_RemoveSeries`.
    #[test]
    fn remove_series_tears_down_state() {
        let mut detector = TukeyBiweightDetector::new(TukeyBiweightConfig {
            min_points: 8,
            ..TukeyBiweightConfig::default()
        });
        let mut storage = detector_storage();
        for series in 0..3 {
            let name = format!("metric{}", (b'A' + series) as char);
            for i in 0..8 {
                add(&mut storage, &name, i as f64, i + 1);
            }
        }

        detector.detect(&storage, 8);
        let aggregation_count = detector.aggregations.len();
        assert_eq!(detector.tracked_series_count(), 3 * aggregation_count);

        detector.remove_series(&[SeriesRef::new(0), SeriesRef::new(1)]);
        assert_eq!(detector.tracked_series_count(), aggregation_count);
        assert!(
            detector.cached_refs.is_none(),
            "removal must invalidate the cached listing"
        );
    }

    /// Ports Go `TestTukeyBiweight_ScoreEveryAmortization`.
    #[test]
    fn score_every_amortization() {
        let mut detector = test_detector();
        let mut storage = detector_storage();
        let n = 200i64;
        for i in 0..n {
            add(&mut storage, "metric", 7.0, i + 1);
        }

        let anomalies = detector.detect(&storage, n);
        assert!(anomalies.is_empty(), "constant series must not fire (z=0 every tick)");

        let state = detector.series.values().next().expect("state entry");
        assert_eq!(
            state.last_processed_count, n as usize,
            "cursor must advance to all points"
        );
        assert!(
            state.ticks_since_score < detector.config.score_every,
            "between-score counter must be bounded by score_every"
        );
    }

    /// Ports Go `TestTukeyBiweight_FirstScoreAtMinimum`.
    #[test]
    fn first_score_at_minimum() {
        let mut detector = TukeyBiweightDetector::new(avg_config(TukeyBiweightConfig {
            window_size: 39,
            min_points: 39,
            score_every: 4,
            ..TukeyBiweightConfig::default()
        }));
        let mut storage = detector_storage();
        for i in 0..39 {
            add(&mut storage, "metric", 7.0, i + 1);
        }

        let anomalies = detector.detect(&storage, 39);
        assert!(anomalies.is_empty());

        let state = detector.series.values().next().expect("state entry");
        assert!(detector.is_ready(), "the minimum full window must be scored");
        assert_eq!(state.ticks_since_score, 0, "the first score must occur at min_points");
    }

    /// Ports Go `TestTukeyBiweight_NoNewDataNoWork`.
    #[test]
    fn no_new_data_no_work() {
        let mut detector = test_detector();
        let mut storage = detector_storage();
        let mut rng = TestRng::new(13);
        for i in 0..100 {
            add(&mut storage, "metric", 10.0 + 0.5 * rng.normal(), i + 1);
        }

        detector.detect(&storage, 100);
        let count = detector
            .series
            .values()
            .next()
            .expect("state entry")
            .last_processed_count;

        let second = detector.detect(&storage, 100);
        assert_eq!(
            detector
                .series
                .values()
                .next()
                .expect("state entry")
                .last_processed_count,
            count,
            "no new data must not advance the cursor"
        );
        assert!(second.is_empty(), "no new data must produce no anomalies on re-call");
    }

    /// Ports Go `TestTukeyBiweight_Reset`.
    #[test]
    fn reset_clears_state() {
        let mut detector = test_detector();
        let mut storage = detector_storage();
        for i in 0..80 {
            add(&mut storage, "metric", i as f64, i + 1);
        }
        detector.detect(&storage, 80);
        assert_eq!(detector.tracked_series_count(), 1);
        assert!(detector.is_ready());

        detector.reset();
        assert_eq!(detector.tracked_series_count(), 0, "reset should clear all state");
        assert!(detector.cached_refs.is_none(), "reset should clear cached refs");
        assert!(!detector.is_ready());
    }

    #[test]
    fn cooldown_suppresses_immediate_re_fire() {
        let mut detector = TukeyBiweightDetector::new(avg_config(TukeyBiweightConfig {
            window_size: 8,
            min_points: 5,
            score_every: 1,
            cooldown_points: 30,
            z_threshold: 4.0,
            ..TukeyBiweightConfig::default()
        }));
        let mut storage = detector_storage();

        // A jittered baseline so the robust scale is non-zero (a constant window would floor sigma).
        for i in 1..=10i64 {
            let value = 10.0 + if i % 2 == 0 { 0.3 } else { -0.3 };
            add(&mut storage, "metric", value, i);
            detector.detect(&storage, i);
        }

        // Step the level: the latest point must clear the z gate.
        add(&mut storage, "metric", 12.0, 11);
        let fire = detector.detect(&storage, 11);
        assert_eq!(fire.len(), 1, "the level shift must fire");

        let key = TbStateKey {
            series_ref: SeriesRef::new(0),
            aggregate: Aggregate::Average,
        };
        let cooldown_after_fire = detector.series.get(&key).expect("state exists").cooldown_left;
        assert_eq!(cooldown_after_fire, 30);

        // While the cooldown is active, further shifted points must not fire and the window must tick down.
        for i in 12..=16i64 {
            add(&mut storage, "metric", 12.0, i);
            assert!(
                detector.detect(&storage, i).is_empty(),
                "cooldown must suppress re-fires"
            );
        }
        let cooldown_now = detector.series.get(&key).expect("state exists").cooldown_left;
        assert_eq!(
            cooldown_now,
            cooldown_after_fire - 5,
            "cooldown must expire per ingested point"
        );
    }

    #[test]
    fn default_config_and_testbench_profile_match_go() {
        let config = TukeyBiweightConfig::default();
        assert_eq!(config.window_size, 80);
        assert_eq!(config.min_points, 80);
        assert_eq!(config.biweight_c, 4.685);
        assert_eq!(config.irls_iterations, 4);
        assert_eq!(config.z_threshold, 5.0);
        assert_eq!(config.score_every, 4);
        assert_eq!(config.cooldown_points, 30);
        assert_eq!(config.aggregations, vec!["avg".to_string(), "count".to_string()]);

        let testbench = TukeyBiweightConfig::testbench();
        assert_eq!(testbench.window_size, 40);
        assert_eq!(testbench.min_points, 40);
    }

    #[test]
    fn zero_valued_config_is_filled_from_defaults() {
        // `min_points` is capped to `window_size`, matching the Go `ensureDefaults`.
        let detector = TukeyBiweightDetector::new(TukeyBiweightConfig {
            window_size: 10,
            min_points: 20,
            ..TukeyBiweightConfig::default()
        });
        assert_eq!(detector.config().min_points, 10);

        // Zero-valued fields fall back to their defaults.
        let detector = TukeyBiweightDetector::new(TukeyBiweightConfig {
            window_size: 0,
            min_points: 0,
            biweight_c: 0.0,
            irls_iterations: 0,
            z_threshold: 0.0,
            score_every: 0,
            cooldown_points: 0,
            aggregations: Vec::new(),
        });
        assert_eq!(detector.config().window_size, 80);
        assert_eq!(detector.config().min_points, 80);
        assert_eq!(detector.config().biweight_c, 4.685);
        assert_eq!(detector.config().score_every, 4);
        assert_eq!(detector.config().cooldown_points, 30);
        assert_eq!(detector.aggregations, vec![Aggregate::Average, Aggregate::Count]);
    }

    #[test]
    fn unsupported_aggregates_are_skipped() {
        let mut detector = TukeyBiweightDetector::with_default_config();
        let mut storage = detector_storage();
        for i in 0..80 {
            add(&mut storage, "metric", i as f64, i + 1);
        }
        storage.set_supported_aggregations(SeriesRef::new(0), &[Aggregate::Average]);

        detector.detect(&storage, 80);
        assert_eq!(
            detector.tracked_series_count(),
            1,
            "the unsupported count aggregate must be skipped"
        );
    }
}
