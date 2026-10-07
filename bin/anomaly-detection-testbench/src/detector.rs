//! Detector construction seam.
//!
//! The replay engine needs concrete detectors, so the testbench constructs them through a
//! [`DetectorFactory`]: the default binary uses [`BuiltinDetectorFactory`], which builds the five
//! migrated metric detectors from their resolved parameter objects, and tests inject a deterministic
//! [`StubDetector`] so the CLI and export paths can be exercised with a trivial detector.
//!
//! A factory returns `Ok(None)` for a detector this build has no implementation for; the replay
//! records it in [`crate::replay::ReplayResult::unlinked_detectors`] and the CLI warns about it,
//! rather than silently dropping the detector.
//!
//! Parameter mapping mirrors the Go catalog's `parseJSON`/factory split precisely:
//!
//! * `bocpd`, `holt_residual`, and `tukey_biweight` overlay every recognised key onto their
//!   production-default config.
//! * `scanmw`/`scanwelch` read only `min_points` and `max_points`; the Go catalog factory copies
//!   exactly those two fields out of the parsed struct, so every other key is inert.
//!
//! Unknown keys are ignored, exactly as `json.Unmarshal` ignores JSON keys with no matching field.

use saluki_anomaly_detection::detectors::bocpd::{BocpdConfig, BocpdDetector};
use saluki_anomaly_detection::detectors::holt_residual::{HoltResidualConfig, HoltResidualDetector};
use saluki_anomaly_detection::detectors::scanmw::{ScanMwConfig, ScanMwDetector};
use saluki_anomaly_detection::detectors::scanwelch::{ScanWelchConfig, ScanWelchDetector};
use saluki_anomaly_detection::detectors::tukey_biweight::{TukeyBiweightConfig, TukeyBiweightDetector};
use saluki_anomaly_detection::engine::AnyDetector;
use serde_json::Value;

/// Builds detectors by name from their resolved parameters.
pub trait DetectorFactory {
    /// Creates a detector for `name`.
    ///
    /// Returns `Ok(Some(detector))` when the detector is available, `Ok(None)` when this build has no
    /// implementation for it, and `Err(message)` when the parameters are unusable.
    fn create(&self, name: &str, params: Option<&Value>) -> Result<Option<Box<dyn AnyDetector>>, String>;
}

/// The factory used by the real binary: builds each migrated metric detector from its parameters.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinDetectorFactory;

impl DetectorFactory for BuiltinDetectorFactory {
    fn create(&self, name: &str, params: Option<&Value>) -> Result<Option<Box<dyn AnyDetector>>, String> {
        let object = params.and_then(Value::as_object);
        let detector: Option<Box<dyn AnyDetector>> = match name {
            "bocpd" => Some(Box::new(BocpdDetector::new(bocpd_config(object)))),
            "scanmw" => Some(Box::new(ScanMwDetector::with_config(scan_mw_config(object)))),
            "scanwelch" => Some(Box::new(ScanWelchDetector::with_config(scan_welch_config(object)))),
            "holt_residual" => Some(Box::new(HoltResidualDetector::with_config(holt_config(object)))),
            "tukey_biweight" => Some(Box::new(TukeyBiweightDetector::new(tukey_config(object)))),
            _ => None,
        };
        Ok(detector)
    }
}

type Params<'a> = Option<&'a serde_json::Map<String, Value>>;

fn f64_param(params: Params<'_>, key: &str) -> Option<f64> {
    params?.get(key).and_then(Value::as_f64)
}

fn i64_param(params: Params<'_>, key: &str) -> Option<i64> {
    params?.get(key).and_then(Value::as_i64)
}

fn usize_param(params: Params<'_>, key: &str) -> Option<usize> {
    params?.get(key).and_then(Value::as_u64).map(|value| value as usize)
}

fn bocpd_config(params: Params<'_>) -> BocpdConfig {
    let mut config = BocpdConfig::default();
    if let Some(value) = usize_param(params, "warmup_points") {
        config.warmup_points = value;
    }
    if let Some(value) = f64_param(params, "hazard") {
        config.hazard = value;
    }
    if let Some(value) = f64_param(params, "cp_threshold") {
        config.cp_threshold = value;
    }
    if let Some(value) = usize_param(params, "short_run_length") {
        config.short_run_length = value;
    }
    if let Some(value) = f64_param(params, "cp_mass_threshold") {
        config.cp_mass_threshold = value;
    }
    if let Some(value) = usize_param(params, "max_run_length") {
        config.max_run_length = value;
    }
    if let Some(value) = f64_param(params, "prior_variance_scale") {
        config.prior_variance_scale = value;
    }
    if let Some(value) = f64_param(params, "min_variance") {
        config.min_variance = value;
    }
    if let Some(value) = usize_param(params, "recovery_points") {
        config.recovery_points = value;
    }
    config
}

fn scan_mw_config(params: Params<'_>) -> ScanMwConfig {
    let mut config = ScanMwConfig::default();
    if let Some(value) = i64_param(params, "min_points") {
        config.min_points = value as i32;
    }
    if let Some(value) = i64_param(params, "max_points") {
        config.max_points = value as i32;
    }
    config
}

fn scan_welch_config(params: Params<'_>) -> ScanWelchConfig {
    let mut config = ScanWelchConfig::default();
    if let Some(value) = i64_param(params, "min_points") {
        config.min_points = value as i32;
    }
    if let Some(value) = i64_param(params, "max_points") {
        config.max_points = value as i32;
    }
    config
}

fn holt_config(params: Params<'_>) -> HoltResidualConfig {
    let mut config = HoltResidualConfig::default();
    if let Some(value) = f64_param(params, "alpha") {
        config.alpha = value;
    }
    if let Some(value) = f64_param(params, "beta") {
        config.beta = value;
    }
    if let Some(value) = usize_param(params, "warmup_points") {
        config.warmup_points = value;
    }
    if let Some(value) = usize_param(params, "residual_window") {
        config.residual_window = value;
    }
    if let Some(value) = f64_param(params, "z_threshold") {
        config.z_threshold = value;
    }
    if let Some(value) = usize_param(params, "confirm_m") {
        config.confirm_m = value;
    }
    if let Some(value) = f64_param(params, "min_deviation_mad") {
        config.min_deviation_mad = value;
    }
    if let Some(value) = usize_param(params, "refractory") {
        config.refractory = value;
    }
    config
}

fn tukey_config(params: Params<'_>) -> TukeyBiweightConfig {
    let mut config = TukeyBiweightConfig::default();
    if let Some(value) = usize_param(params, "window_size") {
        config.window_size = value;
    }
    if let Some(value) = usize_param(params, "min_points") {
        config.min_points = value;
    }
    if let Some(value) = f64_param(params, "biweight_c") {
        config.biweight_c = value;
    }
    if let Some(value) = usize_param(params, "irls_iterations") {
        config.irls_iterations = value;
    }
    if let Some(value) = f64_param(params, "z_threshold") {
        config.z_threshold = value;
    }
    if let Some(value) = usize_param(params, "score_every") {
        config.score_every = value;
    }
    if let Some(value) = usize_param(params, "cooldown_points") {
        config.cooldown_points = value;
    }
    config
}

#[cfg(test)]
pub(crate) use stub::StubDetectorFactory;

#[cfg(test)]
mod stub {
    use std::collections::HashSet;

    use saluki_anomaly_detection::identity::{Aggregate, QueryHandle, SeriesDescriptor, SeriesRef};
    use saluki_anomaly_detection::model::{Anomaly, AnomalyType};
    use saluki_anomaly_detection::traits::{Detector, StorageView};

    use super::DetectorFactory;

    /// A deterministic detector that fires once per metric series whose latest value reaches a
    /// threshold.
    ///
    /// This exists so the replay CLI, JSON export, and score-timeline capture can be tested end to end
    /// without depending on the statistical detector ports. It is intentionally simple: one anomaly per
    /// series, emitted at the second of the crossing point, with no score (so the scorer treats it as an
    /// uncalibrated Medium contributor).
    pub(crate) struct StubDetector {
        name: String,
        threshold: f64,
        fired: HashSet<u64>,
    }

    impl StubDetector {
        /// Creates a stub detector with the given name and firing threshold.
        pub(crate) fn new(name: impl Into<String>, threshold: f64) -> Self {
            Self {
                name: name.into(),
                threshold,
                fired: HashSet::new(),
            }
        }
    }

    impl Detector for StubDetector {
        type Config = ();

        fn name(&self) -> &str {
            &self.name
        }

        fn config(&self) -> &Self::Config {
            &()
        }

        fn is_ready(&self) -> bool {
            true
        }

        fn detect(&mut self, view: &dyn StorageView, data_time_sec: i64) -> Vec<Anomaly> {
            let mut anomalies = Vec::new();
            for meta in view.list_series(Some("parquet")) {
                if self.fired.contains(&meta.series_ref.raw()) {
                    continue;
                }
                let Some(series) = view.get_series_range(meta.series_ref, i64::MIN, data_time_sec, Aggregate::Average)
                else {
                    continue;
                };
                let Some(last) = series.points.last() else {
                    continue;
                };
                if last.value >= self.threshold {
                    self.fired.insert(meta.series_ref.raw());
                    anomalies.push(Anomaly {
                        anomaly_type: AnomalyType::Metric,
                        series: SeriesDescriptor::new(
                            meta.namespace.clone(),
                            meta.name.clone(),
                            meta.host.clone(),
                            meta.tags.clone(),
                            Aggregate::Average,
                        ),
                        series_ref: Some(QueryHandle::new(meta.series_ref, Aggregate::Average)),
                        detector_name: self.name.clone(),
                        context: None,
                        timestamp_sec: last.second,
                        score: None,
                        sampling_interval_sec: 0,
                        evidence: None,
                    });
                }
            }
            anomalies
        }

        fn reset(&mut self) {
            self.fired.clear();
        }

        fn remove_series(&mut self, series: &[SeriesRef]) {
            for series in series {
                self.fired.remove(&series.raw());
            }
        }
    }

    /// Builds [`StubDetector`]s for every detector name in the catalog.
    pub(crate) struct StubDetectorFactory {
        threshold: f64,
    }

    impl StubDetectorFactory {
        /// Creates a factory whose detectors fire at `threshold`.
        pub(crate) fn new(threshold: f64) -> Self {
            Self { threshold }
        }
    }

    impl DetectorFactory for StubDetectorFactory {
        fn create(
            &self, name: &str, _params: Option<&serde_json::Value>,
        ) -> Result<Option<Box<dyn saluki_anomaly_detection::engine::AnyDetector>>, String> {
            if crate::config::catalog_entry(name)
                .is_some_and(|entry| entry.kind == crate::config::ComponentKind::Detector && entry.migrated)
            {
                Ok(Some(Box::new(StubDetector::new(name, self.threshold))))
            } else {
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn params(value: serde_json::Value) -> serde_json::Value {
        value
    }

    #[test]
    fn bocpd_maps_all_recognised_keys() {
        let value = params(json!({
            "warmup_points": 77,
            "hazard": 0.2,
            "cp_threshold": 0.9,
            "short_run_length": 3,
            "cp_mass_threshold": 0.5,
            "max_run_length": 300,
            "prior_variance_scale": 4.0,
            "min_variance": 2.0,
            "recovery_points": 7,
            "unknown_key": 123,
        }));
        let config = bocpd_config(value.as_object());
        assert_eq!(config.warmup_points, 77);
        assert_eq!(config.hazard, 0.2);
        assert_eq!(config.cp_threshold, 0.9);
        assert_eq!(config.short_run_length, 3);
        assert_eq!(config.cp_mass_threshold, 0.5);
        assert_eq!(config.max_run_length, 300);
        assert_eq!(config.prior_variance_scale, 4.0);
        assert_eq!(config.min_variance, 2.0);
        assert_eq!(config.recovery_points, 7);
    }

    #[test]
    fn bocpd_ignores_empty_and_unknown_params() {
        let config = bocpd_config(Some(&serde_json::Map::new()));
        assert_eq!(config, BocpdConfig::default());
    }

    #[test]
    fn scan_detectors_only_read_window_keys() {
        let value = params(json!({
            "min_points": 45,
            "max_points": 200,
            "min_segment": 99,
            "significance_threshold": 0.5,
        }));
        let mw = scan_mw_config(value.as_object());
        assert_eq!(mw.min_points, 45);
        assert_eq!(mw.max_points, 200);
        // The Go catalog factory copies only MinPoints/MaxPoints, so the rest stay at defaults.
        assert_eq!(mw.min_segment, ScanMwConfig::default().min_segment);

        let welch = scan_welch_config(value.as_object());
        assert_eq!(welch.min_points, 45);
        assert_eq!(welch.max_points, 200);
        assert_eq!(welch.min_segment, ScanWelchConfig::default().min_segment);
    }

    #[test]
    fn holt_and_tukey_map_recognised_keys() {
        let holt = holt_config(
            params(json!({
                "alpha": 0.3,
                "beta": 0.1,
                "warmup_points": 18,
                "residual_window": 30,
                "z_threshold": 5.0,
                "confirm_m": 3,
                "min_deviation_mad": 4.0,
                "refractory": 12,
            }))
            .as_object(),
        );
        assert_eq!(holt.alpha, 0.3);
        assert_eq!(holt.beta, 0.1);
        assert_eq!(holt.warmup_points, 18);
        assert_eq!(holt.residual_window, 30);
        assert_eq!(holt.z_threshold, 5.0);
        assert_eq!(holt.confirm_m, 3);
        assert_eq!(holt.min_deviation_mad, 4.0);
        assert_eq!(holt.refractory, 12);

        let tukey = tukey_config(
            params(json!({
                "window_size": 50,
                "min_points": 50,
                "biweight_c": 5.0,
                "irls_iterations": 6,
                "z_threshold": 6.0,
                "score_every": 2,
                "cooldown_points": 10,
            }))
            .as_object(),
        );
        assert_eq!(tukey.window_size, 50);
        assert_eq!(tukey.min_points, 50);
        assert_eq!(tukey.biweight_c, 5.0);
        assert_eq!(tukey.irls_iterations, 6);
        assert_eq!(tukey.z_threshold, 6.0);
        assert_eq!(tukey.score_every, 2);
        assert_eq!(tukey.cooldown_points, 10);
    }

    #[test]
    fn builtin_factory_builds_every_migrated_metric_detector() {
        let factory = BuiltinDetectorFactory;
        for name in ["bocpd", "scanmw", "scanwelch", "holt_residual", "tukey_biweight"] {
            let detector = factory.create(name, None).unwrap();
            assert!(detector.is_some(), "{name} should be linked");
            assert_eq!(detector.unwrap().name(), name);
        }
        assert!(factory.create("time_cluster", None).unwrap().is_none());
    }
}
