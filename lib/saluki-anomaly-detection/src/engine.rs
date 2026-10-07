//! The ordered ingestion engine and its advance cycle.
//!
//! This is the Rust port of the Go observer's `observer/impl/engine.go`. It is an ordinary synchronous
//! core with **exclusive mutable ownership**: no async, no channels, no interior locking. One owner drives
//! ingestion and advances from a single thread, which is what makes replay deterministic and lets the
//! detectors keep unsynchronised per-series state.
//!
//! The engine owns the [`TimeSeriesStorage`], the extractors, the detectors, and (optionally) the scorer.
//! It is deliberately **not** a reporter: callers route the anomalies and scorer outputs returned from
//! [`Engine::advance`] to their own outputs.
//!
//! # Ordering contract
//!
//! The engine preserves the Go ordering exactly (see `README.md` section 9):
//!
//! 1. `ingest_metric` / `ingest_log` normalize and admit the observation, store the sample (or virtual
//!    metrics), and ask the scheduler for advance requests. Input at data second `T` requests an advance
//!    to `T - 1` **only if newer** than the last analyzed time; there are no idle wall-clock advances.
//! 2. `advance(upTo)` first skips when `upTo <= last_analyzed_data_time`. Otherwise it:
//!    1. records the advance trace (data time, reason, drained late-point counters);
//!    2. evicts inactive series and fans the freed refs out to the deduper and detectors;
//!    3. expires anomaly-dedup entries;
//!    4. runs the detectors in **registry order** over read-only store views, and for each emitted
//!       anomaly validates that it carries a storage ref, enriches it with the series context, and
//!       deduplicates it before feeding it to the scorer;
//!    5. advances the scorer **after every detector output for the advance has been submitted** (the
//!       scorer may process several consecutive seconds in one call);
//!    6. performs capacity eviction and fans the freed refs out;
//!    7. returns the accepted anomalies and the scorer outputs.
//!
//! Baseline windows and optional log-count buckets are **absent** from this port (baseline is disabled
//! everywhere); every other step is preserved.
//!
//! # Replay end
//!
//! [`Engine::finish_stream`] flushes through the latest observed data time exactly as Go does. It does not
//! append an invented recovery second and does not force open episodes closed: an EOF flush is just one
//! more advance, at the last data time.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::identity::{Aggregate, QueryHandle, SeriesRef};
use crate::model::{Anomaly, LogObservation, MetricSample};
use crate::scheduler::{AdvanceReason, AdvanceRequest, CurrentBehaviorPolicy, SchedulerPolicy, SchedulerState};
use crate::storage::TimeSeriesStorage;
use crate::traits::{Detector, Extractor, Scorer, StorageView};

/// Maximum number of live anomaly-dedup entries, matching Go's `maxLiveAnomalyDedupEntries`.
const MAX_LIVE_ANOMALY_DEDUP_ENTRIES: usize = 5_000;

/// Cap on the number of distinct anomaly sources retained in history mode, matching Go's
/// `maxUniqueSources`.
const MAX_UNIQUE_ANOMALY_SOURCES: usize = 500;

/// An object-safe view of a [`Detector`] that erases its [`Detector::Config`] associated type.
///
/// [`Detector`] cannot be used as a trait object because it has an associated type. Rather than change the
/// public detector contract (detectors keep their typed config), the engine stores `Box<dyn AnyDetector>`
/// and a blanket implementation projects every [`Detector`] onto this object-safe surface. The engine only
/// needs the behavioral methods, not the configuration.
pub trait AnyDetector {
    /// Returns the detector name (see [`Detector::name`]).
    fn name(&self) -> &str;

    /// Reports readiness (see [`Detector::is_ready`]).
    fn is_ready(&self) -> bool;

    /// Runs one detection pass (see [`Detector::detect`]).
    fn detect(&mut self, view: &dyn StorageView, data_time_sec: i64) -> Vec<Anomaly>;

    /// Clears per-series state (see [`Detector::reset`]).
    fn reset(&mut self);

    /// Drops state for evicted series (see [`Detector::remove_series`]).
    fn remove_series(&mut self, series: &[SeriesRef]);
}

impl<T: Detector> AnyDetector for T {
    fn name(&self) -> &str {
        Detector::name(self)
    }

    fn is_ready(&self) -> bool {
        Detector::is_ready(self)
    }

    fn detect(&mut self, view: &dyn StorageView, data_time_sec: i64) -> Vec<Anomaly> {
        Detector::detect(self, view, data_time_sec)
    }

    fn reset(&mut self) {
        Detector::reset(self)
    }

    fn remove_series(&mut self, series: &[SeriesRef]) {
        Detector::remove_series(self, series)
    }
}

/// A scorer that does nothing, used as the default engine type parameter.
///
/// This lets a metrics-only engine (no scorer configured) still be a concrete [`Engine`] without an
/// awkward `Option` at every call site. It produces no outputs.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopScorer;

impl Scorer for NoopScorer {
    type Output = ();

    fn process_anomaly(&mut self, _anomaly: &Anomaly) {}

    fn advance_to(&mut self, _data_time_sec: i64) {}

    fn reset(&mut self) {}

    fn take_pending_outputs(&mut self) -> Vec<Self::Output> {
        Vec::new()
    }
}

/// The deduplication key for a detector anomaly: source ref, aggregate, detector name, and timestamp.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct AnomalyDedupKey {
    source_ref: u64,
    source_aggregate: Aggregate,
    detector_name: String,
    timestamp: i64,
}

/// A live dedup entry: when it expires (in data time) and how recently it was used.
#[derive(Clone, Copy, Debug)]
struct AnomalyDedupEntry {
    /// Expiry in data time. `0` disables time-based expiry for this entry.
    expires_at: i64,
    /// Recency stamp; larger is more recently used. Used only to choose the LRU victim on capacity
    /// eviction, reproducing Go's `simplelru` move-to-front behavior without a linked list.
    recency: u64,
}

/// Suppresses detector outputs that are re-emitted across consecutive advances.
///
/// Ported from the Go `anomalyDeduper`. In **live** mode it is a fixed-capacity LRU (5000 entries); in
/// **replay history** mode it is an unbounded set so a finite replay keeps complete dedup history.
///
/// Time-based expiry uses the entry's `expires_at`, derived from the source series' point retention: once a
/// series can no longer retain the point that produced an anomaly, the entry is dropped. The bulk scan runs
/// only when the earliest known expiry has passed (the `next_expiry` gate), so the common advance does no
/// scanning.
#[derive(Debug)]
struct AnomalyDeduper {
    /// Live mode entries, or `None` in replay-history mode.
    live: Option<HashMap<AnomalyDedupKey, AnomalyDedupEntry>>,
    /// Replay-history mode keys.
    replay: HashSet<AnomalyDedupKey>,
    /// Earliest known expiry across live entries; `0` means "unknown/none".
    next_expiry: i64,
    /// Monotonic recency clock.
    recency_clock: u64,
    /// Live capacity; ignored in replay-history mode.
    capacity: usize,
}

impl AnomalyDeduper {
    /// Creates a deduper for the requested mode (`track_history` selects the unbounded replay map).
    fn new(track_history: bool) -> Self {
        let capacity = if track_history {
            0
        } else {
            MAX_LIVE_ANOMALY_DEDUP_ENTRIES
        };
        Self::with_capacity(capacity)
    }

    /// Creates a live-mode deduper with an explicit capacity; `0` selects replay-history mode.
    fn with_capacity(capacity: usize) -> Self {
        if capacity == 0 {
            Self {
                live: None,
                replay: HashSet::new(),
                next_expiry: 0,
                recency_clock: 0,
                capacity: 0,
            }
        } else {
            Self {
                live: Some(HashMap::new()),
                replay: HashSet::new(),
                next_expiry: 0,
                recency_clock: 0,
                capacity,
            }
        }
    }

    /// Records `key` unless it is a duplicate. Returns `(accepted, capacity_evicted)`.
    ///
    /// `expires_at` is in data time; `0` disables time-based expiry for the entry.
    fn accept(&mut self, key: AnomalyDedupKey, expires_at: i64) -> (bool, usize) {
        let Some(live) = self.live.as_mut() else {
            // Replay-history mode: the unbounded set keeps every key; an insert that reports `false` was a
            // duplicate.
            return (self.replay.insert(key), 0);
        };

        self.recency_clock += 1;
        let clock = self.recency_clock;

        if let Some(entry) = live.get_mut(&key) {
            entry.recency = clock;
            return (false, 0);
        }

        if expires_at > 0 && (self.next_expiry == 0 || expires_at < self.next_expiry) {
            self.next_expiry = expires_at;
        }
        live.insert(
            key,
            AnomalyDedupEntry {
                expires_at,
                recency: clock,
            },
        );

        if live.len() > self.capacity {
            if let Some(victim) = live
                .iter()
                .min_by_key(|(_, entry)| entry.recency)
                .map(|(key, _)| key.clone())
            {
                live.remove(&victim);
            }
            return (true, 1);
        }
        (true, 0)
    }

    /// Discards live entries once the source series can no longer retain their points.
    ///
    /// Returns the number removed. Replay-history mode keeps complete history and always returns `0`.
    fn remove_expired(&mut self, data_time: i64) -> usize {
        let Some(live) = self.live.as_mut() else {
            return 0;
        };
        if self.next_expiry == 0 || data_time <= self.next_expiry {
            return 0;
        }

        let mut expired = Vec::new();
        let mut next_expiry = 0i64;
        for (key, entry) in live.iter() {
            if entry.expires_at == 0 {
                continue;
            }
            if data_time > entry.expires_at {
                expired.push(key.clone());
            } else if next_expiry == 0 || entry.expires_at < next_expiry {
                next_expiry = entry.expires_at;
            }
        }
        for key in &expired {
            live.remove(key);
        }
        self.next_expiry = next_expiry;
        expired.len()
    }

    /// Discards live entries whose source series no longer exists.
    ///
    /// Returns the number removed. The scan is bounded by the live capacity and happens only when storage
    /// evicts series. Replay-history mode retains complete history and always returns `0`.
    fn remove_source_refs(&mut self, refs: &[SeriesRef]) -> usize {
        let Some(live) = self.live.as_mut() else {
            return 0;
        };
        if refs.is_empty() {
            return 0;
        }

        let removed_refs: HashSet<u64> = refs.iter().map(|series| series.raw()).collect();
        let victims: Vec<AnomalyDedupKey> = live
            .keys()
            .filter(|key| removed_refs.contains(&key.source_ref))
            .cloned()
            .collect();
        for key in &victims {
            live.remove(key);
        }
        victims.len()
    }
}

/// A per-advance trace emitted through [`EngineDiagnostics::on_advance`].
///
/// Ported from the Go `advanceEntry`. Diagnostics are optional and must never change analytical behavior;
/// this value is only built when a diagnostics sink is installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvanceTrace {
    /// The data second analysis advanced to.
    pub data_time: i64,
    /// Why the advance ran.
    pub reason: AdvanceReason,
    /// Points ingested after their timestamp had already been analyzed, since the previous advance.
    pub late_points: u64,
    /// Per-source breakdown of `late_points`.
    pub late_points_by_source: BTreeMap<String, u64>,
}

/// Optional diagnostics callbacks for the engine.
///
/// Every method has a no-op default, and the engine skips building any diagnostics arguments when no sink
/// is installed. Installing a sink never changes algorithm configuration, iteration order, retention, or
/// final output, matching the Go design where these are trace-only callbacks.
pub trait EngineDiagnostics {
    /// Called after series were evicted. `reason` is `inactive`, `capacity`, or `extractor`.
    fn on_storage_series_evicted(&mut self, _reason: &str, _count: usize) {}

    /// Called when a capacity eviction pass ran (regardless of how much it freed).
    fn on_storage_capacity_hit(&mut self) {}

    /// Called when anomaly-dedup entries were evicted. `reason` is `capacity`, `retention`, or
    /// `series_evicted`.
    fn on_anomaly_dedup_evicted(&mut self, _reason: &str, _count: usize) {}

    /// Called when an advance request was skipped because it was not newer than the analyzed time.
    fn on_advance_skipped(&mut self, _reason: &str) {}

    /// Called at the start of every non-skipped advance with the drained late-point counters.
    fn on_advance(&mut self, _trace: &AdvanceTrace) {}
}

/// Configuration for [`Engine::with_config`].
///
/// The default type parameter makes the no-scorer case concise: `Engine::new(storage)` builds a
/// [`NoopScorer`]-backed engine.
pub struct EngineConfig<S: Scorer = NoopScorer> {
    /// The store the engine owns.
    pub storage: TimeSeriesStorage,
    /// Detectors, in the order they should run.
    pub detectors: Vec<Box<dyn AnyDetector>>,
    /// Log-metric extractors, in the order they should run. Names must be unique.
    pub extractors: Vec<Box<dyn Extractor>>,
    /// An optional scorer, fed accepted anomalies and advanced once per advance.
    pub scorer: Option<S>,
    /// The scheduling policy. `None` uses [`CurrentBehaviorPolicy`].
    pub scheduler: Option<Box<dyn SchedulerPolicy>>,
    /// Enables unbounded replay anomaly-dedup history and the raw anomaly list.
    pub track_anomaly_history: bool,
    /// Retains every detector return value before downstream filtering (replay diagnostics only).
    pub track_detector_output_history: bool,
    /// An optional diagnostics sink.
    pub diagnostics: Option<Box<dyn EngineDiagnostics>>,
}

impl<S: Scorer> EngineConfig<S> {
    /// Creates an empty configuration around `storage`, with no detectors, extractors, or scorer.
    pub fn new(storage: TimeSeriesStorage) -> Self {
        Self {
            storage,
            detectors: Vec::new(),
            extractors: Vec::new(),
            scorer: None,
            scheduler: None,
            track_anomaly_history: false,
            track_detector_output_history: false,
            diagnostics: None,
        }
    }
}

/// The outputs of one advance call.
#[derive(Clone, Debug, PartialEq)]
pub struct AdvanceResult<O> {
    /// Accepted anomalies, in detector registry order.
    pub anomalies: Vec<Anomaly>,
    /// Outputs the scorer produced during this advance.
    pub scorer_outputs: Vec<O>,
}

impl<O> Default for AdvanceResult<O> {
    fn default() -> Self {
        Self {
            anomalies: Vec::new(),
            scorer_outputs: Vec::new(),
        }
    }
}

impl<O> AdvanceResult<O> {
    /// Appends another result's contents into this one.
    fn append(&mut self, mut other: AdvanceResult<O>) {
        self.anomalies.append(&mut other.anomalies);
        self.scorer_outputs.append(&mut other.scorer_outputs);
    }
}

/// The ordered ingestion engine.
///
/// See the module documentation for the ordering contract. The engine is not `Clone` and not internally
/// synchronised: it has exactly one owner.
pub struct Engine<S: Scorer = NoopScorer> {
    storage: TimeSeriesStorage,
    extractors: Vec<Box<dyn Extractor>>,
    detectors: Vec<Box<dyn AnyDetector>>,
    scorer: Option<S>,
    scheduler: Box<dyn SchedulerPolicy>,
    diagnostics: Option<Box<dyn EngineDiagnostics>>,

    last_analyzed_data_time: i64,
    latest_data_time: i64,
    inactive_series_eviction_checked: bool,
    last_inactive_series_eviction_check: i64,

    deduper: AnomalyDeduper,
    track_anomaly_history: bool,
    track_detector_output_history: bool,
    raw_anomalies: Vec<Anomaly>,
    detector_output_anomalies: Vec<Anomaly>,
    total_anomaly_count: u64,
    unique_anomaly_sources: BTreeSet<String>,

    late_points: u64,
    late_points_by_source: BTreeMap<String, u64>,
}

impl Engine<NoopScorer> {
    /// Creates a metrics-only engine around `storage` with no detectors, extractors, or scorer.
    ///
    /// This is the concise constructor for the common no-scorer case. Use [`Engine::with_config`] to
    /// configure detectors, extractors, and a scorer.
    pub fn new(storage: TimeSeriesStorage) -> Self {
        Self::with_config(EngineConfig::new(storage))
    }
}

impl<S: Scorer> Engine<S> {
    /// Creates an engine from a configuration, panicking if extractor names are not unique.
    ///
    /// # Panics
    ///
    /// Panics if two extractors share a name. Extractor names are storage namespaces, so duplicates would
    /// merge unrelated series; the Go `validateUniqueExtractorNames` rejects them loudly rather than
    /// silently corrupting the store.
    pub fn with_config(config: EngineConfig<S>) -> Self {
        let EngineConfig {
            storage,
            detectors,
            extractors,
            scorer,
            scheduler,
            track_anomaly_history,
            track_detector_output_history,
            diagnostics,
        } = config;
        validate_unique_extractor_names(&extractors);

        Self {
            storage,
            extractors,
            detectors,
            scorer,
            scheduler: scheduler.unwrap_or_else(|| Box::new(CurrentBehaviorPolicy::new())),
            diagnostics,
            last_analyzed_data_time: 0,
            latest_data_time: 0,
            inactive_series_eviction_checked: false,
            last_inactive_series_eviction_check: 0,
            deduper: AnomalyDeduper::new(track_anomaly_history),
            track_anomaly_history,
            track_detector_output_history,
            raw_anomalies: Vec::new(),
            detector_output_anomalies: Vec::new(),
            total_anomaly_count: 0,
            unique_anomaly_sources: BTreeSet::new(),
            late_points: 0,
            late_points_by_source: BTreeMap::new(),
        }
    }

    /// Returns a read-only reference to the owned store.
    pub fn storage(&self) -> &TimeSeriesStorage {
        &self.storage
    }

    /// Returns a mutable reference to the owned store, for tests and retained-loading callers.
    pub fn storage_mut(&mut self) -> &mut TimeSeriesStorage {
        &mut self.storage
    }

    /// Returns the configured scorer, if any.
    pub fn scorer(&self) -> Option<&S> {
        self.scorer.as_ref()
    }

    /// Returns a mutable reference to the configured scorer, if any.
    pub fn scorer_mut(&mut self) -> Option<&mut S> {
        self.scorer.as_mut()
    }

    /// Returns a slice of the registered detectors.
    pub fn detectors(&self) -> &[Box<dyn AnyDetector>] {
        &self.detectors
    }

    /// Returns the number of registered extractors.
    pub fn extractor_count(&self) -> usize {
        self.extractors.len()
    }

    /// Returns the data second up to which detection has run.
    pub fn last_analyzed_data_time(&self) -> i64 {
        self.last_analyzed_data_time
    }

    /// Returns the latest data timestamp seen across all ingested observations.
    pub fn latest_data_time(&self) -> i64 {
        self.latest_data_time
    }

    /// Returns the total number of anomalies ever detected (including duplicates).
    pub fn total_anomaly_count(&self) -> u64 {
        self.total_anomaly_count
    }

    /// Returns the number of distinct anomaly sources retained in history mode (capped).
    pub fn unique_anomaly_source_count(&self) -> usize {
        self.unique_anomaly_sources.len()
    }

    /// Returns the full raw anomaly history (empty unless `track_anomaly_history` is enabled).
    pub fn raw_anomalies(&self) -> &[Anomaly] {
        &self.raw_anomalies
    }

    /// Returns every detector return value retained before downstream filtering (empty unless
    /// `track_detector_output_history` is enabled).
    pub fn detector_output_anomalies(&self) -> &[Anomaly] {
        &self.detector_output_anomalies
    }

    /// Installs or removes the diagnostics sink.
    pub fn set_diagnostics(&mut self, diagnostics: Option<Box<dyn EngineDiagnostics>>) {
        self.diagnostics = diagnostics;
    }

    /// Ingests one metric sample and returns the scheduler's advance requests.
    ///
    /// The namespace is `sample.source`. The sample is admitted through the store, which drops non-finite
    /// values and the exact `±f64::MAX` sentinels. A sample whose timestamp is at or before the last
    /// analyzed time is counted as a late point: it is stored, but it was invisible to detectors during the
    /// advance that covered its second.
    pub fn ingest_metric(&mut self, sample: &MetricSample) -> Vec<AdvanceRequest> {
        self.storage.add(
            &sample.source,
            &sample.name,
            sample.host.as_deref(),
            sample.value,
            sample.timestamp_sec,
            &sample.tags,
        );
        if sample.timestamp_sec <= self.last_analyzed_data_time {
            self.late_points += 1;
            *self.late_points_by_source.entry(sample.source.clone()).or_insert(0) += 1;
        }
        self.track_latest_data_time(sample.timestamp_sec);
        self.scheduler
            .on_observation(sample.timestamp_sec, self.scheduler_state())
    }

    /// Ingests one log observation and returns the scheduler's advance requests.
    ///
    /// Runs every enabled extractor (there may be none, in metrics-only mode), stores the virtual metrics
    /// each produces, and removes any pattern series the extractors evicted. Every log records an
    /// observation time even when it emits no metric, so the replay timeline still contains its second. The
    /// derived metrics get an `observer_source:<source>` tag when it is absent, and fall back to the log's
    /// hostname when they carry no host.
    pub fn ingest_log(&mut self, log: &LogObservation) -> Vec<AdvanceRequest> {
        let source_tag = format!("observer_source:{}", log.source);
        let data_time_sec = log.timestamp_ms / 1_000;

        for index in 0..self.extractors.len() {
            let output = self.extractors[index].process_log(log);
            let namespace = self.extractors[index].name().to_string();
            self.remove_evicted_metric_series(&namespace, &output.evicted_metric_names);

            for metric in &output.metrics {
                let mut tags = metric.tags.clone();
                if !tags.iter().any(|tag| tag == &source_tag) {
                    tags.push(source_tag.clone());
                }
                let host = match metric.host.as_deref() {
                    Some(host) if !host.is_empty() => host.to_string(),
                    _ => log.hostname.clone(),
                };
                let result = self.storage.add(
                    &namespace,
                    &metric.name,
                    Some(&host),
                    metric.value,
                    data_time_sec,
                    &tags,
                );
                if let (Some(context), Some(series_ref)) = (&metric.context, result.series_ref) {
                    self.storage.set_context(series_ref, context.clone());
                }
            }
        }

        self.storage.record_observation_time(data_time_sec);
        self.track_latest_data_time(data_time_sec);
        self.scheduler.on_observation(data_time_sec, self.scheduler_state())
    }

    /// Ingests a metric and immediately runs any scheduler-requested advances.
    pub fn ingest_metric_and_advance(&mut self, sample: &MetricSample) -> AdvanceResult<S::Output> {
        let requests = self.ingest_metric(sample);
        self.run_requests(requests)
    }

    /// Ingests a log and immediately runs any scheduler-requested advances.
    pub fn ingest_log_and_advance(&mut self, log: &LogObservation) -> AdvanceResult<S::Output> {
        let requests = self.ingest_log(log);
        self.run_requests(requests)
    }

    /// Advances analysis to `up_to_sec` with the [`AdvanceReason::Manual`] reason.
    pub fn advance(&mut self, up_to_sec: i64) -> AdvanceResult<S::Output> {
        self.advance_with_reason(up_to_sec, AdvanceReason::Manual)
    }

    /// Advances analysis to `up_to_sec`, recording `reason`, and returns the outputs.
    ///
    /// An advance that is not newer than the last analyzed time is skipped: it emits the skip diagnostic
    /// (if any) and returns empty. This is the method the scheduler-driven paths call.
    pub fn advance_with_reason(&mut self, up_to_sec: i64, reason: AdvanceReason) -> AdvanceResult<S::Output> {
        if up_to_sec <= self.last_analyzed_data_time {
            if let Some(diagnostics) = self.diagnostics.as_mut() {
                diagnostics.on_advance_skipped(reason.as_str());
            }
            return AdvanceResult::default();
        }
        self.last_analyzed_data_time = up_to_sec;
        self.emit_advance_trace(up_to_sec, reason);

        // Baseline windows and optional log-count buckets are absent from this port.
        self.evict_inactive_series(up_to_sec);
        self.remove_expired_anomaly_dedup(up_to_sec);

        let result = self.run_detectors_and_scorer(up_to_sec);

        let freed = self.storage.evict_default();
        if !freed.is_empty() {
            if let Some(diagnostics) = self.diagnostics.as_mut() {
                diagnostics.on_storage_capacity_hit();
            }
            self.notify_series_evicted("capacity", freed.len());
            self.fan_out_series_removal(&freed);
        }

        result
    }

    /// Flushes analysis through the latest observed data time, without resetting any state.
    ///
    /// This is the streaming end-of-input path: observations were already advanced synchronously as they
    /// arrived, so this only runs the scheduler's replay-end advance (to the last data time, if it is newer
    /// than the analyzed time). It never advances past the latest observed data time.
    pub fn finish_stream(&mut self) -> AdvanceResult<S::Output> {
        let state = self.scheduler_state();
        let requests = self.scheduler.on_replay_end(state);
        self.run_requests(requests)
    }

    /// Replays every distinct stored data timestamp through the scheduler policy.
    ///
    /// This walks storage rather than requiring the caller to re-feed observations, so advances happen at
    /// exactly the same data times as a live stream would. It does **not** reset analysis state: callers
    /// that want a from-scratch replay call [`Engine::reset_analysis_state`] first (which preserves
    /// extractor state and storage, matching the Go retained-loading sequence). It ends with
    /// [`Engine::finish_stream`].
    pub fn replay_stored_data(&mut self) -> AdvanceResult<S::Output> {
        let timestamps = self.storage.data_timestamps();
        let mut result = AdvanceResult::default();
        for timestamp in timestamps {
            self.track_latest_data_time(timestamp);
            let requests = self.scheduler.on_observation(timestamp, self.scheduler_state());
            result.append(self.run_requests(requests));
        }
        result.append(self.finish_stream());
        result
    }

    /// Resets analysis state for a fresh replay, replacing storage with a fresh store.
    ///
    /// Detectors, the scorer, and the extractors are reset, anomaly-dedup state and history are cleared,
    /// the data-time clocks and late-point counters are zeroed, and storage is replaced with an empty store
    /// built from the current configuration.
    pub fn reset(&mut self) {
        self.clear_analysis_state();
        for extractor in &mut self.extractors {
            extractor.reset();
        }
        self.reset_anomaly_tracking();

        let config = self.storage.config().clone();
        self.storage = TimeSeriesStorage::new(config);
        self.late_points = 0;
        self.late_points_by_source.clear();
    }

    /// Resets detector/scorer/dedup state but keeps storage and extractor state.
    ///
    /// This is the Go `resetAnalysisState` equivalent used before a batch replay: extractor state built
    /// during log ingestion is preserved so anomalies can still be enriched with log context, while
    /// detectors and the scorer start clean.
    pub fn reset_analysis_state(&mut self) {
        self.clear_analysis_state();
        self.reset_anomaly_tracking();
    }

    fn run_requests(&mut self, requests: Vec<AdvanceRequest>) -> AdvanceResult<S::Output> {
        let mut result = AdvanceResult::default();
        for request in requests {
            let advance = self.advance_with_reason(request.up_to_sec, request.reason);
            result.append(advance);
        }
        result
    }

    /// Records an incoming observation's timestamp as the latest data time if it is newer.
    fn track_latest_data_time(&mut self, data_time_sec: i64) {
        if data_time_sec > self.latest_data_time {
            self.latest_data_time = data_time_sec;
        }
    }

    /// Returns the read-only scheduler state.
    fn scheduler_state(&self) -> SchedulerState {
        SchedulerState {
            last_analyzed_data_time: self.last_analyzed_data_time,
            latest_data_time: self.latest_data_time,
        }
    }

    /// Removes every storage series for the given metric names in `namespace`, then fans the freed refs out.
    fn remove_evicted_metric_series(&mut self, namespace: &str, evicted_names: &[String]) {
        let mut freed = Vec::new();
        for name in evicted_names {
            if name.is_empty() {
                continue;
            }
            freed.extend(self.storage.remove_series_by_metric_name(namespace, name));
        }
        if !freed.is_empty() {
            self.notify_series_evicted("extractor", freed.len());
        }
        self.fan_out_series_removal(&freed);
    }

    /// Fans freed refs out to the deduper and every detector.
    ///
    /// The deduper is cleared first so a re-created series does not silently suppress a new detection, then
    /// each detector drops its per-series state. Detectors must tolerate unknown refs.
    fn fan_out_series_removal(&mut self, refs: &[SeriesRef]) {
        if refs.is_empty() {
            return;
        }
        let removed = self.deduper.remove_source_refs(refs);
        self.record_dedup_eviction("series_evicted", removed);
        for detector in &mut self.detectors {
            detector.remove_series(refs);
        }
    }

    /// Evicts inactive series, at most once per configured check interval.
    fn evict_inactive_series(&mut self, up_to_sec: i64) {
        let ttl = self.storage.config().inactive_series_ttl_secs;
        let interval = self.storage.config().inactive_series_check_interval_secs;
        if ttl <= 0 || interval <= 0 {
            return;
        }
        if self.inactive_series_eviction_checked && up_to_sec - self.last_inactive_series_eviction_check < interval {
            return;
        }
        self.inactive_series_eviction_checked = true;
        self.last_inactive_series_eviction_check = up_to_sec;

        let freed = self.storage.evict_inactive_before(up_to_sec - ttl);
        if freed.is_empty() {
            return;
        }
        self.notify_series_evicted("inactive", freed.len());
        self.fan_out_series_removal(&freed);
    }

    /// Runs the detectors in registry order and the scorer advance, returning the accepted outputs.
    fn run_detectors_and_scorer(&mut self, up_to: i64) -> AdvanceResult<S::Output> {
        let mut all_anomalies: Vec<Anomaly> = Vec::new();

        for index in 0..self.detectors.len() {
            let raw = {
                let detectors = &mut self.detectors;
                let storage: &dyn StorageView = &self.storage;
                detectors[index].detect(storage, up_to)
            };
            let detector_name = self.detectors[index].name().to_string();

            if self.track_detector_output_history && !raw.is_empty() {
                self.record_detector_outputs(&detector_name, &raw);
            }

            for mut anomaly in raw {
                let Some(handle) = anomaly.series_ref else {
                    // Invalid detector output: no storage series to identify it.
                    continue;
                };
                self.enrich_anomaly(&mut anomaly);
                if !self.accept_anomaly(&anomaly, handle) {
                    continue; // duplicate
                }
                self.process_anomaly(&anomaly);
                all_anomalies.push(anomaly);
            }
        }

        // Advance the scorer only after every detector output for this advance has been submitted.
        if let Some(scorer) = self.scorer.as_mut() {
            scorer.advance_to(up_to);
        }
        let scorer_outputs = match self.scorer.as_mut() {
            Some(scorer) => scorer.take_pending_outputs(),
            None => Vec::new(),
        };

        AdvanceResult {
            anomalies: all_anomalies,
            scorer_outputs,
        }
    }

    /// Decorates an anomaly with the context stored on its source series, when the series is live.
    fn enrich_anomaly(&self, anomaly: &mut Anomaly) {
        if let Some(handle) = anomaly.series_ref {
            if let Some(context) = self.storage.get_context(handle.series) {
                anomaly.context = Some(context);
            }
        }
    }

    /// Feeds one accepted anomaly to the scorer.
    fn process_anomaly(&mut self, anomaly: &Anomaly) {
        if let Some(scorer) = self.scorer.as_mut() {
            scorer.process_anomaly(anomaly);
        }
    }

    /// Deduplicates an anomaly and records history, returning whether it was new.
    fn accept_anomaly(&mut self, anomaly: &Anomaly, handle: QueryHandle) -> bool {
        let expires_at = self.anomaly_dedup_expiry(handle.series, anomaly.timestamp_sec);
        self.total_anomaly_count += 1;

        let key = AnomalyDedupKey {
            source_ref: handle.series.raw(),
            source_aggregate: handle.aggregate,
            detector_name: anomaly.detector_name.clone(),
            timestamp: anomaly.timestamp_sec,
        };
        let (accepted, capacity_evicted) = self.deduper.accept(key, expires_at);

        if accepted && self.track_anomaly_history {
            if self.unique_anomaly_sources.len() < MAX_UNIQUE_ANOMALY_SOURCES {
                self.unique_anomaly_sources.insert(anomaly.series.identity().key());
            }
            self.raw_anomalies.push(anomaly.clone());
        }

        self.record_dedup_eviction("capacity", capacity_evicted);
        accepted
    }

    /// Returns the dedup expiry for an anomaly: its timestamp plus the source series' retention.
    fn anomaly_dedup_expiry(&self, series: SeriesRef, timestamp: i64) -> i64 {
        let retention = self.storage.point_retention_for_series(series);
        if retention <= 0 {
            0
        } else {
            timestamp + retention
        }
    }

    /// Expires live anomaly-dedup entries whose source series can no longer retain their points.
    fn remove_expired_anomaly_dedup(&mut self, data_time: i64) {
        let removed = self.deduper.remove_expired(data_time);
        self.record_dedup_eviction("retention", removed);
    }

    /// Retains detector return values before downstream filtering, filling in an empty detector name.
    fn record_detector_outputs(&mut self, detector_name: &str, anomalies: &[Anomaly]) {
        if !self.track_detector_output_history || anomalies.is_empty() {
            return;
        }
        for anomaly in anomalies {
            let mut anomaly = anomaly.clone();
            if anomaly.detector_name.is_empty() {
                anomaly.detector_name = detector_name.to_string();
            }
            self.detector_output_anomalies.push(anomaly);
        }
    }

    /// Emits the per-advance trace, draining the late-point counters.
    fn emit_advance_trace(&mut self, data_time: i64, reason: AdvanceReason) {
        if self.diagnostics.is_none() {
            return;
        }
        let trace = AdvanceTrace {
            data_time,
            reason,
            late_points: std::mem::take(&mut self.late_points),
            late_points_by_source: std::mem::take(&mut self.late_points_by_source),
        };
        if let Some(diagnostics) = self.diagnostics.as_mut() {
            diagnostics.on_advance(&trace);
        }
    }

    /// Emits a series-eviction diagnostic for a non-zero count.
    fn notify_series_evicted(&mut self, reason: &str, count: usize) {
        if count == 0 {
            return;
        }
        if let Some(diagnostics) = self.diagnostics.as_mut() {
            diagnostics.on_storage_series_evicted(reason, count);
        }
    }

    /// Emits a dedup-eviction diagnostic for a non-zero count.
    fn record_dedup_eviction(&mut self, reason: &str, count: usize) {
        if count == 0 {
            return;
        }
        if let Some(diagnostics) = self.diagnostics.as_mut() {
            diagnostics.on_anomaly_dedup_evicted(reason, count);
        }
    }

    /// Clears the data-time clocks, eviction cadence, and per-component analysis state.
    fn clear_analysis_state(&mut self) {
        self.last_analyzed_data_time = 0;
        self.latest_data_time = 0;
        self.inactive_series_eviction_checked = false;
        self.last_inactive_series_eviction_check = 0;
        for detector in &mut self.detectors {
            detector.reset();
        }
        if let Some(scorer) = self.scorer.as_mut() {
            scorer.reset();
        }
    }

    /// Clears dedup state, anomaly history, and counters.
    fn reset_anomaly_tracking(&mut self) {
        self.deduper = AnomalyDeduper::new(self.track_anomaly_history);
        self.raw_anomalies.clear();
        self.detector_output_anomalies.clear();
        self.total_anomaly_count = 0;
        self.unique_anomaly_sources.clear();
    }
}

/// Panics if two extractors share a name.
///
/// # Panics
///
/// Panics when a duplicate extractor name is found; extractor names are storage namespaces.
fn validate_unique_extractor_names(extractors: &[Box<dyn Extractor>]) {
    let mut seen = HashSet::new();
    for extractor in extractors {
        if !seen.insert(extractor.name().to_string()) {
            panic!("duplicate log extractor name: {:?}", extractor.name());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::identity::SeriesDescriptor;
    use crate::model::{AnomalyEvidence, MetricContext, VirtualMetric};
    use crate::traits::ExtractorOutput;

    /// A shared, append-only trace of pipeline events.
    #[derive(Clone, Default)]
    struct Trace(Rc<RefCell<Vec<String>>>);

    impl Trace {
        fn push(&self, event: impl Into<String>) {
            self.0.borrow_mut().push(event.into());
        }

        fn events(&self) -> Vec<String> {
            self.0.borrow().clone()
        }
    }

    /// Diagnostics implementation that records engine callbacks into a shared trace.
    struct TraceDiagnostics {
        trace: Trace,
    }

    impl EngineDiagnostics for TraceDiagnostics {
        fn on_storage_series_evicted(&mut self, reason: &str, count: usize) {
            self.trace.push(format!("evict:{reason}:{count}"));
        }

        fn on_storage_capacity_hit(&mut self) {
            self.trace.push("capacity_hit");
        }

        fn on_anomaly_dedup_evicted(&mut self, reason: &str, count: usize) {
            self.trace.push(format!("dedup:{reason}:{count}"));
        }

        fn on_advance_skipped(&mut self, reason: &str) {
            self.trace.push(format!("skip:{reason}"));
        }

        fn on_advance(&mut self, trace: &AdvanceTrace) {
            self.trace.push(format!("advance:{}:{}", trace.data_time, trace.reason));
        }
    }

    /// A detector that emits a fixed script of anomalies on every pass and records its calls.
    #[derive(Default)]
    struct ScriptedDetector {
        name: String,
        trace: Trace,
        emitted: Vec<Anomaly>,
        removed: Vec<SeriesRef>,
        /// When true, `emitted` is returned at most once, mimicking scan detectors.
        once: bool,
        fired: bool,
    }

    impl ScriptedDetector {
        fn new(name: &str, trace: &Trace) -> Self {
            Self {
                name: name.to_string(),
                trace: trace.clone(),
                ..Self::default()
            }
        }
    }

    impl Detector for ScriptedDetector {
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

        fn detect(&mut self, _view: &dyn StorageView, data_time_sec: i64) -> Vec<Anomaly> {
            self.trace.push(format!("detect:{}:{data_time_sec}", self.name));
            if self.once && self.fired {
                return Vec::new();
            }
            self.fired = true;
            self.emitted.clone()
        }

        fn reset(&mut self) {
            self.fired = false;
            self.trace.push(format!("reset:{}", self.name));
        }

        fn remove_series(&mut self, series: &[SeriesRef]) {
            self.removed.extend_from_slice(series);
            self.trace.push(format!("remove:{}", self.name));
        }
    }

    /// A scorer that records every anomaly it is fed and every advance, into a shared trace.
    #[derive(Default)]
    struct TracingScorer {
        trace: Trace,
        fed: Vec<Anomaly>,
        advances: Vec<i64>,
        pending: Vec<String>,
    }

    impl TracingScorer {
        fn new(trace: &Trace) -> Self {
            Self {
                trace: trace.clone(),
                ..Self::default()
            }
        }
    }

    impl Scorer for TracingScorer {
        type Output = String;

        fn process_anomaly(&mut self, anomaly: &Anomaly) {
            self.trace.push(format!("score:process:{}", anomaly.timestamp_sec));
            self.fed.push(anomaly.clone());
        }

        fn advance_to(&mut self, data_time_sec: i64) {
            self.trace.push(format!("score:advance:{data_time_sec}"));
            self.advances.push(data_time_sec);
            self.pending.push(format!("advanced:{data_time_sec}"));
        }

        fn reset(&mut self) {
            self.fed.clear();
            self.advances.clear();
            self.pending.clear();
        }

        fn take_pending_outputs(&mut self) -> Vec<Self::Output> {
            std::mem::take(&mut self.pending)
        }
    }

    /// An extractor that emits one virtual metric per log and can report evicted names.
    struct ScriptedExtractor {
        name: String,
        metric_name: String,
        context: Option<MetricContext>,
        evicted: Vec<String>,
    }

    impl ScriptedExtractor {
        fn new(name: &str, metric_name: &str) -> Self {
            Self {
                name: name.to_string(),
                metric_name: metric_name.to_string(),
                context: None,
                evicted: Vec::new(),
            }
        }
    }

    impl Extractor for ScriptedExtractor {
        fn name(&self) -> &str {
            &self.name
        }

        fn process_log(&mut self, _log: &LogObservation) -> ExtractorOutput {
            ExtractorOutput {
                metrics: vec![VirtualMetric {
                    name: self.metric_name.clone(),
                    value: 1.0,
                    host: None,
                    tags: vec!["env:test".to_string()],
                    context: self.context.clone(),
                }],
                evicted_metric_names: std::mem::take(&mut self.evicted),
            }
        }

        fn reset(&mut self) {}
    }

    fn metric(namespace: &str, name: &str, timestamp_sec: i64) -> MetricSample {
        MetricSample {
            name: name.to_string(),
            value: 1.0,
            host: Some("host-a".to_string()),
            tags: vec!["env:test".to_string()],
            timestamp_sec,
            source: namespace.to_string(),
        }
    }

    fn log(source: &str, timestamp_ms: i64) -> LogObservation {
        LogObservation {
            message: "hello".to_string(),
            status: "info".to_string(),
            tags: vec!["env:test".to_string()],
            hostname: "host-log".to_string(),
            timestamp_ms,
            source: source.to_string(),
        }
    }

    fn anomaly_for(series_ref: SeriesRef, aggregate: Aggregate, detector: &str, timestamp: i64) -> Anomaly {
        Anomaly {
            anomaly_type: crate::model::AnomalyType::Metric,
            series: SeriesDescriptor::new("ns", "metric_a", Some("host-a".to_string()), vec![], aggregate),
            series_ref: Some(QueryHandle::new(series_ref, aggregate)),
            detector_name: detector.to_string(),
            context: None,
            timestamp_sec: timestamp,
            score: None,
            sampling_interval_sec: 0,
            evidence: None,
        }
    }

    /// Builds a storage with a small point retention and (usually) a fixed series cap.
    fn storage_with(retention: i64, max_series: usize, ttl: i64, interval: i64) -> TimeSeriesStorage {
        let config = crate::config::StorageConfig {
            point_retention_secs: retention,
            max_series,
            inactive_series_ttl_secs: ttl,
            inactive_series_check_interval_secs: interval,
            ..crate::config::StorageConfig::default()
        };
        TimeSeriesStorage::new(config)
    }

    #[test]
    fn advance_only_to_t_minus_one_and_only_when_newer() {
        let trace = Trace::default();
        let storage = storage_with(120, 0, 0, 0);
        let detector = ScriptedDetector::new("d1", &trace);
        let config: EngineConfig = EngineConfig {
            storage,
            detectors: vec![Box::new(detector)],
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        // Data second 100 advances to 99 (the last second is never analyzed before it is complete).
        engine.ingest_metric_and_advance(&metric("ns", "metric_a", 100));
        // The same second again is not newer, so no second advance.
        engine.ingest_metric_and_advance(&metric("ns", "metric_a", 100));
        // A newer second advances to 100.
        engine.ingest_metric_and_advance(&metric("ns", "metric_a", 101));

        assert_eq!(
            trace.events(),
            vec!["detect:d1:99".to_string(), "detect:d1:100".to_string()]
        );
        assert_eq!(engine.last_analyzed_data_time(), 100);
    }

    #[test]
    fn synchronous_log_stream_advances_and_flushes() {
        let trace = Trace::default();
        let storage = storage_with(0, 0, 0, 0);
        let detector = ScriptedDetector::new("recorder", &trace);
        let extractor = ScriptedExtractor::new("logs", "log.count");
        let config: EngineConfig = EngineConfig {
            storage,
            detectors: vec![Box::new(detector)],
            extractors: vec![Box::new(extractor)],
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        engine.ingest_log_and_advance(&log("parquet", 10_000));
        engine.ingest_log_and_advance(&log("parquet", 12_000));
        let flushed = engine.finish_stream();

        assert_eq!(
            trace.events(),
            vec![
                "detect:recorder:9".to_string(),
                "detect:recorder:11".to_string(),
                "detect:recorder:12".to_string(),
            ]
        );
        assert_eq!(engine.last_analyzed_data_time(), 12);
        assert!(flushed.anomalies.is_empty());
    }

    #[test]
    fn eof_flush_advances_through_latest_observed_time_only() {
        let trace = Trace::default();
        let storage = storage_with(120, 0, 0, 0);
        let detector = ScriptedDetector::new("record", &trace);
        let scorer = TracingScorer::new(&trace);
        let config = EngineConfig {
            storage,
            detectors: vec![Box::new(detector)],
            scorer: Some(scorer),
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        // Ingest a data point at second 500; the input-driven advance goes to 499.
        engine.ingest_metric_and_advance(&metric("ns", "metric_a", 500));
        assert_eq!(engine.last_analyzed_data_time(), 499);

        let flushed = engine.finish_stream();
        // Exactly one final advance, through the latest observed data time; no invented recovery second.
        assert_eq!(engine.last_analyzed_data_time(), 500);
        assert_eq!(
            trace.events(),
            vec![
                "detect:record:499".to_string(),
                "score:advance:499".to_string(),
                "detect:record:500".to_string(),
                "score:advance:500".to_string(),
            ]
        );
        assert_eq!(flushed.scorer_outputs, vec!["advanced:500".to_string()]);
    }

    #[test]
    fn per_advance_ordering_is_preserved() {
        let trace = Trace::default();
        // Retention 5s lets a dedup entry expire in the second advance; max_series 2 makes capacity
        // eviction fire once a third series is ingested; ttl 1000 with interval 1 evicts only the stale
        // series.
        let mut storage = storage_with(5, 2, 1_000, 1);
        let series_a = storage
            .add("ns", "metric_a", Some("host-a"), 1.0, 2_000, &[])
            .series_ref
            .unwrap();
        // A stale series (last activity far behind) is evicted as inactive on the first advance.
        storage.add("ns", "metric_b", Some("host-a"), 1.0, 0, &[]);
        let mut context = MetricContext {
            pattern: "pattern".to_string(),
            example: "example".to_string(),
            source: "logs".to_string(),
            split_tags: BTreeMap::new(),
        };
        context.split_tags.insert("k".to_string(), "v".to_string());
        storage.set_context(series_a, context.clone());

        let mut detector_a = ScriptedDetector::new("alpha", &trace);
        // Detector alpha emits: a valid new anomaly, an invalid (ref-less) anomaly, and a duplicate of the
        // valid one — all within a single pass.
        let valid = anomaly_for(series_a, Aggregate::Average, "alpha", 1_900);
        let mut invalid = valid.clone();
        invalid.series_ref = None;
        detector_a.emitted = vec![valid.clone(), invalid, valid.clone()];
        let detector_b = ScriptedDetector::new("beta", &trace);

        let diagnostics = TraceDiagnostics { trace: trace.clone() };
        let config = EngineConfig {
            storage,
            detectors: vec![Box::new(detector_a), Box::new(detector_b)],
            scorer: Some(TracingScorer::new(&trace)),
            diagnostics: Some(Box::new(diagnostics)),
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        // Advance #1: inactive eviction, then detectors in registry order, then scorer fed + advanced.
        engine.advance(2_000);

        // Ingest two more series so the live count (A, C, D) exceeds max_series (2) at the end of advance
        // #2.
        engine.ingest_metric(&metric("ns", "metric_c", 2_001));
        engine.ingest_metric(&metric("ns", "metric_d", 2_001));

        // Advance #2: dedup expires the entry from advance #1 (its source series is still active), then
        // detectors run again and re-emit the same anomaly (now accepted), then capacity eviction fires.
        engine.advance(2_001);

        let events = trace.events();
        let advance_1: Vec<&String> = events
            .iter()
            .take_while(|event| event.as_str() != "advance:2001:manual")
            .collect();
        assert_eq!(
            advance_1,
            vec![
                &"advance:2000:manual".to_string(),
                &"evict:inactive:1".to_string(),
                &"remove:alpha".to_string(),
                &"remove:beta".to_string(),
                &"detect:alpha:2000".to_string(),
                &"score:process:1900".to_string(),
                &"detect:beta:2000".to_string(),
                &"score:advance:2000".to_string(),
            ]
        );

        let advance_2: Vec<&String> = events
            .iter()
            .skip_while(|event| event.as_str() != "advance:2001:manual")
            .collect();
        assert_eq!(
            advance_2,
            vec![
                &"advance:2001:manual".to_string(),
                &"dedup:retention:1".to_string(),
                &"detect:alpha:2001".to_string(),
                &"score:process:1900".to_string(),
                &"detect:beta:2001".to_string(),
                &"score:advance:2001".to_string(),
                &"capacity_hit".to_string(),
                &"evict:capacity:2".to_string(),
                &"dedup:series_evicted:1".to_string(),
                &"remove:alpha".to_string(),
                &"remove:beta".to_string(),
            ]
        );
    }

    #[test]
    fn valid_anomaly_is_enriched_with_series_context() {
        let trace = Trace::default();
        let mut storage = storage_with(120, 0, 0, 0);
        let series = storage
            .add("ns", "metric_a", Some("host-a"), 1.0, 100, &[])
            .series_ref
            .unwrap();
        let mut context = MetricContext {
            pattern: "p".to_string(),
            example: "e".to_string(),
            source: "logs".to_string(),
            split_tags: BTreeMap::new(),
        };
        context.split_tags.insert("k".to_string(), "v".to_string());
        storage.set_context(series, context.clone());

        let mut detector = ScriptedDetector::new("d1", &trace);
        detector.emitted = vec![anomaly_for(series, Aggregate::Average, "d1", 100)];
        let scorer = TracingScorer::new(&trace);
        let config = EngineConfig {
            storage,
            detectors: vec![Box::new(detector)],
            scorer: Some(scorer),
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        let result = engine.advance(200);
        assert_eq!(result.anomalies.len(), 1);
        assert_eq!(result.anomalies[0].context.as_ref(), Some(&context));
        // The scorer saw the enriched anomaly, not the raw detector output.
        assert_eq!(engine.scorer().unwrap().fed.len(), 1);
        assert_eq!(engine.scorer().unwrap().fed[0].context.as_ref(), Some(&context));
    }

    #[test]
    fn invalid_ref_anomaly_is_discarded_before_the_scorer() {
        let trace = Trace::default();
        let storage = storage_with(120, 0, 0, 0);
        let mut detector = ScriptedDetector::new("d1", &trace);
        let mut invalid = anomaly_for(SeriesRef::new(7), Aggregate::Average, "d1", 100);
        invalid.series_ref = None;
        detector.emitted = vec![invalid];
        let config = EngineConfig {
            storage,
            detectors: vec![Box::new(detector)],
            scorer: Some(TracingScorer::new(&trace)),
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        let result = engine.advance(200);
        assert!(result.anomalies.is_empty());
        assert!(engine.scorer().unwrap().fed.is_empty());
        // The scorer still advanced once.
        assert_eq!(engine.scorer().unwrap().advances, vec![200]);
    }

    #[test]
    fn dedup_suppresses_duplicates_and_expiry_releases_them() {
        let trace = Trace::default();
        let storage = storage_with(5, 0, 0, 0);
        let mut detector = ScriptedDetector::new("d1", &trace);
        detector.emitted = vec![anomaly_for(SeriesRef::new(0), Aggregate::Average, "d1", 200)];
        let config = EngineConfig {
            storage,
            detectors: vec![Box::new(detector)],
            scorer: Some(TracingScorer::new(&trace)),
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        // First advance accepts the anomaly; the next re-emits it (same key) within retention and it is
        // suppressed.
        assert_eq!(engine.advance(200).anomalies.len(), 1);
        assert_eq!(engine.advance(201).anomalies.len(), 0);
        // Third advance is past the retention expiry (200 + 5), so the entry is gone and it is accepted.
        assert_eq!(engine.advance(206).anomalies.len(), 1);
        assert_eq!(engine.total_anomaly_count(), 3);
    }

    #[test]
    fn dedup_key_includes_detector_and_aggregate() {
        let trace = Trace::default();
        let storage = storage_with(0, 0, 0, 0);
        let mut detector_a = ScriptedDetector::new("alpha", &trace);
        detector_a.emitted = vec![anomaly_for(SeriesRef::new(0), Aggregate::Average, "alpha", 100)];
        let mut detector_b = ScriptedDetector::new("beta", &trace);
        detector_b.emitted = vec![
            anomaly_for(SeriesRef::new(0), Aggregate::Average, "beta", 100),
            anomaly_for(SeriesRef::new(0), Aggregate::Sum, "beta", 100),
        ];
        let config = EngineConfig {
            storage,
            detectors: vec![Box::new(detector_a), Box::new(detector_b)],
            scorer: Some(TracingScorer::new(&trace)),
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        // Same ref/timestamp but different detector and different aggregate: all three are distinct.
        let result = engine.advance(200);
        assert_eq!(result.anomalies.len(), 3);
    }

    #[test]
    fn series_removal_fans_out_to_detectors() {
        let trace = Trace::default();
        let storage = storage_with(0, 0, 0, 0);
        let mut extractor = ScriptedExtractor::new("logs", "log.count");
        extractor.evicted = vec!["log.pattern".to_string()];
        let detector = ScriptedDetector::new("d1", &trace);
        let config: EngineConfig = EngineConfig {
            storage,
            detectors: vec![Box::new(detector)],
            extractors: vec![Box::new(extractor)],
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        // Seed a series named "log.pattern" in the extractor namespace so the eviction has something to
        // free, then ingest a log whose extractor reports it as evicted.
        let freed = engine
            .storage_mut()
            .add("logs", "log.pattern", Some("host-log"), 1.0, 10, &[])
            .series_ref
            .unwrap();
        engine.ingest_log(&log("parquet", 20_000));

        // The extractor's removal reached storage and fanned out to the detector.
        assert!(engine.storage().series_meta(freed).is_none());
        assert!(trace.events().iter().any(|event| event == "remove:d1"));
    }

    #[test]
    fn extractor_eviction_reaches_detector_state() {
        struct RecordingDetector {
            removed: Rc<RefCell<Vec<SeriesRef>>>,
        }
        impl Detector for RecordingDetector {
            type Config = ();
            fn name(&self) -> &str {
                "rec"
            }
            fn config(&self) -> &Self::Config {
                &()
            }
            fn is_ready(&self) -> bool {
                true
            }
            fn detect(&mut self, _view: &dyn StorageView, _data_time_sec: i64) -> Vec<Anomaly> {
                Vec::new()
            }
            fn reset(&mut self) {}
            fn remove_series(&mut self, series: &[SeriesRef]) {
                self.removed.borrow_mut().extend_from_slice(series);
            }
        }

        let removed = Rc::new(RefCell::new(Vec::new()));
        let storage = storage_with(0, 0, 0, 0);
        let mut extractor = ScriptedExtractor::new("logs", "log.count");
        extractor.evicted = vec!["log.pattern".to_string()];
        let config: EngineConfig = EngineConfig {
            storage,
            detectors: vec![Box::new(RecordingDetector {
                removed: removed.clone(),
            })],
            extractors: vec![Box::new(extractor)],
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        let freed = engine
            .storage_mut()
            .add("logs", "log.pattern", Some("host-log"), 1.0, 10, &[])
            .series_ref
            .unwrap();
        assert!(engine.storage().series_meta(freed).is_some());

        engine.ingest_log(&log("parquet", 200));

        assert_eq!(&*removed.borrow(), &[freed]);
        assert!(engine.storage().series_meta(freed).is_none());
    }

    #[test]
    fn invalid_refs_after_eviction_do_not_panic() {
        let trace = Trace::default();
        let storage = storage_with(120, 0, 0, 0);
        // A ref that was never allocated, standing in for a ref evicted before the detector reported it.
        let stale = SeriesRef::new(9_999);
        let mut detector = ScriptedDetector::new("d1", &trace);
        detector.emitted = vec![anomaly_for(stale, Aggregate::Average, "d1", 100)];
        let config = EngineConfig {
            storage,
            detectors: vec![Box::new(detector)],
            scorer: Some(TracingScorer::new(&trace)),
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        // Enrichment finds no context, dedup expiry falls back to the store-wide retention, and the anomaly
        // is still processed. None of this may panic.
        let result = engine.advance(200);
        assert_eq!(result.anomalies.len(), 1);
        assert_eq!(result.anomalies[0].context, None);

        // Evicting refs the detector and deduper never saw is likewise a no-op.
        engine.storage_mut().remove_series_by_refs(&[stale]);
        engine.advance(201);
    }

    #[test]
    fn reset_restores_a_fresh_replay_state() {
        let trace = Trace::default();
        let storage = storage_with(120, 0, 0, 0);
        let mut detector = ScriptedDetector::new("d1", &trace);
        detector.once = false;
        detector.emitted = vec![anomaly_for(SeriesRef::new(0), Aggregate::Average, "d1", 100)];
        let scorer = TracingScorer::new(&trace);
        let config = EngineConfig {
            storage,
            detectors: vec![Box::new(detector)],
            extractors: vec![Box::new(ScriptedExtractor::new("logs", "log.count"))],
            scorer: Some(scorer),
            track_anomaly_history: true,
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        engine.ingest_metric(&metric("ns", "metric_a", 500));
        engine.advance(200);
        assert_eq!(engine.total_anomaly_count(), 1);
        assert_eq!(engine.raw_anomalies().len(), 1);

        engine.reset();

        assert_eq!(engine.last_analyzed_data_time(), 0);
        assert_eq!(engine.latest_data_time(), 0);
        assert_eq!(engine.total_anomaly_count(), 0);
        assert_eq!(engine.raw_anomalies().len(), 0);
        assert_eq!(engine.storage().total_series_count(), 0);
        assert_eq!(engine.detectors()[0].name(), "d1");
        // The same anomaly is accepted again after the reset (dedup state was cleared).
        assert_eq!(engine.advance(200).anomalies.len(), 1);
    }

    #[test]
    fn duplicate_extractor_names_panic() {
        let first = ScriptedExtractor::new("logs", "a");
        let second = ScriptedExtractor::new("logs", "b");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let config: EngineConfig = EngineConfig {
                extractors: vec![Box::new(first), Box::new(second)],
                ..EngineConfig::new(TimeSeriesStorage::default())
            };
            let _ = Engine::with_config(config);
        }));
        assert!(result.is_err());
    }

    #[test]
    fn diagnostics_are_optional_and_do_not_change_output() {
        // Without a diagnostics sink, the engine still runs every step and returns the same anomalies.
        let storage = storage_with(120, 0, 0, 0);
        let mut detector = ScriptedDetector::new("d1", &Trace::default());
        detector.emitted = vec![anomaly_for(SeriesRef::new(0), Aggregate::Average, "d1", 100)];
        let config: EngineConfig = EngineConfig {
            storage,
            detectors: vec![Box::new(detector)],
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);
        assert_eq!(engine.advance(200).anomalies.len(), 1);
    }

    #[test]
    fn deduper_live_mode_evicts_lru_on_capacity() {
        let mut deduper = AnomalyDeduper::with_capacity(2);
        let key = |series: u64, timestamp: i64| AnomalyDedupKey {
            source_ref: series,
            source_aggregate: Aggregate::Average,
            detector_name: "d".to_string(),
            timestamp,
        };

        assert_eq!(deduper.accept(key(0, 1), 0), (true, 0));
        assert_eq!(deduper.accept(key(1, 1), 0), (true, 0));
        // Touch key 0 so key 1 becomes the least-recently-used entry.
        assert_eq!(deduper.accept(key(0, 1), 0), (false, 0));
        // Adding a third entry evicts the LRU (key 1); the new entry is still accepted.
        assert_eq!(deduper.accept(key(2, 1), 0), (true, 1));
        assert_eq!(deduper.accept(key(2, 1), 0), (false, 0));
        // Key 1 was evicted, so it is accepted again; this insert evicts the now-LRU key 0.
        assert_eq!(deduper.accept(key(1, 1), 0), (true, 1));
    }

    #[test]
    fn deduper_replay_mode_is_unbounded_and_never_expires() {
        let mut deduper = AnomalyDeduper::with_capacity(0);
        let key = AnomalyDedupKey {
            source_ref: 0,
            source_aggregate: Aggregate::Average,
            detector_name: "d".to_string(),
            timestamp: 1,
        };
        assert_eq!(deduper.accept(key.clone(), 10), (true, 0));
        assert_eq!(deduper.accept(key, 10), (false, 0));
        // Replay mode ignores expiry and series removal, keeping complete dedup history.
        assert_eq!(deduper.remove_expired(1_000_000), 0);
        assert_eq!(deduper.remove_source_refs(&[SeriesRef::new(0)]), 0);
    }

    #[test]
    fn deduper_remove_expired_respects_the_next_expiry_gate() {
        let mut deduper = AnomalyDeduper::with_capacity(8);
        let key = |series: u64| AnomalyDedupKey {
            source_ref: series,
            source_aggregate: Aggregate::Average,
            detector_name: "d".to_string(),
            timestamp: 0,
        };
        deduper.accept(key(0), 10);
        deduper.accept(key(1), 20);
        // Before the earliest expiry, nothing is removed.
        assert_eq!(deduper.remove_expired(10), 0);
        // Past the first expiry, only the first entry is removed.
        assert_eq!(deduper.remove_expired(11), 1);
        // Past the second, the rest goes.
        assert_eq!(deduper.remove_expired(21), 1);
    }

    #[test]
    fn deduper_remove_source_refs_drops_only_matching_entries() {
        let mut deduper = AnomalyDeduper::with_capacity(8);
        let key = |series: u64| AnomalyDedupKey {
            source_ref: series,
            source_aggregate: Aggregate::Average,
            detector_name: "d".to_string(),
            timestamp: 0,
        };
        deduper.accept(key(0), 0);
        deduper.accept(key(1), 0);
        assert_eq!(deduper.remove_source_refs(&[SeriesRef::new(0)]), 1);
        assert_eq!(deduper.accept(key(0), 0), (true, 0));
        assert_eq!(deduper.accept(key(1), 0), (false, 0));
    }

    #[test]
    fn tracked_history_records_raw_and_detector_output_anomalies() {
        let storage = storage_with(120, 0, 0, 0);
        let mut detector = ScriptedDetector::new("d1", &Trace::default());
        let mut invalid = anomaly_for(SeriesRef::new(0), Aggregate::Average, "d1", 100);
        invalid.series_ref = None;
        invalid.detector_name.clear();
        detector.emitted = vec![anomaly_for(SeriesRef::new(0), Aggregate::Average, "d1", 100), invalid];
        let config: EngineConfig = EngineConfig {
            storage,
            detectors: vec![Box::new(detector)],
            track_anomaly_history: true,
            track_detector_output_history: true,
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        engine.advance(200);

        // Raw history keeps only accepted anomalies; detector-output history keeps every raw output,
        // including the invalid one, with the detector name filled in.
        assert_eq!(engine.raw_anomalies().len(), 1);
        assert_eq!(engine.detector_output_anomalies().len(), 2);
        assert_eq!(engine.detector_output_anomalies()[1].detector_name, "d1");
        assert_eq!(engine.unique_anomaly_source_count(), 1);
    }

    #[test]
    fn replay_stored_data_advances_at_stored_timestamps_and_flushes() {
        let trace = Trace::default();
        let mut storage = storage_with(120, 0, 0, 0);
        for second in [100, 102] {
            storage.add("ns", "metric_a", Some("host-a"), 1.0, second, &[]);
        }
        let detector = ScriptedDetector::new("rec", &trace);
        let config = EngineConfig {
            storage,
            detectors: vec![Box::new(detector)],
            scorer: Some(TracingScorer::new(&trace)),
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        engine.replay_stored_data();

        assert_eq!(
            trace.events(),
            vec![
                "detect:rec:99".to_string(),
                "score:advance:99".to_string(),
                "detect:rec:101".to_string(),
                "score:advance:101".to_string(),
                // Replay-end flush to the latest stored timestamp.
                "detect:rec:102".to_string(),
                "score:advance:102".to_string(),
            ]
        );
    }

    #[test]
    fn advance_reason_is_recorded_in_the_trace() {
        let trace = Trace::default();
        let storage = storage_with(120, 0, 0, 0);
        let diagnostics = TraceDiagnostics { trace: trace.clone() };
        let config: EngineConfig = EngineConfig {
            storage,
            diagnostics: Some(Box::new(diagnostics)),
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        engine.advance(50);
        // A stale advance is skipped, recording its reason and producing no output.
        let skipped = engine.advance_with_reason(10, AdvanceReason::Manual);
        assert!(skipped.anomalies.is_empty());
        assert_eq!(
            trace.events(),
            vec!["advance:50:manual".to_string(), "skip:manual".to_string()]
        );
    }

    #[test]
    fn anomaly_evidence_is_carried_through_untouched() {
        let trace = Trace::default();
        let storage = storage_with(120, 0, 0, 0);
        let mut detector = ScriptedDetector::new("d1", &trace);
        let mut anomaly = anomaly_for(SeriesRef::new(0), Aggregate::Average, "d1", 100);
        anomaly.evidence = Some(AnomalyEvidence::TukeyBiweight {
            baseline_median: 1.0,
            baseline_mad: 2.0,
            current_value: 3.0,
            deviation_sigma: 4.0,
            sample_count: 5,
            z_score: 6.0,
        });
        detector.emitted = vec![anomaly];
        let config: EngineConfig = EngineConfig {
            storage,
            detectors: vec![Box::new(detector)],
            ..EngineConfig::new(TimeSeriesStorage::default())
        };
        let mut engine = Engine::with_config(config);

        let result = engine.advance(200);
        assert!(matches!(
            result.anomalies[0].evidence,
            Some(AnomalyEvidence::TukeyBiweight { sample_count: 5, .. })
        ));
    }
}
