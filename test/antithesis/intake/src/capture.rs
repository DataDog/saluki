//! Differential metric context capture.
//!
//! See scenario README for details.

use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use datadog_protos::metrics::metric_payload::{MetricSeries, MetricType, Resource};
use datadog_protos::metrics::{MetricPayload, SketchPayload};
use serde::{Deserialize, Serialize};

use crate::lenient_decode::V3Series;

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
    /// A metric type outside the known set — an out-of-range v3 type nibble. Production keeps such a
    /// series and forwards its type verbatim, so the intake keeps it too rather than dropping it and
    /// masking a producer bug.
    Other,
}

impl MetricKind {
    /// Derives the kind from the v2 wire type field. The accessor defaults any out-of-range type to
    /// `UNSPECIFIED`, which maps to `Other`, keeping the series and forwarding an unknown type rather
    /// than dropping it and masking a producer bug, as the v3 path does for an unknown type nibble.
    fn of(type_: MetricType) -> Self {
        match type_ {
            MetricType::COUNT => Self::Count,
            MetricType::RATE => Self::Rate,
            MetricType::GAUGE => Self::Gauge,
            MetricType::UNSPECIFIED => Self::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct EpochSeconds(i64);

impl EpochSeconds {
    pub(crate) const fn from_epoch_secs(secs: i64) -> Self {
        Self(secs)
    }

    /// The whole seconds since the Unix epoch.
    pub(crate) fn secs(self) -> i64 {
        self.0
    }

    /// The intake's current wall-clock time, or `None` if the clock predates
    /// the epoch or overflows.
    pub(crate) fn now() -> Option<Self> {
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        i64::try_from(secs).ok().map(Self)
    }
}

/// One point value as the native decoder reads it off the wire, kind-agnostic. The intake keeps no
/// curve, only whether a series carries a point that survives the backend's per-point drops, so a
/// series left with none emits no context.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BucketValue {
    /// A count, rate, or gauge scalar.
    Scalar(f64),
    /// A `DDSketch` point: the summary the Agent emits plus its log-grid bins.
    Sketch(SketchValue),
}

/// A `DDSketch` point: the summary the Agent emits plus the log-grid bins as `(key, count)`, key-sorted.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SketchValue {
    pub(crate) count: i64,
    pub(crate) sum: f64,
    pub(crate) min: f64,
    pub(crate) max: f64,
    pub(crate) bins: Vec<(i32, u32)>,
}

/// A metric context: name, tagset, and type.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct Context {
    pub(crate) name: String,
    pub(crate) tagset: BTreeSet<String>,
    pub(crate) kind: MetricKind,
}

/// A context and the time it first arrived on its lane.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ContextAt {
    #[serde(flatten)]
    pub(crate) context: Context,
    pub(crate) first_seen: EpochSeconds,
}

/// One lane's contexts and the intake's current time.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct LaneView {
    pub(crate) now: EpochSeconds,
    pub(crate) contexts: Vec<ContextAt>,
}

#[derive(Debug, Default)]
struct Lanes {
    seen: BTreeMap<(Target, Context), EpochSeconds>,
}

impl Lanes {
    fn record(&mut self, target: Target, contexts: &[Context], now: EpochSeconds) -> usize {
        let mut added = 0;
        for context in contexts {
            if context.name.starts_with(SELF_TELEMETRY_PREFIX) {
                continue;
            }
            if let Entry::Vacant(slot) = self.seen.entry((target, context.clone())) {
                slot.insert(now);
                added += 1;
            }
        }
        added
    }

    fn contexts(&self, target: Target) -> Vec<ContextAt> {
        self.seen
            .iter()
            .filter(|((lane, _), _)| *lane == target)
            .map(|((_, context), &first_seen)| ContextAt {
                context: context.clone(),
                first_seen,
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
        let contexts = observe_series(payload, now.secs());
        self.with_lanes(|lanes| lanes.record(target, &contexts, now))
    }

    pub(crate) fn record_sketches(&self, target: Target, payload: SketchPayload, now: EpochSeconds) -> usize {
        let contexts = observe_sketches(payload);
        self.with_lanes(|lanes| lanes.record(target, &contexts, now))
    }

    pub(crate) fn record_series_v3(&self, target: Target, series: Vec<V3Series>, now: EpochSeconds) -> usize {
        let contexts = observe_series_v3(series, now.secs());
        self.with_lanes(|lanes| lanes.record(target, &contexts, now))
    }

    pub(crate) fn contexts(&self, target: Target) -> Vec<ContextAt> {
        self.with_lanes(|lanes| lanes.contexts(target))
    }

    fn with_lanes<T>(&self, f: impl FnOnce(&mut Lanes) -> T) -> T {
        f(&mut self.lanes.lock().expect("capture lock poisoned"))
    }
}

/// Longest metric name the intake keeps, in bytes.
pub(crate) const MAX_METRIC_NAME_LEN: usize = 350;
/// Most tags the intake keeps on a series. The backend's tag limit is per-org (`tagLimitProvider`),
/// defaulting to `model.MaxTagThresh`=100; the rig hardcodes the default, so an org with a non-default
/// limit would diverge. This is a knowingly-deferred config-parity approximation, sound while the
/// differential only exercises default-org limits.
pub(crate) const MAX_TAG_COUNT: usize = 100;
/// Most resources the intake keeps on a series. Per-org in the backend (`resourceLimitProvider`),
/// defaulting to `model.MaxResourceThresh`=500; the rig hardcodes the default, same deferral as
/// `MAX_TAG_COUNT`.
pub(crate) const MAX_RESOURCE_COUNT: usize = 500;
/// Longest `host` resource name the intake keeps on a series, in bytes.
pub(crate) const MAX_HOST_NAME_LEN: usize = 255;
/// How far past the intake's receipt clock a scalar point may sit before it is dropped, in seconds.
/// Matches the backend's `payload.MaxSecondsInFuture` (intake/payload/normalizer.go:32, ten minutes).
const MAX_SECONDS_IN_FUTURE: i64 = 600;

/// Whether a scalar point is kept, mirroring the backend's per-point drops (v2
/// api_series_v2_handler_helpers.go:264-275, v3 validatePoint api_series_v3_handler.go:549-557): a NaN
/// value is dropped and a timestamp more than `MAX_SECONDS_IN_FUTURE` past the receipt clock is dropped.
/// Past timestamps are kept, since late points are accepted downstream. Sketch points carry no scalar
/// value and are not filtered here, matching the scalar-only scope of the backend's point checks.
fn scalar_point_kept(value: &BucketValue, bucket_start: u64, now_secs: i64) -> bool {
    match value {
        BucketValue::Scalar(v) => {
            !v.is_nan() && i128::from(bucket_start) <= i128::from(now_secs) + i128::from(MAX_SECONDS_IN_FUTURE)
        }
        BucketValue::Sketch(_) => true,
    }
}

/// Whether the intake keeps this metric name: non-empty, at most the max name length in bytes, and
/// carrying at least one ASCII-alphabetic byte. Shared by the v2 and v3 drop rules.
pub(crate) fn metric_name_kept(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_METRIC_NAME_LEN && name.bytes().any(|b| b.is_ascii_alphabetic())
}

/// Whether the intake's v2 ingest keeps this series. It drops any series with an invalid metric name
/// (empty, over the max name length, or no ASCII-alphabetic byte), more than the max tag count, more
/// than the max resource count, or a `host` resource whose name exceeds the max host length. Matching
/// keeps our captured context set equal to what production would store, and keeps the two lanes' drop
/// rules identical to the v3 path.
pub(crate) fn series_kept_by_intake(series: &MetricSeries) -> bool {
    let host_ok = series
        .resources
        .iter()
        .find(|r| r.type_() == "host")
        .is_none_or(|host| host.name().len() <= MAX_HOST_NAME_LEN);
    metric_name_kept(series.metric.as_str())
        && series.tags.len() <= MAX_TAG_COUNT
        && series.resources.len() <= MAX_RESOURCE_COUNT
        && host_ok
}

/// The tagset a series carries, its wire tags plus its `host` resource folded into a `host:<name>` tag,
/// matching the fold the v3 lane applies.
fn tagset_with_host(tags: &[String], host: Option<&str>) -> BTreeSet<String> {
    let mut tagset: BTreeSet<String> = tags.iter().cloned().collect();
    if let Some(host) = host {
        if !host.is_empty() {
            tagset.insert(format!("host:{host}"));
        }
    }
    tagset
}

/// Reads a `/api/v2/series` `MetricPayload` straight off the wire into contexts, no stele in the path.
/// It applies the same `series_kept_by_intake` drop rules and the same `host` resource fold as the v3
/// lane, and derives the kind from the wire type field. A series whose every point is dropped by the
/// backend's per-point NaN and too-far-future checks (keyed on `now_secs`, the intake's receipt clock)
/// emits no context, matching the backend's all-points-dropped series drop.
fn observe_series(payload: MetricPayload, now_secs: i64) -> Vec<Context> {
    let mut contexts = Vec::new();
    for series in payload.series {
        if !series_kept_by_intake(&series) {
            continue;
        }
        let host = series
            .resources
            .iter()
            .find(|r| r.type_() == "host")
            .map(Resource::name);
        let has_point = series.points.iter().any(|point| {
            u64::try_from(point.timestamp)
                .is_ok_and(|ts| scalar_point_kept(&BucketValue::Scalar(point.value), ts, now_secs))
        });
        if !has_point {
            continue;
        }
        contexts.push(Context {
            name: series.metric.clone(),
            tagset: tagset_with_host(&series.tags, host),
            kind: MetricKind::of(series.type_()),
        });
    }
    contexts
}

/// Reads an `/api/beta/sketches` `SketchPayload` straight off the wire into contexts, no stele in the
/// path. Each kept sketch is one `Sketch`-kind context; its `host` folds into a `host:<name>` tag as
/// the v2 series path folds its host resource. A sketch left with no point whose timestamp fits a
/// `u64` bucket-start emits no context.
///
/// The backend's `NormalizeDistributionReq` (intake/payload/normalizer.go:459-503) drops a distribution
/// whose host exceeds the host-length cap, whose tag count exceeds the tag cap, or whose metric name is
/// invalid, keeping the rest. This applies the same per-sketch keep predicate. The backend additionally
/// REWRITES kept metric names (`NormMetricNameParse`) and tags (`NormalizeTags`); that normalization is
/// a separate fidelity gap the sketch, v2, and v3 lanes all share and is not modeled here.
fn observe_sketches(payload: SketchPayload) -> Vec<Context> {
    let mut contexts = Vec::new();
    for sketch in payload.sketches {
        // Per-distribution keep rules, matching the backend. Resource count has no sketch analogue.
        if !metric_name_kept(sketch.metric())
            || sketch.tags.len() > MAX_TAG_COUNT
            || sketch.host().len() > MAX_HOST_NAME_LEN
        {
            continue;
        }
        let has_point = sketch.dogsketches.iter().any(|d| u64::try_from(d.ts).is_ok())
            || sketch.distributions.iter().any(|d| u64::try_from(d.ts).is_ok());
        if !has_point {
            continue;
        }
        contexts.push(Context {
            name: sketch.metric.clone(),
            tagset: tagset_with_host(&sketch.tags, Some(sketch.host())),
            kind: MetricKind::Sketch,
        });
    }
    contexts
}

/// Maps the natively decoded v3 series into contexts. The native decoder in `lenient_decode` already
/// applied the two-tier failure model, the production intake's per-series validation, and the `host`
/// resource fold; this applies the backend's per-point NaN and too-far-future drops (validatePoint),
/// keyed on `now_secs`, and emits a context for each series left with at least one surviving point.
fn observe_series_v3(series: Vec<V3Series>, now_secs: i64) -> Vec<Context> {
    series
        .into_iter()
        .filter(|s| {
            s.points
                .iter()
                .any(|(ts, value)| scalar_point_kept(value, *ts, now_secs))
        })
        .map(|s| Context {
            name: s.name,
            tagset: s.tags.into_iter().collect(),
            kind: s.kind,
        })
        .collect()
}

#[cfg(test)]
mod tests;
