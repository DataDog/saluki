//! Holt's-method forecast-residual detector, ported from the Agent's
//! `observer/impl/metrics_detector_holt_residual.go`.
//!
//! Per `(series, aggregate)`, the detector maintains Holt's double exponential smoothing state (a level
//! and a trend) plus a rolling window of one-step forecast residuals. Each new point is forecast as
//! `L_{t-1} + T_{t-1}`; an anomaly fires when the MAD-standardised residual `|z_t|` crosses the threshold
//! **and** the same sign repeats for `confirm_m` consecutive points **and** the raw deviation
//! `|x_t - L_{t-1}| / MAD(values)` clears the effect-size gate. A short refractory period suppresses
//! repeat fires while the smoother adapts to the new regime.
//!
//! This complements level-shift scans and the Bayesian change-point detector: the residual gate operates
//! on a forecast that adapts to drifting/trending baselines, so a slow ramp punctuated by a jump produces
//! a small forecast residual on the ramp itself and a large one on the jump.
//!
//! # Lifecycle
//!
//! Each `(series, aggregate)` moves through warmup → smoothing. During warmup, scalar half-window sums
//! accumulate until `warmup_points` points have been seen; at that boundary the level and trend are seeded
//! from the two half-window means and smoothing begins. From then on the smoother updates on every
//! ingested point — even when the gate does not fire — so the model tracks new regimes through and after a
//! fire.
//!
//! # Memory and cost
//!
//! Per `(series, aggregate)`: scalar warmup accumulators, a residual MAD window, a raw-value MAD window,
//! and a small timestamp ring — roughly 1.1 KB. Per point: `O(1)` smoother update plus two `O(W log W)`
//! median/MAD recomputations (`W = residual_window`), dominated by the two sorts inside the MAD helper.
//!
//! # Deliberate omissions
//!
//! * The Go detector asserts [`crate::traits::Detector`] plus a `SeriesRemover` interface. The Rust
//!   [`Detector`] trait folds removal into [`Detector::remove_series`], so there is no separate trait.
//! * `DetectorPointWindowRequirement` is a Go interface with a single method. The Rust engine resolves
//!   storage bounds from a plain tuple, so this is exposed as the inherent [`HoltResidualDetector::point_window`]
//!   instead of a trait.
//! * Go's optional storage fast paths (`SupportsAggregate`, ref-only listing via `ListSeriesRefsInto`,
//!   bulk status) are not part of [`StorageView`]. The Rust port therefore takes the documented fallbacks:
//!   every aggregate is treated as supported, and workload refs are the store listing filtered
//!   client-side to exclude the telemetry namespace (the Go `WorkloadSeriesFilter`).
//! * The Go `FormatAnomaly` title/description presentation is not ported; anomaly rendering belongs to a
//!   separate layer, and the detector only populates the structured [`Anomaly`].

use std::collections::HashMap;

use crate::detectors::numerics::{median_timestamp_interval, ScanDetectorWorkspace};
use crate::identity::{Aggregate, QueryHandle, SeriesDescriptor, SeriesRef};
use crate::model::{Anomaly, AnomalyEvidence, AnomalyType, Point, Series};
use crate::storage::TELEMETRY_NAMESPACE;
use crate::traits::{Detector, StorageView};

/// Level smoothing factor `alpha`: higher means more reactive to new values.
pub const DEFAULT_ALPHA: f64 = 0.2;
/// Trend smoothing factor `beta`: lower means a more stable trend estimate.
pub const DEFAULT_BETA: f64 = 0.05;
/// Number of points collected before Holt smoothing begins.
pub const DEFAULT_WARMUP_POINTS: usize = 24;
/// Rolling FIFO size for the residual MAD and raw-value MAD windows.
pub const DEFAULT_RESIDUAL_WINDOW: usize = 60;
/// Standardised-residual gate on `|z|`.
pub const DEFAULT_Z_THRESHOLD: f64 = 4.5;
/// Minimum number of consecutive same-sign `|z| >= z_threshold` observations required to fire.
pub const DEFAULT_CONFIRM_M: usize = 2;
/// Minimum `|x_t - L_{t-1}| / MAD(values)` for the effect-size gate to pass.
pub const DEFAULT_MIN_DEVIATION_MAD: f64 = 3.0;
/// Number of ingested points for which fires are suppressed after a fire.
pub const DEFAULT_REFRACTORY: usize = 20;
/// Size of the recent-timestamp ring used to estimate the sampling interval.
const TIMESTAMP_RING: usize = 16;
/// Number of recent non-fire residuals whose median replaces a fire's residual in the residual window.
const POST_FIRE_SAMPLE_N: usize = 5;

/// Tunable configuration for [`HoltResidualDetector`].
///
/// This is the Rust port of the Go `HoltResidualConfig`. [`HoltResidualConfig::default`] reproduces
/// `DefaultHoltResidualConfig`.
///
/// The detector treats a zero (or, for the float fields, non-positive) value as "unset" and substitutes
/// the matching default on the first [`Detector::detect`] call, mirroring the Go `ensureDefaults`. An empty
/// `aggregations` list therefore resolves to `[Average, Count]`.
#[derive(Clone, Debug, PartialEq)]
pub struct HoltResidualConfig {
    /// Level smoothing factor. Default: `0.2`. Zero or negative resolves to the default.
    pub alpha: f64,
    /// Trend smoothing factor. Default: `0.05`. Zero or negative resolves to the default.
    pub beta: f64,
    /// Points collected before smoothing begins; split into halves to seed level and trend. Default: `24`.
    /// Must be at least `1`; `0` resolves to the default.
    pub warmup_points: usize,
    /// Rolling FIFO size for the residual MAD and raw-value MAD windows. Default: `60`. `0` resolves to
    /// the default.
    pub residual_window: usize,
    /// Standardised-residual gate. Default: `4.5`. Zero or negative resolves to the default.
    pub z_threshold: f64,
    /// Consecutive same-sign confirmations required to fire. Default: `2`. `0` resolves to the default.
    pub confirm_m: usize,
    /// Minimum effect size in MADs. Default: `3.0`. Zero or negative resolves to the default.
    pub min_deviation_mad: f64,
    /// Points for which fires are suppressed after a fire. Default: `20`. `0` resolves to the default.
    pub refractory: usize,
    /// Aggregates to run detection on. Default: `[Average, Count]`. Empty resolves to the default.
    pub aggregations: Vec<Aggregate>,
}

impl Default for HoltResidualConfig {
    /// Returns the production defaults.
    fn default() -> Self {
        Self {
            alpha: DEFAULT_ALPHA,
            beta: DEFAULT_BETA,
            warmup_points: DEFAULT_WARMUP_POINTS,
            residual_window: DEFAULT_RESIDUAL_WINDOW,
            z_threshold: DEFAULT_Z_THRESHOLD,
            confirm_m: DEFAULT_CONFIRM_M,
            min_deviation_mad: DEFAULT_MIN_DEVIATION_MAD,
            refractory: DEFAULT_REFRACTORY,
            aggregations: vec![Aggregate::Average, Aggregate::Count],
        }
    }
}

/// Streaming state for one `(series, aggregate)` pair.
#[derive(Clone, Debug, PartialEq)]
struct HoltSeriesState {
    /// Number of visible buckets processed up to the cursor.
    last_processed_count: usize,
    /// Write generation of the series when the cursor last advanced.
    last_write_gen: u64,
    /// Timestamp of the last ingested point (the cursor).
    last_processed_time: i64,
    /// Value of the last ingested point, used to detect same-bucket merges.
    last_processed_value: f64,

    /// Warmup counter.
    warmup_count: usize,
    /// Value of the first warmup point, used for the degenerate single-point seed.
    warmup_first_value: f64,
    /// Sum of the first half-window of warmup points.
    warmup_first_sum: f64,
    /// Sum of the last half-window of warmup points.
    warmup_last_sum: f64,
    /// Whether warmup has completed and level/trend are seeded.
    warmed_up: bool,

    /// Holt level component.
    level: f64,
    /// Holt trend component.
    trend: f64,

    /// Rolling window of the newest forecast residuals (FIFO, oldest first).
    res_win: Vec<f64>,
    /// Rolling window of the newest raw values (FIFO, oldest first).
    val_win: Vec<f64>,

    /// Consecutive same-sign (positive) threshold breaches.
    consecutive_pos: usize,
    /// Consecutive same-sign (negative) threshold breaches.
    consecutive_neg: usize,
    /// Remaining suppressed points after a fire.
    refractory_remaining: usize,

    /// Ring of the newest point timestamps, for sampling-interval estimation.
    recent_timestamps: Vec<i64>,
    /// Timestamp of the newest point seen (used as the anomaly timestamp on a fire).
    last_seen_timestamp: i64,
}

impl HoltSeriesState {
    /// Creates an empty state, matching the Go `newState` allocation.
    fn new() -> Self {
        Self {
            last_processed_count: 0,
            last_write_gen: 0,
            last_processed_time: 0,
            last_processed_value: 0.0,
            warmup_count: 0,
            warmup_first_value: 0.0,
            warmup_first_sum: 0.0,
            warmup_last_sum: 0.0,
            warmed_up: false,
            level: 0.0,
            trend: 0.0,
            res_win: Vec::new(),
            val_win: Vec::new(),
            consecutive_pos: 0,
            consecutive_neg: 0,
            refractory_remaining: 0,
            recent_timestamps: Vec::new(),
            last_seen_timestamp: 0,
        }
    }
}

/// Holt's-method forecast-residual detector.
///
/// The detector keeps per-`(series, aggregate)` state and implements [`Detector`], including
/// [`Detector::remove_series`] so eviction fan-out keeps that state bounded. Construct it with
/// [`HoltResidualDetector::new`] for defaults, or [`HoltResidualDetector::with_config`] to override
/// tunables.
#[derive(Debug)]
pub struct HoltResidualDetector {
    /// Set once both MAD windows are full for any series.
    ready: bool,
    /// Resolved configuration (zero fields are filled by `ensure_defaults`).
    config: HoltResidualConfig,
    /// Per-`(series, aggregate)` state.
    series: HashMap<(SeriesRef, Aggregate), HoltSeriesState>,
    /// Cached workload refs, refreshed when the series generation changes.
    cached_refs: Option<Vec<SeriesRef>>,
    /// Series generation the cached refs were listed at.
    cached_gen: u64,
    /// Reused median/MAD scratch space; never aliases series state windows.
    workspace: ScanDetectorWorkspace,
}

impl Default for HoltResidualDetector {
    /// Creates a detector with the default configuration.
    fn default() -> Self {
        Self::new()
    }
}

impl HoltResidualDetector {
    /// Creates a detector with the production defaults.
    pub fn new() -> Self {
        Self::with_config(HoltResidualConfig::default())
    }

    /// Creates a detector from `config`.
    ///
    /// Zero-valued fields are left as-is here and resolved by `ensure_defaults` on the first detection
    /// call, matching the Go `NewHoltResidualDetectorWithConfig`.
    pub fn with_config(config: HoltResidualConfig) -> Self {
        Self {
            ready: false,
            config,
            series: HashMap::new(),
            cached_refs: None,
            cached_gen: 0,
            workspace: ScanDetectorWorkspace::new(),
        }
    }

    /// Returns the `(min_points, max_points)` window the detector needs from storage.
    ///
    /// This replaces the Go `DetectorPointWindowRequirement`: `min_points` is the warmup length and
    /// `max_points` is the wider of warmup and the residual window. Zero-valued fields are resolved the
    /// same way [`Detector::detect`] resolves them.
    pub fn point_window(&self) -> (usize, usize) {
        let warmup = if self.config.warmup_points == 0 {
            DEFAULT_WARMUP_POINTS
        } else {
            self.config.warmup_points
        };
        let residual_window = if self.config.residual_window == 0 {
            DEFAULT_RESIDUAL_WINDOW
        } else {
            self.config.residual_window
        };
        (warmup, warmup.max(residual_window))
    }

    /// Fills zero/non-positive configuration fields with defaults, mirroring Go's `ensureDefaults`.
    fn ensure_defaults(&mut self) {
        if self.config.alpha <= 0.0 {
            self.config.alpha = DEFAULT_ALPHA;
        }
        if self.config.beta <= 0.0 {
            self.config.beta = DEFAULT_BETA;
        }
        if self.config.warmup_points == 0 {
            self.config.warmup_points = DEFAULT_WARMUP_POINTS;
        }
        if self.config.residual_window == 0 {
            self.config.residual_window = DEFAULT_RESIDUAL_WINDOW;
        }
        if self.config.z_threshold <= 0.0 {
            self.config.z_threshold = DEFAULT_Z_THRESHOLD;
        }
        if self.config.confirm_m == 0 {
            self.config.confirm_m = DEFAULT_CONFIRM_M;
        }
        if self.config.min_deviation_mad <= 0.0 {
            self.config.min_deviation_mad = DEFAULT_MIN_DEVIATION_MAD;
        }
        if self.config.refractory == 0 {
            self.config.refractory = DEFAULT_REFRACTORY;
        }
        if self.config.aggregations.is_empty() {
            self.config.aggregations = vec![Aggregate::Average, Aggregate::Count];
        }
    }

    /// Streams points in `(start_time, data_time]` into `state`.
    ///
    /// Returns the fired anomalies (with their `series_ref` populated) and whether any point was ingested.
    fn ingest_new_points(
        &mut self, view: &dyn StorageView, series_ref: SeriesRef, agg: Aggregate, state: &mut HoltSeriesState,
        start_time: i64, data_time: i64, allow_fire: bool,
    ) -> (Vec<Anomaly>, bool) {
        if data_time <= start_time {
            return (Vec::new(), false);
        }
        let Some(series) = view.get_series_range(series_ref, start_time, data_time, agg) else {
            return (Vec::new(), false);
        };

        let mut fired = Vec::new();
        let mut points_seen = false;
        for point in &series.points {
            points_seen = true;
            state.last_seen_timestamp = point.second;
            state.last_processed_time = point.second;
            state.last_processed_value = point.value;
            push_timestamp(state, point.second);

            if !state.warmed_up {
                state.warmup_count += 1;
                if state.warmup_count == 1 {
                    state.warmup_first_value = point.value;
                }
                let half = self.config.warmup_points / 2;
                if state.warmup_count <= half {
                    state.warmup_first_sum += point.value;
                }
                if state.warmup_count > self.config.warmup_points - half {
                    state.warmup_last_sum += point.value;
                }
                if state.warmup_count >= self.config.warmup_points {
                    seed_level_trend(state, self.config.warmup_points);
                    state.warmed_up = true;
                }
                continue;
            }

            let anomaly = self.process_point(state, &series, series_ref, agg, *point, allow_fire);
            if state.res_win.len() >= self.config.residual_window && state.val_win.len() >= self.config.residual_window
            {
                self.ready = true;
            }

            match anomaly {
                Some(anomaly) => fired.push(anomaly),
                None if state.refractory_remaining > 0 => {
                    // Refractory countdown — decrement on every non-firing post-warmup ingest. The point
                    // that armed refractory does not consume one of the configured suppressed points.
                    state.refractory_remaining -= 1;
                }
                None => {}
            }
        }
        (fired, points_seen)
    }

    /// Runs one Holt step: forecast → residual → gate → smoother update.
    ///
    /// Returns the populated anomaly when the gate fires and refractory is clear, otherwise `None`. The
    /// smoother recurrences always advance so level/trend track new regimes through and after fires.
    fn process_point(
        &mut self, state: &mut HoltSeriesState, series: &Series, series_ref: SeriesRef, agg: Aggregate, point: Point,
        allow_fire: bool,
    ) -> Option<Anomaly> {
        // 1. One-step forecast and residual.
        let forecast = state.level + state.trend;
        let residual = point.value - forecast;

        // 2. Standardise the residual against the rolling-MAD baseline. Gate values are computed BEFORE
        // pushing the new residual, so standardisation uses the historical baseline, not a window that
        // already contains the candidate point. The range-based MAD floor keeps `z` proportional to the
        // data's natural scale while a bimodal transition window would otherwise collapse the MAD to zero.
        let (median_residual, sigma_residual) = self.median_mad(&state.res_win);
        let sigma_residual = floor_sigma(sigma_residual, &state.res_win);
        let z = (residual - median_residual) / sigma_residual;

        // The effect-size denominator is the rolling MAD over raw values, with the same range floor.
        let (_, sigma_value) = self.median_mad(&state.val_win);
        let sigma_value = floor_sigma(sigma_value, &state.val_win);
        let dev_mad = (point.value - state.level).abs() / sigma_value;

        // 3. Update the confirmation counters only once both baseline windows are representative.
        // Under-filled windows collapse sigma to the floor and would otherwise pre-arm confirmation.
        let windows_ready =
            state.res_win.len() >= self.config.residual_window && state.val_win.len() >= self.config.residual_window;
        let z_mag_passes = windows_ready && z.abs() >= self.config.z_threshold;
        if z_mag_passes {
            if z > 0.0 {
                state.consecutive_pos += 1;
                state.consecutive_neg = 0;
            } else {
                state.consecutive_neg += 1;
                state.consecutive_pos = 0;
            }
        } else {
            state.consecutive_pos = 0;
            state.consecutive_neg = 0;
        }

        let confirmed =
            state.consecutive_pos >= self.config.confirm_m || state.consecutive_neg >= self.config.confirm_m;
        let gate_ok = windows_ready && z_mag_passes && confirmed && dev_mad >= self.config.min_deviation_mad;

        // 4. Smoother update — runs on every post-warmup ingest, regardless of fire/refractory. This is
        // what lets the model adapt through and after a regime shift; the gate decides what to emit, never
        // what to learn.
        let new_level = self.config.alpha * point.value + (1.0 - self.config.alpha) * (state.level + state.trend);
        state.trend = self.config.beta * (new_level - state.level) + (1.0 - self.config.beta) * state.trend;
        state.level = new_level;

        // 5. Decide whether to emit a fire.
        let fire = allow_fire && gate_ok && state.refractory_remaining == 0;

        // 6. Push the residual into the MAD window. On fire it is replaced by the median of the newest
        // non-fire residuals (Hampel rejection — applied to the threshold window only, not the smoother).
        // This stops the flagged anomaly from inflating sigma_residual and blinding us to later shifts.
        let residual_for_window = if fire {
            self.median_of_tail(&state.res_win, POST_FIRE_SAMPLE_N)
        } else {
            residual
        };
        push_fifo(&mut state.res_win, self.config.residual_window, residual_for_window);
        push_fifo(&mut state.val_win, self.config.residual_window, point.value);

        if !fire {
            return None;
        }

        // 7. Build the anomaly with the common metric-detector shape, using post-update level/trend.
        let score = z.abs();
        let anomaly = Anomaly {
            anomaly_type: AnomalyType::Metric,
            series: SeriesDescriptor::new(
                series.namespace.clone(),
                series.name.clone(),
                series.host.clone(),
                series.tags.clone(),
                agg,
            ),
            series_ref: Some(QueryHandle::new(series_ref, agg)),
            detector_name: self.name().to_string(),
            context: None,
            timestamp_sec: state.last_seen_timestamp,
            score: Some(score),
            sampling_interval_sec: median_timestamp_interval(&state.recent_timestamps),
            evidence: Some(AnomalyEvidence::HoltResidual {
                baseline_median: median_residual,
                baseline_mad: sigma_residual,
                current_value: point.value,
                deviation_sigma: z.abs(),
                threshold: self.config.z_threshold,
                forecast,
                residual,
                holt_level: state.level,
                holt_trend: state.trend,
                value_mads: dev_mad,
            }),
        };

        // 8. Reset confirmation counters and arm refractory.
        state.consecutive_pos = 0;
        state.consecutive_neg = 0;
        state.refractory_remaining = self.config.refractory;

        Some(anomaly)
    }

    /// Returns `(median, mad * 1.4826)` of `vals` without allocating, reusing the detector scratch space.
    fn median_mad(&mut self, vals: &[f64]) -> (f64, f64) {
        if vals.is_empty() {
            return (0.0, 0.0);
        }
        let median = self.workspace.median(vals);
        let mad = self.workspace.mad(vals, median) * 1.4826;
        (median, mad)
    }

    /// Returns the median of the newest `n` values of `buf`, clamped to the buffer length.
    fn median_of_tail(&mut self, buf: &[f64], n: usize) -> f64 {
        if buf.is_empty() {
            return 0.0;
        }
        let n = n.min(buf.len());
        let tail = &buf[buf.len() - n..];
        self.workspace.median(tail)
    }
}

impl Detector for HoltResidualDetector {
    type Config = HoltResidualConfig;

    fn name(&self) -> &str {
        "holt_residual"
    }

    fn config(&self) -> &Self::Config {
        &self.config
    }

    fn is_ready(&self) -> bool {
        self.ready
    }

    /// Streams new points into per-series Holt state and emits anomalies when all three gates pass.
    fn detect(&mut self, view: &dyn StorageView, data_time_sec: i64) -> Vec<Anomaly> {
        self.ensure_defaults();

        let generation = view.series_generation();
        if self.cached_refs.is_none() || generation != self.cached_gen {
            self.cached_refs = Some(workload_series_refs(view));
            self.cached_gen = generation;
        }
        // Copy the refs and aggregations so the per-series loop can hold `&mut self` state without an
        // active borrow of the detector.
        let refs = self.cached_refs.clone().unwrap_or_default();
        let aggregations = self.config.aggregations.clone();

        let mut anomalies = Vec::new();
        for series_ref in refs {
            let point_count = view.point_count_up_to(series_ref, data_time_sec);
            let write_generation = view.write_generation(series_ref);

            for &aggregation in &aggregations {
                // Go's `supportsSeriesAggregate` falls back to `true` when storage lacks the optional
                // policy interface. `StorageView` does not expose it, so every aggregate is supported.
                let key = (series_ref, aggregation);
                let exists = self.series.contains_key(&key);
                if !exists && point_count < self.config.warmup_points {
                    continue;
                }
                if !exists {
                    self.series.insert(key, HoltSeriesState::new());
                }

                // Take the state out of the map so helper methods can borrow `&mut self` freely; it is
                // re-inserted before the next iteration (the Go code works on a pointer into the map).
                let mut state = self
                    .series
                    .remove(&key)
                    .expect("state exists for the key just checked or inserted");

                // Replay gate: skip when no new bucket or in-place merge is visible.
                if point_count <= state.last_processed_count && write_generation == state.last_write_gen {
                    self.series.insert(key, state);
                    continue;
                }

                let mut start_time = state.last_processed_time;
                let count_increased = point_count > state.last_processed_count;
                let prefix_count = view.point_count_up_to(series_ref, state.last_processed_time);
                // A full point window can evict an old bucket while appending a new one. That changes the
                // generation without changing the total count; the smaller prefix shows the lost bucket was
                // before our cursor.
                let merge_occurred = point_count == state.last_processed_count
                    && write_generation != state.last_write_gen
                    && prefix_count >= state.last_processed_count;
                let cursor_bucket_changed_with_append = count_increased
                    && write_generation != state.last_write_gen
                    && prefix_count == state.last_processed_count
                    && cursor_point_changed(view, series_ref, aggregation, &state);
                if merge_occurred || prefix_count > state.last_processed_count || cursor_bucket_changed_with_append {
                    state = HoltSeriesState::new();
                    start_time = 0;
                }

                let allow_fire = point_count >= self.config.warmup_points;
                let (fired, points_seen) = self.ingest_new_points(
                    view,
                    series_ref,
                    aggregation,
                    &mut state,
                    start_time,
                    data_time_sec,
                    allow_fire,
                );
                anomalies.extend(fired);

                if points_seen || write_generation != state.last_write_gen {
                    state.last_processed_count = point_count;
                    state.last_write_gen = write_generation;
                }
                self.series.insert(key, state);
            }
        }

        anomalies
    }

    fn reset(&mut self) {
        self.series.clear();
        self.cached_refs = None;
        self.cached_gen = 0;
        self.ready = false;
    }

    fn remove_series(&mut self, series: &[SeriesRef]) {
        self.ensure_defaults();
        if series.is_empty() || self.series.is_empty() {
            return;
        }
        let aggregations = self.config.aggregations.clone();
        for &series_ref in series {
            for &aggregation in &aggregations {
                self.series.remove(&(series_ref, aggregation));
            }
        }
        self.cached_refs = None;
        self.cached_gen = 0;
    }
}

/// Returns the workload refs: every live series except the telemetry namespace.
///
/// This is the fallback for the Go `workloadSeriesRefs`, which uses a ref-only listing optimisation when
/// storage provides it. [`StorageView::list_series`] only filters by an exact namespace, so the telemetry
/// exclusion of the Go `WorkloadSeriesFilter` is applied here instead.
fn workload_series_refs(view: &dyn StorageView) -> Vec<SeriesRef> {
    view.list_series(None)
        .into_iter()
        .filter(|meta| meta.namespace.as_str() != TELEMETRY_NAMESPACE)
        .map(|meta| meta.series_ref)
        .collect()
}

/// Returns whether the point at the cursor's timestamp has changed value since it was processed.
///
/// This detects an in-place merge of the cursor bucket when a later bucket was also appended.
fn cursor_point_changed(
    view: &dyn StorageView, series_ref: SeriesRef, aggregate: Aggregate, state: &HoltSeriesState,
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

/// Seeds level and trend from the warmup half-window aggregates using the classic two-half average:
/// `L_0 = mean(first half)`, `T_0 = (mean(last half) - mean(first half)) / half`.
///
/// For an odd `n` the middle point belongs to neither half. The divisor is `half` (not `n / 2` as a float).
fn seed_level_trend(state: &mut HoltSeriesState, n: usize) {
    let half = n / 2;
    if half < 1 {
        // Degenerate configuration: fall back to a single-point seed.
        if n == 1 {
            state.level = state.warmup_first_value;
            state.trend = 0.0;
        }
        return;
    }
    let mean_first = state.warmup_first_sum / half as f64;
    let mean_last = state.warmup_last_sum / half as f64;
    state.level = mean_first;
    state.trend = (mean_last - mean_first) / half as f64;
}

/// Returns `sigma` when it is already meaningful, otherwise a range-based noise floor.
///
/// The floor applies when the rolling window is bimodal (for example old regime plus new regime during a
/// transition): a sample from a transitioning regime can have a median equal to one mode and a MAD of
/// zero, since over half the values sit exactly on the median. Without the floor any residual would
/// standardise to `|z| → ∞`. The floor is 5% of the window's observed range, bounded below by `1e-6`.
fn floor_sigma(sigma: f64, win: &[f64]) -> f64 {
    const MIN_ABSOLUTE_FLOOR: f64 = 1e-6;
    const RANGE_FRACTION: f64 = 0.05;
    if sigma >= MIN_ABSOLUTE_FLOOR {
        // `sigma >= MIN_ABSOLUTE_FLOOR` subsumes the Go source's redundant `sigma > 0` guard, and both
        // comparisons are false for NaN so a NaN sigma still falls through to the floor.
        // Compute the range only when sigma may have collapsed below the fraction-of-range threshold;
        // otherwise the MAD itself is the stronger floor.
        if sigma >= RANGE_FRACTION * window_range(win) {
            return sigma;
        }
    }
    let mut range_floor = RANGE_FRACTION * window_range(win);
    if range_floor < MIN_ABSOLUTE_FLOOR {
        range_floor = MIN_ABSOLUTE_FLOOR;
    }
    if sigma > range_floor {
        return sigma;
    }
    range_floor
}

/// Returns `max - min` over the window, or `0` for an empty window.
fn window_range(win: &[f64]) -> f64 {
    let Some((&first, rest)) = win.split_first() else {
        return 0.0;
    };
    let mut min_value = first;
    let mut max_value = first;
    for &value in rest {
        if value < min_value {
            min_value = value;
        }
        if value > max_value {
            max_value = value;
        }
    }
    max_value - min_value
}

/// Appends `value` to `buf`, dropping the oldest entry once `buf` reaches `max_len`.
fn push_fifo(buf: &mut Vec<f64>, max_len: usize, value: f64) {
    if buf.len() < max_len {
        buf.push(value);
        return;
    }
    buf.copy_within(1.., 0);
    buf[max_len - 1] = value;
}

/// Appends `timestamp` to the recent-timestamp ring, dropping the oldest entry once it is full.
fn push_timestamp(state: &mut HoltSeriesState, timestamp: i64) {
    if state.recent_timestamps.len() < TIMESTAMP_RING {
        state.recent_timestamps.push(timestamp);
        return;
    }
    state.recent_timestamps.copy_within(1.., 0);
    let last = state.recent_timestamps.len() - 1;
    state.recent_timestamps[last] = timestamp;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageConfig;
    use crate::storage::TimeSeriesStorage;

    /// The Go `rand.New(rand.NewSource(42)).NormFloat64()` stream added to a `0.05 * ts` ramp, one value
    /// per second for `ts = 1..=500`, formatted with `%.17g`. Regenerated with the program that produced the
    /// file; `%.17g` round-trips exactly into `f64`.
    const GAUSS_SEED42_500: &str = include_str!("fixtures/holt_gauss_seed42_500.txt");

    /// Returns a detector pinned to the `Average` aggregate so anomaly counts are deterministic.
    fn test_detector() -> HoltResidualDetector {
        HoltResidualDetector::with_config(HoltResidualConfig {
            aggregations: vec![Aggregate::Average],
            ..HoltResidualConfig::default()
        })
    }

    /// Returns a store with retention disabled, matching the Go `newDetectorTestStorage`.
    fn test_storage() -> TimeSeriesStorage {
        TimeSeriesStorage::new(StorageConfig {
            point_retention_secs: 0,
            ..StorageConfig::default()
        })
    }

    /// Feeds `count` points of the same value at consecutive seconds from `start_ts`.
    fn add_constant(storage: &mut TimeSeriesStorage, name: &str, count: usize, start_ts: i64, value: f64) {
        for i in 0..count {
            storage.add("ns", name, None, value, start_ts + i as i64, &[]);
        }
    }

    /// Feeds `count` points of a linear ramp where the first value is `base + slope` and the `i`-th value is
    /// `base + (i + 1) * slope`.
    fn add_ramp(storage: &mut TimeSeriesStorage, name: &str, count: usize, start_ts: i64, base: f64, slope: f64) {
        for i in 0..count {
            storage.add(
                "ns",
                name,
                None,
                base + slope * (i as f64 + 1.0),
                start_ts + i as i64,
                &[],
            );
        }
    }

    /// Returns the single workload series ref of a test store.
    fn single_ref(storage: &TimeSeriesStorage) -> SeriesRef {
        storage.list_series(None)[0].series_ref
    }

    /// Returns the parsed Gaussian-ramp fixture values.
    fn gaussian_values() -> Vec<f64> {
        GAUSS_SEED42_500
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.parse::<f64>().expect("fixture value parses"))
            .collect()
    }

    /// Returns the timestamps of the anomalies, in order.
    fn anomaly_timestamps(anomalies: &[Anomaly]) -> Vec<i64> {
        anomalies.iter().map(|anomaly| anomaly.timestamp_sec).collect()
    }

    #[test]
    fn constant_does_not_fire() {
        let mut detector = test_detector();
        let mut storage = test_storage();
        add_constant(&mut storage, "metric", 500, 1, 7.0);

        let result = detector.detect(&storage, 500);

        assert!(result.is_empty(), "constant input must not trigger Holt residual");
    }

    #[test]
    fn pure_ramp_does_not_fire() {
        let mut detector = test_detector();
        let mut storage = test_storage();
        add_ramp(&mut storage, "metric", 500, 1, 0.0, 0.05);

        let result = detector.detect(&storage, 500);

        assert!(
            result.is_empty(),
            "noise-free ramp must not fire — Holt should track the trend"
        );
    }

    #[test]
    fn ramp_with_spike_fires_once() {
        let mut detector = test_detector();
        let mut storage = test_storage();

        const SPIKE_START: i64 = 300;
        const SPIKE_LEN: i64 = 2;
        const SPIKE_BOOST: f64 = 20.0;

        for ts in 1..=500 {
            let mut value = 0.05 * ts as f64;
            if (SPIKE_START..SPIKE_START + SPIKE_LEN).contains(&ts) {
                value += SPIKE_BOOST;
            }
            storage.add("ns", "metric", None, value, ts, &[]);
        }

        let result = detector.detect(&storage, 500);

        assert_eq!(
            result.len(),
            1,
            "exactly one fire expected for a 2-point spike on a clean ramp"
        );
        let anomaly = &result[0];
        assert_eq!(anomaly.detector_name, "holt_residual");
        assert_eq!(
            anomaly.series_ref,
            Some(QueryHandle::new(single_ref(&storage), Aggregate::Average))
        );
        let score = anomaly.score.expect("score must be populated");
        assert!(score > 4.5, "score should clear the |z| threshold, got {score}");

        let evidence = anomaly.evidence.as_ref().expect("evidence must be populated");
        let AnomalyEvidence::HoltResidual {
            threshold,
            forecast,
            residual,
            holt_level,
            value_mads,
            ..
        } = evidence
        else {
            panic!("expected HoltResidual evidence");
        };
        assert_eq!(*threshold, 4.5);
        assert_ne!(*forecast, 0.0);
        assert_ne!(*residual, 0.0);
        assert_ne!(*holt_level, 0.0);
        assert_ne!(*value_mads, 0.0);
        // Fire timestamp lands on the second spike point (confirm_m = 2).
        assert_eq!(anomaly.timestamp_sec, SPIKE_START + SPIKE_LEN - 1);
    }

    #[test]
    fn ramp_with_gaussian_noise_fires_at_most_once() {
        let mut detector = test_detector();
        let mut storage = test_storage();

        for (index, value) in gaussian_values().into_iter().enumerate() {
            storage.add("ns", "metric", None, value, index as i64 + 1, &[]);
        }

        let result = detector.detect(&storage, 500);

        assert!(
            result.len() <= 1,
            "Gaussian noise must not produce more than one false fire"
        );
    }

    #[test]
    fn step_change_fires_and_adapts() {
        let mut detector = test_detector();
        let mut storage = test_storage();

        const STEP_START: i64 = 200;
        const STEP_VALUE: f64 = 5.0;

        add_constant(&mut storage, "metric", 199, 1, 0.0);
        add_constant(&mut storage, "metric", 100, STEP_START, STEP_VALUE);

        let result = detector.detect(&storage, 299);

        assert_eq!(
            result.len(),
            1,
            "refractory should suppress the second fire on a single step"
        );
        let anomaly = &result[0];
        assert!(
            anomaly.timestamp_sec >= STEP_START,
            "fire must come on or after the step"
        );
        assert!(
            anomaly.timestamp_sec < STEP_START + detector.config().confirm_m as i64,
            "fire must come within confirm_m points of the step"
        );

        let series_ref = anomaly.series_ref.expect("series ref populated").series;
        let state = detector
            .series
            .get(&(series_ref, Aggregate::Average))
            .expect("state exists for the fired series");
        assert!(
            state.level > 4.0,
            "smoothed level should converge toward the new regime (5.0)"
        );
    }

    #[test]
    fn refractory_configured_window_is_not_consumed_by_firing_point() {
        let mut detector = HoltResidualDetector::with_config(HoltResidualConfig {
            alpha: 0.01,
            beta: 0.01,
            residual_window: 4,
            z_threshold: 0.5,
            confirm_m: 1,
            min_deviation_mad: 0.0,
            refractory: 1,
            aggregations: vec![Aggregate::Average],
            ..HoltResidualConfig::default()
        });
        let mut storage = test_storage();
        add_constant(&mut storage, "metric", 28, 1, 0.0);
        add_constant(&mut storage, "metric", 2, 29, 100.0);

        let result = detector.detect(&storage, 30);

        assert_eq!(
            result.len(),
            1,
            "refractory = 1 must suppress the point immediately after a fire"
        );
        assert_eq!(result[0].timestamp_sec, 29);
    }

    #[test]
    fn refractory_suppresses_nearby_spike() {
        let mut detector = test_detector();
        let mut storage = test_storage();

        add_constant(&mut storage, "metric", 99, 1, 0.0);
        add_constant(&mut storage, "metric", 2, 100, 20.0);
        add_constant(&mut storage, "metric", 4, 102, 0.0);
        add_constant(&mut storage, "metric", 2, 106, 20.0);
        add_constant(&mut storage, "metric", 50, 108, 0.0);

        let result = detector.detect(&storage, 158);

        assert_eq!(
            result.len(),
            1,
            "refractory must suppress the second spike when 5 points apart"
        );
        assert_eq!(result[0].timestamp_sec, 101);
    }

    #[test]
    fn refractory_allows_distant_spike_to_fire_again() {
        let mut detector = test_detector();
        let mut storage = test_storage();

        add_constant(&mut storage, "metric", 99, 1, 0.0);
        add_constant(&mut storage, "metric", 2, 100, 20.0);
        add_constant(&mut storage, "metric", 24, 102, 0.0);
        add_constant(&mut storage, "metric", 2, 126, 20.0);
        add_constant(&mut storage, "metric", 50, 128, 0.0);

        let result = detector.detect(&storage, 178);

        assert_eq!(result.len(), 2, "two spikes 25 points apart must both fire");
        assert_eq!(result[0].timestamp_sec, 101);
        assert_eq!(result[1].timestamp_sec, 127);
    }

    #[test]
    fn remove_series_drops_state_and_invalidates_cache() {
        let mut detector = test_detector();
        let mut storage = test_storage();

        add_constant(&mut storage, "metric", 100, 1, 1.0);
        detector.detect(&storage, 100);
        assert!(
            !detector.series.is_empty(),
            "detect should have populated per-series state"
        );
        assert!(detector.cached_refs.is_some(), "detect should have cached refs");

        let series_ref = single_ref(&storage);
        detector.remove_series(&[series_ref]);

        assert!(
            detector.series.is_empty(),
            "remove_series must drop per-series state for freed refs"
        );
        assert!(
            detector.cached_refs.is_none(),
            "remove_series should invalidate the series cache"
        );
    }

    #[test]
    fn reset_clears_state_and_cache() {
        let mut detector = test_detector();
        let mut storage = test_storage();

        add_constant(&mut storage, "metric", 100, 1, 1.0);
        detector.detect(&storage, 100);
        assert!(!detector.series.is_empty(), "should have state after detection");

        detector.reset();

        assert!(detector.series.is_empty(), "reset must clear per-series state");
        assert!(detector.cached_refs.is_none(), "reset must clear cached series");
        assert!(!detector.is_ready(), "reset must clear readiness");
    }

    #[test]
    fn zero_config_resolves_to_defaults() {
        let mut detector = HoltResidualDetector::with_config(HoltResidualConfig {
            alpha: 0.0,
            beta: 0.0,
            warmup_points: 0,
            residual_window: 0,
            z_threshold: 0.0,
            confirm_m: 0,
            min_deviation_mad: 0.0,
            refractory: 0,
            aggregations: Vec::new(),
        });
        let storage = test_storage();

        let _ = detector.detect(&storage, 1);

        let config = detector.config();
        assert_eq!(config.alpha, 0.2);
        assert_eq!(config.beta, 0.05);
        assert_eq!(config.warmup_points, 24);
        assert_eq!(config.residual_window, 60);
        assert_eq!(config.z_threshold, 4.5);
        assert_eq!(config.confirm_m, 2);
        assert_eq!(config.min_deviation_mad, 3.0);
        assert_eq!(config.refractory, 20);
        assert_eq!(config.aggregations.len(), 2);
        assert!(config.aggregations.contains(&Aggregate::Average));
        assert!(config.aggregations.contains(&Aggregate::Count));
    }

    #[test]
    fn name_is_holt_residual() {
        assert_eq!(HoltResidualDetector::new().name(), "holt_residual");
    }

    #[test]
    fn point_window_uses_warmup_and_residual_window() {
        let detector = test_detector();
        assert_eq!(detector.point_window(), (24, 60));

        let detector = HoltResidualDetector::with_config(HoltResidualConfig {
            warmup_points: 15,
            residual_window: 4,
            ..HoltResidualConfig::default()
        });
        assert_eq!(detector.point_window(), (15, 15));

        // A zeroed config resolves the same way `detect` would.
        let detector = HoltResidualDetector::with_config(HoltResidualConfig {
            warmup_points: 0,
            residual_window: 0,
            ..HoltResidualConfig::default()
        });
        assert_eq!(detector.point_window(), (24, 60));
    }

    #[test]
    fn incremental_advances_match_batch_replay() {
        let mut batch = test_detector();
        let mut incremental = test_detector();
        let mut batch_storage = test_storage();
        let mut incremental_storage = test_storage();

        const END: i64 = 500;
        let mut values = Vec::new();
        for ts in 1..=END {
            let mut value = 0.05 * ts as f64;
            if (300..302).contains(&ts) {
                value += 20.0;
            }
            values.push(value);
            batch_storage.add("ns", "metric", None, value, ts, &[]);
        }

        let batch_result = batch.detect(&batch_storage, END);

        let mut incremental_anomalies = Vec::new();
        for (index, value) in values.into_iter().enumerate() {
            let ts = index as i64 + 1;
            incremental_storage.add("ns", "metric", None, value, ts, &[]);
            let result = incremental.detect(&incremental_storage, ts);
            incremental_anomalies.extend(result);
        }

        assert_eq!(
            anomaly_timestamps(&batch_result),
            anomaly_timestamps(&incremental_anomalies)
        );
    }

    #[test]
    fn reprocesses_same_bucket_merge_and_does_not_skip_late_points() {
        let mut detector = test_detector();
        detector.config.warmup_points = 1;
        detector.config.residual_window = 4;
        let mut storage = test_storage();

        storage.add("ns", "metric", None, 10.0, 10, &[]);
        detector.detect(&storage, 10);

        let series_ref = single_ref(&storage);
        let key = (series_ref, Aggregate::Average);
        let state = detector.series.get(&key).expect("state exists");
        assert!(state.warmed_up);
        assert_eq!(state.last_processed_time, 10);

        storage.add("ns", "metric", None, 30.0, 10, &[]);
        let series = storage
            .get_series_range(series_ref, 0, 10, Aggregate::Average)
            .expect("series is live");
        assert_eq!(series.points.len(), 1);
        assert_eq!(series.points[0].value, 20.0, "storage should expose the merged average");

        detector.detect(&storage, 20);
        let state = detector.series.get(&key).expect("state exists");
        assert!(state.warmed_up);
        assert_eq!(
            state.last_processed_time, 10,
            "merge replay must not advance to data_time"
        );

        storage.add("ns", "metric", None, 50.0, 15, &[]);
        detector.detect(&storage, 20);
        let state = detector.series.get(&key).expect("state exists");
        assert_eq!(state.res_win.len(), 1);
        assert_eq!(state.last_processed_time, 15);
        assert_eq!(state.last_write_gen, storage.write_generation(series_ref));
    }

    #[test]
    fn rebuilds_on_out_of_order_backfill_before_cursor() {
        let mut detector = test_detector();
        detector.config.warmup_points = 1;
        detector.config.residual_window = 4;
        let mut storage = test_storage();

        storage.add("ns", "metric", None, 10.0, 10, &[]);
        detector.detect(&storage, 10);

        let series_ref = single_ref(&storage);
        let key = (series_ref, Aggregate::Average);
        assert!(detector.series.get(&key).expect("state exists").warmed_up);

        storage.add("ns", "metric", None, 5.0, 5, &[]);
        detector.detect(&storage, 10);

        let state = detector.series.get(&key).expect("state exists");
        assert!(state.warmed_up);
        assert_eq!(state.res_win.len(), 1);
        assert_eq!(state.last_processed_count, 2);
        assert_eq!(state.last_processed_time, 10);
    }

    #[test]
    fn rebuilds_on_cursor_merge_with_later_append() {
        let mut detector = test_detector();
        detector.config.warmup_points = 1;
        detector.config.residual_window = 4;
        let mut storage = test_storage();

        storage.add("ns", "metric", None, 10.0, 10, &[]);
        detector.detect(&storage, 10);

        let series_ref = single_ref(&storage);
        let key = (series_ref, Aggregate::Average);
        assert!(detector.series.get(&key).expect("state exists").warmed_up);

        storage.add("ns", "metric", None, 30.0, 10, &[]);
        storage.add("ns", "metric", None, 40.0, 11, &[]);
        detector.detect(&storage, 11);

        let state = detector.series.get(&key).expect("state exists");
        assert!(state.warmed_up);
        assert_eq!(state.res_win.len(), 1);
        assert_eq!(state.last_processed_count, 2);
        assert_eq!(state.last_processed_time, 11);
    }

    #[test]
    fn preserves_state_when_point_cap_evicts_oldest_bucket() {
        let mut detector = test_detector();
        detector.config.warmup_points = 1;
        detector.config.residual_window = 4;
        let mut storage = TimeSeriesStorage::new(StorageConfig {
            max_points_per_series: 2,
            ..StorageConfig::default()
        });

        for timestamp in 1..=3 {
            storage.add("ns", "metric", None, timestamp as f64, timestamp, &[]);
        }
        detector.detect(&storage, 3);

        let series_ref = single_ref(&storage);
        let key = (series_ref, Aggregate::Average);
        let state = detector.series.get(&key).expect("state exists");
        assert_eq!(state.last_processed_count, 3);
        // The first processed bucket carries value 1.0; a rebuild would re-warm-up from bucket 2 and lose it.
        assert_eq!(state.warmup_first_value, 1.0);

        storage.add("ns", "metric", None, 4.0, 4, &[]);
        detector.detect(&storage, 4);

        let state = detector.series.get(&key).expect("state exists");
        // State was updated in place, not rebuilt: it kept the incremental (3-element) residual window
        // rather than re-ingesting the 2 post-eviction buckets.
        assert_eq!(state.last_processed_time, 4);
        assert_eq!(state.last_processed_count, 3);
        assert_eq!(state.warmup_first_value, 1.0);
        assert_eq!(state.res_win.len(), 3);
        assert_eq!(state.val_win.len(), 3);
    }

    #[test]
    fn continues_after_retention_drops_below_warmup() {
        let mut detector = test_detector();
        detector.config.warmup_points = 3;
        detector.config.residual_window = 4;
        let mut storage = test_storage();

        for timestamp in 1..=3 {
            storage.add("ns", "metric", None, timestamp as f64, timestamp, &[]);
        }
        detector.detect(&storage, 3);

        let series_ref = single_ref(&storage);
        let key = (series_ref, Aggregate::Average);
        assert!(detector.series.contains_key(&key));

        storage.set_series_retention(series_ref, 1);
        storage.add("ns", "metric", None, 4.0, 4, &[]);
        detector.detect(&storage, 4);

        let state = detector.series.get(&key).expect("state exists");
        assert_eq!(state.last_processed_time, 4);
        assert_eq!(state.warmup_first_value, 1.0, "state was preserved, not rebuilt");
    }

    #[test]
    fn confirmation_starts_after_windows_ready() {
        let mut detector = HoltResidualDetector::with_config(HoltResidualConfig {
            alpha: 0.01,
            beta: 0.01,
            residual_window: 4,
            z_threshold: 0.5,
            confirm_m: 2,
            min_deviation_mad: 0.0,
            refractory: 0,
            ..HoltResidualConfig::default()
        });
        let mut state = HoltSeriesState::new();
        state.warmed_up = true;
        state.res_win = vec![0.0, 0.0, 0.0];
        state.val_win = vec![0.0, 0.0, 0.0];
        let series = Series {
            namespace: crate::identity::NamespaceId::new("ns"),
            name: "metric".to_string(),
            host: None,
            tags: Vec::new(),
            points: Vec::new(),
        };

        let fired = detector.process_point(
            &mut state,
            &series,
            SeriesRef::new(0),
            Aggregate::Average,
            Point {
                second: 1,
                value: 100.0,
            },
            true,
        );
        assert!(fired.is_none());
        assert_eq!(
            state.consecutive_pos, 0,
            "under-filled windows must not pre-arm confirmation"
        );
        assert_eq!(state.consecutive_neg, 0);

        let fired = detector.process_point(
            &mut state,
            &series,
            SeriesRef::new(0),
            Aggregate::Average,
            Point {
                second: 2,
                value: 100.0,
            },
            true,
        );
        assert!(
            fired.is_none(),
            "first ready-window breach should only arm confirmation"
        );
        assert_eq!(state.consecutive_pos, 1);
        assert_eq!(state.consecutive_neg, 0);

        let fired = detector.process_point(
            &mut state,
            &series,
            SeriesRef::new(0),
            Aggregate::Average,
            Point {
                second: 3,
                value: 100.0,
            },
            true,
        );
        assert!(fired.is_some(), "second ready-window breach should satisfy confirm_m");
    }

    /// Odd warmup length: the middle point lands in neither half, and the trend divides by `half`.
    #[test]
    fn warmup_split_odd_length_puts_middle_point_in_neither_half() {
        let mut detector = test_detector();
        detector.config.warmup_points = 15;
        detector.config.residual_window = 60;
        let mut storage = test_storage();

        // Values 1..=15 at seconds 1..=15.
        for value in 1..=15 {
            storage.add("ns", "metric", None, value as f64, value as i64, &[]);
        }
        detector.detect(&storage, 15);

        let series_ref = single_ref(&storage);
        let state = detector
            .series
            .get(&(series_ref, Aggregate::Average))
            .expect("state exists");
        assert!(state.warmed_up);
        // First half (points 1..=7) sums to 28, last half (points 9..=15) sums to 84; point 8 is in neither.
        assert_eq!(state.warmup_first_sum, 28.0);
        assert_eq!(state.warmup_last_sum, 84.0);
        assert_eq!(state.level, 4.0);
        assert_eq!(state.trend, 8.0 / 7.0);
    }

    /// Even warmup length: both halves cover every warmup point and the trend divides by `half`.
    #[test]
    fn warmup_split_even_length_covers_every_point() {
        let mut detector = test_detector();
        detector.config.warmup_points = 24;
        detector.config.residual_window = 60;
        let mut storage = test_storage();

        for value in 1..=24 {
            storage.add("ns", "metric", None, value as f64, value as i64, &[]);
        }
        detector.detect(&storage, 24);

        let series_ref = single_ref(&storage);
        let state = detector
            .series
            .get(&(series_ref, Aggregate::Average))
            .expect("state exists");
        assert!(state.warmed_up);
        // First half (1..=12) sums to 78, last half (13..=24) sums to 222.
        assert_eq!(state.warmup_first_sum, 78.0);
        assert_eq!(state.warmup_last_sum, 222.0);
        assert_eq!(state.level, 6.5);
        assert_eq!(state.trend, 1.0);
    }

    /// On a fire the residual window receives the median of the newest non-fire residuals (Hampel
    /// rejection), while the value window always receives the raw value.
    #[test]
    fn fire_replaces_residual_with_tail_median_and_keeps_raw_value() {
        let mut detector = HoltResidualDetector::with_config(HoltResidualConfig {
            alpha: 0.5,
            beta: 0.5,
            residual_window: 2,
            z_threshold: 0.5,
            confirm_m: 1,
            min_deviation_mad: 0.0,
            refractory: 0,
            ..HoltResidualConfig::default()
        });
        let mut state = HoltSeriesState::new();
        state.warmed_up = true;
        state.level = 0.0;
        state.trend = 0.0;
        state.res_win = vec![1.0, 2.0];
        state.val_win = vec![10.0, 20.0];
        state.recent_timestamps = vec![1, 2];
        let series = Series {
            namespace: crate::identity::NamespaceId::new("ns"),
            name: "metric".to_string(),
            host: None,
            tags: Vec::new(),
            points: Vec::new(),
        };

        let anomaly = detector
            .process_point(
                &mut state,
                &series,
                SeriesRef::new(0),
                Aggregate::Average,
                Point {
                    second: 3,
                    value: 100.0,
                },
                true,
            )
            .expect("gate should fire");

        // The window holds the median of the pre-push residuals [1, 2] = 1.5, not the fire's residual 100.
        assert_eq!(state.res_win, vec![2.0, 1.5]);
        // The value window always receives the raw value.
        assert_eq!(state.val_win, vec![20.0, 100.0]);

        let evidence = anomaly.evidence.expect("evidence populated");
        let AnomalyEvidence::HoltResidual {
            residual,
            current_value,
            ..
        } = evidence
        else {
            panic!("expected HoltResidual evidence");
        };
        // The evidence reports the true residual, while the threshold window was Hampel-corrected.
        assert_eq!(residual, 100.0);
        assert_eq!(current_value, 100.0);
    }

    #[test]
    fn floor_sigma_uses_range_fraction_and_absolute_minimum() {
        // Healthy sigma above the range fraction passes through unchanged.
        assert_eq!(floor_sigma(5.0, &[0.0, 10.0, 20.0, 30.0, 40.0]), 5.0);
        // A collapsed sigma falls back to 5% of the window range.
        assert_eq!(floor_sigma(0.0, &[0.0, 10.0, 20.0, 30.0, 40.0]), 2.0);
        // Sigma below the range fraction but above the absolute floor is raised to the range fraction.
        assert_eq!(floor_sigma(1.0, &[0.0, 100.0]), 5.0);
        // A zero-range window bounds the floor at the absolute minimum.
        assert_eq!(floor_sigma(1e-7, &[5.0, 5.0, 5.0]), 1e-6);
        // An empty window still yields the absolute minimum.
        assert_eq!(floor_sigma(0.0, &[]), 1e-6);
        // A NaN sigma is not "meaningful" and falls to the floor, matching the Go comparison shape.
        assert_eq!(floor_sigma(f64::NAN, &[0.0, 100.0]), 5.0);
    }

    #[test]
    fn window_range_handles_empty_single_and_multi_value_windows() {
        assert_eq!(window_range(&[]), 0.0);
        assert_eq!(window_range(&[3.0]), 0.0);
        assert_eq!(window_range(&[1.0, -4.0, 9.0, 2.0]), 13.0);
    }
}
