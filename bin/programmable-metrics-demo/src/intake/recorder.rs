//! What one recording intake has received, in bounded memory.
//!
//! Every collection an intake keeps is bounded, whatever it is sent: the series and requests are rings, a family
//! counts at most [`MAX_NAMES_PER_FAMILY`] names, at most [`MAX_FAMILIES`] families are kept (the one seen longest ago
//! makes room for a new one), at most [`MAX_API_KEYS`] distinct API keys are listed, and canary counts cover the last
//! [`CANARY_HISTORY_SECS`] seconds of timestamps.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};

use serde::Serialize;
use stele::{Metric, MetricValue};

use crate::settings::Recipient;

/// How many decoded series an intake keeps, most recent first out.
pub const SERIES_CAPACITY: usize = 4096;

/// How many requests an intake keeps.
pub const REQUEST_CAPACITY: usize = 256;

/// How many one-second buckets a family summary reports.
pub const HISTORY_SECS: u64 = 30;

/// How long a family that stopped arriving is still summarized, in milliseconds.
pub const FAMILY_RETENTION_MS: u64 = 120_000;

/// How long a family may go without arriving and still count as arriving continuously, in milliseconds.
///
/// Payloads leave ADP every second or two, so a longer gap means the family stopped arriving and later came back, as
/// when a routing change sends it to an intake again.
pub const FAMILY_GAP_MS: u64 = 5_000;

/// How many distinct names one family counts before it stops recording more.
pub const MAX_NAMES_PER_FAMILY: usize = 256;

/// How many families an intake keeps.
pub const MAX_FAMILIES: usize = 512;

/// How many distinct API keys an intake lists.
pub const MAX_API_KEYS: usize = 32;

/// The name of the metric whose counts the demo checks for conservation.
pub const CANARY_METRIC: &str = "demo.canary";

/// How many seconds of canary counts, by point timestamp, an intake keeps.
pub const CANARY_HISTORY_SECS: u64 = 300;

/// How far past the time a request arrives a canary timestamp may be and still be counted, in seconds.
const CANARY_FUTURE_SECS: u64 = 60;

/// The kind of one decoded series, as the intake API represents it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SeriesKind {
    /// A count.
    Count,

    /// A rate: a count divided by its interval.
    Rate,

    /// A gauge.
    Gauge,

    /// A distribution sketch.
    Sketch,
}

/// One decoded series, as an intake received it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RecordedSeries {
    /// When the request carrying the series arrived, in milliseconds since the Unix epoch.
    pub received_ms: u64,

    /// The request path.
    pub path: String,

    /// The API key the request carried, if any.
    pub api_key: Option<String>,

    /// The metric name.
    pub name: String,

    /// The tags, with the host folded in as `host:<name>`.
    pub tags: Vec<String>,

    /// The kind of the series' points.
    pub kind: SeriesKind,

    /// How many points (timestamped values or sketches) the series carried.
    pub points: usize,

    /// The series' total: the sum of its counts (rates multiplied by their interval), the number of samples in its
    /// sketches, or, for a gauge, its last value.
    pub total: f64,
}

/// One request an intake received.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RequestRecord {
    /// When the request arrived, in milliseconds since the Unix epoch.
    pub received_ms: u64,

    /// The request path.
    pub path: String,

    /// The API key the request carried, if any.
    pub api_key: Option<String>,

    /// The status code the intake answered with.
    pub status: u16,

    /// How many series the request carried, for metric payloads.
    pub series: usize,

    /// Why the intake refused the request, if it did.
    pub error: Option<String>,
}

/// A summary of one intake, for the UI.
#[derive(Clone, Debug, Serialize)]
pub struct IntakeSummary {
    /// The recipient's short name.
    pub name: &'static str,

    /// The recipient's display label.
    pub label: &'static str,

    /// The intake's port.
    pub port: u16,

    /// The API key ADP is configured to send to this endpoint.
    pub expected_api_key: &'static str,

    /// Whether a resources artifact declares the endpoint, rather than ADP's configuration file.
    pub declared: bool,

    /// Whether the intake is paused: it answers every request with `503 Service Unavailable`.
    pub paused: bool,

    /// Every request received.
    pub requests_total: u64,

    /// Requests answered with `503 Service Unavailable` while the intake was paused, which are not refusals.
    pub unavailable: u64,

    /// Requests the intake refused: an unsupported encoding, a body too large, or a body that could not be
    /// decompressed or decoded.
    pub refused: u64,

    /// The most recent refusal, if any.
    pub last_refusal: Option<RequestRecord>,

    /// When the last request arrived.
    pub last_request_ms: Option<u64>,

    /// The API keys requests carried, with how many requests carried each, up to [`MAX_API_KEYS`] keys.
    pub api_keys: Vec<ApiKeySeen>,

    /// Requests that carried a key beyond the first [`MAX_API_KEYS`] distinct ones.
    pub other_api_key_requests: u64,

    /// Every point received.
    pub points_total: u64,

    /// The total count of the canary metric received.
    pub canary_total: f64,

    /// The metric families received within the retention period, by family name.
    pub families: Vec<FamilySummary>,
}

/// An API key an intake has seen.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ApiKeySeen {
    /// The key, or an empty string for requests without one.
    pub key: String,

    /// How many requests carried it.
    pub requests: u64,

    /// Whether it is the key ADP is configured to send to this endpoint.
    pub expected: bool,
}

/// What a family's per-second values count.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FamilyUnit {
    /// Events: the counted value of counts and rates, and the samples of sketches. They follow what was sent, whatever
    /// the aggregation window.
    Events,

    /// Points, for a family of gauges only, whose values are levels rather than events: one per context per flush.
    Points,
}

/// A summary of one metric family at one intake.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FamilySummary {
    /// The family name: the metric name, with a final all-digit segment written `*`.
    pub family: String,

    /// How many distinct metric names the family has had (counting stops at [`MAX_NAMES_PER_FAMILY`]).
    pub names: usize,

    /// The first metric name of the family, in sorted order.
    pub example: String,

    /// The kinds the family's series have had.
    pub kinds: Vec<SeriesKind>,

    /// When the family last arrived.
    pub last_seen_ms: u64,

    /// How long ago the family last arrived, at the time of the summary.
    pub age_ms: u64,

    /// When the family began arriving without a gap longer than [`FAMILY_GAP_MS`].
    pub active_since_ms: u64,

    /// Every point of the family received.
    pub points_total: u64,

    /// What `per_second` counts.
    pub unit: FamilyUnit,

    /// The events (or, for gauges, points) received in each of the last [`HISTORY_SECS`] seconds, oldest first,
    /// ending with the current second.
    pub per_second: Vec<f64>,
}

/// Returns the family a metric name belongs to: the name, with a final all-digit segment written `*`.
///
/// Numbered metrics, such as `demo.debug.0` through `demo.debug.19`, are one family (`demo.debug.*`), so that the UI
/// shows them as one row.
pub fn family_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => format!("{}.*", head),
        _ => name.to_string(),
    }
}

/// Returns the kind, point count, and total of a decoded metric.
fn describe(metric: &Metric) -> (SeriesKind, usize, f64) {
    let values = metric.values();
    let kind = match values.first().map(|(_, value)| value) {
        Some(MetricValue::Rate { .. }) => SeriesKind::Rate,
        Some(MetricValue::Gauge { .. }) => SeriesKind::Gauge,
        Some(MetricValue::Sketch { .. }) => SeriesKind::Sketch,
        Some(MetricValue::Count { .. }) | None => SeriesKind::Count,
    };
    let mut total = 0.0;
    for (_, value) in values {
        match value {
            MetricValue::Count { value } => total += value,
            MetricValue::Rate { interval, value } => total += value * (*interval as f64),
            MetricValue::Gauge { value } => total = *value,
            MetricValue::Sketch { sketch } => total += sketch.count() as f64,
        }
    }
    (kind, values.len(), total)
}

/// Returns the count a point carries: its value for a count, its value times its interval for a rate.
fn counted(value: &MetricValue) -> Option<f64> {
    match value {
        MetricValue::Count { value } => Some(*value),
        MetricValue::Rate { interval, value } => Some(value * (*interval as f64)),
        MetricValue::Gauge { .. } | MetricValue::Sketch { .. } => None,
    }
}

#[derive(Default)]
struct FamilyState {
    names: BTreeSet<String>,
    kinds: BTreeSet<SeriesKind>,
    last_seen_ms: u64,
    active_since_ms: u64,
    points_total: u64,
    /// Events (points, for gauges) per second, as `(unix second, count)`, oldest first, covering at most the last
    /// `HISTORY_SECS`.
    buckets: VecDeque<(u64, f64)>,
}

impl FamilyState {
    fn record(&mut self, name: &str, kind: SeriesKind, points: usize, total: f64, received_ms: u64) {
        if self.names.len() < MAX_NAMES_PER_FAMILY && !self.names.contains(name) {
            self.names.insert(name.to_string());
        }
        if self.points_total == 0 || received_ms > self.last_seen_ms + FAMILY_GAP_MS {
            self.active_since_ms = received_ms;
        }
        self.kinds.insert(kind);
        self.last_seen_ms = self.last_seen_ms.max(received_ms);
        self.points_total += points as u64;

        let count = match kind {
            SeriesKind::Gauge => points as f64,
            SeriesKind::Count | SeriesKind::Rate | SeriesKind::Sketch => total,
        };
        let second = received_ms / 1000;
        match self.buckets.back_mut() {
            Some((last, bucket)) if *last == second => *bucket += count,
            // Requests are recorded in arrival order, so a bucket for an earlier second is already in the queue (or
            // has expired): count it in the latest one rather than reordering.
            Some((last, bucket)) if *last > second => *bucket += count,
            _ => self.buckets.push_back((second, count)),
        }
        while let Some((first, _)) = self.buckets.front() {
            if *first + HISTORY_SECS <= second {
                self.buckets.pop_front();
            } else {
                break;
            }
        }
    }

    fn summary(&self, family: &str, now_ms: u64) -> FamilySummary {
        let now_second = now_ms / 1000;
        let first_second = (now_second + 1).saturating_sub(HISTORY_SECS);
        let mut per_second = vec![0.0; HISTORY_SECS as usize];
        for (second, count) in &self.buckets {
            if (first_second..=now_second).contains(second) {
                per_second[(second - first_second) as usize] += count;
            }
        }
        let gauges_only = self.kinds.iter().all(|kind| *kind == SeriesKind::Gauge);
        FamilySummary {
            family: family.to_string(),
            names: self.names.len(),
            example: self.names.iter().next().cloned().unwrap_or_default(),
            kinds: self.kinds.iter().copied().collect(),
            last_seen_ms: self.last_seen_ms,
            age_ms: now_ms.saturating_sub(self.last_seen_ms),
            active_since_ms: self.active_since_ms,
            points_total: self.points_total,
            unit: if gauges_only {
                FamilyUnit::Points
            } else {
                FamilyUnit::Events
            },
            per_second,
        }
    }
}

/// Canary counts by point timestamp (Unix seconds): the second ADP's aggregation assigned them to.
#[derive(Default)]
struct CanaryCounts(BTreeMap<u64, f64>);

impl CanaryCounts {
    fn add(&mut self, timestamp: u64, count: f64, received_s: u64) {
        let oldest = received_s.saturating_sub(CANARY_HISTORY_SECS);
        if timestamp < oldest || timestamp > received_s + CANARY_FUTURE_SECS {
            return;
        }
        *self.0.entry(timestamp).or_default() += count;
        self.0 = self.0.split_off(&oldest);
    }

    fn between(&self, from_s: u64, to_s: u64) -> f64 {
        // Folded from 0.0: an empty `sum` of floats is -0.0, which would read as "-0".
        self.0.range(from_s..to_s).fold(0.0, |total, (_, count)| total + count)
    }
}

#[derive(Default)]
struct IntakeState {
    series: VecDeque<RecordedSeries>,
    requests: VecDeque<RequestRecord>,
    families: BTreeMap<String, FamilyState>,
    api_keys: BTreeMap<String, u64>,
    other_api_key_requests: u64,
    paused: bool,
    requests_total: u64,
    unavailable: u64,
    refused: u64,
    last_refusal: Option<RequestRecord>,
    last_request_ms: Option<u64>,
    points_total: u64,
    canary_total: f64,
    canaries: CanaryCounts,
}

impl IntakeState {
    fn count_api_key(&mut self, key: Option<&str>) {
        let key = key.unwrap_or_default();
        if let Some(requests) = self.api_keys.get_mut(key) {
            *requests += 1;
        } else if self.api_keys.len() < MAX_API_KEYS {
            self.api_keys.insert(key.to_string(), 1);
        } else {
            self.other_api_key_requests += 1;
        }
    }

    /// Returns the family named `family`, making room for it, if it is new, by evicting the family seen longest ago.
    fn family(&mut self, family: String) -> &mut FamilyState {
        if !self.families.contains_key(&family) && self.families.len() >= MAX_FAMILIES {
            let oldest = self
                .families
                .iter()
                .min_by_key(|(_, state)| state.last_seen_ms)
                .map(|(name, _)| name.clone());
            if let Some(oldest) = oldest {
                self.families.remove(&oldest);
            }
        }
        self.families.entry(family).or_default()
    }
}

/// The record of what one intake has received.
///
/// Cheap to clone: clones share the record.
#[derive(Clone)]
pub struct IntakeRecorder {
    recipient: Recipient,
    state: Arc<Mutex<IntakeState>>,
}

impl IntakeRecorder {
    /// Creates an empty record for the given recipient.
    pub fn new(recipient: Recipient) -> Self {
        Self {
            recipient,
            state: Arc::new(Mutex::new(IntakeState::default())),
        }
    }

    /// Returns the recipient this intake stands in for.
    pub fn recipient(&self) -> &Recipient {
        &self.recipient
    }

    /// Pauses the intake, so that it answers every request with `503 Service Unavailable`, or resumes it.
    pub fn set_paused(&self, paused: bool) {
        self.lock().paused = paused;
    }

    /// Returns `true` if the intake is paused.
    pub fn is_paused(&self) -> bool {
        self.lock().paused
    }

    /// Records a request a paused intake answered with `503 Service Unavailable` without reading it.
    pub fn record_unavailable(&self, request: RequestRecord) {
        let mut state = self.lock();
        state.requests_total += 1;
        state.unavailable += 1;
        state.last_request_ms = Some(state.last_request_ms.unwrap_or(0).max(request.received_ms));
        state.count_api_key(request.api_key.as_deref());
        if state.requests.len() == REQUEST_CAPACITY {
            state.requests.pop_front();
        }
        state.requests.push_back(request);
    }

    /// Records one request, and the metrics decoded from it.
    pub fn record(&self, request: RequestRecord, metrics: Vec<Metric>) {
        let mut state = self.lock();
        state.requests_total += 1;
        if request.error.is_some() {
            state.refused += 1;
            state.last_refusal = Some(request.clone());
        }
        state.last_request_ms = Some(state.last_request_ms.unwrap_or(0).max(request.received_ms));
        state.count_api_key(request.api_key.as_deref());

        let received_s = request.received_ms / 1000;
        for metric in metrics {
            let (kind, points, total) = describe(&metric);
            if metric.context().name() == CANARY_METRIC {
                for (timestamp, value) in metric.values() {
                    if let Some(count) = counted(value) {
                        state.canary_total += count;
                        state.canaries.add(*timestamp, count, received_s);
                    }
                }
            }
            let (name, tags) = metric.context().clone().into_parts();
            state.points_total += points as u64;
            state
                .family(family_of(&name))
                .record(&name, kind, points, total, request.received_ms);
            if state.series.len() == SERIES_CAPACITY {
                state.series.pop_front();
            }
            state.series.push_back(RecordedSeries {
                received_ms: request.received_ms,
                path: request.path.clone(),
                api_key: request.api_key.clone(),
                name,
                tags,
                kind,
                points,
                total,
            });
        }

        let cutoff = request.received_ms.saturating_sub(FAMILY_RETENTION_MS);
        state.families.retain(|_, family| family.last_seen_ms >= cutoff);

        if state.requests.len() == REQUEST_CAPACITY {
            state.requests.pop_front();
        }
        state.requests.push_back(request);
    }

    /// Summarizes the intake at the given time.
    pub fn summary(&self, now_ms: u64) -> IntakeSummary {
        let state = self.lock();
        let cutoff = now_ms.saturating_sub(FAMILY_RETENTION_MS);
        IntakeSummary {
            name: self.recipient.name,
            label: self.recipient.label,
            port: self.recipient.port,
            expected_api_key: self.recipient.api_key,
            declared: self.recipient.declared,
            paused: state.paused,
            requests_total: state.requests_total,
            unavailable: state.unavailable,
            refused: state.refused,
            last_refusal: state.last_refusal.clone(),
            last_request_ms: state.last_request_ms,
            api_keys: state
                .api_keys
                .iter()
                .map(|(key, requests)| ApiKeySeen {
                    key: key.clone(),
                    requests: *requests,
                    expected: key == self.recipient.api_key,
                })
                .collect(),
            other_api_key_requests: state.other_api_key_requests,
            points_total: state.points_total,
            canary_total: state.canary_total,
            families: state
                .families
                .iter()
                .filter(|(_, family)| family.last_seen_ms >= cutoff)
                .map(|(name, family)| family.summary(name, now_ms))
                .collect(),
        }
    }

    /// Returns the canary count received with point timestamps from second `from_s` up to, not including, second
    /// `to_s` (Unix seconds), within the last [`CANARY_HISTORY_SECS`].
    pub fn canaries_between(&self, from_s: u64, to_s: u64) -> f64 {
        self.lock().canaries.between(from_s, to_s)
    }

    /// Returns up to `limit` of the most recently received series, most recent first.
    pub fn recent_series(&self, limit: usize) -> Vec<RecordedSeries> {
        self.lock().series.iter().rev().take(limit).cloned().collect()
    }

    /// Returns up to `limit` of the most recent requests, most recent first.
    pub fn recent_requests(&self, limit: usize) -> Vec<RequestRecord> {
        self.lock().requests.iter().rev().take(limit).cloned().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, IntakeState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
