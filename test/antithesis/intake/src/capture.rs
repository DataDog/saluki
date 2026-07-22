//! Differential metric context capture.
//!
//! See scenario README for details.

use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use datadog_protos::metrics::{metric_payload::MetricSeries, MetricPayload, SketchPayload};
use serde::{Deserialize, Serialize};
use stele::{Metric, MetricValue};
use tracing::warn;

const SELF_TELEMETRY_PREFIX: &str = "datadog.";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Target {
    Agent,
    Adp,
}

impl Target {
    #[must_use]
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "agent" => Some(Self::Agent),
            "adp" => Some(Self::Adp),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Adp => "adp",
        }
    }
}

/// The flushed type of a metric, part of a context's identity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MetricKind {
    Count,
    Rate,
    Gauge,
    Sketch,
}

impl MetricKind {
    fn of(metric: &Metric) -> Option<Self> {
        metric.values().first().map(|(_, value)| match value {
            MetricValue::Count { .. } => Self::Count,
            MetricValue::Rate { .. } => Self::Rate,
            MetricValue::Gauge { .. } => Self::Gauge,
            MetricValue::Sketch { .. } => Self::Sketch,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct EpochSeconds(i64);

impl EpochSeconds {
    pub(crate) const fn from_epoch_secs(secs: i64) -> Self {
        Self(secs)
    }

    /// The intake's current wall-clock time, or `None` if the clock predates
    /// the epoch or overflows.
    pub(crate) fn now() -> Option<Self> {
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        i64::try_from(secs).ok().map(Self)
    }
}

/// A metric context: name, tagset, and type.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct Context {
    pub(crate) name: String,
    pub(crate) tagset: BTreeSet<String>,
    pub(crate) kind: MetricKind,
}

/// A context, the time it first arrived on its lane, and its cumulative point count.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ContextAt {
    #[serde(flatten)]
    pub(crate) context: Context,
    pub(crate) first_seen: EpochSeconds,
    pub(crate) points: u64,
}

/// One lane's contexts and the intake's current time.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct LaneView {
    pub(crate) now: EpochSeconds,
    pub(crate) contexts: Vec<ContextAt>,
}

/// A context's first-arrival time on a lane and its cumulative point count.
#[derive(Clone, Copy, Debug)]
struct Seen {
    first_seen: EpochSeconds,
    points: u64,
}

#[derive(Debug, Default)]
struct Lanes {
    seen: BTreeMap<(Target, Context), Seen>,
}

impl Lanes {
    fn record(&mut self, target: Target, contexts: &[(Context, u64)], now: EpochSeconds) -> usize {
        let mut added = 0;
        for (context, points) in contexts {
            if context.name.starts_with(SELF_TELEMETRY_PREFIX) {
                continue;
            }
            match self.seen.entry((target, context.clone())) {
                Entry::Vacant(slot) => {
                    slot.insert(Seen {
                        first_seen: now,
                        points: *points,
                    });
                    added += 1;
                }
                Entry::Occupied(mut slot) => slot.get_mut().points += *points,
            }
        }
        added
    }

    fn contexts(&self, target: Target) -> Vec<ContextAt> {
        self.seen
            .iter()
            .filter(|((lane, _), _)| *lane == target)
            .map(|((_, context), seen)| ContextAt {
                context: context.clone(),
                first_seen: seen.first_seen,
                points: seen.points,
            })
            .collect()
    }
}

/// Shared handle to the lanes mechanism. Written to by HTTP handlers, read from
/// by the check programs via control routes.
#[derive(Clone, Debug, Default)]
pub struct State {
    lanes: Arc<Mutex<Lanes>>,
}

impl State {
    /// Creates an empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record_series_v2(&self, target: Target, payload: MetricPayload, now: EpochSeconds) -> usize {
        let contexts = observe_series(target, payload);
        self.with_lanes(|lanes| lanes.record(target, &contexts, now))
    }

    pub(crate) fn record_sketches(&self, target: Target, payload: SketchPayload, now: EpochSeconds) -> usize {
        let contexts = observe_sketches(target, payload);
        self.with_lanes(|lanes| lanes.record(target, &contexts, now))
    }

    pub(crate) fn contexts(&self, target: Target) -> Vec<ContextAt> {
        self.with_lanes(|lanes| lanes.contexts(target))
    }

    fn with_lanes<T>(&self, f: impl FnOnce(&mut Lanes) -> T) -> T {
        f(&mut self.lanes.lock().expect("capture lock poisoned"))
    }
}

/// Longest metric name propjoe stores, in bytes (`model.MaxMetricLen`).
const MAX_METRIC_NAME_LEN: usize = 350;
/// Most tags propjoe keeps on a series (`model.MaxTagThresh`).
const MAX_TAG_COUNT: usize = 100;
/// Most resources propjoe keeps on a series (`model.MaxResourceThresh`).
const MAX_RESOURCE_COUNT: usize = 500;

/// Whether propjoe's v2 ingest keeps this series. It drops any series with an invalid metric
/// name (`ValidateMetricName`: empty, over `MaxMetricLen` bytes, or no ASCII-alphabetic byte),
/// more than `MaxTagThresh` tags, or more than `MaxResourceThresh` resources. Matching keeps
/// our captured context set equal to what production would store.
pub(crate) fn series_kept_by_intake(series: &MetricSeries) -> bool {
    let name = series.metric.as_str();
    let name_ok =
        !name.is_empty() && name.len() <= MAX_METRIC_NAME_LEN && name.bytes().any(|b| b.is_ascii_alphabetic());
    name_ok && series.tags.len() <= MAX_TAG_COUNT && series.resources.len() <= MAX_RESOURCE_COUNT
}

/// Decodes a `/api/v2/series` payload into contexts and their point counts with stele's
/// `Metric::try_from_series_v2`. A metric contributes as many points as it carries values.
fn observe_series(target: Target, payload: MetricPayload) -> Vec<(Context, u64)> {
    let mut contexts = Vec::new();
    for series in payload.series {
        if !series_kept_by_intake(&series) {
            continue;
        }
        let mut single = MetricPayload::new();
        single.series.push(series);
        match Metric::try_from_series_v2(single) {
            Ok(metrics) => contexts.extend(
                metrics
                    .iter()
                    .filter_map(|metric| Some((context_of(metric)?, metric.values().len() as u64))),
            ),
            Err(error) => {
                warn!(target = target.as_str(), %error, "skipped a series that did not convert to a stele metric");
            }
        }
    }
    contexts
}

/// Decodes a sketch payload into contexts and their point counts.
fn observe_sketches(target: Target, payload: SketchPayload) -> Vec<(Context, u64)> {
    let mut contexts = Vec::new();
    for sketch in payload.sketches {
        let mut single = SketchPayload::new();
        single.sketches.push(sketch);
        match Metric::try_from_sketch(single) {
            Ok(metrics) => contexts.extend(
                metrics
                    .iter()
                    .filter_map(|metric| Some((context_of(metric)?, metric.values().len() as u64))),
            ),
            Err(error) => {
                warn!(target = target.as_str(), %error, "skipped a sketch that did not convert to a stele metric");
            }
        }
    }
    contexts
}

fn context_of(metric: &Metric) -> Option<Context> {
    let kind = MetricKind::of(metric)?;
    Some(Context {
        name: metric.context().name().to_string(),
        tagset: metric.context().tags().iter().cloned().collect(),
        kind,
    })
}

#[cfg(test)]
mod tests;
