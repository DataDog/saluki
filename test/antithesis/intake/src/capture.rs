//! Differential metric context capture.
//!
//! See scenario README for details.

use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use datadog_protos::metrics::metric_payload::{MetricSeries, MetricType};
use datadog_protos::metrics::{MetricPayload, SketchPayload};
use harness::SETTLE_HORIZON;
use serde::{Deserialize, Serialize};

use crate::lenient_decode::V3Series;

const SELF_TELEMETRY_PREFIX: &str = "datadog.";

/// Most buckets the intake keeps per (target, context) curve. Older bucket-starts past this bound are
/// evicted, so a misbehaving or long-running producer cannot grow a single curve without limit.
const MAX_BUCKETS_PER_CONTEXT: usize = 512;

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

/// One aggregated bucket value as the intake captures it off the wire, kind-agnostic. The differential
/// oracle's per-kind ground metric consumes this. Both lanes decode into the SAME type so the two lines
/// are comparable without a second decoder in the path.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub(crate) enum BucketValue {
    /// A count, rate, or gauge scalar. A rate's interval rides on its series, not here.
    Scalar(f64),
    /// A `DDSketch` summary plus its log-grid bins.
    Sketch(SketchValue),
}

/// A `DDSketch` point: the summary the Agent emits plus the log-grid bins as `(key, count)`, key-sorted.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub(crate) struct SketchValue {
    pub(crate) count: i64,
    pub(crate) sum: f64,
    pub(crate) min: f64,
    pub(crate) max: f64,
    pub(crate) bins: Vec<(i32, u32)>,
}

/// One aggregated bucket of a context's curve as the intake captured it on a lane: the wire
/// bucket-start, the value, and whether a second differing value landed on the same bucket-start on
/// the same lane. The differential oracle reads the curve as the sequence of these in bucket-start
/// order; `conflict` surfaces a within-lane collision rather than silently overwriting one value.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub(crate) struct Bucket {
    #[allow(
        clippy::struct_field_names,
        reason = "bucket_start is the wire field name the differential scenario deserializes"
    )]
    pub(crate) bucket_start: u64,
    pub(crate) value: BucketValue,
    pub(crate) conflict: bool,
}

/// One series the intake observed on a lane, decoded straight off the wire into the shared
/// `BucketValue` representation. `name`, `tags`, and `kind` form the context identity, `interval`
/// carries the rate divisor (`0` otherwise), and `points` is the captured curve as `(wire
/// bucket-start, value)` in wire order. The v2 series, sketch, and native v3 paths all produce this
/// same shape, so no second decoder sits on either lane.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Observed {
    pub(crate) name: String,
    pub(crate) tags: Vec<String>,
    pub(crate) kind: MetricKind,
    pub(crate) interval: u64,
    pub(crate) points: Vec<(u64, BucketValue)>,
}

impl Observed {
    /// The context identity: name, tagset, and kind. Tag order never decides identity, so the tags
    /// collapse into a set here.
    fn context(&self) -> Context {
        Context {
            name: self.name.clone(),
            tagset: self.tags.iter().cloned().collect(),
            kind: self.kind,
        }
    }
}

/// A metric context: name, tagset, and type.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct Context {
    pub(crate) name: String,
    pub(crate) tagset: BTreeSet<String>,
    pub(crate) kind: MetricKind,
}

/// A context, the time it first arrived on its lane, and its captured curve. The curve DTO the
/// differential oracle consumes: `kind` rides in `context`, `interval` carries the rate divisor, and
/// `buckets` is the aggregation curve in bucket-start order.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ContextAt {
    #[serde(flatten)]
    pub(crate) context: Context,
    pub(crate) first_seen: EpochSeconds,
    /// The rate interval in seconds, `0` for non-rate kinds. A rate interval-divisor mismatch between
    /// the lanes is always a real bug, so the oracle compares it directly.
    #[serde(default)]
    pub(crate) interval: u64,
    /// The context's aggregation curve: its captured buckets in bucket-start order.
    #[serde(default)]
    pub(crate) buckets: Vec<Bucket>,
}

/// One lane's contexts and the intake's current time.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct LaneView {
    pub(crate) now: EpochSeconds,
    pub(crate) contexts: Vec<ContextAt>,
}

/// One captured bucket cell on a lane: the value the intake first saw at a wire bucket-start, plus
/// whether a second differing value later landed on that same bucket-start on the same lane. A
/// conflict never overwrites the first value; it only flags the within-lane collision so the oracle
/// sees it rather than a silent last-writer-wins.
#[derive(Clone, Debug)]
struct Cell {
    value: BucketValue,
    conflict: bool,
}

/// One `(Target, Context)` aggregation curve: when the context first arrived on the lane, its rate
/// interval, and its cells keyed by wire bucket-start. The map is bounded to the most recent
/// `MAX_BUCKETS_PER_CONTEXT` bucket-starts.
#[derive(Debug)]
struct Curve {
    first_seen: EpochSeconds,
    interval: u64,
    cells: BTreeMap<u64, Cell>,
}

#[derive(Debug, Default)]
struct Lanes {
    curves: BTreeMap<(Target, Context), Curve>,
    /// The monotone high-water mark: the max of every recorded intake `now`. It never regresses, even
    /// when the intake clock jumps backward, and it is never keyed on arrival order.
    high_water: Option<EpochSeconds>,
}

impl Lanes {
    /// Folds a batch of observed series into the lane's curves and advances the high-water clock.
    /// Returns how many contexts this batch newly added. Self-telemetry contexts are skipped. Each
    /// point lands at its wire bucket-start: a fresh bucket-start records the value, a repeat with an
    /// equal value is idempotent, and a repeat with a differing value flags a conflict without ever
    /// overwriting the first value. `now` advances `high_water` monotonically and never regresses.
    fn record(&mut self, target: Target, observed: &[Observed], now: EpochSeconds) -> usize {
        let mut added = 0;
        for obs in observed {
            if obs.name.starts_with(SELF_TELEMETRY_PREFIX) {
                continue;
            }
            let curve = match self.curves.entry((target, obs.context())) {
                Entry::Vacant(slot) => {
                    added += 1;
                    slot.insert(Curve {
                        first_seen: now,
                        interval: obs.interval,
                        cells: BTreeMap::new(),
                    })
                }
                Entry::Occupied(slot) => {
                    let curve = slot.into_mut();
                    curve.interval = obs.interval;
                    curve
                }
            };
            for (bucket_start, value) in &obs.points {
                match curve.cells.entry(*bucket_start) {
                    Entry::Vacant(cell) => {
                        cell.insert(Cell {
                            value: value.clone(),
                            conflict: false,
                        });
                    }
                    Entry::Occupied(mut cell) => {
                        if cell.get().value != *value {
                            cell.get_mut().conflict = true;
                        }
                    }
                }
            }
            // Bound the ring: evict the oldest bucket-starts until the curve fits.
            while curve.cells.len() > MAX_BUCKETS_PER_CONTEXT {
                let Some(&oldest) = curve.cells.keys().next() else {
                    break;
                };
                curve.cells.remove(&oldest);
            }
        }
        self.high_water = Some(self.high_water.map_or(now, |hw| hw.max(now)));
        added
    }

    /// One lane's settled contexts. A context appears only once it has at least one settled bucket;
    /// each context carries only its settled buckets in bucket-start order.
    fn contexts(&self, target: Target) -> Vec<ContextAt> {
        let Some(high_water) = self.high_water else {
            return Vec::new();
        };
        self.curves
            .iter()
            .filter(|((lane, _), _)| *lane == target)
            .filter_map(|((_, context), curve)| {
                let buckets: Vec<Bucket> = curve
                    .cells
                    .iter()
                    .filter(|(&bucket_start, _)| settled(high_water, bucket_start))
                    .map(|(&bucket_start, cell)| Bucket {
                        bucket_start,
                        value: cell.value.clone(),
                        conflict: cell.conflict,
                    })
                    .collect();
                if buckets.is_empty() {
                    return None;
                }
                Some(ContextAt {
                    context: context.clone(),
                    first_seen: curve.first_seen,
                    interval: curve.interval,
                    buckets,
                })
            })
            .collect()
    }

    /// One lane's settled curve view: its settled contexts plus the intake's monotone high-water
    /// clock. `now` ages `first_seen` and gates the settle horizon on the same clock; it is `0` before
    /// any record.
    fn view(&self, target: Target) -> LaneView {
        LaneView {
            now: self.high_water.unwrap_or(EpochSeconds::from_epoch_secs(0)),
            contexts: self.contexts(target),
        }
    }
}

/// Whether a bucket at `bucket_start` has settled: the monotone high-water clock has advanced more
/// than `SETTLE_HORIZON` past the bucket's wire start, so no later flush should still change it. Keyed
/// on the wire bucket-start and the high-water clock, never on arrival, so clock jitter cannot unsettle
/// a served bucket.
fn settled(high_water: EpochSeconds, bucket_start: u64) -> bool {
    let horizon = i128::try_from(SETTLE_HORIZON).unwrap_or(i128::MAX);
    i128::from(high_water.secs()) - i128::from(bucket_start) > horizon
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
        let observed = observe_series(payload, now.secs());
        self.with_lanes(|lanes| lanes.record(target, &observed, now))
    }

    pub(crate) fn record_sketches(&self, target: Target, payload: SketchPayload, now: EpochSeconds) -> usize {
        let observed = observe_sketches(payload);
        self.with_lanes(|lanes| lanes.record(target, &observed, now))
    }

    pub(crate) fn record_series_v3(&self, target: Target, series: Vec<V3Series>, now: EpochSeconds) -> usize {
        let observed = observe_series_v3(series, now.secs());
        self.with_lanes(|lanes| lanes.record(target, &observed, now))
    }

    pub(crate) fn contexts(&self, target: Target) -> Vec<ContextAt> {
        self.with_lanes(|lanes| lanes.contexts(target))
    }

    /// One lane's settled curve view: the contexts with at least one settled bucket plus the intake's
    /// monotone high-water clock. What the `/antithesis/curves/{target}` route serves.
    pub(crate) fn view(&self, target: Target) -> LaneView {
        self.with_lanes(|lanes| lanes.view(target))
    }

    fn with_lanes<T>(&self, f: impl FnOnce(&mut Lanes) -> T) -> T {
        f(&mut self.lanes.lock().expect("capture lock poisoned"))
    }
}

/// Longest metric name the intake keeps, in bytes.
const MAX_METRIC_NAME_LEN: usize = 350;
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

/// Reads a `/api/v2/series` `MetricPayload` straight off the wire into observed series, no stele in
/// the path. It applies the same `series_kept_by_intake` drop rules and the same `host` resource fold
/// into a `host:<name>` tag as the v3 lane, derives the kind from the wire type field, carries the
/// rate interval, and materializes one `Scalar` point per `MetricPoint` keyed by its wire timestamp.
/// A point whose timestamp does not fit a `u64` bucket-start is dropped rather than wrapped, and a NaN
/// or too-far-future point is dropped like the backend does, keyed on `now_secs` (the intake's receipt
/// clock). A series left with no points emits no bucket and so no context, matching the backend's
/// all-points-dropped series drop.
fn observe_series(payload: MetricPayload, now_secs: i64) -> Vec<Observed> {
    let mut observed = Vec::new();
    for series in payload.series {
        if !series_kept_by_intake(&series) {
            continue;
        }
        let mut tags = series.tags.clone();
        if let Some(host) = series.resources.iter().find(|r| r.type_() == "host") {
            if !host.name().is_empty() {
                tags.push(format!("host:{}", host.name()));
            }
        }
        let points = series
            .points
            .iter()
            .filter_map(|point| {
                u64::try_from(point.timestamp)
                    .ok()
                    .map(|ts| (ts, BucketValue::Scalar(point.value)))
            })
            .filter(|(ts, value)| scalar_point_kept(value, *ts, now_secs))
            .collect();
        observed.push(Observed {
            name: series.metric.clone(),
            tags,
            kind: MetricKind::of(series.type_()),
            interval: u64::try_from(series.interval).unwrap_or(0),
            points,
        });
    }
    observed
}

/// Reads an `/api/beta/sketches` `SketchPayload` straight off the wire into observed series, no stele
/// in the path. Each sketch is one `Sketch`-kind context; its `host` folds into a `host:<name>` tag as
/// the v2 series path folds its host resource. Each dogsketch materializes as its summary plus the
/// log-grid bins as `zip(k, n)`; each legacy distribution is kept too as its summary with empty bins,
/// since a `Distribution` carries no log-grid keys. A point whose timestamp does not fit a `u64`
/// bucket-start is dropped rather than wrapped.
///
/// The backend's `NormalizeDistributionReq` (intake/payload/normalizer.go:459-503) drops a distribution
/// whose host exceeds the host-length cap, whose tag count exceeds the tag cap, or whose metric name is
/// invalid, keeping the rest. This applies the same per-sketch keep predicate. The backend additionally
/// REWRITES kept metric names (`NormMetricNameParse`) and tags (`NormalizeTags`); that normalization is
/// a separate fidelity gap the sketch, v2, and v3 lanes all share and is not modeled here.
fn observe_sketches(payload: SketchPayload) -> Vec<Observed> {
    let mut observed = Vec::new();
    for sketch in payload.sketches {
        // Per-distribution keep rules, matching the backend. Resource count has no sketch analogue.
        if !metric_name_kept(sketch.metric())
            || sketch.tags.len() > MAX_TAG_COUNT
            || sketch.host().len() > MAX_HOST_NAME_LEN
        {
            continue;
        }
        let mut tags = sketch.tags.clone();
        if !sketch.host().is_empty() {
            tags.push(format!("host:{}", sketch.host()));
        }
        let mut points = Vec::new();
        for dogsketch in &sketch.dogsketches {
            let Ok(bucket_start) = u64::try_from(dogsketch.ts) else {
                continue;
            };
            let bins = dogsketch.k.iter().copied().zip(dogsketch.n.iter().copied()).collect();
            points.push((
                bucket_start,
                BucketValue::Sketch(SketchValue {
                    count: dogsketch.cnt,
                    sum: dogsketch.sum,
                    min: dogsketch.min,
                    max: dogsketch.max,
                    bins,
                }),
            ));
        }
        for dist in &sketch.distributions {
            let Ok(bucket_start) = u64::try_from(dist.ts) else {
                continue;
            };
            points.push((
                bucket_start,
                BucketValue::Sketch(SketchValue {
                    count: dist.cnt,
                    sum: dist.sum,
                    min: dist.min,
                    max: dist.max,
                    bins: Vec::new(),
                }),
            ));
        }
        observed.push(Observed {
            name: sketch.metric.clone(),
            tags,
            kind: MetricKind::Sketch,
            interval: 0,
            points,
        });
    }
    observed
}

/// Maps the natively decoded v3 series into observed series. The native decoder in `lenient_decode`
/// already applied the two-tier failure model, the production intake's per-series validation, the
/// `host` resource fold, and the point materialization, so this re-shapes into the shared type and
/// applies the backend's per-point NaN and too-far-future drops (validatePoint), keyed on `now_secs`.
fn observe_series_v3(series: Vec<V3Series>, now_secs: i64) -> Vec<Observed> {
    series
        .into_iter()
        .map(|s| Observed {
            name: s.name,
            tags: s.tags,
            kind: s.kind,
            interval: s.interval,
            points: s
                .points
                .into_iter()
                .filter(|(ts, value)| scalar_point_kept(value, *ts, now_secs))
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests;
