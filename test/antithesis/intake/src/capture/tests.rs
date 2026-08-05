use std::collections::BTreeSet;
use std::sync::OnceLock;

use datadog_protos::metrics::metric_payload::{MetricPoint, MetricType, Resource};
use datadog_protos::metrics::sketch_payload::sketch::{Distribution, Dogsketch};
use datadog_protos::metrics::sketch_payload::Sketch;
use datadog_protos::metrics::v3;
use protobuf::Message;
use serde_json::json;

use super::*;
use crate::lenient_decode::decode_series_v3;

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

// A recorded context serializes to the flat wire shape the scenario drivers deserialize: name, tagset
// as a sorted set, kind as a snake_case token, and first_seen as a bare number.
#[test]
fn contexts_serialize_to_the_flat_wire_shape() {
    let mut lanes = Lanes::default();
    let ctx = context("requests", &["host:agent-host", "env:test"], MetricKind::Count);
    lanes.record(Target::Adp, &[ctx], EpochSeconds::from_epoch_secs(2_000));

    let wire = serde_json::to_value(lanes.contexts(Target::Adp)).expect("serialize");
    assert_eq!(
        wire,
        json!([{
            "name": "requests",
            "tagset": ["env:test", "host:agent-host"],
            "kind": "count",
            "first_seen": 2_000,
        }])
    );
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

    assert_eq!(lanes.record(Target::Adp, &[a.clone(), b], now), 2);
    assert_eq!(lanes.record(Target::Adp, &[a.clone(), c], now), 1);
    assert_eq!(lanes.record(Target::Agent, &[a], now), 1);
}

// Self-telemetry contexts are skipped: they never count as added and never appear in the served view.
#[test]
fn self_telemetry_contexts_are_skipped() {
    let now = EpochSeconds::from_epoch_secs(2_000);
    let telemetry = context("datadog.agent.running", &[], MetricKind::Gauge);
    let kept = context("adp.req", &["env:test"], MetricKind::Count);
    let mut lanes = Lanes::default();

    let added = lanes.record(Target::Adp, &[telemetry, kept], now);

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

#[test]
fn observe_series_drops_what_propjoe_drops() {
    let mut payload = MetricPayload::new();
    payload.series.push(built_series("adp.requests", 1, 1)); // kept
    payload.series.push(built_series("", 1, 1)); // empty name
    payload.series.push(built_series("999", 1, 1)); // no alpha
    payload.series.push(built_series("adp.toomanytags", 101, 1)); // tag flood

    let contexts = observe_series(payload, NOW_SECS);
    let names: BTreeSet<&str> = contexts.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, BTreeSet::from(["adp.requests"]));
}

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

// A v2 series whose every point is dropped emits no context: a NaN value, a timestamp more than
// MAX_SECONDS_IN_FUTURE past the receipt clock, and a negative timestamp that does not fit a u64
// bucket-start are all dropped, matching the backend's all-points-dropped series drop.
#[test]
fn observe_series_all_points_dropped_emits_no_context() {
    let mut series = MetricSeries::new();
    series.set_metric("adp.count".to_string());
    series.set_type(MetricType::COUNT);
    let far_future = NOW_SECS + MAX_SECONDS_IN_FUTURE + 1;
    for (value, ts) in [(f64::NAN, 100_i64), (2.0, far_future), (3.0, -5)] {
        let mut point = MetricPoint::new();
        point.value = value;
        point.timestamp = ts;
        series.points.push(point);
    }
    let mut payload = MetricPayload::new();
    payload.series.push(series);

    assert!(observe_series(payload, NOW_SECS).is_empty());
}

// A v2 sketch emits one Sketch-kind context, and the sketch host folds into a `host:<name>` tag
// exactly as the v2 series path folds its host resource.
#[test]
fn observe_sketches_emits_sketch_context_and_folds_host() {
    let mut sketch = Sketch::new();
    sketch.metric = "latency".to_string();
    sketch.host = "web-1".to_string();
    sketch.tags.push("env:prod".to_string());
    let mut dogsketch = Dogsketch::new();
    dogsketch.ts = 200;
    dogsketch.cnt = 5;
    sketch.dogsketches.push(dogsketch);
    let mut payload = SketchPayload::new();
    payload.sketches.push(sketch);

    let contexts = observe_sketches(payload);
    assert_eq!(contexts.len(), 1);
    assert_eq!(contexts[0].name, "latency");
    assert_eq!(contexts[0].kind, MetricKind::Sketch);
    assert_eq!(
        contexts[0].tagset,
        ["env:prod".to_string(), "host:web-1".to_string()].into_iter().collect()
    );
}

// A sketch carrying only a legacy `distributions` entry, no dogsketch, still emits its context.
#[test]
fn observe_sketches_keeps_a_distribution_only_sketch() {
    let mut sketch = Sketch::new();
    sketch.metric = "legacy.dist".to_string();
    let mut dist = Distribution::new();
    dist.ts = 300;
    dist.cnt = 4;
    sketch.distributions.push(dist);
    let mut payload = SketchPayload::new();
    payload.sketches.push(sketch);

    let contexts = observe_sketches(payload);
    assert_eq!(contexts.len(), 1);
    assert_eq!(contexts[0].name, "legacy.dist");
    assert_eq!(contexts[0].kind, MetricKind::Sketch);
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

    let contexts = observe_sketches(payload);
    let names: BTreeSet<&str> = contexts.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, BTreeSet::from(["latency"]));
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
    let series = decode_series_v3(Target::Agent, &OnceLock::new(), NOW_SECS, &bytes).expect("decode v3 payload");
    observe_series_v3(series, NOW_SECS)
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
    let v2_set: BTreeSet<Context> = observe_series(v2, NOW_SECS).into_iter().collect();

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

// The v3 lane drops NaN and too-far-future scalar points (validatePoint), keyed on the same receipt
// clock as v2. A series whose every point is dropped emits no context.
#[test]
fn observe_series_v3_all_points_dropped_emits_no_context() {
    let far_future = u64::try_from(NOW_SECS + MAX_SECONDS_IN_FUTURE + 1).expect("fits u64");
    let series = vec![V3Series {
        name: "app.count".to_string(),
        tags: vec!["env:prod".to_string()],
        kind: MetricKind::Count,
        points: vec![
            (110, BucketValue::Scalar(f64::NAN)),
            (far_future, BucketValue::Scalar(2.0)),
        ],
    }];

    assert!(observe_series_v3(series, NOW_SECS).is_empty());
}
