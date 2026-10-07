//! Typed configuration with the Agent's defaults.
//!
//! The values here are ported from the Go observer so the Rust core starts from exactly the same
//! behavior. Every field documents its default and the Go source it came from.
//!
//! Two profiles are provided for scoring: [`AnomalyScorerConfig::default`] is the production profile, and
//! [`AnomalyScorerConfig::testbench`] is the offline replay profile (episodes enabled, cooldown disabled),
//! mirroring the Go `ApplyTestbenchDefaults`.

use std::collections::BTreeMap;

/// Seconds between consecutive detector point windows, from `observer.go`.
pub const STORAGE_POINT_INTERVAL_SECS: i64 = 15;

/// Extra seconds of retention kept beyond the detector-derived window, from `observer.go`.
pub const STORAGE_RETENTION_PAD_SECS: i64 = 16;

/// Configuration for the historical time-series store.
///
/// This is a port of the Go `observer/impl/storage.go` `StorageConfig` and
/// [`StorageConfig::default`] reproduces `DefaultStorageConfig`.
#[derive(Clone, Debug, PartialEq)]
pub struct StorageConfig {
    /// Maximum number of live series before capacity eviction fires. Default: `50_000`.
    ///
    /// When exceeded, the store evicts down to `max_series * eviction_floor_ratio`. Set high for offline
    /// replay where the whole scenario must remain resident. `0` disables the cap.
    pub max_series: usize,

    /// Fraction of `max_series` the store drains to on capacity eviction. Default: `0.5`.
    ///
    /// The value must be in `(0, 1]`; it controls how much headroom is created per eviction pass. Values
    /// near `1.0` evict a single series at a time and scan more often.
    pub eviction_floor_ratio: f64,

    /// How long a point is retained per series, in seconds. Default: `120`.
    ///
    /// Points older than `latest_timestamp - point_retention_secs` are trimmed on each write. `0` disables
    /// trimming, which is what the retained/replay path uses.
    pub point_retention_secs: i64,

    /// Maximum number of processable points kept per series. Default: `0` (unbounded).
    ///
    /// The live Agent derives this from the enabled detectors (see [`StorageConfig::from_detector_windows`]).
    /// Storage keeps one additional pending scheduler bucket beyond this cap.
    pub max_points_per_series: usize,

    /// How long a non-telemetry series may stay inactive before eviction, in seconds. Default: `300` (5 min).
    ///
    /// Inactivity is measured against advance timestamps, not wall clock. `0` disables inactivity eviction.
    pub inactive_series_ttl_secs: i64,

    /// Minimum advance-time gap between inactivity scans, in seconds. Default: `300` (5 min).
    ///
    /// Bounding the scan frequency keeps eviction deterministic under replay. `0` disables inactivity
    /// eviction entirely.
    pub inactive_series_check_interval_secs: i64,
}

impl Default for StorageConfig {
    /// Returns the production defaults from the Go `DefaultStorageConfig`.
    fn default() -> Self {
        Self {
            max_series: 50_000,
            eviction_floor_ratio: 0.5,
            point_retention_secs: 120,
            max_points_per_series: 0,
            inactive_series_ttl_secs: 300,
            inactive_series_check_interval_secs: 300,
        }
    }
}

impl StorageConfig {
    /// Derives the live-Agent storage config from the widest enabled detector window.
    ///
    /// Ports `observer.go`'s `storageConfigFromAgentConfig`: the processable-point cap becomes `max_points`
    /// and retention becomes `max_points * 15s + 16s`, so a detector that needs `120` points gets `1816`
    /// seconds of history. All other fields keep their [`StorageConfig::default`] values.
    pub fn from_detector_windows(max_points: usize) -> Self {
        Self {
            max_points_per_series: max_points,
            point_retention_secs: max_points as i64 * STORAGE_POINT_INTERVAL_SECS + STORAGE_RETENTION_PAD_SECS,
            ..Self::default()
        }
    }
}

/// Configuration for the anomaly scorer.
///
/// This merges the Go def-level EWMA parameters (`observer/def/types.go` `AnomalyScorerConfig`) with the
/// impl-level output toggles (`observer/impl/anomaly_scorer.go` `AnomalyScorerConfig`).
#[derive(Clone, Debug, PartialEq)]
pub struct AnomalyScorerConfig {
    /// EWMA smoothing factor. Default: `0.014`.
    ///
    /// Must satisfy `0 < alpha <= 1`. Lower values make the score curve smoother and slower to react.
    pub alpha: f64,

    /// Saturation constant `k`, where `saturation = 1 - exp(-count / k)`. Default: `5.0`.
    ///
    /// Calibrated against the number of unique anomalous series in the window, not the per-second event
    /// count.
    pub saturation_k: f64,

    /// Seconds a series stays in the active deduplication window. Default: `15`.
    ///
    /// A series seen at time `t` expires after `t + window_secs`. The saturation function is applied to the
    /// number of unique series in the window.
    pub window_secs: i64,

    /// EWMA level defining the Low/Medium severity boundary. Default: `0.15`.
    pub low_threshold: f64,

    /// EWMA level defining the Medium/High severity boundary. Default: `0.40`.
    ///
    /// Also the base for the hysteresis margin (`margin_pct * high_threshold`).
    pub high_threshold: f64,

    /// Hysteresis margin as a fraction of `high_threshold`. Default: `0.20`.
    ///
    /// The effective margin is `high_threshold * margin_pct`; see
    /// [`AnomalyScorerConfig::effective_margin`].
    pub margin_pct: f64,

    /// Per-detector score-to-level boundaries. Each entry is `[low, medium, high, xhigh]`.
    ///
    /// Detectors absent from this map default to level 2 (Medium) regardless of their score. The default
    /// map is calibrated empirically (see the Go `DefaultAnomalyScorerConfig`), and uses a `BTreeMap` so
    /// iteration order is deterministic for diagnostics and export.
    pub detector_thresholds: BTreeMap<String, [f64; 4]>,

    /// Whether severity transitions are logged. Default: `false`.
    pub logs: bool,

    /// Whether scorer severity episodes are tracked. Default: `false` in production.
    ///
    /// Set to `true` by [`AnomalyScorerConfig::testbench`], matching `ApplyTestbenchDefaults`.
    pub correlation_events: bool,

    /// The lowest severity that opens an episode: `"medium"` or `"high"`. Default: `"high"`.
    ///
    /// `"low"` is invalid because Low is the scorer's no-evidence baseline. The testbench profile keeps the
    /// default.
    pub correlation_event_threshold: String,

    /// Minimum interval between de-escalation callbacks, in seconds. Default: `300`.
    ///
    /// The testbench profile sets this to `0`. Must be `>= 0`.
    pub cooldown_secs: i64,

    /// Cap on the number of anomalies stored per episode. Default: `50`. `0` means no cap.
    pub max_episode_anomalies: usize,

    /// Number of metrics shown in a scorer episode event. Default: `16`.
    ///
    /// The Go reader clamps this to `[10, 1000]`; the top-anomaly buffer retains ten times this count.
    pub max_reported_items: usize,

    /// Cap on the number of [`crate::scorer::AnomalyScoreBucket`] entries retained in
    /// [`crate::scorer::AnomalyScorer::score_state`]. Default: `0`.
    ///
    /// `0` means "cap at [`Self::window_secs`]", which is the live-Agent behavior; a large positive value
    /// (for example `i64::MAX`) keeps an unlimited history for offline replay.
    pub max_buckets: i64,
}

impl AnomalyScorerConfig {
    /// Returns the production defaults from the Go `DefaultAnomalyScorerConfig`.
    pub fn production() -> Self {
        let mut detector_thresholds = BTreeMap::new();
        // tukey_biweight scores cap hard at ~50 across all scenarios.
        detector_thresholds.insert("tukey_biweight".to_string(), [5.0, 8.0, 15.0, 30.0]);
        // holt_residual can reach 400+, but 99% stay below ~75.
        detector_thresholds.insert("holt_residual".to_string(), [6.0, 12.0, 20.0, 35.0]);
        // scanmw / scanwelch scores are -log10(p-value), floored at 8.0.
        detector_thresholds.insert("scanmw".to_string(), [8.0, 10.0, 15.0, 25.0]);
        detector_thresholds.insert("scanwelch".to_string(), [8.0, 10.0, 15.0, 25.0]);

        Self {
            alpha: 0.014,
            saturation_k: 5.0,
            window_secs: 15,
            low_threshold: 0.15,
            high_threshold: 0.40,
            margin_pct: 0.20,
            detector_thresholds,
            logs: false,
            correlation_events: false,
            correlation_event_threshold: "high".to_string(),
            cooldown_secs: 300,
            max_episode_anomalies: 50,
            max_reported_items: 16,
            max_buckets: 0,
        }
    }

    /// Returns the offline replay profile from the Go `ApplyTestbenchDefaults`.
    ///
    /// This enables scorer episodes and disables cooldown. It keeps all other production values.
    pub fn testbench() -> Self {
        Self {
            correlation_events: true,
            cooldown_secs: 0,
            ..Self::production()
        }
    }

    /// Returns the hysteresis margin in score units (`high_threshold * margin_pct`).
    pub fn effective_margin(&self) -> f64 {
        self.high_threshold * self.margin_pct
    }
}

impl Default for AnomalyScorerConfig {
    /// Returns the production defaults.
    fn default() -> Self {
        Self::production()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_defaults_match_go() {
        let config = StorageConfig::default();
        assert_eq!(config.max_series, 50_000);
        assert_eq!(config.eviction_floor_ratio, 0.5);
        assert_eq!(config.point_retention_secs, 120);
        assert_eq!(config.max_points_per_series, 0);
        assert_eq!(config.inactive_series_ttl_secs, 300);
        assert_eq!(config.inactive_series_check_interval_secs, 300);
    }

    #[test]
    fn storage_from_detector_windows_matches_live_agent_derivation() {
        let config = StorageConfig::from_detector_windows(120);
        assert_eq!(config.max_points_per_series, 120);
        // 120 * 15s + 16s == 1816s, as the live Agent derives from BOCPD's 120-point window.
        assert_eq!(config.point_retention_secs, 1816);
        // Non-derived fields keep their defaults.
        assert_eq!(config.max_series, 50_000);
        assert_eq!(config.inactive_series_ttl_secs, 300);
    }

    #[test]
    fn scorer_defaults_match_go() {
        let config = AnomalyScorerConfig::default();
        assert_eq!(config.alpha, 0.014);
        assert_eq!(config.saturation_k, 5.0);
        assert_eq!(config.window_secs, 15);
        assert_eq!(config.low_threshold, 0.15);
        assert_eq!(config.high_threshold, 0.40);
        assert_eq!(config.margin_pct, 0.20);
        assert!(!config.correlation_events);
        assert_eq!(config.correlation_event_threshold, "high");
        assert_eq!(config.cooldown_secs, 300);
        assert_eq!(config.max_episode_anomalies, 50);
        assert_eq!(config.max_reported_items, 16);
        assert_eq!(config.max_buckets, 0);
        assert!((config.effective_margin() - 0.08).abs() < 1e-12);
    }

    #[test]
    fn scorer_detector_thresholds_match_go() {
        let config = AnomalyScorerConfig::default();
        assert_eq!(
            config.detector_thresholds.get("tukey_biweight"),
            Some(&[5.0, 8.0, 15.0, 30.0])
        );
        assert_eq!(
            config.detector_thresholds.get("holt_residual"),
            Some(&[6.0, 12.0, 20.0, 35.0])
        );
        assert_eq!(config.detector_thresholds.get("scanmw"), Some(&[8.0, 10.0, 15.0, 25.0]));
        assert_eq!(
            config.detector_thresholds.get("scanwelch"),
            Some(&[8.0, 10.0, 15.0, 25.0])
        );
        assert_eq!(config.detector_thresholds.len(), 4);
    }

    #[test]
    fn scorer_testbench_profile_enables_episodes_and_disables_cooldown() {
        let config = AnomalyScorerConfig::testbench();
        assert!(config.correlation_events);
        assert_eq!(config.cooldown_secs, 0);
        // All other values stay at production defaults.
        assert_eq!(config.alpha, 0.014);
        assert_eq!(config.max_reported_items, 16);
    }
}
