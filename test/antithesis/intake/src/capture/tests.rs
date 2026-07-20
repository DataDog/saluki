use std::collections::{BTreeMap, BTreeSet};

use datadog_protos::metrics::metric_payload::{MetricPoint, MetricType, Resource};
use datadog_protos::metrics::sketch_payload::sketch::{Distribution, Dogsketch};
use datadog_protos::metrics::sketch_payload::Sketch;
use datadog_protos::metrics::v3;
use proptest::prelude::*;
use proptest::strategy::BoxedStrategy;
use protobuf::Message;
use serde_json::json;

use super::*;

/// A fixed receipt clock well past every fixture timestamp, so the too-far-future point drop keeps
/// them all unless a test deliberately dates a point into the future.
const NOW_SECS: i64 = 1_600_000_000;

fn context(name: &str, tags: &[&str], kind: MetricKind) -> Context {
    Context {
        name: name.to_string(),
        tagset: tags.iter().map(|t| (*t).to_string()).collect(),
        kind,
    }
}

/// One observed scalar series carrying `context`'s identity and `points` as `(bucket_start, value)`.
fn observed_scalar(context: &Context, points: &[(u64, f64)]) -> Observed {
    Observed {
        name: context.name.clone(),
        tags: context.tagset.iter().cloned().collect(),
        kind: context.kind,
        interval: 0,
        points: points.iter().map(|&(b, v)| (b, BucketValue::Scalar(v))).collect(),
    }
}

#[test]
fn contexts_serialize_to_the_flat_wire_shape() {
    let mut lanes = Lanes::default();
    let ctx = context("requests", &["host:agent-host", "env:test"], MetricKind::Count);
    // Bucket-start 1000 under high-water 2000 has aged well past SETTLE_HORIZON, so it is served.
    lanes.record(
        Target::Adp,
        &[observed_scalar(&ctx, &[(1_000, 4.0)])],
        EpochSeconds::from_epoch_secs(2_000),
    );

    // Flat: name, tagset (a sorted set), kind as a snake_case token, first_seen as a bare number, the
    // rate interval, and the settled buckets in bucket-start order.
    let wire = serde_json::to_value(lanes.contexts(Target::Adp)).expect("serialize");
    assert_eq!(
        wire,
        json!([{
            "name": "requests",
            "tagset": ["env:test", "host:agent-host"],
            "kind": "count",
            "first_seen": 2_000,
            "interval": 0,
            "buckets": [{ "bucket_start": 1_000, "value": { "Scalar": 4.0 }, "conflict": false }],
        }])
    );
}

// The curve DTO round-trips through serde: a context carrying an interval and a mix of scalar and
// sketch buckets, one flagged as a within-lane conflict, decodes back byte-for-byte.
#[test]
fn context_curve_dto_round_trips() {
    let original = ContextAt {
        context: context("app.dist", &["env:prod"], MetricKind::Sketch),
        first_seen: EpochSeconds::from_epoch_secs(1_000),
        interval: 10,
        buckets: vec![
            Bucket {
                bucket_start: 100,
                value: BucketValue::Scalar(1.5),
                conflict: false,
            },
            Bucket {
                bucket_start: 110,
                value: BucketValue::Sketch(SketchValue {
                    count: 5,
                    sum: 30.0,
                    min: 1.0,
                    max: 9.0,
                    bins: vec![(3, 2), (5, 3)],
                }),
                conflict: true,
            },
        ],
    };

    let json = serde_json::to_string(&original).expect("serialize");
    let back: ContextAt = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(back.interval, original.interval);
    assert_eq!(back.buckets, original.buckets);
    assert_eq!(back.context, original.context);
}

// --- value-bearing capture model ---

impl Arbitrary for EpochSeconds {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;
    fn arbitrary_with((): ()) -> Self::Strategy {
        any::<i64>().prop_map(EpochSeconds::from_epoch_secs).boxed()
    }
}

/// All the settled buckets a lane serves for its single recorded context, in bucket-start order.
fn served_buckets(lanes: &Lanes, target: Target) -> Vec<Bucket> {
    lanes.contexts(target).into_iter().flat_map(|c| c.buckets).collect()
}

proptest! {
    // Buckets key on the wire bucket-start, never on arrival order. Recording the same points in
    // forward and reverse arrival order yields the identical bucket-sorted curve. An arrival-keyed
    // store would order or collide differently under the two orders.
    #[test]
    fn property_test_buckets_key_on_wire_bucket_start_not_arrival(
        points in prop::collection::vec((0_u64..1_000_000, -1e6f64..1e6f64), 0..40),
    ) {
        // One value per wire bucket-start, so no differing-value conflict clouds the keying property.
        let mut seen = BTreeSet::new();
        let unique: Vec<(u64, f64)> = points.into_iter().filter(|&(b, _)| seen.insert(b)).collect();
        let ctx = context("adp.req", &["env:test"], MetricKind::Count);
        // High-water far past every bucket-start, so the settle gate serves them all.
        let now = EpochSeconds::from_epoch_secs(10_000_000);

        let mut forward = Lanes::default();
        for &(b, v) in &unique {
            forward.record(Target::Adp, &[observed_scalar(&ctx, &[(b, v)])], now);
        }
        let mut reverse = Lanes::default();
        for &(b, v) in unique.iter().rev() {
            reverse.record(Target::Adp, &[observed_scalar(&ctx, &[(b, v)])], now);
        }

        let want: Vec<Bucket> = unique
            .iter()
            .copied()
            .collect::<BTreeMap<u64, f64>>()
            .into_iter()
            .map(|(bucket_start, v)| Bucket { bucket_start, value: BucketValue::Scalar(v), conflict: false })
            .collect();
        prop_assert_eq!(served_buckets(&forward, Target::Adp), want.clone());
        prop_assert_eq!(served_buckets(&reverse, Target::Adp), want);
    }

    // High-water is the running max of every recorded `now` and never regresses, even when the intake
    // clock jumps backward between records (SMPTNG-767-safe).
    #[test]
    fn property_test_high_water_is_monotone_under_backward_now(
        nows in prop::collection::vec(any::<EpochSeconds>(), 1..30),
    ) {
        let ctx = context("adp.req", &["env:test"], MetricKind::Count);
        let mut lanes = Lanes::default();
        let mut running_max: Option<EpochSeconds> = None;
        for &now in &nows {
            lanes.record(Target::Adp, &[observed_scalar(&ctx, &[(1, 1.0)])], now);
            running_max = Some(running_max.map_or(now, |m| m.max(now)));
            prop_assert_eq!(lanes.high_water, running_max);
        }
    }

    // The settle gate serves a bucket iff the high-water clock has advanced more than SETTLE_HORIZON
    // past its wire bucket-start. Freshly arrived (or future) buckets are withheld.
    #[test]
    fn property_test_settle_gate_serves_only_aged_buckets(
        high in 0_i64..2_000_000,
        starts in prop::collection::btree_set(0_u64..2_000_000, 0..40),
    ) {
        let ctx = context("adp.req", &["env:test"], MetricKind::Count);
        let now = EpochSeconds::from_epoch_secs(high);
        let points: Vec<(u64, f64)> = starts.iter().map(|&b| (b, 1.0)).collect();
        let mut lanes = Lanes::default();
        lanes.record(Target::Adp, &[observed_scalar(&ctx, &points)], now);

        let want: BTreeSet<u64> = starts.iter().copied().filter(|&b| settled(now, b)).collect();
        let got: BTreeSet<u64> =
            served_buckets(&lanes, Target::Adp).into_iter().map(|b| b.bucket_start).collect();
        prop_assert_eq!(got, want);
    }

    // Each context's bucket ring is bounded: past MAX_BUCKETS_PER_CONTEXT distinct bucket-starts the
    // oldest are evicted, keeping exactly the newest (largest) bucket-starts.
    #[test]
    fn property_test_ring_bound_keeps_the_newest_bucket_starts(extra in 0_usize..64) {
        let ctx = context("adp.req", &["env:test"], MetricKind::Count);
        let total = MAX_BUCKETS_PER_CONTEXT + extra;
        let points: Vec<(u64, f64)> =
            (0..total).map(|b| (u64::try_from(b).expect("fits u64"), 1.0)).collect();
        let mut lanes = Lanes::default();
        lanes.record(Target::Adp, &[observed_scalar(&ctx, &points)], EpochSeconds::from_epoch_secs(0));

        let cells = &lanes.curves[&(Target::Adp, ctx)].cells;
        prop_assert_eq!(cells.len(), MAX_BUCKETS_PER_CONTEXT);
        // The lowest surviving key is `extra` and the highest is `total - 1`.
        prop_assert_eq!(cells.keys().next().copied(), Some(u64::try_from(extra).expect("fits u64")));
        prop_assert_eq!(cells.keys().next_back().copied(), Some(u64::try_from(total - 1).expect("fits u64")));
    }
}

// A second differing value at a bucket-start flags a conflict and never overwrites the first value;
// an equal replay is idempotent and never sets the flag.
#[test]
fn conflict_flags_a_differing_value_without_overwriting() {
    let ctx = context("adp.req", &["env:test"], MetricKind::Count);
    let now = EpochSeconds::from_epoch_secs(1_000);
    let mut lanes = Lanes::default();

    lanes.record(Target::Adp, &[observed_scalar(&ctx, &[(100, 1.0)])], now);
    // Equal replay: idempotent, no conflict.
    lanes.record(Target::Adp, &[observed_scalar(&ctx, &[(100, 1.0)])], now);
    let cell = &lanes.curves[&(Target::Adp, ctx.clone())].cells[&100];
    assert!(!cell.conflict);
    assert_eq!(cell.value, BucketValue::Scalar(1.0));

    // Differing value at the same bucket-start: conflict flagged, first value stands.
    lanes.record(Target::Adp, &[observed_scalar(&ctx, &[(100, 2.0)])], now);
    let cell = &lanes.curves[&(Target::Adp, ctx)].cells[&100];
    assert!(cell.conflict);
    assert_eq!(cell.value, BucketValue::Scalar(1.0));
}

// A collision arriving under a backward clock still flags the conflict and holds the first value,
// while high-water holds the earlier, larger `now`.
#[test]
fn backward_clock_collision_holds_high_water_and_flags_conflict() {
    let ctx = context("adp.req", &["env:test"], MetricKind::Count);
    let mut lanes = Lanes::default();

    lanes.record(
        Target::Adp,
        &[observed_scalar(&ctx, &[(100, 1.0)])],
        EpochSeconds::from_epoch_secs(100),
    );
    lanes.record(
        Target::Adp,
        &[observed_scalar(&ctx, &[(100, 2.0)])],
        EpochSeconds::from_epoch_secs(50),
    );

    assert_eq!(lanes.high_water, Some(EpochSeconds::from_epoch_secs(100)));
    let cell = &lanes.curves[&(Target::Adp, ctx)].cells[&100];
    assert!(cell.conflict);
    assert_eq!(cell.value, BucketValue::Scalar(1.0));
}

// `record` returns exactly how many contexts the batch newly added on the given lane. A known context
// re-recorded adds nothing; the same context is new on the other lane.
#[test]
fn record_returns_the_count_of_newly_added_contexts() {
    let now = EpochSeconds::from_epoch_secs(0);
    let a = context("adp.a", &["env:test"], MetricKind::Count);
    let b = context("adp.b", &["env:test"], MetricKind::Gauge);
    let c = context("adp.c", &["env:test"], MetricKind::Rate);
    let mut lanes = Lanes::default();

    assert_eq!(
        lanes.record(
            Target::Adp,
            &[observed_scalar(&a, &[(1, 1.0)]), observed_scalar(&b, &[(1, 1.0)])],
            now
        ),
        2
    );
    assert_eq!(
        lanes.record(
            Target::Adp,
            &[observed_scalar(&a, &[(2, 1.0)]), observed_scalar(&c, &[(1, 1.0)])],
            now
        ),
        1
    );
    assert_eq!(lanes.record(Target::Agent, &[observed_scalar(&a, &[(1, 1.0)])], now), 1);
}

// Self-telemetry contexts are skipped: they never count as added and never appear in the served view.
#[test]
fn self_telemetry_contexts_are_skipped() {
    let now = EpochSeconds::from_epoch_secs(2_000);
    let telemetry = context("datadog.agent.running", &[], MetricKind::Gauge);
    let kept = context("adp.req", &["env:test"], MetricKind::Count);
    let mut lanes = Lanes::default();

    let added = lanes.record(
        Target::Adp,
        &[
            observed_scalar(&telemetry, &[(1_000, 1.0)]),
            observed_scalar(&kept, &[(1_000, 1.0)]),
        ],
        now,
    );

    assert_eq!(added, 1);
    let served = lanes.contexts(Target::Adp);
    assert_eq!(served.len(), 1);
    assert_eq!(served[0].context.name, "adp.req");
}

// --- production-parity per-series drops ---

fn built_series(name: &str, tags: usize, resources: usize) -> MetricSeries {
    let mut s = MetricSeries::new();
    s.set_metric(name.to_string());
    s.set_type(MetricType::COUNT);
    for i in 0..tags {
        s.tags.push(format!("k{i}:v"));
    }
    for i in 0..resources {
        let mut r = Resource::new();
        r.set_type("host".to_string());
        r.set_name(format!("h{i}"));
        s.resources.push(r);
    }
    let mut p = MetricPoint::new();
    p.value = 1.0;
    p.timestamp = 1_600_000_000;
    s.points.push(p);
    s
}

#[test]
fn series_kept_matches_propjoe_validation() {
    // Valid, and the count boundaries propjoe keeps.
    assert!(series_kept_by_intake(&built_series("adp.requests", 1, 1)));
    assert!(series_kept_by_intake(&built_series(&"a".repeat(350), 1, 1)));
    assert!(series_kept_by_intake(&built_series("ok", 100, 1)));
    assert!(series_kept_by_intake(&built_series("ok", 1, 500)));

    // Dropped: empty, no ASCII-alphabetic char, over the byte limit.
    assert!(!series_kept_by_intake(&built_series("", 1, 1)));
    assert!(!series_kept_by_intake(&built_series("123.456", 1, 1)));
    assert!(!series_kept_by_intake(&built_series(&"a".repeat(351), 1, 1)));
    // Dropped: over the tag and resource count thresholds.
    assert!(!series_kept_by_intake(&built_series("ok", 101, 1)));
    assert!(!series_kept_by_intake(&built_series("ok", 1, 501)));
}

// --- v3 series capture ---

// v3 type field constants (metricType | valueType), from intake_v3.proto.
const V3_COUNT: u64 = 1;
const V3_GAUGE: u64 = 3;
const V3_FLOAT64: u64 = 0x30;

/// Build a v3 varint-length-prefixed string dictionary. Entries stay under 128 bytes so each length
/// prefix is a single byte.
fn v3_str_dict(strings: &[&str]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for s in strings {
        bytes.push(u8::try_from(s.len()).expect("dict entry under 128 bytes"));
        bytes.extend_from_slice(s.as_bytes());
    }
    bytes
}

/// Pad the ref columns and fill the flat point columns so a fixture of Float64 scalar series is
/// well-formed. Ref 0 resolves to the base-1 empty dict entry, and the production reader requires an
/// entry in every per-metric column. Every Float64 scalar point consumes one timestamp and one
/// Float64 value, so both columns get one entry per declared point across all metrics.
fn v3_fill_columns(data: &mut v3::MetricData) {
    let n = data.types.len();
    data.sourceTypeNameRefs = vec![0; n];
    data.originInfoRefs = vec![0; n];
    let total = usize::try_from(data.numPoints.iter().sum::<u64>()).expect("total points fit usize");
    data.timestamps = vec![0; total];
    data.valsFloat64 = vec![0.0; total];
}

/// Serialize a v3 payload, run it through the native decoder, and map the kept series to contexts.
/// This exercises the whole intake path: protobuf parse, dictionary + delta reconstruction, the
/// two-tier failure model, and per-series validation.
fn v3_contexts(payload: &v3::Payload) -> Vec<Context> {
    let bytes = payload.write_to_bytes().expect("serialize v3 payload");
    let series = crate::lenient_decode::decode_series_v3(&bytes).expect("decode v3 payload");
    observe_series_v3(series, NOW_SECS)
        .iter()
        .map(Observed::context)
        .collect()
}

// A valid v3 payload decodes to exactly the expected contexts. Two scalar series
// (app.count COUNT, app.gauge GAUGE) share the {env:prod} tagset. This is the reference-vector
// round-trip: dictionary + delta encoded columns in, contexts + numPoints out.
#[test]
fn v3_valid_payload_records_expected_contexts() {
    let mut data = v3::MetricData::new();
    data.dictNameStr = v3_str_dict(&["app.count", "app.gauge"]);
    data.dictTagStr = v3_str_dict(&["env:prod"]);
    // One tagset: length 1, the single tag at dict index 1.
    data.dictTagsets = vec![1, 1];
    data.types = vec![V3_COUNT | V3_FLOAT64, V3_GAUGE | V3_FLOAT64];
    // nameRefs delta-decode to absolute [1, 2]; both tagsetRefs resolve to tagset 1.
    data.nameRefs = vec![1, 1];
    data.tagsetRefs = vec![1, 0];
    data.resourcesRefs = vec![0, 0];
    data.intervals = vec![0, 0];
    data.numPoints = vec![3, 1];
    v3_fill_columns(&mut data);

    let mut payload = v3::Payload::new();
    payload.metricData = Some(data).into();

    assert_eq!(
        v3_contexts(&payload),
        vec![
            context("app.count", &["env:prod"], MetricKind::Count),
            context("app.gauge", &["env:prod"], MetricKind::Gauge),
        ]
    );
}

// Lane parity: the same logical metric encoded as v2 and as v3 yields the identical context set,
// including the v2 host-resource fold into a `host:<name>` tag. The native v3 decoder must fold host
// the same way, or v3 and v2 contexts would diverge for equivalent input.
#[test]
fn v3_lane_parity_matches_v2() {
    // v2 side: one series with a host resource, which the v2 path folds into a host:<name> tag.
    let mut series = MetricSeries::new();
    series.set_metric("app.count".to_string());
    series.set_type(MetricType::COUNT);
    series.tags.push("env:prod".to_string());
    let mut host = Resource::new();
    host.set_type("host".to_string());
    host.set_name("web-1".to_string());
    series.resources.push(host);
    let mut point = MetricPoint::new();
    point.value = 1.0;
    point.timestamp = 1_600_000_000;
    series.points.push(point);
    let mut v2 = MetricPayload::new();
    v2.series.push(series);
    let v2_set: BTreeSet<Context> = observe_series(v2, NOW_SECS).iter().map(Observed::context).collect();

    // v3 side: the same logical metric, dictionary + delta encoded, host carried as a resource.
    let mut data = v3::MetricData::new();
    data.dictNameStr = v3_str_dict(&["app.count"]);
    data.dictTagStr = v3_str_dict(&["env:prod"]);
    data.dictTagsets = vec![1, 1];
    data.dictResourceStr = v3_str_dict(&["host", "web-1"]);
    data.dictResourceLen = vec![1];
    data.dictResourceType = vec![1]; // idx 1 -> "host"
    data.dictResourceName = vec![2]; // idx 2 -> "web-1"
    data.types = vec![V3_COUNT | V3_FLOAT64];
    data.nameRefs = vec![1];
    data.tagsetRefs = vec![1];
    data.resourcesRefs = vec![1];
    data.intervals = vec![0];
    data.numPoints = vec![1];
    v3_fill_columns(&mut data);
    let mut payload = v3::Payload::new();
    payload.metricData = Some(data).into();
    let v3_set: BTreeSet<Context> = v3_contexts(&payload).into_iter().collect();

    assert_eq!(v3_set, v2_set);
    // The fold actually happened; the parity is not vacuously between two empty sets.
    assert!(v2_set.contains(&context("app.count", &["env:prod", "host:web-1"], MetricKind::Count)));
}

// One bad series among several valid ones drops only that series and keeps the rest. A no-ASCII-alpha
// name is the validation failure; the payload must never come back empty.
#[test]
fn v3_one_bad_series_drops_only_that_series() {
    let mut data = v3::MetricData::new();
    // Second name "123" has no ASCII-alphabetic byte, so its series is dropped.
    data.dictNameStr = v3_str_dict(&["app.first", "123", "app.third"]);
    data.dictTagStr = v3_str_dict(&["env:prod"]);
    data.dictTagsets = vec![1, 1];
    data.types = vec![V3_COUNT | V3_FLOAT64; 3];
    data.nameRefs = vec![1, 1, 1]; // absolute [1, 2, 3]
    data.tagsetRefs = vec![1, 0, 0]; // all tagset 1
    data.resourcesRefs = vec![0, 0, 0];
    data.intervals = vec![0, 0, 0];
    data.numPoints = vec![1, 1, 1];
    v3_fill_columns(&mut data);
    let mut payload = v3::Payload::new();
    payload.metricData = Some(data).into();

    assert_eq!(
        v3_contexts(&payload),
        vec![
            context("app.first", &["env:prod"], MetricKind::Count),
            context("app.third", &["env:prod"], MetricKind::Count),
        ]
    );
}

// Over-limit tags and over-limit resources each drop only their own series. A series with more than
// MaxTagThresh (100) tags and a series with more than MaxResourceThresh (500) resources are dropped,
// while the valid series survives.
#[test]
fn v3_over_limit_tags_and_resources_dropped() {
    // Tag dictionary: env:prod, then 101 flood tags (k0:v .. k100:v).
    let mut tag_strings = vec!["env:prod".to_string()];
    for i in 0..101 {
        tag_strings.push(format!("k{i}:v"));
    }
    let tag_refs: Vec<&str> = tag_strings.iter().map(String::as_str).collect();

    let mut data = v3::MetricData::new();
    data.dictNameStr = v3_str_dict(&["app.valid", "app.tags", "app.res"]);
    data.dictTagStr = v3_str_dict(&tag_refs);
    // Tagset 1: {env:prod}. Tagset 2: 101 flood tags at dict indices 2..=102, delta-encoded.
    let mut dict_tagsets = vec![1_i64, 1, 101, 2];
    dict_tagsets.extend(std::iter::repeat_n(1, 100));
    data.dictTagsets = dict_tagsets;
    // Resource group 1: 501 (type="r", name="r") pairs, delta-encoded as first index 1 then +0.
    data.dictResourceStr = v3_str_dict(&["r"]);
    data.dictResourceLen = vec![501];
    let mut res_refs = vec![1_i64];
    res_refs.extend(std::iter::repeat_n(0, 500));
    data.dictResourceType = res_refs.clone();
    data.dictResourceName = res_refs;
    data.types = vec![V3_COUNT | V3_FLOAT64; 3];
    data.nameRefs = vec![1, 1, 1]; // absolute [1, 2, 3]
    data.tagsetRefs = vec![1, 1, -1]; // absolute [1, 2, 1]
    data.resourcesRefs = vec![0, 0, 1]; // absolute [0, 0, 1]
    data.intervals = vec![0, 0, 0];
    data.numPoints = vec![1, 1, 1];
    v3_fill_columns(&mut data);
    let mut payload = v3::Payload::new();
    payload.metricData = Some(data).into();

    assert_eq!(
        v3_contexts(&payload),
        vec![context("app.valid", &["env:prod"], MetricKind::Count)]
    );
}

// An out-of-range metric type nibble is kept, not dropped. Production keeps such a series and
// forwards its type verbatim, so the intake keeps it as `Other` rather than masking a producer bug.
#[test]
fn v3_unknown_type_kept_as_other() {
    let mut data = v3::MetricData::new();
    data.dictNameStr = v3_str_dict(&["app.weird"]);
    data.dictTagStr = v3_str_dict(&["env:prod"]);
    data.dictTagsets = vec![1, 1];
    data.types = vec![5 | V3_FLOAT64]; // nibble 5 is not a known metric type
    data.nameRefs = vec![1];
    data.tagsetRefs = vec![1];
    data.resourcesRefs = vec![0];
    data.intervals = vec![0];
    data.numPoints = vec![1];
    v3_fill_columns(&mut data);
    let mut payload = v3::Payload::new();
    payload.metricData = Some(data).into();

    assert_eq!(
        v3_contexts(&payload),
        vec![context("app.weird", &["env:prod"], MetricKind::Other)]
    );
}

// When the flat point columns are shorter than the declared count, production truncates the metric to
// the points actually carried but still keeps the series. The intake keeps the context.
#[test]
fn v3_short_point_columns_keep_series() {
    let mut data = v3::MetricData::new();
    data.dictNameStr = v3_str_dict(&["app.count"]);
    data.dictTagStr = v3_str_dict(&["env:prod"]);
    data.dictTagsets = vec![1, 1];
    data.types = vec![V3_COUNT | V3_FLOAT64];
    data.nameRefs = vec![1];
    data.tagsetRefs = vec![1];
    data.resourcesRefs = vec![0];
    data.intervals = vec![0];
    data.numPoints = vec![3]; // declares three points
    data.sourceTypeNameRefs = vec![0];
    data.originInfoRefs = vec![0];
    // Only two points' worth of columns are present, so the metric truncates internally but is kept.
    data.timestamps = vec![0, 0];
    data.valsFloat64 = vec![0.0, 0.0];
    let mut payload = v3::Payload::new();
    payload.metricData = Some(data).into();

    assert_eq!(
        v3_contexts(&payload),
        vec![context("app.count", &["env:prod"], MetricKind::Count)]
    );
}

// A v2 series whose host resource name exceeds the cap is dropped, matching the v3 lane. Guards the
// host-length check that keeps the two lanes' drop rules identical.
#[test]
fn v2_over_long_host_name_is_dropped() {
    let mut series = MetricSeries::new();
    series.set_metric("adp.requests".to_string());
    series.set_type(MetricType::COUNT);
    let mut host = Resource::new();
    host.set_type("host".to_string());
    host.set_name("h".repeat(MAX_HOST_NAME_LEN + 1));
    series.resources.push(host);
    let mut point = MetricPoint::new();
    point.value = 1.0;
    point.timestamp = 1_600_000_000;
    series.points.push(point);
    let mut payload = MetricPayload::new();
    payload.series.push(series);

    assert!(observe_series(payload, NOW_SECS).is_empty());
}

#[test]
fn observe_series_drops_what_propjoe_drops() {
    let mut payload = MetricPayload::new();
    payload.series.push(built_series("adp.requests", 1, 1)); // kept
    payload.series.push(built_series("", 1, 1)); // empty name
    payload.series.push(built_series("999", 1, 1)); // no alpha
    payload.series.push(built_series("adp.toomanytags", 101, 1)); // tag flood

    let observed = observe_series(payload, NOW_SECS);
    let names: BTreeSet<&str> = observed.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, BTreeSet::from(["adp.requests"]));
}

// --- value-bearing capture off the wire (no stele) ---

// MetricKind derives from the v2 wire type nibble, not from a stele metric. UNSPECIFIED (and any
// out-of-range type the accessor defaults to it) maps to Other, matching the v3 path's keep-and-
// forward rule for an unknown type rather than dropping the series and masking a producer bug.
#[test]
fn metric_kind_of_derives_from_wire_type() {
    assert_eq!(MetricKind::of(MetricType::COUNT), MetricKind::Count);
    assert_eq!(MetricKind::of(MetricType::RATE), MetricKind::Rate);
    assert_eq!(MetricKind::of(MetricType::GAUGE), MetricKind::Gauge);
    assert_eq!(MetricKind::of(MetricType::UNSPECIFIED), MetricKind::Other);
}

// A v2 series carries its curve straight off the wire: the metric kind from the type field, the rate
// interval, and one Scalar point per MetricPoint keyed by its wire timestamp as bucket-start.
#[test]
fn observe_series_carries_points_interval_and_kind() {
    let mut series = MetricSeries::new();
    series.set_metric("adp.rate".to_string());
    series.set_type(MetricType::RATE);
    series.interval = 10;
    for (value, ts) in [(1.0, 100_i64), (2.0, 110)] {
        let mut point = MetricPoint::new();
        point.value = value;
        point.timestamp = ts;
        series.points.push(point);
    }
    let mut payload = MetricPayload::new();
    payload.series.push(series);

    let observed = observe_series(payload, NOW_SECS);
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].kind, MetricKind::Rate);
    assert_eq!(observed[0].interval, 10);
    assert_eq!(
        observed[0].points,
        vec![(100, BucketValue::Scalar(1.0)), (110, BucketValue::Scalar(2.0))]
    );
}

// A point with a negative wire timestamp does not fit a u64 bucket-start; it is dropped rather than
// wrapped, and the surviving points keep their order.
#[test]
fn observe_series_drops_negative_timestamp_points() {
    let mut series = MetricSeries::new();
    series.set_metric("adp.count".to_string());
    series.set_type(MetricType::COUNT);
    for (value, ts) in [(1.0, -5_i64), (2.0, 100)] {
        let mut point = MetricPoint::new();
        point.value = value;
        point.timestamp = ts;
        series.points.push(point);
    }
    let mut payload = MetricPayload::new();
    payload.series.push(series);

    let observed = observe_series(payload, NOW_SECS);
    assert_eq!(observed[0].points, vec![(100, BucketValue::Scalar(2.0))]);
}

// A v2 sketch decodes its dogsketch straight off the wire: the summary plus the log-grid bins as
// zip(k, n), and the sketch host folds into a `host:<name>` tag exactly as the v2 series path folds
// its host resource.
#[test]
fn observe_sketches_materializes_dogsketch_bins_and_folds_host() {
    let mut sketch = Sketch::new();
    sketch.metric = "latency".to_string();
    sketch.host = "web-1".to_string();
    sketch.tags.push("env:prod".to_string());
    let mut dogsketch = Dogsketch::new();
    dogsketch.ts = 200;
    dogsketch.cnt = 5;
    dogsketch.min = 1.0;
    dogsketch.max = 9.0;
    dogsketch.sum = 30.0;
    dogsketch.k = vec![3, 5];
    dogsketch.n = vec![2, 3];
    sketch.dogsketches.push(dogsketch);
    let mut payload = SketchPayload::new();
    payload.sketches.push(sketch);

    let observed = observe_sketches(payload);
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].kind, MetricKind::Sketch);
    assert_eq!(
        observed[0].context().tagset,
        ["env:prod".to_string(), "host:web-1".to_string()].into_iter().collect()
    );
    assert_eq!(
        observed[0].points,
        vec![(
            200,
            BucketValue::Sketch(SketchValue {
                count: 5,
                sum: 30.0,
                min: 1.0,
                max: 9.0,
                bins: vec![(3, 2), (5, 3)],
            })
        )]
    );
}

// A sketch's legacy `distributions` are kept too, not silently dropped. A Distribution carries no
// log-grid k/n bins, so it materializes as its summary with empty bins.
#[test]
fn observe_sketches_keeps_distributions_with_empty_bins() {
    let mut sketch = Sketch::new();
    sketch.metric = "legacy.dist".to_string();
    let mut dist = Distribution::new();
    dist.ts = 300;
    dist.cnt = 4;
    dist.min = 2.0;
    dist.max = 8.0;
    dist.sum = 20.0;
    sketch.distributions.push(dist);
    let mut payload = SketchPayload::new();
    payload.sketches.push(sketch);

    let observed = observe_sketches(payload);
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].kind, MetricKind::Sketch);
    assert_eq!(
        observed[0].points,
        vec![(
            300,
            BucketValue::Sketch(SketchValue {
                count: 4,
                sum: 20.0,
                min: 2.0,
                max: 8.0,
                bins: Vec::new(),
            })
        )]
    );
}

// The backend drops a v2 scalar point whose value is NaN or whose timestamp sits more than
// MAX_SECONDS_IN_FUTURE past the receipt clock, keeping past and in-window points. A series left with
// no points emits no context.
#[test]
fn observe_series_drops_nan_and_future_points() {
    let mut series = MetricSeries::new();
    series.set_metric("adp.count".to_string());
    series.set_type(MetricType::COUNT);
    let far_future = NOW_SECS + MAX_SECONDS_IN_FUTURE + 1;
    for (value, ts) in [
        (1.0, 100_i64),                          // past, kept
        (f64::NAN, 110),                         // NaN, dropped
        (2.0, NOW_SECS + MAX_SECONDS_IN_FUTURE), // at the future bound, kept
        (3.0, far_future),                       // too far in the future, dropped
    ] {
        let mut point = MetricPoint::new();
        point.value = value;
        point.timestamp = ts;
        series.points.push(point);
    }
    let mut payload = MetricPayload::new();
    payload.series.push(series);

    let observed = observe_series(payload, NOW_SECS);
    assert_eq!(
        observed[0].points,
        vec![
            (100, BucketValue::Scalar(1.0)),
            (
                u64::try_from(NOW_SECS + MAX_SECONDS_IN_FUTURE).expect("fits u64"),
                BucketValue::Scalar(2.0)
            ),
        ]
    );
}

// A v2 series whose points are all NaN or all too-far-future is dropped whole: no points survive, so
// it emits no settled bucket and no context, matching the backend's all-points-dropped series drop.
#[test]
fn observe_series_all_points_dropped_emits_no_context() {
    let mut series = MetricSeries::new();
    series.set_metric("adp.count".to_string());
    series.set_type(MetricType::COUNT);
    let mut point = MetricPoint::new();
    point.value = f64::NAN;
    point.timestamp = 100;
    series.points.push(point);
    let mut payload = MetricPayload::new();
    payload.series.push(series);

    let observed = observe_series(payload, NOW_SECS);
    // The series is kept by name/tag/resource validation but carries no points, so no bucket is
    // recorded and no context is served.
    let mut lanes = Lanes::default();
    lanes.record(Target::Adp, &observed, EpochSeconds::from_epoch_secs(NOW_SECS));
    assert!(lanes.contexts(Target::Adp).is_empty());
}

// The v3 lane drops NaN and too-far-future scalar points too (validatePoint), keyed on the same
// receipt clock as v2.
#[test]
fn observe_series_v3_drops_nan_and_future_points() {
    let far_future = u64::try_from(NOW_SECS + MAX_SECONDS_IN_FUTURE + 1).expect("fits u64");
    let series = vec![V3Series {
        name: "app.count".to_string(),
        tags: vec!["env:prod".to_string()],
        kind: MetricKind::Count,
        interval: 0,
        points: vec![
            (100, BucketValue::Scalar(1.0)),
            (110, BucketValue::Scalar(f64::NAN)),
            (far_future, BucketValue::Scalar(2.0)),
        ],
    }];

    let observed = observe_series_v3(series, NOW_SECS);
    assert_eq!(observed[0].points, vec![(100, BucketValue::Scalar(1.0))]);
}

// The backend's NormalizeDistributionReq drops a distribution with an invalid metric name, more than
// the tag cap, or a host over the host-length cap, keeping the valid ones. observe_sketches mirrors it.
#[test]
fn observe_sketches_drops_invalid_name_tag_flood_and_long_host() {
    fn sketch(metric: &str, tags: usize, host: &str) -> Sketch {
        let mut s = Sketch::new();
        s.metric = metric.to_string();
        s.host = host.to_string();
        for i in 0..tags {
            s.tags.push(format!("k{i}:v"));
        }
        let mut d = Dogsketch::new();
        d.ts = 300;
        d.cnt = 1;
        s.dogsketches.push(d);
        s
    }

    let mut payload = SketchPayload::new();
    payload.sketches.push(sketch("latency", 1, "web-1")); // kept
    payload.sketches.push(sketch("123", 1, "web-1")); // no ASCII-alpha, dropped
    payload.sketches.push(sketch("", 1, "web-1")); // empty name, dropped
    payload.sketches.push(sketch("flood", MAX_TAG_COUNT + 1, "web-1")); // tag flood, dropped
    payload
        .sketches
        .push(sketch("longhost", 1, &"h".repeat(MAX_HOST_NAME_LEN + 1))); // host too long, dropped

    let observed = observe_sketches(payload);
    let names: BTreeSet<&str> = observed.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, BTreeSet::from(["latency"]));
}

proptest! {
    // Every kept point round-trips off the wire: for a valid COUNT series with non-negative
    // timestamps and finite values, observe_series emits exactly one Scalar point per input point,
    // keyed by its timestamp, in order. An unfaithful lane would poison every downstream curve.
    #[test]
    fn property_test_observe_series_materializes_scalar_points(
        points in prop::collection::vec((-1e9f64..1e9f64, 0_i64..1_000_000), 0..6),
    ) {
        let mut series = MetricSeries::new();
        series.set_metric("adp.req".to_string());
        series.set_type(MetricType::COUNT);
        for (value, ts) in &points {
            let mut point = MetricPoint::new();
            point.value = *value;
            point.timestamp = *ts;
            series.points.push(point);
        }
        let mut payload = MetricPayload::new();
        payload.series.push(series);

        let observed = observe_series(payload, NOW_SECS);
        prop_assert_eq!(observed.len(), 1);
        let want: Vec<(u64, BucketValue)> =
            points
                .iter()
                .map(|(value, ts)| (u64::try_from(*ts).expect("ts is non-negative"), BucketValue::Scalar(*value)))
                .collect();
        prop_assert_eq!(&observed[0].points, &want);
    }
}
