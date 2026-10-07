//! Bayesian online change-point detection (BOCPD).
//!
//! This is the Rust port of the Go `observer/impl/metrics_detector_bocpd.go`. It detects changes in a
//! metric's level by maintaining a run-length posterior over the observed points: at each new point the
//! posterior is updated with the standard Adams & MacKay (2007) recurrence, and an anomaly fires when
//! either the change-point probability `P(r_t = 0)` or the short-run posterior mass `P(r_t <= k)` crosses
//! its threshold.
//!
//! # Streaming state and cursors
//!
//! State is kept per `(series, aggregate)` and is private to the detector. Every advance processes only
//! the points that became visible since the previous one (tracked by a point-count cursor plus the store's
//! write generation, so same-bucket merges are not missed), except for activation: the first time a series
//! reaches the warmup count, the baseline is estimated and the warmup points are replayed through the
//! posterior so the state is identical to a full replay.
//!
//! # Faithfulness
//!
//! The algorithm is ported operation for operation, including its quirks: the `>=` trigger boundaries, the
//! high-to-low run-length update order, the discarded horizon hypothesis that still contributes to the
//! full-posterior normalization, the uniform fallback that uses the *untruncated* length, and the
//! `allowAlert` flag being frozen at the batch's visible count rather than per point. Do not "clean these
//! up": they are observable behavior.

use std::collections::HashMap;

use super::workload_series_refs;
use crate::identity::{Aggregate, QueryHandle, SeriesDescriptor, SeriesRef};
use crate::model::{Anomaly, AnomalyEvidence, AnomalyType, BocpdTrigger, Point, Series};
use crate::traits::{Detector, StorageView};

/// Default warmup length in points, from the Go `defaultBOCPDWarmupPoints`.
const DEFAULT_WARMUP_POINTS: usize = 60;

/// Default cap on tracked run-length hypotheses, from the Go `defaultBOCPDMaxRunLength`.
const DEFAULT_MAX_RUN_LENGTH: usize = 120;

/// Floor applied to a Gaussian PDF's variance, from the Go `gaussianPDF`.
const GAUSSIAN_MIN_VARIANCE: f64 = 1e-12;

/// Identifies one `(series, aggregate)` pair of BOCPD state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct BocpdStateKey {
    series_ref: SeriesRef,
    aggregate: Aggregate,
}

/// Per-series streaming BOCPD state.
///
/// The Go state also tracks `alertStart`, but nothing reads it; it is omitted rather than carried as
/// write-only state. The baseline fields are set once, after warmup.
#[derive(Debug, Default)]
struct BocpdSeriesState {
    /// Cursor tracking the last timestamp advanced by [`Detector::detect`].
    last_processed_time: i64,
    /// Point count visible at the last detect; used to skip advances with no new data.
    last_processed_count: usize,
    /// Write generation at the last detect; used to catch same-bucket merges.
    last_write_gen: u64,

    initialized: bool,

    baseline_mean: f64,
    baseline_stddev: f64,
    obs_var: f64,
    prior_mean: f64,
    prior_precision: f64,

    /// BOCPD posterior state, persisting across advances.
    run_probs: Vec<f64>,
    means: Vec<f64>,
    precisions: Vec<f64>,

    /// Alert lifecycle.
    in_alert: bool,
    /// Consecutive non-triggering points since the last trigger.
    recovery_count: usize,
}

/// Configuration for [`BocpdDetector`], mirroring the Go `BOCPDConfig`.
///
/// `Default` reproduces `DefaultBOCPDConfig`. Zero-valued fields are replaced with their defaults by
/// [`BocpdDetector::new`], exactly as `NewBOCPDDetector` does.
#[derive(Clone, Debug, PartialEq)]
pub struct BocpdConfig {
    /// Number of initial points used to estimate the baseline. Default: `60` (~1 minute at 1 Hz).
    ///
    /// A longer warmup captures more natural variability and reduces false positives. Values below 2 are
    /// replaced by the default because the sample variance needs Bessel's correction (`n - 1`).
    pub warmup_points: usize,

    /// Constant change-point hazard probability. Default: `0.05`. Must be in `(0, 1)`.
    pub hazard: f64,

    /// Posterior `P(change-point at t)` threshold at which an anomaly is emitted. Default: `0.6`.
    pub cp_threshold: f64,

    /// Run-length horizon `k` for the short-run posterior mass `P(r_t <= k)`. Default: `5`.
    pub short_run_length: usize,

    /// Threshold for the short-run posterior mass. Default: `0.7`.
    pub cp_mass_threshold: f64,

    /// Caps the tracked run-length hypotheses and the raw history. Must be at least
    /// [`BocpdConfig::warmup_points`]; when it is smaller the warmup value is used instead (the Go
    /// implementation logs a warning here, which this dependency-free crate drops). Default: `120`.
    pub max_run_length: usize,

    /// Prior variance over the mean, relative to the observed variance. Default: `10.0`.
    pub prior_variance_scale: f64,

    /// Floor for the observation variance. Default: `1.0`.
    ///
    /// Without it a constant warmup window would produce a pathologically sharp PDF that flags any tiny
    /// fluctuation as a change point.
    pub min_variance: f64,

    /// Consecutive non-triggering points required to leave alert state. Default: `10`.
    pub recovery_points: usize,

    /// Aggregates to run detection on. Default: `[Average, Count]`.
    pub aggregations: Vec<Aggregate>,
}

impl Default for BocpdConfig {
    /// Returns the production/testbench baseline configuration, matching `DefaultBOCPDConfig`.
    fn default() -> Self {
        Self {
            warmup_points: DEFAULT_WARMUP_POINTS,
            hazard: 0.05,
            cp_threshold: 0.6,
            short_run_length: 5,
            cp_mass_threshold: 0.7,
            max_run_length: DEFAULT_MAX_RUN_LENGTH,
            prior_variance_scale: 10.0,
            min_variance: 1.0,
            recovery_points: 10,
            aggregations: vec![Aggregate::Average, Aggregate::Count],
        }
    }
}

impl BocpdConfig {
    /// Returns the offline replay profile: the default configuration with a 40-point warmup.
    ///
    /// This matches the BOCPD entry of Go's `ApplyTestbenchDefaults`.
    pub fn testbench() -> Self {
        Self {
            warmup_points: 40,
            ..Self::default()
        }
    }

    /// Returns the configuration with zero-valued fields replaced by their defaults.
    ///
    /// Ports the normalization in the Go `NewBOCPDDetector`.
    fn normalized(mut self) -> Self {
        let defaults = Self::default();
        // Warmup needs at least 2 points for Bessel's correction (n-1 denominator).
        if self.warmup_points < 2 {
            self.warmup_points = defaults.warmup_points;
        }
        if self.hazard <= 0.0 || self.hazard >= 1.0 {
            self.hazard = defaults.hazard;
        }
        if self.cp_threshold <= 0.0 || self.cp_threshold >= 1.0 {
            self.cp_threshold = defaults.cp_threshold;
        }
        if self.short_run_length == 0 {
            self.short_run_length = defaults.short_run_length;
        }
        if self.cp_mass_threshold <= 0.0 || self.cp_mass_threshold >= 1.0 {
            self.cp_mass_threshold = defaults.cp_mass_threshold;
        }
        if self.max_run_length == 0 {
            self.max_run_length = defaults.max_run_length;
        }
        if self.max_run_length < self.warmup_points {
            // Go logs a warning here; this crate has no logging dependency.
            self.max_run_length = self.warmup_points;
        }
        if self.prior_variance_scale <= 0.0 {
            self.prior_variance_scale = defaults.prior_variance_scale;
        }
        if self.min_variance <= 0.0 {
            self.min_variance = defaults.min_variance;
        }
        if self.recovery_points == 0 {
            self.recovery_points = defaults.recovery_points;
        }
        if self.aggregations.is_empty() {
            self.aggregations = defaults.aggregations;
        }
        self
    }
}

/// Streaming Bayesian online change-point detector, mirroring the Go `BOCPDDetector`.
#[derive(Debug)]
pub struct BocpdDetector {
    config: BocpdConfig,
    ready: bool,
    /// Per-`(series, aggregate)` state.
    series: HashMap<BocpdStateKey, BocpdSeriesState>,
    /// Series refs discovered on the last listing; refreshed when the series generation changes.
    cached_refs: Option<Vec<SeriesRef>>,
    cached_gen: u64,
}

impl BocpdDetector {
    /// Creates a detector with the given configuration, filling zero-valued fields from
    /// [`BocpdConfig::default`].
    pub fn new(config: BocpdConfig) -> Self {
        Self {
            config: config.normalized(),
            ready: false,
            series: HashMap::new(),
            cached_refs: None,
            cached_gen: 0,
        }
    }

    /// Creates a detector with the default configuration.
    pub fn with_default_config() -> Self {
        Self::new(BocpdConfig::default())
    }
}

impl Detector for BocpdDetector {
    type Config = BocpdConfig;

    fn name(&self) -> &str {
        "bocpd"
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
            for aggregate in &self.config.aggregations {
                self.series.remove(&BocpdStateKey {
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

        let mut anomalies = Vec::new();
        let warmup_points = self.config.warmup_points;
        // `Aggregate` is `Copy` and the list holds one or two entries, so a clone is the cheapest way to
        // iterate it without holding a borrow of `self.config` across the state mutations below.
        let aggregations = self.config.aggregations.clone();
        // Temporarily take the listing so the loop can iterate the refs while mutating detector state.
        let refs = self.cached_refs.take().unwrap_or_default();

        for &series_ref in &refs {
            let visible_count = view.point_count_up_to(series_ref, data_time_sec);

            for &aggregate in &aggregations {
                if !view.supports_aggregate(series_ref, aggregate) {
                    continue;
                }
                let key = BocpdStateKey { series_ref, aggregate };

                if !self.series.contains_key(&key) {
                    if visible_count < warmup_points {
                        continue;
                    }
                    let mut state = BocpdSeriesState::default();
                    if initialize_from_storage(&self.config, view, series_ref, data_time_sec, aggregate, &mut state) {
                        self.ready = true;
                    }
                    // The state is inserted even when initialization did not complete, matching the Go
                    // detector (which relies on the visible-count guard above to make that unreachable).
                    self.series.insert(key, state);
                }

                let write_gen = view.write_generation(series_ref);
                let state = self.series.get_mut(&key).expect("state was just inserted");
                if visible_count <= state.last_processed_count && write_gen == state.last_write_gen {
                    continue;
                }

                let allow_alert = visible_count >= warmup_points;
                if let Some(series) =
                    view.get_series_range(series_ref, state.last_processed_time, data_time_sec, aggregate)
                {
                    for point in &series.points {
                        if let Some(anomaly) =
                            process_point(&self.config, state, point, &series, series_ref, aggregate, allow_alert)
                        {
                            anomalies.push(anomaly);
                        }
                        state.last_processed_time = point.second;
                    }
                }
                // Advance the cursors whether or not the range read found points.
                state.last_processed_count = visible_count;
                state.last_write_gen = write_gen;
            }
        }

        self.cached_refs = Some(refs);
        anomalies
    }
}

impl BocpdDetector {
    /// Returns the number of `(series, aggregate)` state entries currently held.
    ///
    /// This exists so integration code and tests can assert that eviction fan-out really tears state down.
    pub fn tracked_series_count(&self) -> usize {
        self.series.len()
    }
}

/// Initializes the baseline and replays the warmup samples through the posterior.
///
/// Ports the Go `initializeFromStorage`: a Welford pass estimates the mean and sample variance over the
/// first `warmup_points` visible points, the variance is floored at `min_variance`, the prior is seeded,
/// and the same warmup prefix is replayed so the posterior matches a full replay. Returns `true` when
/// initialization completed (fewer than `warmup_points` points leaves the state uninitialized).
fn initialize_from_storage(
    config: &BocpdConfig, view: &dyn StorageView, series_ref: SeriesRef, data_time_sec: i64, aggregate: Aggregate,
    state: &mut BocpdSeriesState,
) -> bool {
    let Some(series) = view.get_series_range(series_ref, 0, data_time_sec, aggregate) else {
        return false;
    };

    let mut count = 0usize;
    let mut mean = 0.0;
    let mut m2 = 0.0;
    for point in &series.points {
        if count >= config.warmup_points {
            break;
        }
        count += 1;
        let delta = point.value - mean;
        mean += delta / count as f64;
        m2 += delta * (point.value - mean);
        state.last_processed_time = point.second;
    }
    if count < config.warmup_points {
        return false;
    }

    let mut variance = m2 / (count - 1) as f64; // sample variance (Bessel's correction)
    let mut stddev = variance.sqrt();
    if variance < config.min_variance {
        variance = config.min_variance;
        stddev = variance.sqrt();
    }

    state.baseline_mean = mean;
    state.baseline_stddev = stddev;
    state.obs_var = variance;
    state.prior_mean = mean;
    state.prior_precision = 1.0 / (variance * config.prior_variance_scale);

    let buffer_size = config.max_run_length + 1;
    state.run_probs = Vec::with_capacity(buffer_size);
    state.means = Vec::with_capacity(buffer_size);
    state.precisions = Vec::with_capacity(buffer_size);
    state.run_probs.push(1.0);
    state.means.push(state.prior_mean);
    state.precisions.push(state.prior_precision);

    // Replay the retained warmup prefix so the posterior is seeded exactly as in a full replay.
    for point in &series.points[..count] {
        update_posterior(config, state, point.value);
    }

    state.initialized = true;
    true
}

/// Handles one new observation for a series, returning an anomaly on a new alert onset.
///
/// Ports the Go `processPoint`. `allow_alert` is the batch's readiness (`visible_count >= warmup`), frozen
/// for the whole advance, exactly as in Go.
fn process_point(
    config: &BocpdConfig, state: &mut BocpdSeriesState, point: &Point, series: &Series, series_ref: SeriesRef,
    aggregate: Aggregate, allow_alert: bool,
) -> Option<Anomaly> {
    if !state.initialized {
        return None;
    }
    let (triggered, cp_prob, short_run_mass) = update_posterior(config, state, point.value);
    if !allow_alert {
        state.in_alert = false;
        state.recovery_count = 0;
        return None;
    }

    if triggered {
        state.recovery_count = 0;
        if !state.in_alert {
            state.in_alert = true;
            return Some(make_anomaly(
                config,
                state,
                point,
                series,
                series_ref,
                aggregate,
                TriggerProbabilities {
                    change_point: cp_prob,
                    short_run_mass,
                },
            ));
        }
        return None;
    }

    if state.in_alert {
        state.recovery_count += 1;
        if state.recovery_count >= config.recovery_points {
            state.in_alert = false;
            state.recovery_count = 0;
        }
    }
    None
}

/// Performs one step of the BOCPD recurrence, returning `(triggered, cp_prob, short_run_mass)`.
///
/// Ports the Go `updatePosterior` operation for operation. See the module documentation for the
/// deliberately preserved quirks.
fn update_posterior(config: &BocpdConfig, state: &mut BocpdSeriesState, x: f64) -> (bool, f64, f64) {
    let hazard = config.hazard;

    let old_len = state.run_probs.len();
    let full_len = old_len + 1;
    let new_len = std::cmp::min(full_len, config.max_run_length + 1);
    // The Go code reslices only when growing; `resize` reproduces both the grow-by-one and the
    // already-at-horizon (len unchanged) cases.
    state.run_probs.resize(new_len, 0.0);
    state.means.resize(new_len, 0.0);
    state.precisions.resize(new_len, 0.0);

    // Update from high run lengths to low ones so each source hypothesis is read before its successor
    // overwrites the next slot. At the horizon, the final successor is deliberately not retained, but
    // remains part of the full posterior used for trigger probabilities below.
    let mut cp_mass = 0.0;
    let mut short_run_raw_mass = 0.0;
    let mut discarded_growth_prob = 0.0;
    for r in (0..old_len).rev() {
        let pred = gaussian_pdf(x, state.means[r], state.obs_var + 1.0 / state.precisions[r]);
        let growth_prob = state.run_probs[r] * (1.0 - hazard) * pred;
        if r + 1 < new_len {
            state.run_probs[r + 1] = growth_prob;
            let (mean, precision) = normal_posterior(state.means[r], state.precisions[r], x, state.obs_var);
            state.means[r + 1] = mean;
            state.precisions[r + 1] = precision;
        } else {
            discarded_growth_prob = growth_prob;
        }
        cp_mass += state.run_probs[r] * pred;
        // Go writes this as `r+1 <= ShortRunLength`, which is the same condition (`r < ShortRunLength`).
        if r < config.short_run_length {
            short_run_raw_mass += growth_prob;
        }
    }
    let cp_raw = hazard * cp_mass;
    state.run_probs[0] = cp_raw;
    let (mean, precision) = normal_posterior(state.prior_mean, state.prior_precision, x, state.obs_var);
    state.means[0] = mean;
    state.precisions[0] = precision;

    // Normalize first over the full posterior (including a discarded horizon tail), then normalize the
    // retained posterior after truncation. Trigger values intentionally use the full posterior.
    let mut full_total = cp_raw;
    full_total += state.run_probs[1..new_len].iter().sum::<f64>();
    // The raw tail is not resident, but was included in cp_mass above. It is the only full-posterior
    // probability missing from the retained slice.
    full_total += discarded_growth_prob;

    let (cp_prob, short_run_mass);
    if full_total <= 0.0 || full_total.is_nan() || full_total.is_infinite() {
        let uniform = 1.0 / full_len as f64;
        cp_prob = uniform;
        short_run_mass = std::cmp::min(config.short_run_length, full_len - 1) as f64 * uniform;
        for probability in state.run_probs.iter_mut() {
            *probability = uniform;
        }
    } else {
        cp_prob = cp_raw / full_total;
        short_run_mass = short_run_raw_mass / full_total;
        for probability in state.run_probs.iter_mut() {
            *probability /= full_total;
        }
    }
    normalize_probs(&mut state.run_probs);

    // Short-run mass is only meaningful when run-length hypotheses exist beyond the short-run window;
    // otherwise all mass is trivially "short".
    let triggered_by_peak = cp_prob >= config.cp_threshold;
    let triggered_by_shift =
        short_run_mass >= config.cp_mass_threshold && state.run_probs.len() > config.short_run_length + 1;
    (triggered_by_peak || triggered_by_shift, cp_prob, short_run_mass)
}

/// The trigger probabilities computed for one observation, carried together to keep [`make_anomaly`]'s
/// argument list manageable.
#[derive(Clone, Copy, Debug)]
struct TriggerProbabilities {
    /// Posterior `P(r_t = 0)`.
    change_point: f64,
    /// Short-run posterior mass `P(r_t <= k)`.
    short_run_mass: f64,
}

/// Constructs the anomaly for a new alert onset, mirroring the Go `makeAnomaly`.
fn make_anomaly(
    config: &BocpdConfig, state: &BocpdSeriesState, point: &Point, series: &Series, series_ref: SeriesRef,
    aggregate: Aggregate, probabilities: TriggerProbabilities,
) -> Anomaly {
    let deviation = (point.value - state.baseline_mean) / state.baseline_stddev;

    let (trigger, trigger_threshold) = if probabilities.change_point >= config.cp_threshold {
        (BocpdTrigger::ChangePointProbability, config.cp_threshold)
    } else {
        (BocpdTrigger::ShortRunMass, config.cp_mass_threshold)
    };

    Anomaly {
        anomaly_type: AnomalyType::Metric,
        series: SeriesDescriptor::new(
            series.namespace.clone(),
            series.name.clone(),
            series.host.clone(),
            series.tags.clone(),
            aggregate,
        ),
        series_ref: Some(QueryHandle::new(series_ref, aggregate)),
        detector_name: "bocpd".to_string(),
        context: None,
        timestamp_sec: point.second,
        // The Go detector leaves Score nil for BOCPD anomalies.
        score: None,
        sampling_interval_sec: 0,
        evidence: Some(AnomalyEvidence::Bocpd {
            baseline_mean: state.baseline_mean,
            baseline_stddev: state.baseline_stddev,
            threshold: trigger_threshold,
            current_value: point.value,
            deviation_sigma: deviation,
            trigger,
            change_point_prob: probabilities.change_point,
            short_run_mass: probabilities.short_run_mass,
            short_run_length: config.short_run_length,
        }),
    }
}

/// Returns the posterior mass over run lengths `1..=short_run_length`.
///
/// Ports the Go `shortRunLengthMass` helper. Its deliberately excluded index 0 is the change-point
/// probability, which is tested separately via [`BocpdConfig::cp_threshold`]; including it would make the
/// two trigger conditions non-independent. The trigger path computes its own short-run raw mass, so this
/// helper is kept for parity and unit coverage rather than called by [`update_posterior`].
pub fn short_run_length_mass(run_probs: &[f64], short_run_length: usize) -> f64 {
    if run_probs.is_empty() {
        return 0.0;
    }
    let max_index = std::cmp::min(short_run_length, run_probs.len() - 1);
    let mut mass = 0.0;
    for probability in &run_probs[1..=max_index] {
        mass += probability;
    }
    mass
}

/// Applies the Bayesian posterior update for a Normal-Inverse-Gamma observation, returning `(mean,
/// precision)`.
fn normal_posterior(prior_mean: f64, prior_precision: f64, x: f64, obs_var: f64) -> (f64, f64) {
    let obs_precision = 1.0 / obs_var;
    let precision = prior_precision + obs_precision;
    let mean = (prior_precision * prior_mean + obs_precision * x) / precision;
    (mean, precision)
}

/// Normalizes a probability vector in place, falling back to a uniform distribution when the total is not
/// a positive finite number.
fn normalize_probs(probs: &mut [f64]) {
    let total: f64 = probs.iter().sum();
    if total <= 0.0 || total.is_nan() || total.is_infinite() {
        let uniform = 1.0 / probs.len() as f64;
        for probability in probs.iter_mut() {
            *probability = uniform;
        }
        return;
    }
    for probability in probs.iter_mut() {
        *probability /= total;
    }
}

/// Evaluates the Gaussian probability density, flooring the variance like the Go `gaussianPDF`.
fn gaussian_pdf(x: f64, mean: f64, variance: f64) -> f64 {
    let variance = if variance < GAUSSIAN_MIN_VARIANCE {
        GAUSSIAN_MIN_VARIANCE
    } else {
        variance
    };
    let z = x - mean;
    let denominator = (2.0 * std::f64::consts::PI * variance).sqrt();
    (-(z * z) / (2.0 * variance)).exp() / denominator
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageConfig;
    use crate::storage::TimeSeriesStorage;

    /// Builds a store with point retention disabled, mirroring the Go `newDetectorTestStorage` helper.
    fn detector_storage() -> TimeSeriesStorage {
        TimeSeriesStorage::new(StorageConfig {
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

    /// The Go `testBOCPDDetector`: default config with a 20-point warmup.
    fn test_detector() -> BocpdDetector {
        BocpdDetector::new(BocpdConfig {
            warmup_points: 20,
            ..BocpdConfig::default()
        })
    }

    /// A detector pinned to the average aggregate so counts are deterministic.
    fn single_aggregate_detector(config: BocpdConfig) -> BocpdDetector {
        BocpdDetector::new(BocpdConfig {
            aggregations: vec![Aggregate::Average],
            ..config
        })
    }

    #[test]
    fn name_is_bocpd() {
        assert_eq!(BocpdDetector::with_default_config().name(), "bocpd");
    }

    #[test]
    fn ensures_warmup_fits_run_length() {
        // Ported from Go `TestBOCPDDetector_EnsuresWarmupFitsRunLength`.
        assert_eq!(BocpdConfig::default().max_run_length, 120);

        let config = BocpdConfig {
            warmup_points: 40,
            max_run_length: 20,
            ..BocpdConfig::default()
        };
        assert_eq!(BocpdDetector::new(config).config().max_run_length, 40);

        let config = BocpdConfig {
            warmup_points: 40,
            max_run_length: 0,
            ..BocpdConfig::default()
        };
        assert_eq!(BocpdDetector::new(config).config().max_run_length, 120);
    }

    #[test]
    fn not_enough_points_leaves_no_state() {
        // Ported from Go `TestBOCPDDetector_NotEnoughPoints`.
        let mut detector = test_detector();
        let mut storage = detector_storage();
        add(&mut storage, "test.metric", 100.0, 1);

        let anomalies = detector.detect(&storage, 1);
        assert!(anomalies.is_empty());
        assert_eq!(
            detector.tracked_series_count(),
            0,
            "cold series must not allocate state"
        );
        assert!(!detector.is_ready());
    }

    #[test]
    fn activation_survives_retention() {
        // Ported from Go `TestBOCPDDetector_ActivationSurvivesRetention`.
        let mut detector = single_aggregate_detector(BocpdConfig {
            warmup_points: 3,
            ..BocpdConfig::default()
        });
        let mut storage = detector_storage();

        add(&mut storage, "test.metric", 1.0, 1);
        add(&mut storage, "test.metric", 2.0, 2);
        detector.detect(&storage, 2);
        assert_eq!(detector.tracked_series_count(), 0);

        add(&mut storage, "test.metric", 3.0, 3);
        detector.detect(&storage, 3);
        let key = BocpdStateKey {
            series_ref: SeriesRef::new(0),
            aggregate: Aggregate::Average,
        };
        assert!(
            detector
                .series
                .get(&key)
                .expect("series activates at warmup")
                .initialized,
            "activation must replay retained warmup points"
        );
        assert!(detector.is_ready());

        // Drop retention below the warmup length: the active state must survive and keep advancing.
        storage.set_series_retention(SeriesRef::new(0), 1);
        add(&mut storage, "test.metric", 4.0, 4);
        detector.detect(&storage, 4);
        let state = detector.series.get(&key).expect("state survives retention");
        assert_eq!(
            state.last_processed_time, 4,
            "active state must continue after retention"
        );
    }

    #[test]
    fn stable_data_does_not_trigger() {
        // Ported from Go `TestBOCPDDetector_StableData`.
        let mut detector = test_detector();
        let mut storage = detector_storage();
        for i in 0..40i64 {
            add(&mut storage, "test.metric", 100.0 + ((i % 3) - 1) as f64, i + 1);
        }

        assert!(
            detector.detect(&storage, 40).is_empty(),
            "stable data should not trigger BOCPD"
        );
    }

    #[test]
    fn detects_upward_step_change() {
        // Ported from Go `TestBOCPDDetector_DetectsStepChange`.
        let mut detector = test_detector();
        let mut storage = detector_storage();
        for i in 0..20 {
            add(&mut storage, "test.metric", 100.0, i + 1);
        }
        for i in 20..40 {
            add(&mut storage, "test.metric", 140.0, i + 1);
        }

        let anomalies = detector.detect(&storage, 40);
        assert!(!anomalies.is_empty(), "should detect step change");
        let anomaly = &anomalies[0];
        assert_eq!(anomaly.detector_name, "bocpd");
        assert_eq!(anomaly.series.name, "test.metric");
        assert_eq!(anomaly.series.aggregate, Aggregate::Average);
        assert!(anomaly.timestamp_sec >= 21);
        match anomaly.evidence.as_ref().expect("bocpd evidence") {
            AnomalyEvidence::Bocpd {
                trigger, current_value, ..
            } => {
                // The Go test only asserts that the anomaly fired; either trigger condition is valid.
                assert_ne!(*trigger, BocpdTrigger::Unknown);
                assert_eq!(*current_value, 140.0);
            }
            other => panic!("unexpected evidence {other:?}"),
        }
    }

    #[test]
    fn detects_downward_step_change() {
        // Ported from Go `TestBOCPDDetector_DetectsDownwardStepChange`.
        let mut detector = test_detector();
        let mut storage = detector_storage();
        for i in 0..25 {
            add(&mut storage, "test.metric", 100.0, i + 1);
        }
        for i in 25..50 {
            add(&mut storage, "test.metric", 70.0, i + 1);
        }

        let anomalies = detector.detect(&storage, 50);
        assert!(!anomalies.is_empty(), "should detect downward step change");
        assert_eq!(anomalies[0].series.name, "test.metric");
    }

    #[test]
    fn detects_sustained_shift_via_short_run_mass() {
        // Ported from Go `TestBOCPDDetector_DetectsSustainedShiftViaShortRunMass`.
        let config = BocpdConfig {
            warmup_points: 20,
            cp_threshold: 0.99,
            cp_mass_threshold: 0.55,
            short_run_length: 6,
            ..BocpdConfig::default()
        };
        let mut detector = single_aggregate_detector(config);
        let mut storage = detector_storage();

        for i in 0..30i64 {
            add(&mut storage, "test.metric", 100.0 + ((i % 3) - 1) as f64 * 5.0, i + 1);
        }
        for i in 30..60 {
            add(&mut storage, "test.metric", 115.0, i + 1);
        }

        let anomalies = detector.detect(&storage, 60);
        assert!(!anomalies.is_empty(), "should detect sustained shift");
        match anomalies[0].evidence.as_ref().expect("bocpd evidence") {
            AnomalyEvidence::Bocpd { trigger, .. } => assert_eq!(*trigger, BocpdTrigger::ShortRunMass),
            other => panic!("unexpected evidence {other:?}"),
        }
    }

    #[test]
    fn sustained_incident_emits_once() {
        // Ported from Go `TestBOCPDDetector_SustainedIncidentEmitsOnce`.
        let mut detector = test_detector();
        let mut storage = detector_storage();
        for i in 0..20 {
            add(&mut storage, "test.metric", 100.0, i + 1);
        }
        for i in 20..60 {
            add(&mut storage, "test.metric", 200.0, i + 1);
        }

        let anomalies = detector.detect(&storage, 60);
        let matching = anomalies
            .iter()
            .filter(|anomaly| anomaly.series.name == "test.metric" && anomaly.series.aggregate == Aggregate::Average)
            .count();
        assert_eq!(
            matching, 1,
            "sustained incident should emit exactly one anomaly per series/agg"
        );
    }

    #[test]
    fn incremental_advance() {
        // Ported from Go `TestBOCPDDetector_IncrementalAdvance`.
        let mut detector = test_detector();
        let mut storage = detector_storage();
        for i in 0..20 {
            add(&mut storage, "test.metric", 100.0, i + 1);
        }
        assert!(detector.detect(&storage, 20).is_empty(), "no anomaly in stable data");

        for i in 20..30 {
            add(&mut storage, "test.metric", 200.0, i + 1);
        }
        assert!(
            !detector.detect(&storage, 30).is_empty(),
            "should detect step change on second advance"
        );

        assert!(
            detector.detect(&storage, 30).is_empty(),
            "no new data should produce no anomalies"
        );
    }

    #[test]
    fn recovery_and_re_alert() {
        // Ported from Go `TestBOCPDDetector_RecoveryAndReAlert`.
        let mut detector = single_aggregate_detector(BocpdConfig {
            warmup_points: 20,
            recovery_points: 5,
            ..BocpdConfig::default()
        });
        let mut storage = detector_storage();
        let mut timestamp = 0i64;
        let add_n = |storage: &mut TimeSeriesStorage, timestamp: &mut i64, count: usize, value: f64| {
            for _ in 0..count {
                *timestamp += 1;
                add(storage, "m", value, *timestamp);
            }
        };

        add_n(&mut storage, &mut timestamp, 25, 100.0);
        assert!(detector.detect(&storage, timestamp).is_empty());

        add_n(&mut storage, &mut timestamp, 10, 300.0);
        assert!(
            !detector.detect(&storage, timestamp).is_empty(),
            "should detect first incident"
        );

        add_n(&mut storage, &mut timestamp, 20, 100.0);
        let recovery = detector.detect(&storage, timestamp);

        add_n(&mut storage, &mut timestamp, 10, 300.0);
        let second = detector.detect(&storage, timestamp);

        let count = recovery
            .iter()
            .chain(second.iter())
            .filter(|anomaly| anomaly.series.name == "m" && anomaly.series.aggregate == Aggregate::Average)
            .count();
        assert!(count >= 1, "should detect second incident after recovery");
    }

    #[test]
    fn reset_clears_state() {
        // Ported from Go `TestBOCPDDetector_Reset`.
        let mut detector = test_detector();
        let mut storage = detector_storage();
        for i in 0..30 {
            add(&mut storage, "test.metric", 100.0, i + 1);
        }
        detector.detect(&storage, 30);
        assert!(detector.tracked_series_count() > 0, "should have state after detection");
        assert!(detector.is_ready());

        detector.reset();
        assert_eq!(detector.tracked_series_count(), 0, "reset should clear all state");
        assert!(detector.cached_refs.is_none(), "reset should clear cached refs");
        assert!(!detector.is_ready());
    }

    #[test]
    fn deterministic_replay() {
        // Ported from Go `TestBOCPDDetector_DeterministicReplay`.
        let storage = {
            let mut storage = detector_storage();
            for i in 0..20 {
                add(&mut storage, "m", 100.0, i + 1);
            }
            for i in 20..40 {
                add(&mut storage, "m", 200.0, i + 1);
            }
            storage
        };

        let mut first = test_detector();
        let first_anomalies = first.detect(&storage, 40);
        let mut second = test_detector();
        let second_anomalies = second.detect(&storage, 40);

        assert_eq!(
            first_anomalies.len(),
            second_anomalies.len(),
            "replay should produce same count"
        );
        for (first, second) in first_anomalies.iter().zip(second_anomalies.iter()) {
            assert_eq!(first.timestamp_sec, second.timestamp_sec);
            assert_eq!(first.series, second.series);
            assert_eq!(first.evidence, second.evidence);
        }
    }

    #[test]
    fn default_config_matches_go() {
        // Ported from Go `TestBOCPDDetector_DefaultAggregations`,
        // `TestBOCPDDetector_DefaultWarmup60`, and `TestBOCPDConfig_DefaultMinVarianceIsPositive`.
        let config = BocpdConfig::default();
        assert_eq!(config.aggregations, vec![Aggregate::Average, Aggregate::Count]);
        assert_eq!(config.warmup_points, 60, "default warmup should be 60 points");
        assert!(config.min_variance > 0.0, "default MinVariance should be positive");
        assert_eq!(config.max_run_length, 120);
        assert_eq!(config.hazard, 0.05);
        assert_eq!(config.recovery_points, 10);
    }

    #[test]
    fn testbench_profile_uses_forty_point_warmup() {
        // Ports the BOCPD entry of Go `ApplyTestbenchDefaults`.
        assert_eq!(BocpdConfig::testbench().warmup_points, 40);
        assert_eq!(BocpdConfig::testbench().max_run_length, 120);
    }

    /// Ports Go `TestFindingH3_CPProbUsesOnlyPriorPredictiveNotSumOverRunLengths`: the change-point
    /// probability must come from the standard recurrence (summed over every run-length hypothesis), not
    /// from the prior predictive alone.
    #[test]
    fn finding_h3_cp_prob_uses_standard_recurrence() {
        let config = BocpdConfig {
            warmup_points: 120,
            prior_variance_scale: 100.0,
            ..BocpdConfig::default()
        };
        let mut detector = single_aggregate_detector(config);
        let mut storage = detector_storage();
        for i in 0..120 {
            add(&mut storage, "metric", 10.0, i + 1);
        }
        for i in 120..270 {
            add(&mut storage, "metric", 12.0, i + 1);
        }
        detector.detect(&storage, 270);

        let key = BocpdStateKey {
            series_ref: SeriesRef::new(0),
            aggregate: Aggregate::Average,
        };
        let state = detector.series.get_mut(&key).expect("state exists");
        assert!(state.initialized);

        let x = 14.0;
        let hazard = detector.config.hazard;
        let snapshot_run_probs = state.run_probs.clone();
        let snapshot_means = state.means.clone();
        let snapshot_precisions = state.precisions.clone();
        let obs_var = state.obs_var;
        let prior_mean = state.prior_mean;
        let prior_precision = state.prior_precision;

        let (_, implementation_cp_prob, _) = update_posterior(&detector.config, state, x);

        // Independently compute the standard BOCPD formula from the snapshot.
        let new_len = snapshot_run_probs.len() + 1;
        let mut standard_probs = vec![0.0; new_len];
        let mut cp_mass = 0.0;
        for r in 0..snapshot_run_probs.len() {
            let pred = gaussian_pdf(x, snapshot_means[r], obs_var + 1.0 / snapshot_precisions[r]);
            standard_probs[r + 1] = snapshot_run_probs[r] * (1.0 - hazard) * pred;
            cp_mass += snapshot_run_probs[r] * pred;
        }
        standard_probs[0] = hazard * cp_mass;
        normalize_probs(&mut standard_probs);
        let expected_cp_prob = standard_probs[0];

        // Independently compute the prior-only formula from the snapshot.
        let mut prior_probs = vec![0.0; new_len];
        let pred_prior = gaussian_pdf(x, prior_mean, obs_var + 1.0 / prior_precision);
        for r in 0..snapshot_run_probs.len() {
            let pred = gaussian_pdf(x, snapshot_means[r], obs_var + 1.0 / snapshot_precisions[r]);
            prior_probs[r + 1] = snapshot_run_probs[r] * (1.0 - hazard) * pred;
        }
        prior_probs[0] = hazard * pred_prior;
        normalize_probs(&mut prior_probs);
        let prior_only_cp_prob = prior_probs[0];

        assert!(
            (expected_cp_prob - prior_only_cp_prob).abs() > 1e-6,
            "test setup: standard and prior-only formulas should differ"
        );
        assert!(
            (expected_cp_prob - implementation_cp_prob).abs() <= 1e-10,
            "implementation cpProb {implementation_cp_prob} should match the standard recurrence {expected_cp_prob}"
        );
    }

    /// Ports Go `TestFindingM6_BOCPDSkipsSameBucketValueMerges`: a same-bucket merge must advance the
    /// write generation the detector stored, even though the visible point count does not change.
    #[test]
    fn finding_m6_same_bucket_merge_advances_write_generation() {
        let mut detector = single_aggregate_detector(BocpdConfig {
            warmup_points: 5,
            ..BocpdConfig::default()
        });
        let mut storage = detector_storage();
        for i in 1..=4 {
            add(&mut storage, "metric", 100.0, i);
        }
        add(&mut storage, "metric", 100.0, 5);
        detector.detect(&storage, 5);

        let series_ref = SeriesRef::new(0);
        let key = BocpdStateKey {
            series_ref,
            aggregate: Aggregate::Average,
        };
        let gen_before = detector.series.get(&key).expect("state exists").last_write_gen;

        // A second value at the same timestamp merges into the bucket: the average becomes 150.
        add(&mut storage, "metric", 200.0, 5);
        let merged = storage
            .get_series_range(series_ref, 4, 5, Aggregate::Average)
            .expect("series is live");
        assert_eq!(merged.points.len(), 1);
        assert_eq!(
            merged.points[0].value, 150.0,
            "storage should have merged the two values"
        );

        detector.detect(&storage, 5);
        let gen_after = detector.series.get(&key).expect("state exists").last_write_gen;
        assert!(
            gen_after > gen_before,
            "detector should re-process when a same-bucket merge changes the value ({gen_before} == {gen_after})"
        );
    }

    /// Ports Go `TestFindingM7_WarmupPointsOneCausesNaN`: a warmup of one is replaced by the default, so
    /// no state and no NaN is produced.
    #[test]
    fn finding_m7_warmup_one_is_guarded() {
        let mut detector = single_aggregate_detector(BocpdConfig {
            warmup_points: 1,
            ..BocpdConfig::default()
        });
        assert_eq!(
            detector.config().warmup_points,
            60,
            "warmup of one must fall back to the default"
        );

        let mut storage = detector_storage();
        for i in 0..10 {
            add(&mut storage, "metric", 100.0 + i as f64, i + 1);
        }
        let anomalies = detector.detect(&storage, 10);
        for anomaly in &anomalies {
            if let Some(AnomalyEvidence::Bocpd {
                baseline_mean,
                baseline_stddev,
                current_value,
                deviation_sigma,
                ..
            }) = &anomaly.evidence
            {
                assert!(!baseline_mean.is_nan());
                assert!(!baseline_stddev.is_nan());
                assert!(!current_value.is_nan());
                assert!(!deviation_sigma.is_nan());
            }
        }
        assert_eq!(
            detector.tracked_series_count(),
            0,
            "below warmup there is nothing to corrupt"
        );
    }

    /// Ports Go `TestBOCPDDebugInfoRetainsTriggerEvidence`.
    #[test]
    fn anomaly_evidence_retains_trigger_details() {
        let detector = BocpdDetector::with_default_config();
        let state = BocpdSeriesState {
            baseline_mean: 10.0,
            baseline_stddev: 2.0,
            ..BocpdSeriesState::default()
        };
        let point = Point {
            second: 42,
            value: 16.0,
        };
        let series = Series {
            namespace: "ns".into(),
            name: "metric".to_string(),
            host: None,
            tags: Vec::new(),
            points: Vec::new(),
        };

        let short_run = make_anomaly(
            &detector.config,
            &state,
            &point,
            &series,
            SeriesRef::new(0),
            Aggregate::Average,
            TriggerProbabilities {
                change_point: 0.2,
                short_run_mass: 0.7,
            },
        );
        assert_eq!(short_run.timestamp_sec, 42);
        match short_run.evidence.as_ref().expect("evidence") {
            AnomalyEvidence::Bocpd {
                trigger,
                change_point_prob,
                short_run_mass,
                short_run_length,
                threshold,
                deviation_sigma,
                ..
            } => {
                assert_eq!(*trigger, BocpdTrigger::ShortRunMass);
                assert_eq!(*change_point_prob, 0.2);
                assert_eq!(*short_run_mass, 0.7);
                assert_eq!(*short_run_length, detector.config.short_run_length);
                assert_eq!(*threshold, detector.config.cp_mass_threshold);
                assert_eq!(*deviation_sigma, 3.0);
            }
            other => panic!("unexpected evidence {other:?}"),
        }

        let change_point = make_anomaly(
            &detector.config,
            &state,
            &point,
            &series,
            SeriesRef::new(0),
            Aggregate::Average,
            TriggerProbabilities {
                change_point: detector.config.cp_threshold,
                short_run_mass: 0.7,
            },
        );
        match change_point.evidence.as_ref().expect("evidence") {
            AnomalyEvidence::Bocpd { trigger, threshold, .. } => {
                assert_eq!(*trigger, BocpdTrigger::ChangePointProbability);
                assert_eq!(*threshold, detector.config.cp_threshold);
            }
            other => panic!("unexpected evidence {other:?}"),
        }
    }

    /// Ports Go `TestFindingM8_ShortRunMassExcludesCPProb`.
    #[test]
    fn finding_m8_short_run_mass_excludes_cp_prob() {
        let mut run_probs = vec![0.0; 20];
        run_probs[0] = 0.55; // cpProb
        run_probs[1] = 0.05;
        for probability in run_probs.iter_mut().take(6).skip(2) {
            *probability = 0.04;
        }
        let remaining = 1.0 - (0.55 + 0.05 + 0.04 * 4.0);
        let tail_count = run_probs.len() - 6;
        for probability in run_probs.iter_mut().skip(6) {
            *probability = remaining / tail_count as f64;
        }

        let mass = short_run_length_mass(&run_probs, 5);
        let expected = 0.05 + 0.04 * 4.0;
        assert!(
            (mass - expected).abs() <= 0.001,
            "short_run_length_mass should exclude runProbs[0] (cpProb): got {mass}, expected {expected}"
        );
        assert!(
            mass < 0.7,
            "short-run mass without cpProb should be below CPMassThreshold"
        );
    }

    #[test]
    fn remove_series_tears_down_state() {
        let mut detector = test_detector();
        let mut storage = detector_storage();
        for i in 0..30 {
            add(&mut storage, "test.metric", 100.0, i + 1);
        }
        detector.detect(&storage, 30);
        assert!(detector.tracked_series_count() > 0);

        detector.remove_series(&[SeriesRef::new(0)]);
        assert_eq!(detector.tracked_series_count(), 0, "removal must drop per-series state");
        assert!(
            detector.cached_refs.is_none(),
            "removal must invalidate the cached listing"
        );

        // Removing a ref the detector never observed is a no-op, not a panic.
        detector.remove_series(&[SeriesRef::new(999)]);
        assert_eq!(detector.tracked_series_count(), 0);
    }

    #[test]
    fn unsupported_aggregates_are_skipped() {
        let mut detector = BocpdDetector::with_default_config();
        let mut storage = detector_storage();
        for i in 0..80 {
            add(&mut storage, "metric", 100.0, i + 1);
        }
        // Only the average aggregate is meaningful for this series.
        storage.set_supported_aggregations(SeriesRef::new(0), &[Aggregate::Average]);

        detector.detect(&storage, 80);
        let key_count = detector.tracked_series_count();
        assert_eq!(key_count, 1, "the unsupported count aggregate must be skipped");
    }
}
