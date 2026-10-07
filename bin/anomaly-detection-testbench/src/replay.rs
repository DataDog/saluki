//! Headless replay driver: feeds a scenario through the ordered engine and collects its output.
//!
//! Two replay modes mirror the Go testbench:
//!
//! * **Streaming** (the default) reads the scenario lazily in merged timestamp order and advances the
//!   engine as observations arrive, then flushes at end of input.
//! * **Retained** (`--retain-parquet`) preloads every row sorted by timestamp, runs the same engine
//!   pass over the stored data, and exists for recordings whose rows are not already ordered.
//!
//! In both modes the replay owns its diagnostic collectors: a score-tick hook captures one
//! [`ScoreTick`] per scorer second, and scorer episode events are merged by pattern into the
//! correlation set. Retained preloading advances the engine, so its diagnostic output is discarded
//! when analysis state is reset before the authoritative replay pass.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use saluki_anomaly_detection::config::StorageConfig;
use saluki_anomaly_detection::engine::{AdvanceResult, AnyDetector, Engine, EngineConfig};
use saluki_anomaly_detection::model::{Anomaly, LogObservation as ModelLogObservation, MetricSample};
use saluki_anomaly_detection::scorer::{ActiveCorrelation, AnomalyScorer, CorrelatorEvent, ScoreTick};
use saluki_anomaly_detection::storage::TimeSeriesStorage;

use crate::config::ResolvedSettings;
use crate::detector::DetectorFactory;
use crate::parquet::{
    load_all, open_stream, LoadError, LoadOptions, Observation, ParquetFormat, REPLAY_INGESTION_SOURCE,
};

/// The replay mode used for a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayMode {
    /// Read lazily in merged timestamp order and advance as observations arrive.
    Streaming,
    /// Preload and sort every row, then replay stored data.
    Retained,
}

impl ReplayMode {
    /// Returns the wire label used in the exported metadata.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Streaming => "streaming",
            Self::Retained => "retained",
        }
    }
}

/// An error raised while running a replay.
#[derive(Debug)]
pub enum ReplayError {
    /// Loading the scenario failed.
    Load(LoadError),
    /// A detector could not be constructed.
    Detector(String),
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Load(err) => write!(f, "{err}"),
            Self::Detector(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ReplayError {}

impl From<LoadError> for ReplayError {
    fn from(err: LoadError) -> Self {
        Self::Load(err)
    }
}

/// Everything a headless run collects for export.
pub struct ReplayResult {
    /// Correlation episodes (closed plus still-open at end of input), sorted by first-seen second then
    /// pattern.
    pub correlations: Vec<ActiveCorrelation>,
    /// Every pre-pipeline detector output, in emission order.
    pub detector_anomalies: Vec<Anomaly>,
    /// Whether the detector ledger was requested; controls whether export emits it (even when empty).
    pub detector_anomalies_requested: bool,
    /// One tick per scorer second, in ascending second order.
    pub score_timeline: Vec<ScoreTick>,
    /// Number of accepted metric observations.
    pub input_metrics_count: usize,
    /// Number of unique metric series (name, host, sorted tags).
    pub input_metrics_cardinality: usize,
    /// Number of log observations.
    pub input_logs_count: usize,
    /// Total number of accepted detector anomalies.
    pub input_anomalies_count: usize,
    /// First observed data second.
    pub timeline_start: i64,
    /// Last observed data second.
    pub timeline_end: i64,
    /// Enabled detectors this build has no implementation for.
    pub unlinked_detectors: Vec<String>,
    /// The replay mode.
    pub replay_mode: ReplayMode,
    /// Files skipped by the retained loader because they could not be decoded.
    pub skipped_files: Vec<PathBuf>,
}

/// Runs a headless replay of `data_dir`.
pub fn run_replay<F: DetectorFactory>(
    data_dir: &Path, format: ParquetFormat, load: &LoadOptions, retained: bool, settings: &ResolvedSettings,
    include_detector_anomalies: bool, factory: &F,
) -> Result<ReplayResult, ReplayError> {
    let (detectors, unlinked_detectors) = build_detectors(settings, factory)?;

    let tick_log: Rc<RefCell<Vec<ScoreTick>>> = Rc::new(RefCell::new(Vec::new()));
    let scorer = build_scorer(settings, &tick_log);
    let mut engine = Engine::with_config(engine_config(
        storage_config(retained),
        detectors,
        scorer,
        include_detector_anomalies,
    ));

    let mut stats = InputStats::default();
    let mut correlations: BTreeMap<String, ActiveCorrelation> = BTreeMap::new();
    let mut skipped_files = Vec::new();

    let (timeline_start, timeline_end) = if retained {
        let loaded = load_all(data_dir, format, load)?;
        skipped_files = loaded.skipped_files.clone();
        preload_retained(&mut engine, &loaded, &mut stats);
        // Discard the preload pass's diagnostics; the replay pass below is authoritative.
        tick_log.borrow_mut().clear();
        correlations.clear();
        engine.reset_analysis_state();
        let result = engine.replay_stored_data();
        collect_events(result, &mut correlations);
        stats.bounds
    } else {
        let mut stream = open_stream(data_dir, format, load)?;
        for observation in &mut stream {
            let observation = observation?;
            stats.observe(&observation);
            match observation {
                Observation::Metric(metric) => {
                    let result = engine.ingest_metric_and_advance(&metric_sample(&metric));
                    collect_events(result, &mut correlations);
                }
                Observation::Log(log) => {
                    let result = engine.ingest_log_and_advance(&model_log(&log));
                    collect_events(result, &mut correlations);
                }
            }
        }
        let result = engine.finish_stream();
        collect_events(result, &mut correlations);
        stats.bounds
    };

    if let Some(scorer) = engine.scorer() {
        for correlation in scorer.active_correlations() {
            merge_correlation(&mut correlations, correlation);
        }
    }

    let mut correlations: Vec<ActiveCorrelation> = correlations.into_values().collect();
    correlations.sort_by(|left, right| {
        left.first_seen
            .cmp(&right.first_seen)
            .then_with(|| left.pattern.cmp(&right.pattern))
    });

    let score_timeline = tick_log.borrow().clone();
    let detector_anomalies = if include_detector_anomalies {
        engine.detector_output_anomalies().to_vec()
    } else {
        Vec::new()
    };
    let input_anomalies_count = engine.raw_anomalies().len();

    Ok(ReplayResult {
        correlations,
        detector_anomalies,
        detector_anomalies_requested: include_detector_anomalies,
        score_timeline,
        input_metrics_count: stats.metrics,
        input_metrics_cardinality: stats.metric_series.len(),
        input_logs_count: stats.logs,
        input_anomalies_count,
        timeline_start,
        timeline_end,
        unlinked_detectors,
        replay_mode: if retained {
            ReplayMode::Retained
        } else {
            ReplayMode::Streaming
        },
        skipped_files,
    })
}

/// Detectors constructed from the resolved settings, plus the enabled detectors this build could not
/// link.
type BuiltDetectors = (Vec<Box<dyn AnyDetector>>, Vec<String>);

fn build_detectors<F: DetectorFactory>(
    settings: &ResolvedSettings, factory: &F,
) -> Result<BuiltDetectors, ReplayError> {
    let mut detectors = Vec::new();
    let mut unlinked = Vec::new();
    for name in settings.detectors_enabled() {
        let params = settings
            .component(&name)
            .and_then(|component| component.params.as_ref());
        match factory.create(&name, params).map_err(ReplayError::Detector)? {
            Some(detector) => detectors.push(detector),
            None => unlinked.push(name),
        }
    }
    Ok((detectors, unlinked))
}

fn build_scorer(settings: &ResolvedSettings, tick_log: &Rc<RefCell<Vec<ScoreTick>>>) -> Option<AnomalyScorer> {
    if !settings.enabled("anomaly_scorer") {
        return None;
    }
    let mut scorer = AnomalyScorer::new(settings.scorer().clone());
    let log = Rc::clone(tick_log);
    scorer.set_tick_hook(Box::new(move |tick| log.borrow_mut().push(*tick)));
    Some(scorer)
}

/// Builds the engine storage configuration.
///
/// Streaming keeps the production point retention but disables inactivity eviction so the replay does
/// not drop series mid-scenario; retained mode additionally disables point retention because the whole
/// scenario stays resident.
fn storage_config(retained: bool) -> StorageConfig {
    let mut config = StorageConfig {
        inactive_series_ttl_secs: 0,
        inactive_series_check_interval_secs: 0,
        ..StorageConfig::default()
    };
    if retained {
        config.point_retention_secs = 0;
    }
    config
}

fn engine_config(
    storage: StorageConfig, detectors: Vec<Box<dyn AnyDetector>>, scorer: Option<AnomalyScorer>,
    include_detector_anomalies: bool,
) -> EngineConfig<AnomalyScorer> {
    let mut config = EngineConfig::new(TimeSeriesStorage::new(storage));
    config.detectors = detectors;
    config.scorer = scorer;
    config.track_anomaly_history = true;
    config.track_detector_output_history = include_detector_anomalies;
    config
}

/// Preloads retained rows into the engine: metrics advance (as Go's `IngestMetricSync` does), logs only
/// build extractor and storage state (as Go's `IngestLogForReplay` does).
fn preload_retained(
    engine: &mut Engine<AnomalyScorer>, loaded: &crate::parquet::LoadedScenario, stats: &mut InputStats,
) {
    for metric in &loaded.metrics {
        stats.observe_metric(metric);
        let _ = engine.ingest_metric_and_advance(&metric_sample(metric));
    }
    for log in &loaded.logs {
        stats.observe_log(log);
        let _ = engine.ingest_log(&model_log(log));
    }
}

fn metric_sample(metric: &crate::parquet::MetricObservation) -> MetricSample {
    MetricSample {
        name: metric.name.clone(),
        value: metric.value,
        host: Some(metric.host.clone()),
        tags: metric.tags.to_vec(),
        timestamp_sec: metric.timestamp_sec,
        source: REPLAY_INGESTION_SOURCE.to_string(),
    }
}

fn model_log(log: &crate::parquet::LogObservation) -> ModelLogObservation {
    ModelLogObservation {
        message: String::from_utf8_lossy(&log.message).into_owned(),
        status: log.status.clone(),
        tags: log.tags.to_vec(),
        hostname: log.hostname.clone(),
        timestamp_ms: log.timestamp_ms,
        source: REPLAY_INGESTION_SOURCE.to_string(),
    }
}

fn collect_events(result: AdvanceResult<CorrelatorEvent>, correlations: &mut BTreeMap<String, ActiveCorrelation>) {
    for event in result.scorer_outputs {
        merge_correlation(correlations, event.correlation);
    }
}

/// Merges a correlation into the pattern map, keeping the one with more anomalies and, on a tie, the
/// more recent `last_updated`, matching the Go correlation-history merge rule.
fn merge_correlation(map: &mut BTreeMap<String, ActiveCorrelation>, correlation: ActiveCorrelation) {
    match map.get(&correlation.pattern) {
        Some(existing)
            if existing.anomalies.len() > correlation.anomalies.len()
                || (existing.anomalies.len() == correlation.anomalies.len()
                    && existing.last_updated >= correlation.last_updated) => {}
        _ => {
            map.insert(correlation.pattern.clone(), correlation);
        }
    }
}

/// Accumulates the replay input counts and the observed timeline bounds.
#[derive(Default)]
struct InputStats {
    metrics: usize,
    logs: usize,
    metric_series: HashSet<String>,
    bounds: (i64, i64),
    has_bounds: bool,
}

impl InputStats {
    fn observe(&mut self, observation: &Observation) {
        self.observe_second(observation.timestamp_sec());
        match observation {
            Observation::Metric(metric) => self.observe_metric(metric),
            Observation::Log(_) => self.logs += 1,
        }
    }

    fn observe_log(&mut self, log: &crate::parquet::LogObservation) {
        self.logs += 1;
        self.observe_second(log.timestamp_ms / 1000);
    }

    fn observe_metric(&mut self, metric: &crate::parquet::MetricObservation) {
        self.metrics += 1;
        self.observe_second(metric.timestamp_sec);
        let mut tags: Vec<&str> = metric.tags.iter().map(String::as_str).collect();
        tags.sort_unstable();
        self.metric_series
            .insert(format!("{}|{}|{}", metric.name, metric.host, tags.join(",")));
    }

    fn observe_second(&mut self, second: i64) {
        if !self.has_bounds {
            self.bounds = (second, second);
            self.has_bounds = true;
            return;
        }
        if second < self.bounds.0 {
            self.bounds.0 = second;
        }
        if second > self.bounds.1 {
            self.bounds.1 = second;
        }
    }
}
