//! Parity tests for the trace-agent's trace normalization.
//!
//! Behaviors the trace-agent tests on V0 spans only, but that also apply to V1 spans, are tested on the V1 span
//! shape.

use libdd_trace_normalization::normalizer_v1::{normalize_span_with, MalformedReason, NormalizeObserver as _};
use saluki_common::collections::FastHashMap;
use saluki_core::components::test_util::TestComponentDriver;
use saluki_core::data_model::event::metric::MetricValues;
use saluki_core::data_model::event::trace::{AttributeValue, AttributeValueExt as _, Span, SpanLink, TraceIngestInfo};
use saluki_core::data_model::event::Event;

use super::*;
use crate::common::traces::normalize::MAX_TYPE_LEN;

const TEST_SPAN_ID: u64 = 0x5eed_5eed;
const PEER_SERVICE_KEY: &str = "peer.service";
const BASE_SERVICE_KEY: &str = "_dd.base_service";
const HTTP_STATUS_CODE_KEY: &str = "http.status_code";
const LINK_NAME_KEY: &str = "link.name";

/// Builds a valid V1 test span with a fixed span ID.
fn new_test_span_v1() -> Span {
    let attributes: FastHashMap<MetaString, AttributeValue> = [
        ("user", AttributeValue::String("leo".into())),
        ("pool", AttributeValue::String("fondue".into())),
        ("cheese_weight", AttributeValue::Float(100000.0)),
    ]
    .into_iter()
    .map(|(key, value)| (MetaString::from(key), value))
    .collect();
    let link = SpanLink::new(42, 1).with_attributes(link_name_attributes(AttributeValue::String("name".into())));
    Span::new(
        "django",
        "django.controller",
        "GET /some/raclette",
        "",
        TEST_SPAN_ID,
        1111,
        1448466874000000000,
        10000000,
        0,
    )
    .with_attributes(attributes)
    .with_span_links(vec![link])
}

fn link_name_attributes(value: AttributeValue) -> FastHashMap<MetaString, AttributeValue> {
    [(MetaString::from(LINK_NAME_KEY), value)].into_iter().collect()
}

fn new_test_trace(spans: Vec<Span>) -> Trace {
    let mut trace = Trace::new(spans, TraceIngestInfo::Datadog);
    trace.trace_id_low = 424242;
    trace
}

/// Normalizes one span, returning the result and the counts it recorded.
fn normalize_v1(span: &mut Span, lang: &str) -> (Result<(), DropReason>, ReasonCounts) {
    normalize_v1_with(span, lang, false)
}

fn normalize_v1_with(span: &mut Span, lang: &str, component2name: bool) -> (Result<(), DropReason>, ReasonCounts) {
    let mut counts = ReasonCounts::default();
    let opts = NormalizeOptions { lang, component2name };
    let result = normalize_span_with(span, &opts, &mut counts);
    if let Err(reason) = result {
        counts.drop(reason);
    }
    (result, counts)
}

/// Normalizes a trace chunk, returning the result and the counts it recorded.
fn normalize_trace_chunk_v1(trace: &mut Trace) -> (Result<(), DropReason>, ReasonCounts) {
    let mut counts = ReasonCounts::default();
    let result = DatadogTraceNormalizer::new(false).normalize_trace(trace, &mut counts);
    (result, counts)
}

/// Counts one malformed span per reason.
fn ts_malformed(reasons: &[MalformedReason]) -> ReasonCounts {
    let mut counts = ReasonCounts::default();
    for reason in reasons {
        counts.malformed(*reason);
    }
    counts
}

/// Counts one dropped trace for the reason.
fn ts_dropped(reason: DropReason) -> ReasonCounts {
    let mut counts = ReasonCounts::default();
    counts.drop(reason);
    counts
}

fn string_attribute<'a>(span: &'a Span, key: &str) -> Option<&'a str> {
    span.attributes
        .get(key)
        .and_then(AttributeValue::as_string)
        .map(|value| value.as_ref())
}

fn set_string_attribute(span: &mut Span, key: &str, value: &str) {
    span.attributes
        .insert(MetaString::from(key), AttributeValue::String(MetaString::from(value)));
}

fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

#[test]
fn normalize_service_pass_thru_v1() {
    let mut s = new_test_span_v1().with_service("foo");
    set_string_attribute(&mut s, PEER_SERVICE_KEY, "foo");
    set_string_attribute(&mut s, BASE_SERVICE_KEY, "bar");
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.service(), "foo");
    assert_eq!(string_attribute(&s, PEER_SERVICE_KEY), Some("foo"));
    assert_eq!(string_attribute(&s, BASE_SERVICE_KEY), Some("bar"));
    assert_eq!(counts, ReasonCounts::default());
}

#[test]
fn normalize_empty_service_no_lang_v1() {
    let mut s = new_test_span_v1().with_service("");
    set_string_attribute(&mut s, PEER_SERVICE_KEY, "");
    set_string_attribute(&mut s, BASE_SERVICE_KEY, "");
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.service(), "unnamed-service");
    // No fallback on the peer and base service tags.
    assert_eq!(string_attribute(&s, PEER_SERVICE_KEY), Some(""));
    assert_eq!(string_attribute(&s, BASE_SERVICE_KEY), Some(""));
    assert_eq!(counts, ts_malformed(&[MalformedReason::ServiceEmpty]));
}

#[test]
fn normalize_empty_service_with_lang_v1() {
    let mut s = new_test_span_v1().with_service("");
    set_string_attribute(&mut s, PEER_SERVICE_KEY, "");
    set_string_attribute(&mut s, BASE_SERVICE_KEY, "");
    let (result, counts) = normalize_v1(&mut s, "java");
    assert!(result.is_ok());
    assert_eq!(s.service(), "unnamed-java-service");
    assert_eq!(string_attribute(&s, PEER_SERVICE_KEY), Some(""));
    assert_eq!(string_attribute(&s, BASE_SERVICE_KEY), Some(""));
    assert_eq!(counts, ts_malformed(&[MalformedReason::ServiceEmpty]));
}

#[test]
fn normalize_long_service_v1() {
    let mut s = new_test_span_v1().with_service("CAMEMBERT".repeat(100));
    set_string_attribute(&mut s, PEER_SERVICE_KEY, &"BRIE".repeat(100));
    set_string_attribute(&mut s, BASE_SERVICE_KEY, &"ROQUEFORT".repeat(100));
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.service(), &"camembert".repeat(100)[..100]);
    assert_eq!(string_attribute(&s, PEER_SERVICE_KEY), Some(&"brie".repeat(100)[..100]));
    assert_eq!(
        string_attribute(&s, BASE_SERVICE_KEY),
        Some(&"roquefort".repeat(100)[..100])
    );
    assert_eq!(
        counts,
        ts_malformed(&[
            MalformedReason::ServiceTruncate,
            MalformedReason::PeerServiceTruncate,
            MalformedReason::BaseServiceTruncate,
        ])
    );
}

#[test]
fn normalize_name_pass_thru_v1() {
    let mut s = new_test_span_v1();
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.name(), "django.controller");
    assert_eq!(counts, ReasonCounts::default());
}

#[test]
fn normalize_empty_name_v1() {
    let mut s = new_test_span_v1().with_name("");
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.name(), "unnamed_operation");
    assert_eq!(counts, ts_malformed(&[MalformedReason::SpanNameEmpty]));
}

#[test]
fn normalize_long_name_v1() {
    let mut s = new_test_span_v1().with_name("CAMEMBERT".repeat(100));
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.name(), &"CAMEMBERT".repeat(100)[..100]);
    assert_eq!(counts, ts_malformed(&[MalformedReason::SpanNameTruncate]));
}

#[test]
fn normalize_name_no_alphanumeric_v1() {
    let mut s = new_test_span_v1().with_name("/");
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.name(), "unnamed_operation");
    assert_eq!(counts, ts_malformed(&[MalformedReason::SpanNameInvalid]));
}

#[test]
fn normalize_resource_pass_thru_v1() {
    let mut s = new_test_span_v1();
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.resource(), "GET /some/raclette");
    assert_eq!(counts, ReasonCounts::default());
}

#[test]
fn normalize_empty_resource_v1() {
    let mut s = new_test_span_v1();
    s.set_resource("");
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.resource(), s.name());
    assert_eq!(counts, ts_malformed(&[MalformedReason::ResourceEmpty]));
}

#[test]
fn normalize_no_span_id_v1() {
    let mut s = new_test_span_v1().with_span_id(0);
    let (result, counts) = normalize_v1(&mut s, "");
    assert_eq!(result, Err(DropReason::SpanIdZero));
    assert_eq!(counts, ts_dropped(DropReason::SpanIdZero));
}

#[test]
fn normalize_start_v1_pass_through() {
    let mut s = new_test_span_v1();
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.start(), 1448466874000000000);
    assert_eq!(counts, ReasonCounts::default());
}

#[test]
fn normalize_start_v1_too_small() {
    let mut s = new_test_span_v1().with_start(42);
    let min_start = now_nanos() - s.duration();
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert!(s.start() >= min_start);
    assert!(s.start() <= now_nanos() - s.duration());
    assert_eq!(counts, ts_malformed(&[MalformedReason::InvalidStartDate]));
}

#[test]
fn normalize_start_v1_too_small_with_large_duration() {
    let mut s = new_test_span_v1().with_start(42).with_duration(now_nanos() * 2);
    let min_start = now_nanos();
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(counts, ts_malformed(&[MalformedReason::InvalidStartDate]));
    assert!(s.start() >= min_start, "start should have been reset to current time");
    assert!(s.start() <= now_nanos(), "start should have been reset to current time");
}

#[test]
fn normalize_duration_pass_thru_v1() {
    let mut s = new_test_span_v1();
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.duration(), 10000000);
    assert_eq!(counts, ReasonCounts::default());
}

#[test]
fn normalize_empty_duration_v1() {
    let mut s = new_test_span_v1().with_duration(0);
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.duration(), 0);
    assert_eq!(counts, ReasonCounts::default());
}

#[test]
fn normalize_large_duration_v1() {
    let mut s = new_test_span_v1().with_duration(i64::MAX as u64);
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.duration(), 0);
    assert_eq!(counts, ts_malformed(&[MalformedReason::InvalidDuration]));
}

#[test]
fn normalize_duration_above_i64_max_with_zero_start_v1() {
    // The trace-agent zeroes a `u64` duration above `i64::MAX` even when the start is 0, so the sum doesn't overflow.
    let mut s = new_test_span_v1().with_start(0).with_duration(u64::MAX);
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.duration(), 0);
    assert_eq!(
        counts,
        ts_malformed(&[MalformedReason::InvalidDuration, MalformedReason::InvalidStartDate])
    );
}

#[test]
fn normalize_type_pass_thru_v1() {
    let mut s = new_test_span_v1().with_span_type("http");
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.span_type(), "http");
    assert_eq!(counts, ReasonCounts::default());
}

#[test]
fn normalize_type_too_long_v1() {
    let mut s = new_test_span_v1().with_span_type("sql".repeat(1000));
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.span_type().len(), MAX_TYPE_LEN);
    assert_eq!(counts, ts_malformed(&[MalformedReason::TypeTruncate]));
}

#[test]
fn normalize_service_tag_v1() {
    let mut s = new_test_span_v1().with_service("retargeting(api-Staging ");
    set_string_attribute(&mut s, PEER_SERVICE_KEY, "retargeting(api-Peer ");
    set_string_attribute(&mut s, BASE_SERVICE_KEY, "retargeting(api-Base ");
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.service(), "retargeting_api-staging");
    assert_eq!(string_attribute(&s, PEER_SERVICE_KEY), Some("retargeting_api-peer"));
    assert_eq!(string_attribute(&s, BASE_SERVICE_KEY), Some("retargeting_api-base"));
    assert_eq!(counts, ReasonCounts::default());
}

#[test]
fn normalize_env_v1() {
    let mut s = new_test_span_v1().with_env("123DEVELOPMENT");
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.env, "123development");
    assert_eq!(counts, ReasonCounts::default());
}

#[test]
fn normalize_span_link_name_v1() {
    for (link_name, expected) in [
        // An empty link name.
        ("", "unnamed_operation"),
        // An invalid link name.
        ("!@#$%^&*()_+", "unnamed_operation"),
        // A valid link name.
        ("valid_name", "valid_name"),
    ] {
        let mut s = new_test_span_v1();
        s.span_links_mut()[0] =
            SpanLink::new(42, 1).with_attributes(link_name_attributes(AttributeValue::String(link_name.into())));
        let (result, _) = normalize_v1(&mut s, "");
        assert!(result.is_ok());
        let normalized = s.span_links()[0].attributes().get(LINK_NAME_KEY);
        assert_eq!(
            normalized.and_then(AttributeValue::as_string).map(|name| name.as_ref()),
            Some(expected),
            "link name {link_name:?}"
        );
    }
}

/// The trace ID is already a fixed pair of 64-bit halves here, so only the zero check applies: the coercion of short
/// and over-long IDs to 16 bytes belongs to the decoder.
#[test]
fn normalize_trace_chunk_v1_trace_id() {
    for (name, high, low, want_drop) in [
        ("exact", 0x0102030405060708, 0x090a0b0c0d0e0f10, false),
        ("low-64-bits-zero", 0x0102030405060708, 0, false),
        ("high-64-bits-zero", 0, 1, false),
        ("all-zero", 0, 0, true),
    ] {
        let mut trace = new_test_trace(vec![new_test_span_v1()]);
        trace.trace_id_high = high;
        trace.trace_id_low = low;
        let (result, counts) = normalize_trace_chunk_v1(&mut trace);
        if want_drop {
            assert_eq!(result, Err(DropReason::TraceIdZero), "{name}");
            assert_eq!(counts, ts_dropped(DropReason::TraceIdZero), "{name}");
        } else {
            assert!(result.is_ok(), "{name}");
            assert_eq!(counts, ReasonCounts::default(), "{name}");
            assert_eq!((trace.trace_id_high, trace.trace_id_low), (high, low), "{name}");
        }
    }
}

/// With `component2name`, the span name is replaced by its `component` when set, a promoted field on V1 spans.
#[test]
fn normalize_component2name() {
    // On, with a component.
    let mut s = new_test_span_v1().with_component("component");
    assert!(normalize_v1_with(&mut s, "", true).0.is_ok());
    assert_eq!(s.name(), "component");

    // On, without a component.
    let mut s = new_test_span_v1();
    assert!(normalize_v1_with(&mut s, "", true).0.is_ok());
    assert_eq!(s.name(), "django.controller");

    // Off.
    let mut s = new_test_span_v1().with_component("component");
    assert!(normalize_v1_with(&mut s, "", false).0.is_ok());
    assert_eq!(s.name(), "django.controller");
}

/// A Zipkin root span, whose parent ID equals its span ID, gets a zero parent ID; the V1 check compares the parent ID
/// with the span ID only.
#[test]
fn special_zipkin_root_span() {
    let mut s = new_test_span_v1().with_span_id(42).with_parent_id(42);
    let (result, counts) = normalize_v1(&mut s, "");
    assert!(result.is_ok());
    assert_eq!(s.parent_id(), 0);
    assert_eq!(s.span_id(), 42);
    assert_eq!(counts, ReasonCounts::default());
}

#[test]
fn normalize_trace_empty() {
    let mut trace = new_test_trace(Vec::new());
    let (result, counts) = normalize_trace_chunk_v1(&mut trace);
    assert_eq!(result, Err(DropReason::EmptyTrace));
    assert_eq!(counts, ts_dropped(DropReason::EmptyTrace));
}

#[test]
fn normalize_trace_invalid_span() {
    let span2 = new_test_span_v1().with_span_id(TEST_SPAN_ID + 1).with_name("");
    let mut trace = new_test_trace(vec![new_test_span_v1(), span2]);
    let (result, counts) = normalize_trace_chunk_v1(&mut trace);
    assert!(result.is_ok());
    assert_eq!(counts, ts_malformed(&[MalformedReason::SpanNameEmpty]));
}

#[test]
fn normalize_trace_duplicate_span_id() {
    let mut trace = new_test_trace(vec![new_test_span_v1(), new_test_span_v1()]);
    let (result, counts) = normalize_trace_chunk_v1(&mut trace);
    assert!(result.is_ok());
    assert_eq!(counts, ts_malformed(&[MalformedReason::DuplicateSpanId]));
}

#[test]
fn normalize_trace() {
    let span2 = new_test_span_v1().with_span_id(TEST_SPAN_ID + 1);
    let mut trace = new_test_trace(vec![new_test_span_v1(), span2]);
    let (result, counts) = normalize_trace_chunk_v1(&mut trace);
    assert!(result.is_ok());
    assert_eq!(counts, ReasonCounts::default());
}

/// A span with a zero span ID drops the whole chunk, but the spans before it have already been normalized and
/// counted, as in the trace-agent.
#[test]
fn normalize_trace_chunk_v1_counts_spans_before_the_invalid_one() {
    let span1 = new_test_span_v1().with_name("");
    let span2 = new_test_span_v1().with_span_id(0);
    let mut trace = new_test_trace(vec![span1, span2]);
    let (result, counts) = normalize_trace_chunk_v1(&mut trace);
    assert_eq!(result, Err(DropReason::SpanIdZero));
    let mut expected = ts_malformed(&[MalformedReason::SpanNameEmpty]);
    expected.drop(DropReason::SpanIdZero);
    assert_eq!(counts, expected);
}

/// An `http.status_code` outside 100–599 is dropped, with any attribute type read as a string, as the trace-agent
/// does.
#[test]
fn normalize_v1_drops_invalid_http_status_code() {
    for (code, valid) in [
        (AttributeValue::String("200".into()), true),
        (AttributeValue::Int(599), true),
        (AttributeValue::Float(404.0), true),
        (AttributeValue::Bytes(b"503".to_vec()), true),
        (AttributeValue::String("600".into()), false),
        // The trace-agent parses the code with `strconv.ParseUint`, which rejects a sign.
        (AttributeValue::String("+200".into()), false),
        (AttributeValue::String("99".into()), false),
        (AttributeValue::String("abc".into()), false),
        (AttributeValue::Int(-200), false),
        (AttributeValue::Float(200.5), false),
        (AttributeValue::Bool(true), false),
    ] {
        let mut s = new_test_span_v1();
        s.attributes
            .insert(MetaString::from(HTTP_STATUS_CODE_KEY), code.clone());
        let (result, counts) = normalize_v1(&mut s, "");
        assert!(result.is_ok());
        if valid {
            assert_eq!(s.attributes.get(HTTP_STATUS_CODE_KEY), Some(&code));
            assert_eq!(counts, ReasonCounts::default(), "{code:?}");
        } else {
            assert!(!s.attributes.contains_key(HTTP_STATUS_CODE_KEY), "{code:?}");
            assert_eq!(
                counts,
                ts_malformed(&[MalformedReason::InvalidHttpStatusCode]),
                "{code:?}"
            );
        }
    }
}

#[test]
fn format_as_string_matches_go_any_value_as_string() {
    for (value, expected) in [
        (AttributeValue::Float(200.0), "200"),
        (AttributeValue::Float(0.1), "0.1"),
        (AttributeValue::Float(1e21), "1000000000000000000000"),
        (AttributeValue::Float(f64::INFINITY), "+Inf"),
        (AttributeValue::Float(f64::NAN), "NaN"),
        (AttributeValue::Bool(false), "false"),
        (
            AttributeValue::Array(vec![AttributeValue::Int(1), AttributeValue::String("a".into())]),
            "[1,a]",
        ),
        (
            AttributeValue::KeyValueList(vec![(MetaString::from("k"), AttributeValue::Bool(true))]),
            "{k=true}",
        ),
    ] {
        assert_eq!(value.format_as_string(), expected, "{value:?}");
    }
}

/// The component forwards normalized traces, drops invalid chunks, and reports the counts on its metrics output.
#[tokio::test]
async fn transform_drops_invalid_chunks_and_reports_telemetry() {
    let (control, mut outputs) = TestComponentDriver::transform(DatadogTraceNormalizerConfiguration::new())
        .await
        .expect("the normalizer should start");

    let mut valid = new_test_trace(vec![new_test_span_v1().with_name("")]);
    valid.payload.language_name = MetaString::from("go");
    let mut invalid = new_test_trace(vec![new_test_span_v1()]);
    invalid.trace_id_low = 0;
    control.send_event(Event::Trace(valid)).await;
    control.send_event(Event::Trace(invalid)).await;
    control.shutdown().await.expect("the normalizer should stop cleanly");

    let traces = outputs.default_output().collect_events().await;
    assert_eq!(traces.len(), 1, "{traces:?}");
    let Event::Trace(trace) = &traces[0] else {
        panic!("expected a trace: {:?}", traces[0]);
    };
    assert_eq!(trace.spans()[0].name(), "unnamed_operation");

    let metrics = outputs.named_output("metrics").collect_events().await;
    let counter = |name: &str, tags: &[&str]| {
        metrics.iter().find_map(|event| {
            let metric = event.try_as_metric()?;
            let matches = metric.context().name() == name
                && metric.context().tags().len() == tags.len()
                && tags.iter().all(|tag| metric.context().tags().has_tag(tag));
            matches.then(|| metric.values().clone())
        })
    };
    assert_eq!(
        counter(
            "datadog.trace_agent.normalizer.spans_malformed",
            &["lang:go", "reason:span_name_empty"]
        ),
        Some(MetricValues::counter(1.0)),
        "{metrics:?}"
    );
    assert_eq!(
        counter(
            "datadog.trace_agent.normalizer.traces_dropped",
            &["reason:trace_id_zero"]
        ),
        Some(MetricValues::counter(1.0)),
        "{metrics:?}"
    );
}

/// The trace-agent skips an empty chunk before normalizing it, so it's dropped without telemetry.
#[tokio::test]
async fn transform_drops_empty_chunks_silently() {
    let (control, mut outputs) = TestComponentDriver::transform(DatadogTraceNormalizerConfiguration::new())
        .await
        .expect("the normalizer should start");

    control.send_event(Event::Trace(new_test_trace(Vec::new()))).await;
    control.shutdown().await.expect("the normalizer should stop cleanly");

    let traces = outputs.default_output().collect_events().await;
    assert!(traces.is_empty(), "{traces:?}");
    let metrics = outputs.named_output("metrics").collect_events().await;
    assert!(metrics.is_empty(), "{metrics:?}");
}

/// The payload env is normalized as a tag value, as the trace-agent does, and gets the same result as the span env.
#[test]
fn normalize_payload_env() {
    let mut normalizer = DatadogTraceNormalizer::new(false);
    for (env, expected) in [
        ("Prod Env!", "prod_env"),
        ("staging", "staging"),
        ("", ""),
        ("123DEVELOPMENT", "123development"),
        // `libdd-trace-normalization` keeps `²`, which Go's `unicode.IsDigit` drops.
        ("api\u{00B2}", "api\u{00B2}"),
    ] {
        let mut trace = new_test_trace(vec![new_test_span_v1().with_env(env)]);
        trace.payload.env = MetaString::from(env);
        normalizer.normalize_payload_env(&mut trace);
        assert_eq!(trace.payload.env, expected, "env {env:?}");
        assert!(normalizer
            .normalize_trace(&mut trace, &mut ReasonCounts::default())
            .is_ok());
        assert_eq!(trace.spans()[0].env, expected, "span env {env:?}");
    }
}
