//! The unified anomaly scorer: EWMA severity scoring, episodes, and episode contributors.
//!
//! This is a port of the Go `observer/impl/anomaly_scorer.go`. The scorer has three concerns:
//!
//! 1. **EWMA core**: buffers anomalies, maintains the per-series deduplication window, and computes the
//!    saturation + EWMA score per second.
//! 2. **Event manager**: a [`Dispatcher`] per subscription receives the scorer's derived per-second raw
//!    severity levels and applies each subscription's filter/cooldown.
//! 3. **Internal watcher** (optional, enabled with [`AnomalyScorerConfig::correlation_events`]):
//!    self-subscribes to the severity stream and tracks configured-severity episodes, emitting
//!    [`CorrelatorEvent`] lifecycle outputs.
//!
//! # Lifecycle
//!
//! ```text
//! process_anomaly → buffers the raw anomaly keyed by its (possibly clamped) second
//! advance_to(t)    → finalizes every second in (last_advanced_sec, t]:
//!                     merge pending anomalies into the window map,
//!                     evict stale per-level timestamps,
//!                     count unique live series at their highest live level,
//!                     compute saturation + EWMA,
//!                     derive the raw severity level;
//!                   then drives the dispatchers over the per-second states in order
//! take_pending_outputs → drains episode lifecycle events produced during the last advance
//! active_correlations → returns the open configured-threshold episode, when enabled
//! score_state      → returns the retained per-second buckets and the resolved settings
//! reset            → clears all internal state for reanalysis
//! ```
//!
//! The per-second states are computed **before** the dispatchers are driven, exactly as the Go
//! implementation does; this ordering is load-bearing for episode contributor snapshots and must not be
//! "repaired".

use std::collections::{BTreeMap, HashMap};

use super::dispatcher::Dispatcher;
use super::severity::{SeverityEvent, SeverityEventListener, SeverityEventsConfiguration, SeverityLevel};
use super::top_anomaly::TopAnomalyBuffer;
use crate::config::AnomalyScorerConfig;
use crate::identity::SeriesDescriptor;
use crate::model::Anomaly;
use crate::traits::Scorer;

/// The EWMA weight of each anomaly level (0–4), from the Go `levelWeights`.
///
/// Level 0 = VeryLow, 1 = Low, 2 = Medium, 3 = High, 4 = XHigh.
pub const LEVEL_WEIGHTS: [f64; 5] = [0.2, 0.5, 1.0, 2.0, 3.0];

/// The canonical label of each anomaly level, from the Go `anomalySeverityLabels`.
const ANOMALY_SEVERITY_LABELS: [&str; 5] = ["xlow", "low", "medium", "high", "xhigh"];

/// The lower clamp for [`AnomalyScorerConfig::max_reported_items`], from the Go `minMaxReportedItems`.
const MIN_MAX_REPORTED_ITEMS: usize = 10;

/// The upper clamp for [`AnomalyScorerConfig::max_reported_items`], from the Go `maxMaxReportedItems`.
const MAX_MAX_REPORTED_ITEMS: usize = 1000;

/// Returns the canonical label of an anomaly level (`xlow`…`xhigh`), or `unknown` when out of range.
pub fn anomaly_severity_label(level: usize) -> &'static str {
    ANOMALY_SEVERITY_LABELS.get(level).copied().unwrap_or("unknown")
}

/// Returns the initial (un-hysteresis'd) severity level of one EWMA tick.
///
/// Used only for the first advanced second, to seed the raw severity state.
fn raw_severity_level(ewma: f64, low: f64, high: f64) -> SeverityLevel {
    if ewma >= high {
        return SeverityLevel::High;
    }
    if ewma >= low {
        return SeverityLevel::Medium;
    }
    SeverityLevel::Low
}

/// Returns the next severity level given the current EWMA, the current state, and the thresholds with the
/// hysteresis margin applied to downward transitions.
///
/// Escalations use the bare thresholds; de-escalations require the EWMA to fall below
/// `threshold - margin` (`low - margin` from Medium, `high - margin` from High).
fn next_severity_level(ewma: f64, current: SeverityLevel, low: f64, high: f64, margin: f64) -> SeverityLevel {
    match current {
        SeverityLevel::Low => {
            if ewma >= high {
                SeverityLevel::High
            } else if ewma >= low {
                SeverityLevel::Medium
            } else {
                SeverityLevel::Low
            }
        }
        SeverityLevel::Medium => {
            if ewma >= high {
                SeverityLevel::High
            } else if ewma < low - margin {
                // De-escalate only when the EWMA drops below low - margin.
                SeverityLevel::Low
            } else {
                SeverityLevel::Medium
            }
        }
        SeverityLevel::High => {
            if ewma < high - margin {
                // De-escalate only when the EWMA drops below high - margin.
                if ewma >= low {
                    SeverityLevel::Medium
                } else {
                    SeverityLevel::Low
                }
            } else {
                SeverityLevel::High
            }
        }
    }
}

/// Returns the severity level that opens episodes for a configured threshold name (`medium` or `high`).
fn correlation_event_severity(threshold: &str) -> SeverityLevel {
    if threshold == "medium" {
        SeverityLevel::Medium
    } else {
        SeverityLevel::High
    }
}

/// Normalizes a correlation-event threshold name.
///
/// Returns `Some("high")` for empty or `"high"`, `Some("medium")` for `"medium"` (case-insensitive,
/// whitespace-trimmed), and `None` for `"low"` or any other value, mirroring the Go
/// `normalizeCorrelationEventThreshold`. `"low"` is invalid because Low is the scorer's no-evidence
/// baseline.
fn normalize_correlation_event_threshold(value: &str) -> Option<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "high" => Some("high".to_string()),
        "medium" => Some("medium".to_string()),
        _ => None,
    }
}

/// Returns the storage handle key of an anomaly, preferring the compact handle when the pipeline set one.
///
/// This is the Go `seriesID`: `SourceRef.CompactID()` when available, otherwise `Source.Key()`. The
/// result is never empty.
fn series_id(anomaly: &Anomaly) -> String {
    match anomaly.series_ref {
        Some(handle) => handle.compact_id(),
        None => anomaly.series.identity().key(),
    }
}

/// Returns the 0–4 anomaly level of an anomaly for the given configuration.
///
/// If the detector has calibrated thresholds, the numeric score is compared against the four boundaries.
/// A calibrated detector with an absent score is level 0 (VeryLow); detectors without calibration entries
/// (including unscored BOCPD) default to level 2 (Medium).
pub(crate) fn anomaly_level(anomaly: &Anomaly, config: &AnomalyScorerConfig) -> usize {
    if let Some(thresholds) = config.detector_thresholds.get(&anomaly.detector_name) {
        let Some(score) = anomaly.score else {
            return 0; // treat an absent score from a scored detector as VeryLow
        };
        for (level, threshold) in thresholds.iter().enumerate() {
            if score < *threshold {
                return level;
            }
        }
        return 4;
    }
    2 // detectors without explicit thresholds default to Medium
}

/// Returns the continuous contributor weight of an anomaly, interpolated between the adjacent calibrated
/// [`LEVEL_WEIGHTS`].
///
/// This is a separate quantity from the discrete EWMA level weight: it is used only to rank retained top
/// anomalies and to weight episode contributors; the scorer's EWMA keeps using the discrete calibrated
/// weights. Detectors without a calibration entry retain Medium's weight; an absent score from a
/// calibrated detector keeps VeryLow's weight.
pub(crate) fn contributor_weight(anomaly: &Anomaly, config: &AnomalyScorerConfig) -> f64 {
    let Some(thresholds) = config.detector_thresholds.get(&anomaly.detector_name) else {
        return LEVEL_WEIGHTS[2]; // uncalibrated detectors retain Medium's weight
    };
    let Some(score) = anomaly.score else {
        return LEVEL_WEIGHTS[0];
    };

    if score <= thresholds[0] {
        return LEVEL_WEIGHTS[0];
    }

    let mut lower_threshold = thresholds[0];
    for (level, upper_threshold) in thresholds.iter().enumerate() {
        if score < *upper_threshold {
            let span = upper_threshold - lower_threshold;
            if span <= 0.0 {
                return LEVEL_WEIGHTS[level];
            }
            let fraction = (score - lower_threshold) / span;
            return LEVEL_WEIGHTS[level] + fraction * (LEVEL_WEIGHTS[level + 1] - LEVEL_WEIGHTS[level]);
        }
        lower_threshold = *upper_threshold;
    }
    LEVEL_WEIGHTS[LEVEL_WEIGHTS.len() - 1]
}

/// One metric contributing to a scorer episode, with its aggregated weight and normalized share.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScorerContributor {
    /// The storage handle of the contributing series.
    pub handle: crate::identity::QueryHandle,
    /// The aggregated contributor weight of the series' retained anomalies.
    pub weight: f64,
    /// The weight's share of the selected top contributors' total, in `[0, 1]`.
    pub share: f64,
}

/// The per-second telemetry unit emitted by the scorer.
///
/// One bucket is produced for every 1-second tick, even when it has no anomalies.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnomalyScoreBucket {
    /// The Unix timestamp (floor) for this bucket.
    pub second: i64,
    /// The number of deduplicated anomalous series at each level (`bins[level]`, 0 = VeryLow … 4 = XHigh).
    pub bins: [usize; 5],
    /// The total number of anomalous series in this bucket (the sum of `bins`).
    pub count: usize,
    /// The sum of the discrete level weights of all anomalous series in this bucket.
    pub weight_sum: f64,
    /// The EWMA value after processing this bucket.
    pub ewma: f64,
}

/// A snapshot of the scorer's retained telemetry and resolved settings.
#[derive(Clone, Debug, PartialEq)]
pub struct AnomalyScoreState {
    /// The most recently retained per-second buckets (capped at
    /// [`AnomalyScorerConfig::max_buckets`] or [`AnomalyScorerConfig::window_secs`]).
    pub buckets: Vec<AnomalyScoreBucket>,
    /// The resolved configuration the scorer is running with.
    pub config: AnomalyScorerConfig,
}

/// A scorer severity episode: a period during which the delivered severity is at or above the configured
/// event threshold.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveCorrelation {
    /// The episode's pattern name, for example `anomaly_scorer_high:1789000000`.
    pub pattern: String,
    /// The display title of the episode.
    pub title: String,
    /// The series descriptors participating in the episode. The scorer never resolves members; the field
    /// exists for parity with the Go correlation model.
    pub members: Vec<SeriesDescriptor>,
    /// The anomalies that arrived while the episode was open, bounded by
    /// [`AnomalyScorerConfig::max_episode_anomalies`].
    pub anomalies: Vec<Anomaly>,
    /// When the episode opened (Unix seconds, from data).
    pub first_seen: i64,
    /// The most recent contributing signal (Unix seconds, from data).
    pub last_updated: i64,
}

/// The kind of a scorer lifecycle event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CorrelatorEventKind {
    /// The scorer entered its configured episode threshold.
    EpisodeStarted,
    /// The scorer left its configured episode threshold.
    EpisodeEnded,
}

/// A typed scorer lifecycle event produced during an advance.
///
/// Episode-start events carry a contributor snapshot taken at the moment of the transition; episode-end
/// events carry the closed episode with its final `last_updated` time.
#[derive(Clone, Debug, PartialEq)]
pub struct CorrelatorEvent {
    /// The kind of lifecycle event.
    pub kind: CorrelatorEventKind,
    /// The name of the correlator that produced the event (`anomaly_scorer`).
    pub correlator_name: String,
    /// The data time (Unix seconds) when the event occurred.
    pub timestamp_sec: i64,
    /// The episode associated with the event.
    pub correlation: ActiveCorrelation,
    /// The severity transition that produced the event.
    pub from_level: SeverityLevel,
    /// The severity level after the transition.
    pub to_level: SeverityLevel,
    /// The contributor snapshot, populated only for [`CorrelatorEventKind::EpisodeStarted`] events.
    pub contributors: Vec<ScorerContributor>,
}

/// One diagnostic score tick, captured for every scorer second (including empty seconds).
///
/// The tick pairs the per-second calculation with the raw severity derived from the EWMA and the
/// delivered severity of the internal episode-watcher subscription (the level after the scorer's
/// configured cooldown). `delivered_severity` is `None` when episode tracking is disabled and there is
/// therefore no watcher subscription.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScoreTick {
    /// The scorer second this tick covers.
    pub second: i64,
    /// The number of deduplicated anomalous series at each level (`bins[level]`).
    pub bins: [usize; 5],
    /// The total number of anomalous series.
    pub count: usize,
    /// The sum of the discrete level weights.
    pub weight_sum: f64,
    /// The saturated per-second input, before EWMA smoothing.
    pub input: f64,
    /// The EWMA value after this second.
    pub ewma: f64,
    /// The raw (un-cooldowned) severity derived from the EWMA.
    pub raw_severity: SeverityLevel,
    /// The delivered severity of the internal episode-watcher subscription after this second, when
    /// installed.
    pub delivered_severity: Option<SeverityLevel>,
}

/// A per-second score-tick capture callback for diagnostics and replay export.
///
/// The hook is disabled by default; installing one must not change any scorer output.
pub type ScoreTickHook = Box<dyn FnMut(&ScoreTick)>;

/// The last second at which each anomaly level (0–4) was observed for a series within the active window.
///
/// Index is the level, value is the last second observed (`0` means never seen or already evicted).
/// Storing per-level timestamps (rather than a single max level + last-seen second) ensures that when a
/// high-severity peak expires from the window, the series is re-scored at the highest level that still
/// has an active timestamp, rather than carrying the stale peak forward.
type WindowEntry = [i64; 5];

/// The per-second calculation of one advanced second, before the raw severity level is derived.
struct SecondCalculation {
    second: i64,
    bins: [usize; 5],
    count: usize,
    weight_sum: f64,
    input: f64,
    ewma: f64,
}

/// The complete per-second state, including the derived raw severity level.
struct SecondState {
    second: i64,
    bins: [usize; 5],
    count: usize,
    weight_sum: f64,
    input: f64,
    ewma: f64,
    level: SeverityLevel,
}

/// The unified anomaly scoring pipeline.
///
/// See the [module documentation](self) for the lifecycle. Construct with [`AnomalyScorer::new`]; invalid
/// EWMA parameter values are clamped to safe defaults exactly as the Go constructor does. Drive through
/// the [`Scorer`] trait (anomaly submission, advancing, reset, output draining); the additional inherent
/// methods expose the scorer-specific surface used by the engine and the testbench (episodes,
/// subscriptions, telemetry snapshot, and the optional score-tick hook).
pub struct AnomalyScorer {
    config: AnomalyScorerConfig,

    /// Anomalies received since the last advance, grouped by their effective second.
    ///
    /// Past-timestamped anomalies are clamped to `last_advanced_sec + 1` in
    /// [`AnomalyScorer::process_anomaly`], so the past side is always drained on the next advance. The
    /// map key is the **effective** scorer second while each stored anomaly keeps its original
    /// `timestamp_sec`, so both the anomaly time and the scheduling time are retained for diagnosis.
    /// Future-timestamped anomalies accumulate until the advance reaches that second.
    pending: BTreeMap<i64, Vec<Anomaly>>,

    window_map: HashMap<String, WindowEntry>,

    /// Bounded five-minute attribution, active only when correlation events are enabled.
    top_anomalies: Option<TopAnomalyBuffer>,

    ewma: f64,

    last_advanced_sec: i64,

    /// The most recent per-second buckets, capped at `max_buckets` (or `window_secs` when zero).
    buckets: Vec<AnomalyScoreBucket>,

    /// The un-cooldowned per-second severity state, derived directly from the EWMA stream.
    raw_level: SeverityLevel,
    raw_level_initialized: bool,

    /// The dispatchers fanning the raw severity stream out to subscribers. The internal episode watcher
    /// (when installed) is always first, mirroring the Go construction order.
    dispatchers: Vec<(u64, Dispatcher)>,
    next_dispatcher_id: u64,
    watcher_id: Option<u64>,

    /// The currently open configured-threshold episode; active only when correlation events are enabled.
    open_episode: Option<ActiveCorrelation>,

    /// Lifecycle events produced during the last advance, drained once by
    /// [`Scorer::take_pending_outputs`].
    pending_events: Vec<CorrelatorEvent>,

    /// Optional per-second diagnostic capture; disabled by default.
    tick_hook: Option<ScoreTickHook>,
}

impl AnomalyScorer {
    /// Creates a scorer with the given configuration.
    ///
    /// Invalid EWMA parameters, out-of-range `max_reported_items`, and invalid correlation-event
    /// thresholds are clamped to the calibrated defaults, exactly as the Go `newAnomalyScorerBase` does.
    /// When correlation events are enabled, the internal watcher self-subscribes with the configured
    /// cooldown so episode lifecycle events are produced.
    pub fn new(config: AnomalyScorerConfig) -> Self {
        let defaults = AnomalyScorerConfig::production();
        let mut config = config;
        if config.window_secs < 1 {
            config.window_secs = defaults.window_secs;
        }
        if config.alpha <= 0.0 || config.alpha >= 1.0 {
            config.alpha = defaults.alpha;
        }
        if config.saturation_k <= 0.0 {
            config.saturation_k = defaults.saturation_k;
        }
        config.max_reported_items = config
            .max_reported_items
            .clamp(MIN_MAX_REPORTED_ITEMS, MAX_MAX_REPORTED_ITEMS);
        match normalize_correlation_event_threshold(&config.correlation_event_threshold) {
            Some(normalized) => config.correlation_event_threshold = normalized,
            None => config.correlation_event_threshold = defaults.correlation_event_threshold,
        }

        let mut scorer = Self {
            config,
            pending: BTreeMap::new(),
            window_map: HashMap::new(),
            top_anomalies: None,
            ewma: 0.0,
            last_advanced_sec: 0,
            buckets: Vec::new(),
            raw_level: SeverityLevel::Low,
            raw_level_initialized: false,
            dispatchers: Vec::new(),
            next_dispatcher_id: 0,
            watcher_id: None,
            open_episode: None,
            pending_events: Vec::new(),
            tick_hook: None,
        };
        if scorer.config.correlation_events {
            scorer.top_anomalies = Some(TopAnomalyBuffer::new(scorer.config.max_reported_items));
            // Self-subscribe the internal watcher, as newAnomalyScorerWithTelemetry does. The watcher
            // dispatcher carries no listener; the scorer applies its episode logic to the returned events
            // because a listener cannot re-enter the scorer it belongs to.
            let watcher_id = scorer.subscribe_dispatcher(
                SeverityEventsConfiguration {
                    cooldown_secs: scorer.config.cooldown_secs,
                    ..Default::default()
                },
                None,
            );
            scorer.watcher_id = Some(watcher_id);
        }
        scorer
    }

    /// Returns the scorer's name, which identifies it in correlation output.
    pub fn name(&self) -> &'static str {
        "anomaly_scorer"
    }

    /// Returns the resolved configuration the scorer is running with.
    ///
    /// The values reflect the clamping applied by [`AnomalyScorer::new`], not necessarily the raw input.
    pub fn config(&self) -> &AnomalyScorerConfig {
        &self.config
    }

    /// Returns the most recently computed EWMA score.
    pub fn last_score(&self) -> f64 {
        self.ewma
    }

    /// Returns a snapshot of the retained per-second buckets and the resolved settings.
    pub fn score_state(&self) -> AnomalyScoreState {
        AnomalyScoreState {
            buckets: self.buckets.clone(),
            config: self.config.clone(),
        }
    }

    /// Returns the currently open episode, if any.
    ///
    /// Closed episodes are no longer buffered here; they are emitted as
    /// [`CorrelatorEventKind::EpisodeEnded`] outputs and accumulated by the engine from there. At the end
    /// of a replay, an episode still open here is the open-at-EOF episode. Returns an empty vector when
    /// correlation events are disabled or no episode is open.
    pub fn active_correlations(&self) -> Vec<ActiveCorrelation> {
        if !self.config.correlation_events {
            return Vec::new();
        }
        match &self.open_episode {
            Some(episode) => vec![episode.clone()],
            None => Vec::new(),
        }
    }

    /// Registers a push-based severity-event subscription and returns its subscription id.
    ///
    /// When the raw severity level is already known and is not Low, a synthetic initial event
    /// (`Low → current`) is delivered to the listener immediately; when it is Low, no initial event is
    /// emitted. Use [`AnomalyScorer::unsubscribe_severity_events`] with the returned id to stop delivery.
    pub fn subscribe_severity_events(
        &mut self, config: SeverityEventsConfiguration, listener: Box<dyn SeverityEventListener>,
    ) -> u64 {
        self.subscribe_dispatcher(config, Some(listener))
    }

    /// Removes a severity-event subscription. Returns whether a matching subscription existed.
    ///
    /// Removing the internal watcher's subscription is possible but leaves episode tracking disabled
    /// until the scorer is reconstructed; the Go unsubscribe behaves the same way.
    pub fn unsubscribe_severity_events(&mut self, id: u64) -> bool {
        let before = self.dispatchers.len();
        self.dispatchers.retain(|(dispatcher_id, _)| *dispatcher_id != id);
        self.dispatchers.len() != before
    }

    /// Installs an optional per-second score-tick capture hook.
    ///
    /// The hook receives one [`ScoreTick`] per scorer second — including empty seconds — after the
    /// dispatchers have advanced that second, so the tick's delivered severity is final. Installing a
    /// hook must not change any scorer output.
    pub fn set_tick_hook(&mut self, hook: ScoreTickHook) {
        self.tick_hook = Some(hook);
    }

    /// Removes the score-tick capture hook, restoring default (disabled) capture.
    pub fn clear_tick_hook(&mut self) {
        self.tick_hook = None;
    }

    fn subscribe_dispatcher(
        &mut self, config: SeverityEventsConfiguration, listener: Option<Box<dyn SeverityEventListener>>,
    ) -> u64 {
        let mut dispatcher = Dispatcher::new(config, listener);
        if self.raw_level_initialized {
            dispatcher.deliver_initial(self.last_advanced_sec, self.raw_level);
        }
        let id = self.next_dispatcher_id;
        self.next_dispatcher_id += 1;
        self.dispatchers.push((id, dispatcher));
        id
    }

    /// Buffers the anomaly into the pending map keyed by its effective second.
    ///
    /// If the anomaly's timestamp is in the past (already advanced past), it is clamped to
    /// `last_advanced_sec + 1` so it participates in the next advance, as the Go scorer does; the anomaly
    /// itself keeps its original timestamp. Also appends to the open episode, if one is active.
    fn process_anomaly_inner(&mut self, anomaly: &Anomaly) {
        let mut sec = anomaly.timestamp_sec;
        if self.last_advanced_sec > 0 && sec <= self.last_advanced_sec {
            sec = self.last_advanced_sec + 1;
        }
        self.pending.entry(sec).or_default().push(anomaly.clone());

        if let Some(episode) = self.open_episode.as_mut() {
            if self.config.max_episode_anomalies == 0 || episode.anomalies.len() < self.config.max_episode_anomalies {
                episode.anomalies.push(anomaly.clone());
                if anomaly.timestamp_sec > episode.last_updated {
                    episode.last_updated = anomaly.timestamp_sec;
                }
            }
        }
    }

    /// Finalizes all 1-second buckets from `last_advanced_sec + 1` up to `data_time_sec` (inclusive),
    /// then drives the dispatchers over the per-second states in order.
    ///
    /// On the first advance, the loop starts from the earliest pending anomaly (or `data_time_sec` when
    /// none is pending) to avoid emitting buckets for every second since the epoch. Per-second states are
    /// computed first and dispatchers are driven afterwards, preserving the Go ordering; episode
    /// contributor snapshots are sensitive to it because the top-anomaly buffer has already merged every
    /// second of the advance by the time the watcher's transitions fire.
    fn advance_inner(&mut self, data_time_sec: i64) {
        let start = if self.last_advanced_sec == 0 {
            let mut start = data_time_sec;
            for sec in self.pending.keys() {
                if *sec < start {
                    start = *sec;
                }
            }
            start
        } else {
            self.last_advanced_sec + 1
        };

        let mut states = Vec::with_capacity((data_time_sec - start + 1).max(0) as usize);
        for sec in start..=data_time_sec {
            let calculation = self.advance_second(sec);
            let level = self.advance_raw_level(calculation.ewma);
            states.push(SecondState {
                second: calculation.second,
                bins: calculation.bins,
                count: calculation.count,
                weight_sum: calculation.weight_sum,
                input: calculation.input,
                ewma: calculation.ewma,
                level,
            });
        }
        self.last_advanced_sec = data_time_sec;

        // Drive the dispatchers outside the state computation so the watcher's episode logic runs after
        // every second of this advance has been finalized, exactly as the Go implementation orders it.
        for state in &states {
            let mut watcher_events: Vec<SeverityEvent> = Vec::new();
            for (id, dispatcher) in &mut self.dispatchers {
                if let Some(event) = dispatcher.advance(state.second, state.level) {
                    if Some(*id) == self.watcher_id {
                        watcher_events.push(event);
                    }
                }
            }
            for event in watcher_events {
                self.handle_watcher_event(event);
            }
            if self.tick_hook.is_some() {
                let tick = ScoreTick {
                    second: state.second,
                    bins: state.bins,
                    count: state.count,
                    weight_sum: state.weight_sum,
                    input: state.input,
                    ewma: state.ewma,
                    raw_severity: state.level,
                    delivered_severity: self.delivered_severity(),
                };
                if let Some(hook) = self.tick_hook.as_mut() {
                    hook(&tick);
                }
            }
        }
    }

    /// Returns the delivered severity of the internal watcher subscription, when installed.
    fn delivered_severity(&self) -> Option<SeverityLevel> {
        let watcher_id = self.watcher_id?;
        self.dispatchers
            .iter()
            .find(|(id, _)| *id == watcher_id)
            .map(|(_, dispatcher)| dispatcher.level())
    }

    /// Processes a single second, updating all EWMA state. Returns the resulting per-second calculation.
    ///
    /// Steps:
    ///
    /// 1. **Merge**: record the latest second per level for each series in the window map.
    /// 2. **Evict**: zero per-level timestamps that have fallen outside the window; drop series with no
    ///    live level.
    /// 3. **Bucket**: count unique live series at their highest live level.
    /// 4. **Saturate + EWMA**: `mean_weight = weight_sum / count` (zero when the count is zero),
    ///    `input = mean_weight * (1 - exp(-count / saturation_k))`, then
    ///    `ewma = alpha * input + (1 - alpha) * previous_ewma`.
    fn advance_second(&mut self, sec: i64) -> SecondCalculation {
        let anomalies = self.pending.remove(&sec).unwrap_or_default();
        if let Some(buffer) = self.top_anomalies.as_mut() {
            buffer.update(sec, &anomalies, &self.config);
        }

        // Step 1: merge the new anomalies into the window.
        for anomaly in &anomalies {
            let sid = series_id(anomaly);
            let level = anomaly_level(anomaly, &self.config);
            let entry = self.window_map.entry(sid).or_insert([0; 5]);
            if sec > entry[level] {
                entry[level] = sec;
            }
        }

        // Step 2: evict per-level timestamps that have fallen out of the window, and remove the series
        // entirely when no level remains active.
        let window_start = sec - self.config.window_secs + 1;
        self.window_map.retain(|_, entry| {
            let mut alive = false;
            for timestamp in entry.iter_mut() {
                if *timestamp > 0 && *timestamp < window_start {
                    *timestamp = 0;
                }
                if *timestamp > 0 {
                    alive = true;
                }
            }
            alive
        });

        // Step 3: bucket from the live window. Each series contributes at the highest level that still
        // has an active timestamp.
        let mut bins = [0usize; 5];
        let mut count = 0usize;
        let mut weight_sum = 0.0;
        for entry in self.window_map.values() {
            for level in (0..5).rev() {
                if entry[level] == 0 {
                    continue;
                }
                bins[level] += 1;
                count += 1;
                weight_sum += LEVEL_WEIGHTS[level];
                break;
            }
        }

        // Step 4: saturated input → EWMA. An empty second decays the EWMA by (1 - alpha).
        let mut input = 0.0;
        if count > 0 {
            let mean_weight = weight_sum / count as f64;
            input = mean_weight * (1.0 - (-(count as f64) / self.config.saturation_k).exp());
        }
        self.ewma = self.config.alpha * input + (1.0 - self.config.alpha) * self.ewma;

        self.buckets.push(AnomalyScoreBucket {
            second: sec,
            bins,
            count,
            weight_sum,
            ewma: self.ewma,
        });
        let bucket_cap = if self.config.max_buckets > 0 {
            self.config.max_buckets
        } else {
            self.config.window_secs
        };
        if self.buckets.len() as i64 > bucket_cap {
            let keep = bucket_cap as usize;
            self.buckets.drain(..self.buckets.len() - keep);
        }

        SecondCalculation {
            second: sec,
            bins,
            count,
            weight_sum,
            input,
            ewma: self.ewma,
        }
    }

    /// Updates the scorer's un-cooldowned severity state from one EWMA tick and returns the resulting
    /// level.
    ///
    /// The first tick seeds the state with the bare thresholds (no hysteresis); later ticks apply the
    /// downward hysteresis margin (`high_threshold * margin_pct`).
    fn advance_raw_level(&mut self, ewma: f64) -> SeverityLevel {
        if !self.raw_level_initialized {
            self.raw_level = raw_severity_level(ewma, self.config.low_threshold, self.config.high_threshold);
            self.raw_level_initialized = true;
            return self.raw_level;
        }
        let margin = self.config.high_threshold * self.config.margin_pct;
        self.raw_level = next_severity_level(
            ewma,
            self.raw_level,
            self.config.low_threshold,
            self.config.high_threshold,
            margin,
        );
        self.raw_level
    }

    /// Applies the internal watcher's episode logic to one delivered severity transition.
    ///
    /// This is the Go `OnSeverityTransition`: an episode opens when the delivered severity crosses up
    /// through the configured threshold, and closes when it crosses back down while an episode is open.
    /// The episode-start contributor snapshot is taken here, after every second of the current advance
    /// has already been merged into the top-anomaly buffer.
    fn handle_watcher_event(&mut self, event: SeverityEvent) {
        if !self.config.correlation_events {
            return;
        }
        let threshold = correlation_event_severity(&self.config.correlation_event_threshold);
        let threshold_name = self.config.correlation_event_threshold.clone();
        if event.from_level < threshold && event.to_level >= threshold {
            let episode = ActiveCorrelation {
                pattern: format!("anomaly_scorer_{}:{}", threshold_name, event.timestamp_sec),
                title: format!("Anomaly scorer: {}-or-higher severity period", threshold_name),
                members: Vec::new(),
                anomalies: Vec::new(),
                first_seen: event.timestamp_sec,
                last_updated: event.timestamp_sec,
            };
            let contributors = self
                .top_anomalies
                .as_ref()
                .map(|buffer| buffer.contributors(self.config.max_reported_items))
                .unwrap_or_default();
            self.open_episode = Some(episode.clone());
            self.pending_events.push(CorrelatorEvent {
                kind: CorrelatorEventKind::EpisodeStarted,
                correlator_name: self.name().to_string(),
                timestamp_sec: event.timestamp_sec,
                correlation: episode,
                from_level: event.from_level,
                to_level: event.to_level,
                contributors,
            });
        } else if event.from_level >= threshold && event.to_level < threshold && self.open_episode.is_some() {
            let mut episode = self.open_episode.take().expect("open episode checked above");
            episode.last_updated = event.timestamp_sec;
            self.pending_events.push(CorrelatorEvent {
                kind: CorrelatorEventKind::EpisodeEnded,
                correlator_name: self.name().to_string(),
                timestamp_sec: event.timestamp_sec,
                correlation: episode,
                from_level: event.from_level,
                to_level: event.to_level,
                contributors: Vec::new(),
            });
        }
    }

    /// Returns and drains the episode lifecycle events accumulated during the last advance.
    ///
    /// Returns an empty vector when correlation events are disabled or nothing is pending.
    fn take_pending_events_inner(&mut self) -> Vec<CorrelatorEvent> {
        if !self.config.correlation_events {
            return Vec::new();
        }
        std::mem::take(&mut self.pending_events)
    }

    /// Clears all internal EWMA/window/episode state and resets every dispatcher so subscriptions re-seed
    /// on the next advance.
    fn reset_inner(&mut self) {
        self.pending.clear();
        self.window_map.clear();
        if let Some(buffer) = self.top_anomalies.as_mut() {
            buffer.reset();
        }
        self.ewma = 0.0;
        self.last_advanced_sec = 0;
        self.buckets.clear();
        self.raw_level = SeverityLevel::Low;
        self.raw_level_initialized = false;
        self.open_episode = None;
        self.pending_events.clear();
        for (_, dispatcher) in &mut self.dispatchers {
            dispatcher.reset();
        }
    }
}

impl Scorer for AnomalyScorer {
    type Output = CorrelatorEvent;

    fn process_anomaly(&mut self, anomaly: &Anomaly) {
        self.process_anomaly_inner(anomaly);
    }

    fn advance_to(&mut self, data_time_sec: i64) {
        self.advance_inner(data_time_sec);
    }

    fn reset(&mut self) {
        self.reset_inner();
    }

    fn take_pending_outputs(&mut self) -> Vec<Self::Output> {
        self.take_pending_events_inner()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::config::AnomalyScorerConfig as ScorerConfig;
    use crate::identity::{Aggregate, QueryHandle, SeriesRef};
    use crate::model::AnomalyType;
    use crate::scorer::severity::{SeverityEventDirection, SeverityEventFilter};

    // ---- test helpers, mirroring the Go test file's helpers ----

    /// Creates an anomaly like the Go `makeAnomaly`: detector, timestamp, optional score, series
    /// `test/series` with tag `host:h1`, and no storage handle.
    fn make_anomaly(detector: &str, timestamp_sec: i64, score: Option<f64>) -> Anomaly {
        Anomaly {
            anomaly_type: AnomalyType::Metric,
            series: SeriesDescriptor::new("test", "series", None, vec!["host:h1".to_string()], Aggregate::None),
            series_ref: None,
            detector_name: detector.to_string(),
            context: None,
            timestamp_sec,
            score,
            sampling_interval_sec: 0,
            evidence: None,
        }
    }

    /// Creates an anomaly on series `ns/<name>` with tag `host:h`, like the Go window-test fixtures.
    fn window_anomaly(detector: &str, timestamp_sec: i64, score: Option<f64>, name: &str) -> Anomaly {
        Anomaly {
            series: SeriesDescriptor::new("ns", name, None, vec!["host:h".to_string()], Aggregate::None),
            series_ref: None,
            ..make_anomaly(detector, timestamp_sec, score)
        }
    }

    fn handle(ref_id: u64) -> Option<QueryHandle> {
        Some(QueryHandle::new(SeriesRef::new(ref_id), Aggregate::Average))
    }

    /// The Go `episodeTestCfg`: fast, deterministic episode lifecycle over two advances.
    fn episode_test_config() -> ScorerConfig {
        ScorerConfig {
            alpha: 0.99,
            saturation_k: 1.0,
            window_secs: 1,
            cooldown_secs: 0,
            ..ScorerConfig::production()
        }
    }

    /// The Go `seedAndCrossHighThreshold`: one empty advance, then a spike that drives the EWMA above the
    /// high threshold and opens an episode.
    fn seed_and_cross_high_threshold(scorer: &mut AnomalyScorer, t0: i64) -> i64 {
        scorer.advance_to(t0);
        scorer.process_anomaly(&make_anomaly("holt_residual", t0 + 1, Some(40.0)));
        scorer.advance_to(t0 + 1);
        t0 + 1
    }

    /// A severity listener that shares its collected events with the test through an `Rc<RefCell>`.
    struct SharedListener {
        events: Rc<RefCell<Vec<SeverityEvent>>>,
    }

    impl SeverityEventListener for SharedListener {
        fn on_severity_transition(&mut self, event: SeverityEvent) {
            self.events.borrow_mut().push(event);
        }
    }

    fn shared_listener() -> (Box<SharedListener>, Rc<RefCell<Vec<SeverityEvent>>>) {
        let events = Rc::new(RefCell::new(Vec::new()));
        (Box::new(SharedListener { events: events.clone() }), events)
    }

    fn last_bucket(scorer: &AnomalyScorer) -> &AnomalyScoreBucket {
        scorer.buckets.last().expect("at least one bucket")
    }

    fn bucket_at(scorer: &AnomalyScorer, second: i64) -> Option<&AnomalyScoreBucket> {
        scorer.buckets.iter().find(|bucket| bucket.second == second)
    }

    fn deescalations(events: &[SeverityEvent]) -> usize {
        events
            .iter()
            .filter(|event| event.direction == SeverityEventDirection::Deescalation)
            .count()
    }

    fn escalations(events: &[SeverityEvent]) -> usize {
        events
            .iter()
            .filter(|event| event.direction == SeverityEventDirection::Escalation)
            .count()
    }

    // ---- levels, weights, and calibration ----

    #[test]
    fn level_weights_match_go() {
        assert_eq!(LEVEL_WEIGHTS, [0.2, 0.5, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn anomaly_severity_labels_match_go() {
        assert_eq!(anomaly_severity_label(0), "xlow");
        assert_eq!(anomaly_severity_label(1), "low");
        assert_eq!(anomaly_severity_label(2), "medium");
        assert_eq!(anomaly_severity_label(3), "high");
        assert_eq!(anomaly_severity_label(4), "xhigh");
        assert_eq!(anomaly_severity_label(5), "unknown");
    }

    /// Ports `TestAnomalyLevel`, including the exact per-detector thresholds.
    #[test]
    fn anomaly_level_matches_go_calibration() {
        let config = ScorerConfig::production();
        let cases = [
            ("holt_residual", Some(0.0), 0), // < 6 → VeryLow
            ("holt_residual", Some(5.9), 0),
            ("holt_residual", Some(6.0), 1), // 6 ≤ … < 12 → Low
            ("holt_residual", Some(11.9), 1),
            ("holt_residual", Some(12.0), 2), // Medium
            ("holt_residual", Some(19.9), 2),
            ("holt_residual", Some(20.0), 3), // High
            ("holt_residual", Some(34.9), 3),
            ("holt_residual", Some(35.0), 4), // XHigh
            ("holt_residual", Some(100.0), 4),
            ("holt_residual", None, 0),       // absent score from a scored detector → VeryLow
            ("bocpd", None, 2),               // uncalibrated → fixed Medium
            ("bocpd", Some(99.0), 2),         // score ignored for uncalibrated detectors
            ("unknown_detector", None, 2),    // default Medium
            ("tukey_biweight", Some(7.0), 1), // Low (per-detector threshold)
        ];
        for (detector, score, want) in cases {
            let anomaly = make_anomaly(detector, 1000, score);
            assert_eq!(
                anomaly_level(&anomaly, &config),
                want,
                "detector {detector} score {score:?}"
            );
        }
    }

    /// Pins the exact calibrated per-detector thresholds from the Go defaults.
    #[test]
    fn detector_thresholds_are_exact() {
        let config = ScorerConfig::production();
        assert_eq!(config.detector_thresholds["tukey_biweight"], [5.0, 8.0, 15.0, 30.0]);
        assert_eq!(config.detector_thresholds["holt_residual"], [6.0, 12.0, 20.0, 35.0]);
        assert_eq!(config.detector_thresholds["scanmw"], [8.0, 10.0, 15.0, 25.0]);
        assert_eq!(config.detector_thresholds["scanwelch"], [8.0, 10.0, 15.0, 25.0]);
        assert_eq!(config.detector_thresholds.len(), 4);
    }

    /// Ports `TestContributorWeight_IsContinuousAndBounded`.
    #[test]
    fn contributor_weight_is_continuous_and_bounded() {
        let config = ScorerConfig::production();

        let below = make_anomaly("holt_residual", 1000, Some(-1.0));
        assert_eq!(contributor_weight(&below, &config), 0.2);

        let below_first = make_anomaly("holt_residual", 1000, Some(1.0));
        assert_eq!(contributor_weight(&below_first, &config), LEVEL_WEIGHTS[0]);

        // 16 is halfway between holt_residual's medium (12) and high (20) thresholds, so the weight
        // interpolates between their calibrated weights (1 and 2).
        let middle = make_anomaly("holt_residual", 1000, Some(16.0));
        assert!((contributor_weight(&middle, &config) - 1.5).abs() < 1e-9);

        let above = make_anomaly("holt_residual", 1000, Some(100.0));
        assert_eq!(contributor_weight(&above, &config), 3.0);

        // Uncalibrated detectors keep Medium's weight; an absent score from a calibrated detector keeps
        // VeryLow's weight.
        let uncalibrated = make_anomaly("bocpd", 1000, Some(99.0));
        assert_eq!(contributor_weight(&uncalibrated, &config), LEVEL_WEIGHTS[2]);
        let unscored = make_anomaly("holt_residual", 1000, None);
        assert_eq!(contributor_weight(&unscored, &config), LEVEL_WEIGHTS[0]);
    }

    /// Ports `TestNormalizeCorrelationEventThreshold`.
    #[test]
    fn normalize_correlation_event_threshold_matches_go() {
        assert_eq!(normalize_correlation_event_threshold(""), Some("high".to_string()));
        assert_eq!(normalize_correlation_event_threshold("high"), Some("high".to_string()));
        assert_eq!(
            normalize_correlation_event_threshold(" MEDIUM "),
            Some("medium".to_string())
        );
        assert_eq!(normalize_correlation_event_threshold("low"), None);
        assert_eq!(normalize_correlation_event_threshold("unexpected"), None);
    }

    /// Ports the clamping from `newAnomalyScorerBase` (plus `TestParseSettingsFromJSON` defaults).
    #[test]
    fn constructor_clamps_invalid_settings() {
        let resolved = AnomalyScorer::new(ScorerConfig {
            window_secs: 0,
            alpha: 1.0,
            saturation_k: 0.0,
            max_reported_items: 1,
            correlation_event_threshold: "low".to_string(),
            ..ScorerConfig::production()
        })
        .config()
        .clone();

        assert_eq!(resolved.window_secs, 15);
        assert_eq!(resolved.alpha, 0.014);
        assert_eq!(resolved.saturation_k, 5.0);
        assert_eq!(resolved.max_reported_items, MIN_MAX_REPORTED_ITEMS);
        assert_eq!(resolved.correlation_event_threshold, "high");

        let resolved = AnomalyScorer::new(ScorerConfig {
            alpha: 0.0,
            max_reported_items: 2000,
            correlation_event_threshold: " MEDIUM ".to_string(),
            ..ScorerConfig::production()
        })
        .config()
        .clone();
        assert_eq!(resolved.alpha, 0.014);
        assert_eq!(resolved.max_reported_items, MAX_MAX_REPORTED_ITEMS);
        assert_eq!(resolved.correlation_event_threshold, "medium");

        // Valid values pass through untouched.
        let resolved = AnomalyScorer::new(ScorerConfig {
            alpha: 0.99,
            ..ScorerConfig::production()
        })
        .config()
        .clone();
        assert_eq!(resolved.alpha, 0.99);
    }

    // ---- EWMA core: exact per-second formula ----

    /// Ports `TestEWMABasic`: first-bucket seeding and empty-second decay with window 1.
    #[test]
    fn ewma_seeds_and_decays() {
        let config = ScorerConfig {
            alpha: 0.5,
            saturation_k: 1.0,
            window_secs: 1,
            ..ScorerConfig::production()
        };
        let mut scorer = AnomalyScorer::new(config.clone());
        let alpha = config.alpha;

        // holt_residual score 20 → level 3 → weight 2.0.
        scorer.process_anomaly(&make_anomaly("holt_residual", 1000, Some(20.0)));
        scorer.advance_to(1000);

        let state = scorer.score_state();
        assert_eq!(state.buckets.len(), 1);
        let bucket = &state.buckets[0];
        assert_eq!(bucket.second, 1000);
        assert_eq!(bucket.count, 1);
        assert_eq!(bucket.bins[3], 1);
        assert_eq!(bucket.weight_sum, 2.0);

        // count=1, weight=2.0, meanWeight=2.0, saturation=1−exp(−1/1); EWMA = alpha*input + (1-alpha)*0.
        let expected_input = 2.0 * (1.0 - (-1.0f64).exp());
        let expected_ewma = alpha * expected_input;
        assert!((bucket.ewma - expected_ewma).abs() < 1e-9);
        assert!((bucket.ewma - 0.6321).abs() < 1e-3);

        // Second advance with no anomalies → the EWMA decays by (1-alpha).
        scorer.advance_to(1001);
        let state = scorer.score_state();
        // With WindowSecs=1 the bucket history is capped at 1 entry.
        assert_eq!(state.buckets.len(), 1);
        assert_eq!(state.buckets[0].second, 1001);
        let expected2 = alpha * 0.0 + (1.0 - alpha) * expected_ewma;
        assert!((state.buckets[0].ewma - expected2).abs() < 1e-9);
        assert_eq!(state.buckets[0].count, 0);
    }

    /// Pins the exact per-second formula — mean weight, saturation, EWMA, and empty-second decay — with
    /// multiple series and an explicit closed-form expectation.
    #[test]
    fn per_second_formula_is_exact_including_empty_second_decay() {
        let config = ScorerConfig {
            alpha: 0.25,
            saturation_k: 4.0,
            window_secs: 15,
            ..ScorerConfig::production()
        };
        let alpha = config.alpha;
        let mut scorer = AnomalyScorer::new(config);

        // Two distinct series at the same second: bocpd (level 2, weight 1.0) and holt_residual score 20
        // (level 3, weight 2.0).
        scorer.process_anomaly(&window_anomaly("bocpd", 1000, None, "m1"));
        scorer.process_anomaly(&window_anomaly("holt_residual", 1000, Some(20.0), "m2"));
        scorer.advance_to(1000);

        let input = 1.5 * (1.0 - (-(2.0 / 4.0_f64)).exp()); // mean_weight 1.5, saturation 1−exp(−2/4)
        let bucket = bucket_at(&scorer, 1000).expect("bucket at 1000");
        assert_eq!(bucket.count, 2);
        assert_eq!(bucket.bins, [0, 0, 1, 1, 0]);
        assert_eq!(bucket.weight_sum, 3.0);
        assert!((bucket.ewma - input * (1.0 - (1.0 - alpha))).abs() < 1e-12);

        // One more second with the window still live: EWMA moves toward the input.
        scorer.advance_to(1001);
        let bucket = bucket_at(&scorer, 1001).expect("bucket at 1001");
        assert_eq!(bucket.count, 2);
        assert!(
            (bucket.ewma - input * (1.0 - (1.0 - alpha).powi(2))).abs() < 1e-12,
            "ewma after two input seconds"
        );

        // The window (15s) keeps both series live through second 1014; at 1015 they expire and the
        // second becomes empty, decaying the EWMA by (1-alpha).
        scorer.advance_to(1015);
        let decayed_from = input * (1.0 - (1.0 - alpha).powi(15)); // ewma at 1014
        let bucket = bucket_at(&scorer, 1015).expect("bucket at 1015");
        assert_eq!(bucket.count, 0);
        assert_eq!(bucket.bins, [0; 5]);
        assert_eq!(bucket.weight_sum, 0.0);
        assert!(
            (bucket.ewma - (1.0 - alpha) * decayed_from).abs() < 1e-12,
            "empty second decays the ewma"
        );
    }

    /// Ports `TestEmptySeconds`: advancing over a gap produces empty buckets for every crossed second.
    #[test]
    fn empty_seconds_generate_buckets() {
        let config = ScorerConfig {
            alpha: 0.5,
            window_secs: 1,
            ..ScorerConfig::production()
        };
        let mut scorer = AnomalyScorer::new(config);

        scorer.process_anomaly(&make_anomaly("holt_residual", 1000, Some(25.0))); // level 3
        scorer.advance_to(1002); // covers seconds 1000, 1001, 1002

        let state = scorer.score_state();
        assert_eq!(state.buckets.len(), 1, "cap = window_secs = 1");
        assert_eq!(state.buckets[0].second, 1002);
        assert_eq!(state.buckets[0].count, 0);
        // EWMA after three seconds: seed → decay → decay. The score stays positive but has decayed
        // twice from its seeded value.
        let input = 2.0 * (1.0 - (-(1.0 / 5.0_f64)).exp()); // level 3 weight 2.0, count 1, k = 5
        assert!(scorer.last_score() > 0.0, "expected a decayed, non-zero EWMA");
        assert!((scorer.last_score() - 0.5 * 0.5_f64.powi(2) * input).abs() < 1e-12);
    }

    /// Ports `TestDeduplication`: two anomalies on the same series at the same second collapse to the
    /// higher-level one — the scorer counts distinct anomalous series, not detector emissions.
    #[test]
    fn same_series_same_second_deduplicates_to_highest_level() {
        let mut scorer = AnomalyScorer::new(ScorerConfig::production());

        // Levels 1 (Low, holt score 8) and 3 (High, tukey score 25) on the same series: only the High
        // one survives in the window.
        scorer.process_anomaly(&window_anomaly("holt_residual", 1000, Some(8.0), "m"));
        scorer.process_anomaly(&window_anomaly("tukey_biweight", 1000, Some(25.0), "m"));
        scorer.advance_to(1000);

        let bucket = last_bucket(&scorer);
        assert_eq!(bucket.count, 1);
        assert_eq!(bucket.bins[3], 1);
        assert_eq!(bucket.weight_sum, 2.0);
    }

    /// Ports `TestWindowDedup`: the same series firing at different seconds within the window counts
    /// once, at the highest live level.
    #[test]
    fn same_series_across_seconds_counts_once() {
        let mut scorer = AnomalyScorer::new(ScorerConfig {
            window_secs: 15,
            ..ScorerConfig::production()
        });

        // Fires at t=1000 (Medium) and again at t=1005 (Low): count stays 1, level stays 2.
        scorer.process_anomaly(&window_anomaly("bocpd", 1000, None, "m"));
        scorer.advance_to(1000);
        scorer.process_anomaly(&window_anomaly("bocpd", 1005, None, "m"));
        scorer.advance_to(1005);

        let bucket = last_bucket(&scorer);
        assert_eq!(bucket.second, 1005);
        assert_eq!(bucket.count, 1);
        assert_eq!(bucket.bins[2], 1);
    }

    /// Ports `TestWindowExpiry`: a series is evicted once it falls outside the window.
    #[test]
    fn window_expires_at_boundary() {
        let mut scorer = AnomalyScorer::new(ScorerConfig {
            window_secs: 15,
            ..ScorerConfig::production()
        });

        scorer.process_anomaly(&window_anomaly("bocpd", 1000, None, "m"));
        scorer.advance_to(1014);
        assert_eq!(last_bucket(&scorer).count, 1, "alive at t=1014 (windowStart = 1000)");

        scorer.advance_to(1015);
        assert_eq!(last_bucket(&scorer).count, 0, "expired at t=1015 (windowStart = 1001)");
    }

    /// Ports `TestWindowLevelExpiry`: per-level timestamps mean an expired high-severity peak re-scores
    /// the series at the highest level that still has an active timestamp.
    #[test]
    fn expired_peak_falls_back_to_highest_live_level() {
        let mut scorer = AnomalyScorer::new(ScorerConfig {
            window_secs: 15,
            ..ScorerConfig::production()
        });

        // High at t=1000, Low at t=1010 on the same series. At t=1015 the window starts at 1001: the
        // High timestamp (1000) is outside the window, so the series must count at level 1, not 3.
        scorer.process_anomaly(&window_anomaly("holt_residual", 1000, Some(20.0), "m"));
        scorer.advance_to(1000);
        scorer.process_anomaly(&window_anomaly("holt_residual", 1010, Some(7.0), "m"));
        scorer.advance_to(1010);
        scorer.advance_to(1015);

        let bucket = last_bucket(&scorer);
        assert_eq!(bucket.count, 1);
        assert_eq!(bucket.bins[3], 0, "expired High peak must not inflate level 3");
        assert_eq!(bucket.bins[1], 1, "series re-scored at the still-live Low level");
    }

    /// Ports `TestDeduplicationDifferentSeries`: anomalies on different series never merge.
    #[test]
    fn different_series_never_merge() {
        let mut scorer = AnomalyScorer::new(ScorerConfig::production());

        scorer.process_anomaly(&window_anomaly("bocpd", 1000, None, "m1"));
        scorer.process_anomaly(&window_anomaly("bocpd", 1000, None, "m2"));
        scorer.advance_to(1000);

        assert_eq!(last_bucket(&scorer).count, 2);
    }

    /// Pins the bucket-retention cap: `window_secs` by default, `max_buckets` when positive.
    #[test]
    fn buckets_are_capped_at_window_and_overridable() {
        let mut scorer = AnomalyScorer::new(ScorerConfig {
            window_secs: 15,
            ..ScorerConfig::production()
        });
        scorer.advance_to(1000); // first advance: one bucket
        scorer.advance_to(1014); // 14 more: 15 buckets, 1000..=1014
        assert_eq!(scorer.score_state().buckets.len(), 15);
        scorer.advance_to(1015); // 16th bucket trims the oldest
        let buckets = scorer.score_state().buckets;
        assert_eq!(buckets.len(), 15);
        assert_eq!(buckets[0].second, 1001);

        let mut scorer = AnomalyScorer::new(ScorerConfig {
            window_secs: 15,
            max_buckets: 2,
            ..ScorerConfig::production()
        });
        scorer.advance_to(1000);
        scorer.advance_to(1002);
        let buckets = scorer.score_state().buckets;
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].second, 1001);
        assert_eq!(buckets[1].second, 1002);
    }

    // ---- pending-anomaly scheduling ----

    /// Ports `TestLateAnomalyClamp`: an anomaly with a historical timestamp is scheduled at
    /// `lastAdvancedSecond + 1`.
    #[test]
    fn late_anomaly_is_scheduled_at_last_advanced_plus_one() {
        let mut scorer = AnomalyScorer::new(ScorerConfig {
            window_secs: 15,
            ..ScorerConfig::production()
        });

        scorer.advance_to(1010);
        scorer.process_anomaly(&window_anomaly("scanmw", 1000, Some(20.0), "m"));
        scorer.advance_to(1011);

        let bucket = bucket_at(&scorer, 1011).expect("bucket at the clamped second 1011");
        assert_eq!(bucket.count, 1);
        assert_eq!(bucket.bins[3], 1); // scanmw score 20 → level 3
    }

    /// Ports `TestLateAnomalyNoLeakInPending`: after clamping, the historical second has no pending
    /// entry and the effective second does — both the anomaly time and the scheduling time are retained.
    #[test]
    fn late_anomaly_does_not_leak_in_pending() {
        let mut scorer = AnomalyScorer::new(ScorerConfig {
            window_secs: 15,
            ..ScorerConfig::production()
        });

        scorer.advance_to(1010);
        scorer.process_anomaly(&window_anomaly("scanmw", 1000, Some(20.0), "m"));

        assert!(
            !scorer.pending.contains_key(&1000),
            "historical second must not stay pending"
        );
        let pending = scorer.pending.get(&1011).expect("clamped second 1011 is pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].timestamp_sec, 1000,
            "the anomaly keeps its original anomaly time"
        );
    }

    /// Ports `TestLateAnomalyBeforeFirstAdvance`: anomalies received before the first advance keep their
    /// original timestamp; the first advance starts at the earliest pending second.
    #[test]
    fn anomaly_before_first_advance_keeps_its_second() {
        let mut scorer = AnomalyScorer::new(ScorerConfig::production());

        scorer.process_anomaly(&window_anomaly("scanmw", 1000, Some(20.0), "m"));
        scorer.advance_to(1005);

        let bucket = bucket_at(&scorer, 1000).expect("bucket at the original second 1000");
        assert_eq!(bucket.count, 1);
        // The first advance covered every second up to the data time.
        assert_eq!(scorer.score_state().buckets.len(), 6);
    }

    /// A future-timestamped anomaly waits in pending until the advance reaches its second.
    #[test]
    fn future_anomaly_waits_for_its_second() {
        let mut scorer = AnomalyScorer::new(ScorerConfig {
            window_secs: 15,
            ..ScorerConfig::production()
        });

        scorer.advance_to(1010);
        scorer.process_anomaly(&window_anomaly("bocpd", 1015, None, "m"));
        scorer.advance_to(1012);
        assert_eq!(last_bucket(&scorer).count, 0, "not yet at the anomaly's second");

        scorer.advance_to(1015);
        let bucket = bucket_at(&scorer, 1015).expect("bucket at 1015");
        assert_eq!(bucket.count, 1);
    }

    // ---- raw severity state machine ----

    /// Ports `TestRawSeverityLevel`.
    #[test]
    fn raw_severity_level_seeds_with_bare_thresholds() {
        let cases = [
            (0.000, SeverityLevel::Low),
            (0.039, SeverityLevel::Low),
            (0.040, SeverityLevel::Medium),
            (0.059, SeverityLevel::Medium),
            (0.060, SeverityLevel::High),
            (1.000, SeverityLevel::High),
        ];
        for (ewma, want) in cases {
            assert_eq!(raw_severity_level(ewma, 0.040, 0.060), want, "ewma {ewma}");
        }
    }

    /// Ports `TestNextSeverityLevelEscalation`: upward transitions use the bare thresholds.
    #[test]
    fn next_severity_level_escalates_without_hysteresis() {
        let cases = [
            (0.060, SeverityLevel::Low, SeverityLevel::High), // skip straight to High
            (0.045, SeverityLevel::Low, SeverityLevel::Medium), // crosses the low threshold
            (0.030, SeverityLevel::Low, SeverityLevel::Low),  // stays Low
            (0.065, SeverityLevel::Medium, SeverityLevel::High), // Medium → High
        ];
        for (ewma, current, want) in cases {
            assert_eq!(
                next_severity_level(ewma, current, 0.040, 0.060, 0.060 * 0.20),
                want,
                "ewma {ewma} from {current:?}"
            );
        }
    }

    /// Ports `TestNextSeverityLevelHysteresis`: downward transitions need `threshold - margin`.
    #[test]
    fn next_severity_level_downward_hysteresis_uses_margin() {
        // low=0.040, high=0.060, margin=0.060*0.20=0.012: from High drop only below 0.048, from Medium
        // only below 0.028.
        let cases = [
            (
                0.049,
                SeverityLevel::High,
                SeverityLevel::High,
                "High: within hysteresis band",
            ),
            (
                0.047,
                SeverityLevel::High,
                SeverityLevel::Medium,
                "High: below hysteresis → Medium",
            ),
            (0.005, SeverityLevel::High, SeverityLevel::Low, "High: far below → Low"),
            (
                0.029,
                SeverityLevel::Medium,
                SeverityLevel::Medium,
                "Medium: within hysteresis band",
            ),
            (
                0.027,
                SeverityLevel::Medium,
                SeverityLevel::Low,
                "Medium: below hysteresis → Low",
            ),
        ];
        for (ewma, current, want, desc) in cases {
            assert_eq!(
                next_severity_level(ewma, current, 0.040, 0.060, 0.060 * 0.20),
                want,
                "{desc}: ewma {ewma}"
            );
        }
    }

    // ---- severity-event subscriptions ----

    /// Ports `TestSubscribeBasic`.
    #[test]
    fn subscribe_delivers_escalation_on_threshold_crossing() {
        let config = ScorerConfig {
            alpha: 0.99,
            saturation_k: 1.0,
            window_secs: 5,
            ..ScorerConfig::production()
        };
        let mut scorer = AnomalyScorer::new(config);
        let (listener, events) = shared_listener();
        scorer.subscribe_severity_events(SeverityEventsConfiguration::default(), listener);

        scorer.advance_to(1000);
        assert!(events.borrow().is_empty(), "no events after an empty advance");

        // One bocpd anomaly: level 2, weight 1.0, saturation(1, k=1) ≈ 0.632 → EWMA ≈ 0.626 ≥ 0.40.
        scorer.process_anomaly(&make_anomaly("bocpd", 1001, None));
        scorer.advance_to(1001);

        assert_eq!(events.borrow().len(), 1);
        let event = events.borrow()[0];
        assert_eq!(event.from_level, SeverityLevel::Low);
        assert_eq!(event.to_level, SeverityLevel::High);
        assert_eq!(event.direction, SeverityEventDirection::Escalation);
    }

    /// Ports `TestSubscribeCooldown`.
    #[test]
    fn subscribe_cooldown_blocks_then_releases_deescalation() {
        let config = ScorerConfig {
            alpha: 0.99,
            saturation_k: 1.0,
            window_secs: 1,
            ..ScorerConfig::production()
        };
        let mut scorer = AnomalyScorer::new(config);
        let (listener, events) = shared_listener();
        scorer.subscribe_severity_events(
            SeverityEventsConfiguration {
                cooldown_secs: 60,
                ..Default::default()
            },
            listener,
        );

        scorer.advance_to(1000); // seeds at Low
        scorer.process_anomaly(&make_anomaly("bocpd", 1001, None));
        scorer.advance_to(1001); // EWMA ≈ 0.63 → High
        scorer.advance_to(1002); // EWMA ≈ 0 → raw Low, blocked by the 60s cooldown

        assert_eq!(escalations(&events.borrow()), 1);
        assert_eq!(deescalations(&events.borrow()), 0);

        scorer.advance_to(1062); // the cooldown has expired by second 1061
        assert_eq!(deescalations(&events.borrow()), 1);
    }

    /// Ports `TestSubscribeFilter`.
    #[test]
    fn subscribe_filter_suppresses_non_matching_transitions() {
        let config = ScorerConfig {
            alpha: 0.99,
            saturation_k: 1.0,
            window_secs: 1,
            ..ScorerConfig::production()
        };
        let mut scorer = AnomalyScorer::new(config);
        let (listener, events) = shared_listener();
        scorer.subscribe_severity_events(
            SeverityEventsConfiguration {
                filter: SeverityEventFilter {
                    direction: SeverityEventDirection::Escalation,
                    ..Default::default()
                },
                cooldown_secs: 0,
            },
            listener,
        );

        scorer.advance_to(1000);
        scorer.process_anomaly(&make_anomaly("bocpd", 1001, None));
        scorer.advance_to(1001); // escalation delivered
        scorer.advance_to(1002); // de-escalation filtered out

        let events = events.borrow();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].direction, SeverityEventDirection::Escalation);
    }

    /// Ports `TestUnsubscribe`.
    #[test]
    fn unsubscribe_stops_delivery() {
        let config = ScorerConfig {
            alpha: 0.99,
            saturation_k: 1.0,
            window_secs: 1,
            ..ScorerConfig::production()
        };
        let mut scorer = AnomalyScorer::new(config);
        let (listener, events) = shared_listener();
        let id = scorer.subscribe_severity_events(SeverityEventsConfiguration::default(), listener);

        scorer.advance_to(1000);
        scorer.process_anomaly(&make_anomaly("bocpd", 1001, None));
        scorer.advance_to(1001); // escalation fires
        assert!(scorer.unsubscribe_severity_events(id));

        scorer.advance_to(1002); // would de-escalate, but the subscription is gone
        assert_eq!(events.borrow().len(), 1);
        assert!(!scorer.unsubscribe_severity_events(id), "already unsubscribed");
    }

    /// Ports `TestResetClearsSubscriptionState`.
    #[test]
    fn reset_reseeds_subscriptions() {
        let config = ScorerConfig {
            alpha: 0.99,
            saturation_k: 1.0,
            window_secs: 1,
            ..ScorerConfig::production()
        };
        let mut scorer = AnomalyScorer::new(config);
        let (listener, events) = shared_listener();
        scorer.subscribe_severity_events(
            SeverityEventsConfiguration {
                cooldown_secs: 3600,
                ..Default::default()
            },
            listener,
        );

        scorer.process_anomaly(&make_anomaly("bocpd", 1000, None));
        scorer.advance_to(1000); // Low → High

        scorer.reset();

        // Replaying the same sequence must fire the escalation again, proving the subscription
        // re-seeded rather than carrying over the High state.
        let before = events.borrow().len();
        scorer.advance_to(2000); // seeds at Low (EWMA 0)
        scorer.process_anomaly(&make_anomaly("bocpd", 2001, None));
        scorer.advance_to(2001); // Low → High again
        let events = events.borrow();
        assert_eq!(events.len() - before, 1);
        assert_eq!(events[events.len() - 1].direction, SeverityEventDirection::Escalation);
    }

    /// Ports `TestSubscribeSeverityEventsCreatesIndependentDispatchers`.
    #[test]
    fn subscriptions_have_independent_dispatchers() {
        let config = ScorerConfig {
            alpha: 0.99,
            saturation_k: 1.0,
            window_secs: 1,
            ..ScorerConfig::production()
        };
        let mut scorer = AnomalyScorer::new(config);
        let (fast_listener, fast) = shared_listener();
        let (slow_listener, slow) = shared_listener();
        scorer.subscribe_severity_events(SeverityEventsConfiguration::default(), fast_listener);
        scorer.subscribe_severity_events(
            SeverityEventsConfiguration {
                cooldown_secs: 60,
                ..Default::default()
            },
            slow_listener,
        );

        scorer.advance_to(1000);
        scorer.process_anomaly(&make_anomaly("bocpd", 1001, None));
        scorer.advance_to(1001);
        scorer.advance_to(1002);

        assert_eq!(fast.borrow().len(), 2, "escalation plus immediate de-escalation");
        assert_eq!(slow.borrow().len(), 1, "cooldown suppresses the de-escalation");

        scorer.advance_to(1062);
        assert_eq!(slow.borrow().len(), 2);
        assert_eq!(slow.borrow()[1].direction, SeverityEventDirection::Deescalation);
    }

    /// A subscription made after the severity is known is seeded with a synthetic initial event.
    #[test]
    fn subscription_after_advance_is_seeded() {
        let config = ScorerConfig {
            alpha: 0.99,
            saturation_k: 1.0,
            window_secs: 5,
            ..ScorerConfig::production()
        };
        let mut scorer = AnomalyScorer::new(config);

        scorer.advance_to(1000);
        scorer.process_anomaly(&make_anomaly("bocpd", 1001, None));
        scorer.advance_to(1001); // raw level is now High

        let (listener, events) = shared_listener();
        scorer.subscribe_severity_events(SeverityEventsConfiguration::default(), listener);

        let events = events.borrow();
        assert_eq!(events.len(), 1, "the initial Low → High snapshot is delivered");
        assert_eq!(events[0].timestamp_sec, 1001);
        assert_eq!(events[0].to_level, SeverityLevel::High);
    }

    // ---- episodes ----

    /// Ports `TestActiveCorrelationsNilWhenDisabled`.
    #[test]
    fn episodes_are_nil_when_disabled() {
        let mut scorer = AnomalyScorer::new(episode_test_config());
        scorer.advance_to(1000);
        assert!(scorer.active_correlations().is_empty());
        assert!(scorer.take_pending_outputs().is_empty());
    }

    /// Ports `TestTopAnomalyBuffer_EnabledOnlyForCorrelationEvents`: the contributor buffer exists only
    /// when correlation events are enabled.
    #[test]
    fn top_anomaly_buffer_enabled_only_with_episodes() {
        let disabled = AnomalyScorer::new(ScorerConfig::production());
        assert!(disabled.top_anomalies.is_none(), "buffer stays disabled by default");

        let enabled = AnomalyScorer::new(ScorerConfig {
            correlation_events: true,
            ..ScorerConfig::production()
        });
        assert!(enabled.top_anomalies.is_some(), "buffer tracks episodes when enabled");
    }

    /// Ports `TestEpisodeOpenClose`, `TestPendingEvents_EpisodeStarted`, and
    /// `TestPendingEvents_EpisodeEnded`.
    #[test]
    fn episode_opens_and_closes() {
        let config = ScorerConfig {
            correlation_events: true,
            ..episode_test_config()
        };
        let mut scorer = AnomalyScorer::new(config);

        let spike_sec = seed_and_cross_high_threshold(&mut scorer, 1000);
        assert_eq!(scorer.active_correlations().len(), 1, "episode open after the spike");

        let started = scorer.take_pending_outputs();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].kind, CorrelatorEventKind::EpisodeStarted);
        assert_eq!(started[0].to_level, SeverityLevel::High);
        assert_eq!(started[0].from_level, SeverityLevel::Low);
        assert_eq!(started[0].timestamp_sec, spike_sec);
        assert!(!started[0].correlation.pattern.is_empty());
        assert_eq!(started[0].correlator_name, "anomaly_scorer");

        // One no-anomaly advance collapses the EWMA (window 1): the episode closes.
        scorer.advance_to(spike_sec + 1);
        assert!(
            scorer.active_correlations().is_empty(),
            "episode closed after the decay"
        );

        let ended = scorer.take_pending_outputs();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].kind, CorrelatorEventKind::EpisodeEnded);
        assert_eq!(ended[0].from_level, SeverityLevel::High);
        assert_eq!(ended[0].timestamp_sec, spike_sec + 1);
        assert_eq!(ended[0].correlation.last_updated, spike_sec + 1);
        assert_eq!(ended[0].correlation.pattern, started[0].correlation.pattern);

        // The drain is once-only.
        assert!(scorer.take_pending_outputs().is_empty());
    }

    /// Ports `TestEpisodeMediumCorrelationEventThreshold`.
    #[test]
    fn episode_opens_at_medium_threshold() {
        let config = ScorerConfig {
            correlation_events: true,
            correlation_event_threshold: "medium".to_string(),
            low_threshold: 0.1,
            high_threshold: 0.9,
            margin_pct: 0.05, // the high-relative margin stays below the low threshold
            ..episode_test_config()
        };
        let mut scorer = AnomalyScorer::new(config);

        scorer.advance_to(1000);
        scorer.process_anomaly(&make_anomaly("bocpd", 1001, None));
        scorer.advance_to(1001); // Low → Medium; the score stays below the High threshold

        let correlations = scorer.active_correlations();
        assert_eq!(correlations.len(), 1);
        assert_eq!(correlations[0].pattern, "anomaly_scorer_medium:1001");

        scorer.take_pending_outputs();
        scorer.advance_to(1002); // Medium → Low closes the episode
        assert!(scorer.active_correlations().is_empty());
    }

    /// Ports `TestActiveCorrelationsSnapshotSafe`: repeated reads are stable and events drain once.
    #[test]
    fn active_correlations_are_snapshot_safe() {
        let config = ScorerConfig {
            correlation_events: true,
            ..episode_test_config()
        };
        let mut scorer = AnomalyScorer::new(config);

        let spike_sec = seed_and_cross_high_threshold(&mut scorer, 1000);
        let first = scorer.active_correlations();
        let second = scorer.active_correlations();
        assert_eq!(first.len(), second.len());
        assert_eq!(first, second);

        scorer.take_pending_outputs();
        scorer.advance_to(spike_sec + 1);
        assert!(scorer.active_correlations().is_empty());

        // EpisodeEnded is in the outputs, and the drain is once-only.
        let ended = scorer.take_pending_outputs();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].kind, CorrelatorEventKind::EpisodeEnded);
        assert!(scorer.take_pending_outputs().is_empty(), "second drain is empty");
    }

    /// Ports `TestActiveCorrelationsOpenEpisodeVisible`: an episode that never de-escalates stays open
    /// at EOF (end of replay).
    #[test]
    fn episode_stays_open_at_eof() {
        let config = ScorerConfig {
            correlation_events: true,
            ..episode_test_config()
        };
        let mut scorer = AnomalyScorer::new(config);

        let spike_sec = seed_and_cross_high_threshold(&mut scorer, 1000);

        let correlations = scorer.active_correlations();
        assert_eq!(correlations.len(), 1, "open episode at EOF");
        let episode = &correlations[0];
        assert!(!episode.pattern.is_empty());
        assert_eq!(episode.first_seen, spike_sec);
        assert_eq!(episode.last_updated, spike_sec);
        assert!(episode.title.contains("high"));
    }

    /// Ports `TestMaxEpisodeAnomalies`.
    #[test]
    fn episode_anomalies_are_capped() {
        let config = ScorerConfig {
            correlation_events: true,
            max_episode_anomalies: 3,
            window_secs: 30, // keeps the episode open across the subsequent advances
            ..episode_test_config()
        };
        let mut scorer = AnomalyScorer::new(config);

        let spike_sec = seed_and_cross_high_threshold(&mut scorer, 1000);
        for i in 1..=10 {
            scorer.process_anomaly(&make_anomaly("bocpd", spike_sec + i, None));
            scorer.advance_to(spike_sec + i);
        }

        let correlations = scorer.active_correlations();
        assert!(!correlations.is_empty(), "episode still open with window 30");
        assert!(correlations[0].anomalies.len() <= 3, "bounded episode anomalies");
    }

    /// Ports `TestPendingEvents_EpisodeStartedIncludesContributors`.
    #[test]
    fn episode_started_includes_contributors() {
        let config = ScorerConfig {
            correlation_events: true,
            max_reported_items: 1,
            ..episode_test_config()
        };
        let mut scorer = AnomalyScorer::new(config);

        scorer.advance_to(1000); // seed at Low
        let mut anomaly = make_anomaly("holt_residual", 1001, Some(40.0));
        anomaly.series_ref = handle(42);
        scorer.process_anomaly(&anomaly);
        scorer.advance_to(1001);

        let events = scorer.take_pending_outputs();
        assert_eq!(events.len(), 1);
        let contributors = &events[0].contributors;
        assert_eq!(contributors.len(), 1);
        assert_eq!(contributors[0].handle, anomaly.series_ref.unwrap());
        assert_eq!(contributors[0].weight, 3.0);
        assert_eq!(contributors[0].share, 1.0);
    }

    /// Episode anomalies keep their original timestamps, and `last_updated` follows the original
    /// timestamps rather than the effective scheduling second.
    #[test]
    fn episode_anomalies_keep_original_timestamps() {
        let config = ScorerConfig {
            correlation_events: true,
            window_secs: 30,
            ..episode_test_config()
        };
        let mut scorer = AnomalyScorer::new(config);

        let spike_sec = seed_and_cross_high_threshold(&mut scorer, 1000);

        // A late anomaly (historical timestamp, clamped for scheduling) while the episode is open.
        scorer.process_anomaly(&make_anomaly("bocpd", spike_sec - 2, None));
        // A future anomaly: appended to the episode before the advance reaches it.
        scorer.process_anomaly(&make_anomaly("bocpd", spike_sec + 4, None));

        let episode = &scorer.active_correlations()[0];
        assert_eq!(episode.anomalies.len(), 2);
        assert_eq!(
            episode.anomalies[0].timestamp_sec,
            spike_sec - 2,
            "late anomaly kept its time"
        );
        assert_eq!(
            episode.anomalies[1].timestamp_sec,
            spike_sec + 4,
            "future anomaly kept its time"
        );
        assert_eq!(episode.last_updated, spike_sec + 4, "max of the original timestamps");
    }

    /// Ports `TestPendingEvents_DisabledWhenCorrelationEventsOff` and
    /// `TestPendingEvents_ResetClearsPending`.
    #[test]
    fn pending_events_disabled_and_cleared_by_reset() {
        let mut scorer = AnomalyScorer::new(episode_test_config());
        seed_and_cross_high_threshold(&mut scorer, 1000);
        assert!(scorer.take_pending_outputs().is_empty(), "episodes disabled");

        let config = ScorerConfig {
            correlation_events: true,
            ..episode_test_config()
        };
        let mut scorer = AnomalyScorer::new(config);
        seed_and_cross_high_threshold(&mut scorer, 1000);
        scorer.reset();
        assert!(scorer.take_pending_outputs().is_empty(), "reset discards unread events");
        assert!(scorer.active_correlations().is_empty(), "reset closes the open episode");
    }

    /// Ports `TestReset`: all accumulated state is cleared.
    #[test]
    fn reset_clears_ewma_and_buckets() {
        let mut scorer = AnomalyScorer::new(ScorerConfig::production());
        scorer.process_anomaly(&make_anomaly("bocpd", 1000, None));
        scorer.advance_to(1000);
        assert!(!scorer.score_state().buckets.is_empty());

        scorer.reset();
        assert!(scorer.score_state().buckets.is_empty());
        assert_eq!(scorer.last_score(), 0.0);
        assert!(scorer.active_correlations().is_empty());
    }

    /// Ports `TestActiveCorrelationsResetClearsEpisodes` plus the dispatcher reset: after a reset the
    /// watcher re-seeds at Low and the replayed spike reopens an episode.
    #[test]
    fn reset_reopens_episodes_on_replay() {
        let config = ScorerConfig {
            correlation_events: true,
            ..episode_test_config()
        };
        let mut scorer = AnomalyScorer::new(config);

        seed_and_cross_high_threshold(&mut scorer, 1000);
        assert_eq!(scorer.active_correlations().len(), 1);
        scorer.reset();

        // A new escalation after the reset must open a fresh episode (cooldown state also reset).
        seed_and_cross_high_threshold(&mut scorer, 2000);
        assert_eq!(scorer.active_correlations().len(), 1);
        let events = scorer.take_pending_outputs();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, CorrelatorEventKind::EpisodeStarted);
        assert_eq!(events[0].timestamp_sec, 2001);
    }

    /// The watcher honors the production cooldown (300s): the de-escalation — and therefore the episode
    /// close — is delayed until the cooldown expires, while the raw severity drops immediately.
    #[test]
    fn episode_close_is_delayed_by_cooldown_three_hundred() {
        let config = ScorerConfig {
            correlation_events: true,
            cooldown_secs: 300,
            ..episode_test_config()
        };
        let mut scorer = AnomalyScorer::new(config);

        let spike_sec = seed_and_cross_high_threshold(&mut scorer, 1000); // Low → High at 1001
        scorer.take_pending_outputs();

        scorer.advance_to(1002); // raw drops to Low; the watcher's cooldown blocks the delivery
        assert_eq!(
            scorer.active_correlations().len(),
            1,
            "episode held open by the cooldown"
        );

        scorer.advance_to(1300); // 1300 - 1001 = 299 < 300: still blocked
        assert_eq!(scorer.active_correlations().len(), 1);

        scorer.advance_to(1301); // 1301 - 1001 = 300: the de-escalation is delivered
        assert!(
            scorer.active_correlations().is_empty(),
            "episode closes after the cooldown"
        );

        let events = scorer.take_pending_outputs();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, CorrelatorEventKind::EpisodeEnded);
        assert_eq!(events[0].timestamp_sec, 1301);
        assert_eq!(events[0].from_level, SeverityLevel::High);
        assert_eq!(events[0].to_level, SeverityLevel::Low);
        assert_eq!(spike_sec, 1001);
    }

    // ---- multi-second advance ordering ----

    /// Pins the Go multi-second `Advance` ordering: every per-second state is computed **before** the
    /// dispatchers are driven, so the episode-start contributor snapshot already contains anomalies from
    /// later seconds of the same advance. Do not repair this ordering.
    #[test]
    fn multi_second_advance_finalizes_states_before_dispatchers() {
        let config = ScorerConfig {
            correlation_events: true,
            window_secs: 15,
            ..episode_test_config()
        };
        let mut scorer = AnomalyScorer::new(config);

        scorer.advance_to(1000); // seed at Low

        // Two anomalies on distinct series in consecutive seconds, submitted before one advance.
        let mut first = make_anomaly("holt_residual", 1001, Some(40.0));
        first.series_ref = handle(1);
        let mut second = make_anomaly("holt_residual", 1002, Some(40.0));
        second.series_ref = handle(2);
        scorer.process_anomaly(&first);
        scorer.process_anomaly(&second);

        scorer.advance_to(1002); // one multi-second advance

        let events = scorer.take_pending_outputs();
        assert_eq!(events.len(), 1, "episode opened");
        assert_eq!(events[0].kind, CorrelatorEventKind::EpisodeStarted);
        assert_eq!(
            events[0].timestamp_sec, 1001,
            "the episode opens at the crossing second"
        );

        // The contributor snapshot was taken while driving the dispatchers, after second 1002's
        // anomalies were already merged into the top-anomaly buffer: both series appear, ranked by
        // weight (tie) then ref, with normalized 50/50 shares.
        let contributors = &events[0].contributors;
        assert_eq!(contributors.len(), 2, "the later second's anomaly is in the snapshot");
        assert_eq!(contributors[0].handle.series.raw(), 1);
        assert_eq!(contributors[1].handle.series.raw(), 2);
        assert_eq!(contributors[0].weight, 3.0);
        assert_eq!(contributors[1].weight, 3.0);
        assert!((contributors[0].share - 0.5).abs() < 1e-12);
        assert!((contributors[1].share - 0.5).abs() < 1e-12);

        // Both seconds were finalized in the same advance.
        assert_eq!(bucket_at(&scorer, 1001).map(|bucket| bucket.count), Some(1));
        assert_eq!(bucket_at(&scorer, 1002).map(|bucket| bucket.count), Some(2));
    }

    // ---- score-tick capture hook ----

    /// The hook produces one tick per scorer second — including empty seconds — with the exact
    /// per-second calculation, without changing any scorer output.
    #[test]
    fn tick_hook_emits_one_tick_per_second_including_empty() {
        let config = ScorerConfig {
            correlation_events: true,
            alpha: 0.5,
            window_secs: 15,
            ..episode_test_config()
        };

        // Reference run without the hook.
        let mut plain = AnomalyScorer::new(config.clone());
        plain.advance_to(1000);
        plain.process_anomaly(&make_anomaly("holt_residual", 1001, Some(40.0)));
        plain.advance_to(1003);

        // Instrumented run with the hook.
        let ticks: Rc<RefCell<Vec<ScoreTick>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = ticks.clone();
        let mut scorer = AnomalyScorer::new(config);
        scorer.set_tick_hook(Box::new(move |tick| sink.borrow_mut().push(*tick)));

        scorer.advance_to(1000); // empty second
        scorer.process_anomaly(&make_anomaly("holt_residual", 1001, Some(40.0)));
        scorer.advance_to(1003); // seconds 1001, 1002, 1003

        let ticks = ticks.borrow();
        assert_eq!(
            ticks.iter().map(|tick| tick.second).collect::<Vec<_>>(),
            vec![1000, 1001, 1002, 1003],
            "one tick per scorer second, empty seconds included"
        );

        let alpha = 0.5;
        let input = 3.0 * (1.0 - (-1.0f64).exp()); // holt 40 → level 4, weight 3.0, count 1, k = 1
        let empty = &ticks[0];
        assert_eq!(empty.count, 0);
        assert_eq!(empty.input, 0.0);
        assert_eq!(empty.ewma, 0.0);
        assert_eq!(empty.raw_severity, SeverityLevel::Low);
        assert_eq!(empty.delivered_severity, Some(SeverityLevel::Low));

        let spike = &ticks[1];
        assert_eq!(spike.bins, [0, 0, 0, 0, 1]);
        assert_eq!(spike.count, 1);
        assert_eq!(spike.weight_sum, 3.0);
        assert!((spike.input - input).abs() < 1e-12);
        assert!((spike.ewma - alpha * input).abs() < 1e-12);
        assert_eq!(spike.raw_severity, SeverityLevel::High);
        assert_eq!(spike.delivered_severity, Some(SeverityLevel::High));

        let next = &ticks[2];
        assert_eq!(next.count, 1, "window 15 keeps the series live");
        assert!((next.ewma - (alpha * input + (1.0 - alpha) * spike.ewma)).abs() < 1e-12);

        // The hook must not alter the outputs: identical buckets, score, and events.
        assert_eq!(plain.score_state().buckets, scorer.score_state().buckets);
        assert_eq!(plain.last_score(), scorer.last_score());
        assert_eq!(plain.take_pending_outputs(), scorer.take_pending_outputs());
        assert_eq!(plain.active_correlations(), scorer.active_correlations());
    }

    /// The tick's delivered severity reflects the watcher's cooldown, not the raw severity.
    #[test]
    fn tick_delivered_severity_reflects_cooldown() {
        let config = ScorerConfig {
            correlation_events: true,
            cooldown_secs: 300,
            ..episode_test_config()
        };
        let mut scorer = AnomalyScorer::new(config);

        let ticks: Rc<RefCell<Vec<ScoreTick>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = ticks.clone();
        scorer.set_tick_hook(Box::new(move |tick| sink.borrow_mut().push(*tick)));

        seed_and_cross_high_threshold(&mut scorer, 1000); // Low → High at 1001
        scorer.advance_to(1002); // raw Low, delivered still High (cooldown)

        let ticks = ticks.borrow();
        let blocked = ticks.last().expect("tick at 1002");
        assert_eq!(blocked.second, 1002);
        assert_eq!(blocked.raw_severity, SeverityLevel::Low);
        assert_eq!(blocked.delivered_severity, Some(SeverityLevel::High));
    }

    /// With episodes disabled there is no watcher subscription, so the tick has no delivered severity.
    #[test]
    fn tick_delivered_severity_is_none_when_episodes_disabled() {
        let mut scorer = AnomalyScorer::new(ScorerConfig {
            alpha: 0.99,
            saturation_k: 1.0,
            window_secs: 1,
            ..ScorerConfig::production()
        });

        let ticks: Rc<RefCell<Vec<ScoreTick>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = ticks.clone();
        scorer.set_tick_hook(Box::new(move |tick| sink.borrow_mut().push(*tick)));

        scorer.process_anomaly(&make_anomaly("bocpd", 1001, None));
        scorer.advance_to(1001);

        let ticks = ticks.borrow();
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].raw_severity, SeverityLevel::High);
        assert_eq!(ticks[0].delivered_severity, None);
    }
}
