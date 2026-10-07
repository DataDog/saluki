//! Detector construction seam.
//!
//! The replay engine needs concrete detectors, but the detector implementations live in separate
//! library cards (the BOCPD, ScanMW/ScanWelch, Holt, and Tukey ports). The testbench therefore
//! constructs detectors through a [`DetectorFactory`]: the default binary uses
//! [`BuiltinDetectorFactory`], which only knows about the detectors linked into the current build, and
//! tests inject a deterministic [`StubDetector`] so the CLI and export paths can be exercised without
//! the statistical ports.
//!
//! A factory returns `Ok(None)` for a detector that is configured but not linked into this build; the
//! replay records it in [`crate::replay::ReplayResult::unlinked_detectors`] and the CLI warns about it,
//! rather than silently dropping the detector.

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

/// The factory used by the real binary.
///
/// It currently links no metric detectors: the detector ports are landing in separate cards, and this
/// factory is the single place they will be registered. Configured-but-unlinked detectors are reported
/// rather than silently ignored.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinDetectorFactory;

impl DetectorFactory for BuiltinDetectorFactory {
    fn create(&self, _name: &str, _params: Option<&Value>) -> Result<Option<Box<dyn AnyDetector>>, String> {
        Ok(None)
    }
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
