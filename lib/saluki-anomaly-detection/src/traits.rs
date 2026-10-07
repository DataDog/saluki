//! The small interfaces the engine drives: the read-only store view, detectors, extractors, and the scorer.
//!
//! These traits are intentionally minimal. They are the contract between the engine and each pipeline
//! stage, and later work (the store, the detectors, the extractors, and the scorer) implements them. Keep
//! detector and extractor state private behind the trait; the engine owns the objects and calls them
//! synchronously from a single thread.

use crate::identity::{Aggregate, SeriesRef};
use crate::model::{Anomaly, LogObservation, MetricContext, Series, SeriesMeta, VirtualMetric};

/// A read-only view of the historical store, as seen by detectors during a detection pass.
///
/// The store implementation owns the mutable side; detectors only read. All methods take a [`SeriesRef`]
/// for O(1) lookup, and return `None`/`0` for refs that are not live, so detectors must tolerate series
/// that have been evicted between passes.
pub trait StorageView {
    /// Returns metadata for every live series, optionally restricted to one namespace.
    ///
    /// Passing `Some(namespace)` is the efficient path; `None` lists all namespaces, which the engine uses
    /// for telemetry and diagnostics rather than for detector workload.
    fn list_series(&self, namespace: Option<&str>) -> Vec<SeriesMeta>;

    /// Returns metadata for a single series, or `None` if it is not live.
    fn series_meta(&self, series: SeriesRef) -> Option<SeriesMeta>;

    /// Returns the context attached to a series, or `None` if it has none or is not live.
    fn get_context(&self, series: SeriesRef) -> Option<MetricContext>;

    /// Returns the points of a series within `(start_sec, end_sec]` for the requested aggregate, or `None`
    /// if the series is not live. `start_sec` is exclusive and `end_sec` is inclusive; use `i64::MIN` to
    /// read from the beginning.
    fn get_series_range(&self, series: SeriesRef, start_sec: i64, end_sec: i64, aggregate: Aggregate)
        -> Option<Series>;

    /// Returns the number of stored buckets (points) with timestamp `<= end_sec`, or `0` if the series is
    /// not live. A bucket may hold more than one raw sample after a same-second merge.
    fn point_count_up_to(&self, series: SeriesRef, end_sec: i64) -> usize;

    /// Returns a per-series write counter that increments on every write, including same-bucket merges.
    /// Returns `0` if the series is not live. Detectors use this to notice updates without re-reading.
    fn write_generation(&self, series: SeriesRef) -> u64;

    /// Returns a global counter that increments only when the set of live series changes. Detectors use this
    /// to cache [`StorageView::list_series`] results and refresh them only when series appear or disappear.
    fn series_generation(&self) -> u64;
}

/// A detector analyzes stored series for anomalies.
///
/// The engine calls [`Detector::detect`] periodically for each ready detector, then forwards accepted
/// anomalies to the scorer. Detectors own their per-series state and must implement
/// [`Detector::remove_series`] so that eviction fan-out keeps that state bounded.
pub trait Detector {
    /// The detector's typed configuration.
    type Config;

    /// Returns the detector name, which identifies it in anomaly output and configuration.
    fn name(&self) -> &str;

    /// Returns the detector's current configuration.
    fn config(&self) -> &Self::Config;

    /// Reports whether at least one series has reached the detector's scoring condition.
    ///
    /// Readiness is monotonic until [`Detector::reset`] is called.
    fn is_ready(&self) -> bool;

    /// Runs one detection pass over the read-only store view.
    ///
    /// Only data with timestamp `<= data_time_sec` may be read, so a pass is deterministic given the store
    /// state. Each returned anomaly must identify its source series and aggregate via
    /// [`crate::model::Anomaly::series_ref`].
    fn detect(&mut self, view: &dyn StorageView, data_time_sec: i64) -> Vec<Anomaly>;

    /// Clears all per-series state for a fresh replay.
    fn reset(&mut self);

    /// Drops state for series the store has evicted.
    ///
    /// Called by the engine immediately after the store frees refs, so detector state stays symmetric with
    /// storage state. Implementations must tolerate refs they never observed.
    fn remove_series(&mut self, series: &[SeriesRef]);
}

/// The result of processing one log line with an [`Extractor`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtractorOutput {
    /// Virtual metrics derived from the log, to be stored under the extractor's namespace.
    pub metrics: Vec<VirtualMetric>,
    /// Metric names whose series should be removed from storage (for example after LRU eviction or garbage
    /// collection inside the extractor).
    pub evicted_metric_names: Vec<String>,
}

/// An extractor turns log observations into virtual metric observations.
///
/// The extractor's [`Extractor::name`] defines the namespace its metrics are stored under, so different
/// extractors never collide. Extractors may keep lightweight state for pattern tracking.
pub trait Extractor {
    /// Returns the extractor name, which is also its storage namespace.
    fn name(&self) -> &str;

    /// Examines a log and returns any derived metrics plus any series removals.
    fn process_log(&mut self, log: &LogObservation) -> ExtractorOutput;
}

/// A scorer consumes accepted anomalies and emits severity/episode output.
///
/// The engine submits every accepted anomaly for an advance, then calls [`Scorer::advance_to`] once, after
/// all detector outputs have been submitted, so the scorer can process several consecutive seconds in one
/// call.
pub trait Scorer {
    /// The type of output the scorer produces (for example severity transitions or episode events).
    type Output;

    /// Submits an accepted anomaly for accumulation.
    fn process_anomaly(&mut self, anomaly: &Anomaly);

    /// Advances the scorer to `data_time_sec`, emitting outputs for every second crossed.
    fn advance_to(&mut self, data_time_sec: i64);

    /// Clears all accumulated state for a fresh replay.
    fn reset(&mut self);

    /// Returns and drains the outputs produced since the last call.
    ///
    /// The caller owns the returned values; the scorer discards them afterwards. Returns an empty vector
    /// when nothing is pending.
    fn take_pending_outputs(&mut self) -> Vec<Self::Output>;
}
