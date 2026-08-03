//! Differential metric context capture.
//!
//! See scenario README for details.

use std::cmp::Ordering;
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use datadog_protos::metrics::metric_payload::{MetricSeries, MetricType, Resource};
use datadog_protos::metrics::{MetricPayload, SketchPayload};
use harness::Phase;
use serde::{Deserialize, Serialize};

use crate::context_diff;
use crate::lenient_decode::V3Series;
use crate::oracle::{self, ContextsReport, DivergingOut, FailureOut, SeriesReport, SkipCounts, SAMPLE_LIMIT};
use crate::series::{self, ScalarView, SketchSample, Skip, Verdict};

const SELF_TELEMETRY_PREFIX: &str = "datadog.";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Target {
    Agent,
    Adp,
}

impl Target {
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

/// A point as read off the wire, before the store assigns its `seq`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Observed {
    pub(crate) timestamp: EpochSeconds,
    /// The reporting interval the point covers, in seconds. The rate fold weights by it.
    pub(crate) interval: u32,
    pub(crate) value: BucketValue,
}

/// A kept series: its context and the points the intake stores for it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Observation {
    pub(crate) context: Context,
    pub(crate) points: Vec<Observed>,
}

/// A stored point. `seq` is the point's arrival ordinal within its series on its lane, so it
/// distinguishes and orders two points that share a timestamp without consulting insertion order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Point {
    pub(crate) timestamp: EpochSeconds,
    pub(crate) seq: u32,
    pub(crate) interval: u32,
    pub(crate) value: BucketValue,
}

/// Columnar layout, 24 bytes a point against the 88 a `Point` costs once padded to its `Sketch` variant.
#[derive(Debug, Default)]
struct ScalarSeries {
    timestamps: Vec<i64>,
    seqs: Vec<u32>,
    intervals: Vec<u32>,
    values: Vec<f64>,
}

impl ScalarSeries {
    fn push(&mut self, timestamp: i64, seq: u32, interval: u32, value: f64) {
        self.timestamps.push(timestamp);
        self.seqs.push(seq);
        self.intervals.push(interval);
        self.values.push(value);
    }

    fn len(&self) -> usize {
        self.timestamps.len()
    }
}

/// Kept apart from the scalars so those columns stay narrow. Sketches carry bins, and are rare.
#[derive(Debug, Default)]
struct SketchSeries {
    timestamps: Vec<i64>,
    seqs: Vec<u32>,
    intervals: Vec<u32>,
    values: Vec<SketchValue>,
}

/// Keyed on `Arc<Context>`, so one allocation covers a context across all three maps on both lanes.
/// Each extra reference costs a refcount bump, not a `String` plus a `BTreeSet`. `BTreeMap` keeps
/// iteration deterministic.
#[derive(Debug, Default)]
struct Lanes {
    seen: BTreeMap<(Target, Arc<Context>), EpochSeconds>,
    scalars: BTreeMap<(Target, Arc<Context>), ScalarSeries>,
    sketches: BTreeMap<(Target, Arc<Context>), SketchSeries>,
}

impl Lanes {
    fn record(
        &mut self, target: Target, observations: impl IntoIterator<Item = Observation>, now: EpochSeconds,
    ) -> usize {
        let mut added = 0;
        for observation in observations {
            if observation.context.name.starts_with(SELF_TELEMETRY_PREFIX) {
                continue;
            }
            let context = Arc::new(observation.context);
            if let Entry::Vacant(slot) = self.seen.entry((target, Arc::clone(&context))) {
                slot.insert(now);
                added += 1;
            }
            for point in observation.points {
                let timestamp = point.timestamp.secs();
                match point.value {
                    BucketValue::Scalar(value) => {
                        let series = self.scalars.entry((target, Arc::clone(&context))).or_default();
                        let seq = u32::try_from(series.len()).unwrap_or(u32::MAX);
                        series.push(timestamp, seq, point.interval, value);
                    }
                    BucketValue::Sketch(sketch) => {
                        let series = self.sketches.entry((target, Arc::clone(&context))).or_default();
                        let seq = u32::try_from(series.timestamps.len()).unwrap_or(u32::MAX);
                        series.timestamps.push(timestamp);
                        series.seqs.push(seq);
                        series.intervals.push(point.interval);
                        series.values.push(sketch);
                    }
                }
            }
        }
        added
    }

    /// The scalar points stored for one context on one lane, in arrival order. Borrows the columns and
    /// materializes a `Point` per step, so a read costs no allocation.
    #[allow(dead_code)]
    fn scalar_points<'a>(&'a self, target: Target, context: &'a Context) -> impl Iterator<Item = Point> + 'a {
        self.scalars
            .iter()
            .filter(move |((lane, ctx), _)| *lane == target && ctx.as_ref() == context)
            .flat_map(|(_, s)| {
                (0..s.len()).map(move |i| Point {
                    timestamp: EpochSeconds::from_epoch_secs(s.timestamps[i]),
                    seq: s.seqs[i],
                    interval: s.intervals[i],
                    value: BucketValue::Scalar(s.values[i]),
                })
            })
    }

    /// Every context either lane holds, each once. Ordered by context, so the oracle walks them the
    /// same way on every run.
    fn every_context(&self) -> impl Iterator<Item = &Arc<Context>> {
        let mut seen: BTreeSet<&Arc<Context>> = BTreeSet::new();
        for (_, context) in self.seen.keys() {
            seen.insert(context);
        }
        seen.into_iter()
    }

    /// A borrowed view onto one context's scalar columns on one lane. Empty where the lane holds none.
    fn scalar_view(&self, target: Target, context: &Arc<Context>) -> ScalarView<'_> {
        self.scalars
            .get(&(target, Arc::clone(context)))
            .map_or_else(ScalarView::default, |s| ScalarView {
                timestamps: &s.timestamps,
                seqs: &s.seqs,
                intervals: &s.intervals,
                values: &s.values,
            })
    }

    /// One context's sketch points on one lane.
    fn sketch_samples(&self, target: Target, context: &Arc<Context>) -> Vec<SketchSample> {
        self.sketches
            .get(&(target, Arc::clone(context)))
            .map(|s| {
                (0..s.timestamps.len())
                    .map(|i| SketchSample {
                        timestamp: s.timestamps[i],
                        seq: s.seqs[i],
                        value: s.values[i].clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// One lane's contexts with their first-seen times, borrowed rather than cloned.
    fn contexts(&self, target: Target) -> impl Iterator<Item = (&Arc<Context>, EpochSeconds)> {
        self.seen
            .iter()
            .filter(move |((lane, _), _)| *lane == target)
            .map(|((_, context), &first_seen)| (context, first_seen))
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
        let observations = observe_series(payload, now.secs());
        self.with_lanes(|lanes| lanes.record(target, observations, now))
    }

    pub(crate) fn record_sketches(&self, target: Target, payload: SketchPayload, now: EpochSeconds) -> usize {
        let observations = observe_sketches(payload);
        self.with_lanes(|lanes| lanes.record(target, observations, now))
    }

    pub(crate) fn record_series_v3(&self, target: Target, series: Vec<V3Series>, now: EpochSeconds) -> usize {
        let observations = observe_series_v3(series, now.secs());
        self.with_lanes(|lanes| lanes.record(target, observations, now))
    }

    /// Run the contexts oracle. Both lanes are read under one lock, so the difference describes one
    /// snapshot rather than two moments.
    pub(crate) fn compare_contexts(&self, now: EpochSeconds, budget_secs: i64) -> ContextsReport {
        self.with_lanes(|lanes| {
            let agent: Vec<_> = lanes.contexts(Target::Agent).collect();
            let adp: Vec<_> = lanes.contexts(Target::Adp).collect();
            let mut difference = context_diff::difference(&agent, &adp, now);
            let overdue = context_diff::overdue(&difference, budget_secs);
            // Split by lane over the whole difference, with distinct names alongside the member counts.
            // The sample is too small to carry either, and a member count alone cannot separate one
            // name diverging many times from many names diverging once.
            let mut adp_only = 0;
            let mut adp_names: BTreeSet<&str> = BTreeSet::new();
            let mut agent_names: BTreeSet<&str> = BTreeSet::new();
            for member in &difference {
                match member.lane {
                    Target::Adp => {
                        adp_only += 1;
                        adp_names.insert(member.context.name.as_str());
                    }
                    Target::Agent => {
                        agent_names.insert(member.context.name.as_str());
                    }
                }
            }
            let agent_only = difference.len() - adp_only;
            // A context on both lanes is counted once, so `compared` is the population the oracle saw.
            let compared = agent.len() + adp.len() - (agent.len() + adp.len() - difference.len()) / 2;
            // Descending age, ties broken by identity so a replay lists the same members. The
            // difference arrives ordered by context, so taking it unsorted would sample by name.
            difference.sort_by(|a, b| b.age_secs.cmp(&a.age_secs).then_with(|| a.context.cmp(b.context)));
            let sample: Vec<_> = difference
                .iter()
                .take(SAMPLE_LIMIT)
                .map(|d| {
                    let (name, tagset, kind) = oracle::flatten(d.context);
                    DivergingOut {
                        lane: d.lane,
                        name,
                        tagset,
                        kind,
                        age_secs: d.age_secs,
                    }
                })
                .collect();
            ContextsReport {
                compared,
                diverged: difference.len(),
                adp_only,
                agent_only,
                adp_only_names: adp_names.len(),
                agent_only_names: agent_names.len(),
                overdue,
                acceptable_flush_delay_secs: budget_secs,
                listed: sample.len(),
                sample,
            }
        })
    }

    /// Run the series oracle over every context either lane holds.
    pub(crate) fn compare_series(&self, w: i64, leash: usize, threshold: f64, phase: Phase) -> SeriesReport {
        self.with_lanes(|lanes| {
            let mut report = SeriesReport {
                compared: 0,
                failed: 0,
                listed: 0,
                skipped: SkipCounts::default(),
                population: 0,
                bucket_width: w,
                leash_width: leash,
                equivalence_threshold: threshold,
                failures: Vec::new(),
            };
            let mut failures: Vec<(f64, FailureOut)> = Vec::new();

            for context in lanes.every_context() {
                let verdict = if context.kind == MetricKind::Sketch {
                    series::compare_sketch(
                        &lanes.sketch_samples(Target::Agent, context),
                        &lanes.sketch_samples(Target::Adp, context),
                        w,
                        leash,
                        phase,
                    )
                } else {
                    series::compare(
                        context.kind,
                        lanes.scalar_view(Target::Agent, context),
                        lanes.scalar_view(Target::Adp, context),
                        w,
                        leash,
                        series::resubmit_rule(context.kind),
                        phase,
                    )
                };

                // Flatten only on the failure paths. Doing it up front would clone a name and a
                // tagset for every context in the run, almost all of which agree and report nothing.
                let mut record = |distance: Option<f64>, sort_key: f64| {
                    let (name, tagset, kind) = oracle::flatten(context);
                    failures.push((
                        sort_key,
                        FailureOut {
                            name,
                            tagset,
                            kind,
                            distance,
                            quantization_mismatch: distance.is_none(),
                        },
                    ));
                };
                match verdict {
                    // A context neither lane could be compared on is a pass only while load runs, where
                    // one lane may simply not have flushed yet. Once load has stopped there is nothing left
                    // to wait for, so no overlap is a divergence rather than a context to set aside.
                    Verdict::Skipped(Skip::NoOverlap) if phase == Phase::Finally => {
                        report.compared += 1;
                        report.failed += 1;
                        record(None, f64::INFINITY);
                    }
                    Verdict::Skipped(skip) => report.skipped.record(skip),
                    Verdict::QuantizationMismatch => {
                        report.compared += 1;
                        report.failed += 1;
                        // Sorts above every distance, since a mismatch is the worse finding.
                        record(None, f64::INFINITY);
                    }
                    Verdict::Distance(distance) => {
                        report.compared += 1;
                        if distance >= threshold {
                            report.failed += 1;
                            record(Some(distance), distance);
                        }
                    }
                }
            }

            // Descending distance, ties broken by identity so a replay lists the same exemplars.
            failures.sort_by(|(left, a), (right, b)| {
                right
                    .partial_cmp(left)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| (&a.name, &a.tagset, &a.kind).cmp(&(&b.name, &b.tagset, &b.kind)))
            });
            failures.truncate(SAMPLE_LIMIT);
            report.failures = failures.into_iter().map(|(_, f)| f).collect();
            report.listed = report.failures.len();
            report.population = report.compared + report.skipped.total();
            report
        })
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
fn observe_series(payload: MetricPayload, now_secs: i64) -> Vec<Observation> {
    let mut observations = Vec::new();
    for series in payload.series {
        if !series_kept_by_intake(&series) {
            continue;
        }
        let host = series
            .resources
            .iter()
            .find(|r| r.type_() == "host")
            .map(Resource::name);
        let interval = u32::try_from(series.interval()).unwrap_or(0);
        let points: Vec<Observed> = series
            .points
            .iter()
            .filter(|point| {
                u64::try_from(point.timestamp)
                    .is_ok_and(|ts| scalar_point_kept(&BucketValue::Scalar(point.value), ts, now_secs))
            })
            .map(|point| Observed {
                timestamp: EpochSeconds::from_epoch_secs(point.timestamp),
                interval,
                value: BucketValue::Scalar(point.value),
            })
            .collect();
        if points.is_empty() {
            continue;
        }
        observations.push(Observation {
            context: Context {
                name: series.metric.clone(),
                tagset: tagset_with_host(&series.tags, host),
                kind: MetricKind::of(series.type_()),
            },
            points,
        });
    }
    observations
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
fn observe_sketches(payload: SketchPayload) -> Vec<Observation> {
    let mut observations = Vec::new();
    for sketch in payload.sketches {
        // Per-distribution keep rules, matching the backend. Resource count has no sketch analogue.
        if !metric_name_kept(sketch.metric())
            || sketch.tags.len() > MAX_TAG_COUNT
            || sketch.host().len() > MAX_HOST_NAME_LEN
        {
            continue;
        }
        // A `Dogsketch` carries the log-grid bins the bucket merge needs. A `Distribution` carries the
        // same summary under a different encoding and no comparable grid, so it stores empty bins and
        // contributes only its summary series.
        // `k` and `n` are parallel columns of one bin list, so a length mismatch is a malformed sketch
        // rather than a shorter one. Zipping would silently keep the shorter prefix and compare a
        // distribution neither lane sent, so the point is dropped and the payload property reports it.
        let dogsketches = sketch.dogsketches.iter().filter(|d| d.k.len() == d.n.len()).map(|d| {
            (
                d.ts,
                SketchValue {
                    count: d.cnt,
                    sum: d.sum,
                    min: d.min,
                    max: d.max,
                    bins: d.k.iter().copied().zip(d.n.iter().copied()).collect(),
                },
            )
        });
        let distributions = sketch.distributions.iter().map(|d| {
            (
                d.ts,
                SketchValue {
                    count: d.cnt,
                    sum: d.sum,
                    min: d.min,
                    max: d.max,
                    bins: Vec::new(),
                },
            )
        });
        let points: Vec<Observed> = dogsketches
            .chain(distributions)
            .filter(|(ts, _)| u64::try_from(*ts).is_ok())
            .map(|(ts, value)| Observed {
                timestamp: EpochSeconds::from_epoch_secs(ts),
                interval: 0,
                value: BucketValue::Sketch(value),
            })
            .collect();
        if points.is_empty() {
            continue;
        }
        observations.push(Observation {
            context: Context {
                name: sketch.metric.clone(),
                tagset: tagset_with_host(&sketch.tags, Some(sketch.host())),
                kind: MetricKind::Sketch,
            },
            points,
        });
    }
    observations
}

/// Maps the natively decoded v3 series into contexts. The native decoder in `lenient_decode` already
/// applied the two-tier failure model, the production intake's per-series validation, and the `host`
/// resource fold; this applies the backend's per-point NaN and too-far-future drops (validatePoint),
/// keyed on `now_secs`, and emits a context for each series left with at least one surviving point.
fn observe_series_v3(series: Vec<V3Series>, now_secs: i64) -> Vec<Observation> {
    let mut observations = Vec::new();
    for s in series {
        let interval = s.interval;
        let points: Vec<Observed> = s
            .points
            .iter()
            .filter(|(ts, value)| scalar_point_kept(value, *ts, now_secs))
            .filter_map(|(ts, value)| {
                Some(Observed {
                    timestamp: EpochSeconds::from_epoch_secs(i64::try_from(*ts).ok()?),
                    interval,
                    value: value.clone(),
                })
            })
            .collect();
        if points.is_empty() {
            continue;
        }
        observations.push(Observation {
            context: Context {
                name: s.name,
                tagset: s.tags.into_iter().collect(),
                kind: s.kind,
            },
            points,
        });
    }
    observations
}

#[cfg(test)]
mod tests;
