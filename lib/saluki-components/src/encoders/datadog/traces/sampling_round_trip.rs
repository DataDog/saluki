//! Round-trip tests from trace ingestion through the trace sampler and the trace encoder to the wire.
//!
//! Regression tests: the sampler once classified traces by a span attribute and wrote the APM sampler's rate into
//! `_dd.otlp_sr`, and the encoder filled in a configured rate and an `otlp-` tracer version prefix for every trace.
//! OTLP traces could report the wrong receiver rate, and native traces reported a rate they never had.
//!
//! OTLP traces come from the OTLP translator, which stamps the receiver sampling rate on each trace but doesn't sample.
//! Native traces come from the v1 decoder. The sampler and encoder run as components. The tests check that the stamped
//! receiver rate survives sampling and encoding, independently of APM sampling decisions, and that native traces carry
//! no OTLP receiver rate on the wire.

use std::collections::HashMap;
use std::time::Duration;

use base64::{engine::general_purpose, Engine as _};
use datadog_agent_apm::semantics::SemanticRegistryProvider;
use datadog_protos::traces::AgentPayload;
use http_body_util::BodyExt as _;
use otlp_protos::opentelemetry::proto::common::v1::{any_value::Value, AnyValue, KeyValue};
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use otlp_protos::opentelemetry::proto::trace::v1::{
    span::SpanKind, status::StatusCode, ResourceSpans, ScopeSpans, Span as OtlpSpan, Status,
};
use protobuf::Message as _;
use saluki_core::components::test_util::TestComponentDriver;
use saluki_core::data_model::event::trace::{AttributeValue, Trace};
use saluki_core::data_model::event::Event;
use stringtheory::MetaString;

use super::{trace_configuration, DatadogTraceConfiguration};
use crate::common::otlp::traces::translator::{traces_settings, OtlpTracesSettings, OtlpTracesTranslator};
use crate::common::otlp::Metrics;
use crate::config::AgentEnv;
use crate::decoders::datadog::decode_v1_payload;
use crate::transforms::{sampler_configuration, TraceSamplerConfiguration};

const RECEIVER_SAMPLING_PERCENTAGE: f64 = 50.0;

/// Default maximum resource name length, in bytes.
const MAX_RESOURCE_LEN: usize = 5000;

// Deterministic receiver sampling decisions at a rate of 0.5, from the Go trace agent's `TestCreateChunks`:
// https://github.com/DataDog/datadog-agent/blob/c6f7e0bc2d5744601a10f231751948ac7a4ee781/pkg/trace/api/otlp_test.go#L740-L810
const RECEIVER_KEPT_TRACE_ID: u64 = 123;
const RECEIVER_DROPPED_TRACE_ID: u64 = 1237892138897;

const OTLP_SDK_VERSION: &str = "1.30.0";
const OTLP_SERVICE: &str = "otlp-svc";

/// v1 tracer payloads produced by the Go trace agent's reference encoder.
const NATIVE_FIXTURES_JSON: &str = include_str!("../../../decoders/datadog/testdata/v1_decoder_cases.json");
// The `minimal_span` fixture's tracer version and root span service.
const NATIVE_TRACER_VERSION: &str = "v2.1.0";
const NATIVE_SERVICE: &str = "checkout";

const KEY_SPAN_SAMPLING_MECHANISM: &str = "_dd.span_sampling.mechanism";
const KEY_TOP_LEVEL: &str = "_top_level";
const KEY_OTLP_SAMPLING_RATE: &str = "_dd.otlp_sr";
const KEY_DECISION_MAKER: &str = "_dd.p.dm";
const KEY_ETS_ERROR: &str = "_dd.error_tracking_standalone.error";

/// One encoded trace chunk, with string table references resolved.
#[derive(Debug)]
struct WireChunk {
    tracer_version: String,
    trace_id_low: u64,
    priority: i32,
    dropped_trace: bool,
    /// String-valued chunk attributes.
    attributes: HashMap<String, String>,
    span_ids: Vec<u64>,
}

impl WireChunk {
    fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }
}

/// Translates an OTLP trace with a server root span (ID 1) and an internal child span (ID 2).
fn otlp_trace(receiver_sampling_percentage: f64, trace_id_low: u64, error: bool) -> Trace {
    let mut trace_id = [0u8; 16];
    trace_id[8..].copy_from_slice(&trace_id_low.to_be_bytes());
    let status = error.then(|| Status {
        code: StatusCode::Error.into(),
        ..Default::default()
    });
    let span = |span_id: u64, parent_span_id: Vec<u8>, kind: SpanKind| OtlpSpan {
        trace_id: trace_id.to_vec(),
        span_id: span_id.to_be_bytes().to_vec(),
        parent_span_id,
        name: "op".to_string(),
        kind: kind.into(),
        start_time_unix_nano: 1,
        end_time_unix_nano: 2,
        status: status.clone(),
        ..Default::default()
    };
    let string_kv = |key: &str, value: &str| KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(Value::StringValue(value.to_string())),
        }),
    };
    let resource_spans = ResourceSpans {
        resource: Some(Resource {
            attributes: vec![
                string_kv("service.name", OTLP_SERVICE),
                string_kv("telemetry.sdk.version", OTLP_SDK_VERSION),
            ],
            ..Default::default()
        }),
        scope_spans: vec![ScopeSpans {
            spans: vec![
                span(1, Vec::new(), SpanKind::Server),
                span(2, 1u64.to_be_bytes().to_vec(), SpanKind::Internal),
            ],
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut translator = OtlpTracesTranslator::new(
        OtlpTracesSettings {
            probabilistic_sampler_sampling_percentage: receiver_sampling_percentage,
            ..traces_settings()
        },
        MAX_RESOURCE_LEN,
        SemanticRegistryProvider::default(),
    );
    let mut traces: Vec<Trace> = translator
        .translate_spans(resource_spans, &Metrics::for_tests())
        .filter_map(Event::try_into_trace)
        .collect();
    assert_eq!(traces.len(), 1);
    traces.remove(0)
}

/// Decodes the single-span `minimal_span` v1 fixture, then applies the given sampling priority.
fn native_trace(priority: i32) -> Trace {
    let fixtures: serde_json::Value = serde_json::from_str(NATIVE_FIXTURES_JSON).expect("fixtures should parse");
    let payload = fixtures["cases"]
        .as_array()
        .and_then(|cases| cases.iter().find(|case| case["name"] == "minimal_span"))
        .and_then(|case| case["payload_base64"].as_str())
        .expect("the `minimal_span` fixture should exist");
    let bytes = general_purpose::STANDARD
        .decode(payload)
        .expect("fixture should be base64");
    let mut traces = decode_v1_payload(&bytes).expect("fixture should decode");
    assert_eq!(traces.len(), 1);
    let mut trace = traces.remove(0);
    trace.priority = Some(priority);
    trace
}

fn mark_span(trace: &mut Trace, span_index: usize, key: &'static str) {
    trace.spans_mut()[span_index]
        .attributes
        .insert(MetaString::from_static(key), AttributeValue::Float(1.0));
}

/// Returns sampler settings with the Datadog schema defaults the sampler reads.
fn sampler() -> TraceSamplerConfiguration {
    TraceSamplerConfiguration {
        target_traces_per_second: 10.0,
        errors_per_second: 10.0,
        max_events_per_second: 200.0,
        ..sampler_configuration(AgentEnv::resolve(None, None, &[], &[]))
    }
}

/// Runs `traces` through the trace sampler and the trace encoder, and decodes every encoded chunk.
///
/// The encoder shares the sampler's targets and Error Tracking Standalone setting, as both read them from the same
/// configuration.
async fn sample_and_encode(sampler: TraceSamplerConfiguration, traces: Vec<Trace>) -> Vec<WireChunk> {
    let encoder = DatadogTraceConfiguration {
        flush_timeout: Duration::from_millis(10),
        target_traces_per_second: sampler.target_traces_per_second,
        errors_per_second: sampler.errors_per_second,
        error_tracking_standalone: sampler.error_tracking_standalone,
        ..trace_configuration(sampler.agent_env.clone())
    };

    let (sampler, mut sampler_outputs) = TestComponentDriver::transform(sampler)
        .await
        .expect("trace sampler should build");
    sampler.send_events(traces.into_iter().map(Event::Trace)).await;
    sampler.shutdown().await.expect("trace sampler should stop cleanly");
    let sampled = sampler_outputs.default_output().collect_events().await;

    let (encoder, mut payloads) = TestComponentDriver::encoder(encoder)
        .await
        .expect("trace encoder should build");
    encoder.send_events(sampled).await;
    encoder.shutdown().await.expect("trace encoder should stop cleanly");

    let mut chunks = Vec::new();
    for payload in payloads.collect().await {
        let (_, request) = payload
            .try_into_http_payload()
            .expect("traces are sent over HTTP")
            .into_parts();
        let body = request
            .into_body()
            .collect()
            .await
            .expect("body should collect")
            .to_bytes();
        let body = zstd::stream::decode_all(body.as_ref()).expect("body should decompress");
        let agent_payload = AgentPayload::parse_from_bytes(&body).expect("body should be an agent payload");
        for tracer_payload in &agent_payload.idxTracerPayloads {
            let strings = tracer_payload.strings();
            let resolve = |string_ref: u32| strings[string_ref as usize].clone();
            for chunk in tracer_payload.chunks() {
                let attributes = chunk
                    .attributes()
                    .iter()
                    .filter_map(|(key, value)| match &value.value {
                        Some(datadog_protos::traces::idx::any_value::Value::StringValueRef(value)) => {
                            Some((resolve(*key), resolve(*value)))
                        }
                        _ => None,
                    })
                    .collect();
                let trace_id: [u8; 16] = chunk.traceID().try_into().expect("trace IDs are 16 bytes");
                chunks.push(WireChunk {
                    tracer_version: resolve(tracer_payload.tracerVersionRef()),
                    trace_id_low: u64::from_be_bytes(trace_id[8..].try_into().unwrap()),
                    priority: chunk.priority(),
                    dropped_trace: chunk.droppedTrace(),
                    attributes,
                    span_ids: chunk.spans().iter().map(|span| span.spanID()).collect(),
                });
            }
        }
    }
    chunks
}

/// Splits encoded chunks into the OTLP chunk and the native chunk, by tracer version.
#[track_caller]
fn split_by_protocol(mut chunks: Vec<WireChunk>) -> (WireChunk, WireChunk) {
    assert_eq!(chunks.len(), 2, "expected one OTLP and one native chunk: {chunks:?}");
    chunks.sort_by_key(|chunk| chunk.tracer_version != NATIVE_TRACER_VERSION);
    let otlp = chunks.pop().unwrap();
    let native = chunks.pop().unwrap();
    assert_eq!(otlp.tracer_version, format!("otlp-{OTLP_SDK_VERSION}"));
    assert_eq!(native.tracer_version, NATIVE_TRACER_VERSION);
    (otlp, native)
}

#[tokio::test]
async fn default_configuration_keeps_traces_and_reports_receiver_rate_for_otlp_only() {
    let traces = vec![otlp_trace(100.0, RECEIVER_DROPPED_TRACE_ID, false), native_trace(1)];
    let (otlp, native) = split_by_protocol(sample_and_encode(sampler(), traces).await);

    assert_eq!(otlp.priority, 1);
    assert!(!otlp.dropped_trace);
    assert_eq!(otlp.span_ids, [1, 2]);
    assert_eq!(otlp.attribute(KEY_DECISION_MAKER), Some("-9"));
    assert_eq!(otlp.attribute(KEY_OTLP_SAMPLING_RATE), Some("1.00"));

    assert_eq!(native.priority, 1);
    assert!(!native.dropped_trace);
    assert_eq!(native.attribute(KEY_OTLP_SAMPLING_RATE), None);
}

#[tokio::test]
async fn single_span_sampling_rescues_receiver_drop_with_apm_sampler_disabled() {
    let mut otlp = otlp_trace(RECEIVER_SAMPLING_PERCENTAGE, RECEIVER_DROPPED_TRACE_ID, false);
    mark_span(&mut otlp, 1, KEY_SPAN_SAMPLING_MECHANISM);
    let mut native = native_trace(0);
    mark_span(&mut native, 0, KEY_SPAN_SAMPLING_MECHANISM);

    let (otlp, native) = split_by_protocol(sample_and_encode(sampler(), vec![otlp, native]).await);

    assert_eq!(otlp.priority, 2);
    assert!(!otlp.dropped_trace);
    assert_eq!(otlp.span_ids, [2]);
    assert_eq!(otlp.attribute(KEY_OTLP_SAMPLING_RATE), Some("0.50"));

    assert_eq!(native.priority, 2);
    assert!(!native.dropped_trace);
    assert_eq!(native.attribute(KEY_OTLP_SAMPLING_RATE), None);
}

#[tokio::test]
async fn apm_probabilistic_sampler_keep_reports_receiver_rate() {
    let mut sampler = sampler();
    sampler.probabilistic_sampler_enabled = true;
    sampler.sampling_percentage = 100.0;
    // The receiver rate would drop this trace, but the APM sampler decides once it is enabled.
    let traces = vec![
        otlp_trace(RECEIVER_SAMPLING_PERCENTAGE, RECEIVER_DROPPED_TRACE_ID, false),
        native_trace(1),
    ];
    let (otlp, native) = split_by_protocol(sample_and_encode(sampler, traces).await);

    assert_eq!(otlp.priority, 1);
    assert!(!otlp.dropped_trace);
    assert_eq!(otlp.span_ids, [1, 2]);
    assert_eq!(otlp.attribute(KEY_DECISION_MAKER), Some("-9"));
    assert_eq!(otlp.attribute(KEY_OTLP_SAMPLING_RATE), Some("0.50"));

    assert_eq!(native.priority, 1);
    assert_eq!(native.attribute(KEY_DECISION_MAKER), Some("-9"));
    assert_eq!(native.attribute(KEY_OTLP_SAMPLING_RATE), None);
}

#[tokio::test]
async fn single_span_sampling_rescues_apm_probabilistic_sampler_drop() {
    let mut sampler = sampler();
    sampler.probabilistic_sampler_enabled = true;
    sampler.sampling_percentage = 0.0;
    let mut otlp = otlp_trace(RECEIVER_SAMPLING_PERCENTAGE, RECEIVER_KEPT_TRACE_ID, false);
    mark_span(&mut otlp, 1, KEY_SPAN_SAMPLING_MECHANISM);
    let mut native = native_trace(1);
    mark_span(&mut native, 0, KEY_SPAN_SAMPLING_MECHANISM);

    let (otlp, native) = split_by_protocol(sample_and_encode(sampler, vec![otlp, native]).await);

    assert_eq!(otlp.priority, 2);
    assert_eq!(otlp.span_ids, [2]);
    assert_eq!(otlp.attribute(KEY_OTLP_SAMPLING_RATE), Some("0.50"));

    assert_eq!(native.priority, 2);
    assert_eq!(native.attribute(KEY_OTLP_SAMPLING_RATE), None);
}

#[tokio::test]
async fn analyzed_events_from_dropped_traces_report_receiver_rate() {
    let mut sampler = sampler();
    sampler.analyzed_rate_by_service =
        HashMap::from([(OTLP_SERVICE.to_string(), 1.0), (NATIVE_SERVICE.to_string(), 1.0)]);
    // Only the roots are top-level, so only the roots are extracted as events.
    let mut otlp = otlp_trace(RECEIVER_SAMPLING_PERCENTAGE, RECEIVER_DROPPED_TRACE_ID, false);
    mark_span(&mut otlp, 0, KEY_TOP_LEVEL);
    let mut native = native_trace(0);
    mark_span(&mut native, 0, KEY_TOP_LEVEL);

    let (otlp, native) = split_by_protocol(sample_and_encode(sampler, vec![otlp, native]).await);

    assert!(otlp.dropped_trace);
    assert_eq!(otlp.span_ids, [1]);
    assert_eq!(otlp.attribute(KEY_OTLP_SAMPLING_RATE), Some("0.50"));

    assert!(native.dropped_trace);
    assert_eq!(native.span_ids.len(), 1);
    assert_eq!(native.attribute(KEY_OTLP_SAMPLING_RATE), None);
}

#[tokio::test]
async fn error_tracking_standalone_reports_receiver_rate() {
    let mut sampler = sampler();
    sampler.error_tracking_standalone = true;
    let native_error_trace = {
        let mut trace = native_trace(1);
        let spans = trace.spans().iter().cloned().map(|span| span.with_error(1)).collect();
        trace.set_spans(spans);
        trace
    };
    let traces = vec![
        otlp_trace(RECEIVER_SAMPLING_PERCENTAGE, RECEIVER_KEPT_TRACE_ID, true),
        native_error_trace,
        otlp_trace(RECEIVER_SAMPLING_PERCENTAGE, RECEIVER_DROPPED_TRACE_ID, false),
        native_trace(1),
    ];
    // Non-error traces are forwarded as dropped.
    let (non_error, error): (Vec<_>, Vec<_>) = sample_and_encode(sampler, traces)
        .await
        .into_iter()
        .partition(|chunk| chunk.dropped_trace);
    let error = split_by_protocol(error);
    let non_error = split_by_protocol(non_error);

    for (otlp, native) in [&error, &non_error] {
        assert_eq!(otlp.attribute(KEY_OTLP_SAMPLING_RATE), Some("0.50"));
        assert_eq!(native.attribute(KEY_OTLP_SAMPLING_RATE), None);
    }

    let (otlp, native) = error;
    assert_eq!(otlp.priority, 1);
    assert_eq!(otlp.attribute(KEY_DECISION_MAKER), Some("-9"));
    assert_eq!(otlp.attribute(KEY_ETS_ERROR), Some("true"));
    assert_eq!(native.attribute(KEY_ETS_ERROR), Some("true"));

    let (otlp, native) = non_error;
    assert_eq!(otlp.priority, 0);
    assert_eq!(otlp.attribute(KEY_ETS_ERROR), None);
    assert_eq!(native.attribute(KEY_ETS_ERROR), None);
}

#[tokio::test]
async fn same_trace_id_from_both_protocols_encodes_separate_chunks() {
    let otlp = otlp_trace(RECEIVER_SAMPLING_PERCENTAGE, RECEIVER_KEPT_TRACE_ID, false);
    let mut native = native_trace(1);
    native.trace_id_high = otlp.trace_id_high;
    native.trace_id_low = otlp.trace_id_low;

    let (otlp, native) = split_by_protocol(sample_and_encode(sampler(), vec![otlp, native]).await);

    assert_eq!(otlp.trace_id_low, RECEIVER_KEPT_TRACE_ID);
    assert_eq!(native.trace_id_low, RECEIVER_KEPT_TRACE_ID);
    assert_eq!(otlp.priority, 1);
    assert!(!otlp.dropped_trace);
    assert_eq!(otlp.attribute(KEY_DECISION_MAKER), Some("-9"));
    assert_eq!(native.priority, 1);
    assert!(!native.dropped_trace);
    assert_eq!(otlp.attribute(KEY_OTLP_SAMPLING_RATE), Some("0.50"));
    assert_eq!(native.attribute(KEY_OTLP_SAMPLING_RATE), None);
}
