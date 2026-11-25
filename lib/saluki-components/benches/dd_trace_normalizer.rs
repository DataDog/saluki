//! Throughput of the Datadog trace normalizer transform.
//!
//! The bench sends one buffer of Datadog traces through the running transform and waits for the normalized traces on
//! its default output. Each trace has several spans with the attributes that the normalizer reads. One group holds
//! traces that are already normalized, and the other one holds traces where half of the spans need repairs.

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use saluki_common::collections::FastHashMap;
use saluki_components::transforms::DatadogTraceNormalizerConfiguration;
use saluki_core::components::test_util::{ComponentControl, Outputs, TestComponentDriver};
use saluki_core::data_model::event::trace::{AttributeValue, Span, SpanLink, Trace, TraceIngestInfo};
use saluki_core::data_model::event::Event;
use saluki_core::topology::EventsBuffer;
use stringtheory::MetaString;
use tokio::runtime::Runtime;

const TRACES: usize = 128;
const SPANS_PER_TRACE: usize = 8;

fn text(s: &str) -> MetaString {
    // A decoder builds its strings from the payload, so short strings are inlined and long ones are owned.
    MetaString::from(s)
}

fn attributes(entries: &[(&str, AttributeValue)]) -> FastHashMap<MetaString, AttributeValue> {
    entries.iter().map(|(k, v)| (text(k), v.clone())).collect()
}

/// Builds a span. A span that needs repairs has an uppercase service, a long name with invalid characters, an empty
/// resource, an invalid status code, an env with uppercase letters, and a link name to fix.
fn span(trace: usize, index: usize, repair: bool) -> Span {
    let span_id = (trace * SPANS_PER_TRACE + index + 1) as u64;
    let (service, name, resource, env, status, link_name) = if repair {
        (
            "Checkout Service",
            "Django.Controller!!/handle request for the checkout page with a long name",
            "",
            "Production",
            AttributeValue::String(text("+200")),
            AttributeValue::Int(42),
        )
    } else {
        (
            "checkout-service",
            "django.controller",
            "GET /api/v2/checkout/{cart_id}/items",
            "production",
            AttributeValue::Int(200),
            AttributeValue::String(text("next_request")),
        )
    };
    Span::new(
        text(service),
        text(name),
        text(resource),
        text("web"),
        span_id,
        if index == 0 { 0 } else { span_id - 1 },
        1_700_000_000_000_000_000 + index as u64 * 1_000,
        5_000_000,
        0,
    )
    .with_env(text(env))
    .with_version(text("1.42.0"))
    .with_component(text("django"))
    .with_kind(2)
    .with_attributes(attributes(&[
        ("http.status_code", status),
        ("http.method", AttributeValue::String(text("GET"))),
        (
            "http.url",
            AttributeValue::String(text("https://shop.example.com/api/v2/checkout/123/items")),
        ),
        ("peer.service", AttributeValue::String(text("postgres"))),
        ("_dd.base_service", AttributeValue::String(text("checkout"))),
        ("db.rows", AttributeValue::Int(12)),
        ("_dd.measured", AttributeValue::Float(1.0)),
        ("_sampling_priority_v1", AttributeValue::Float(1.0)),
    ]))
    .with_span_links(vec![
        SpanLink::new(7, 9).with_attributes(attributes(&[("link.name", link_name)]))
    ])
}

fn traces(repair: bool) -> Vec<Event> {
    (0..TRACES)
        .map(|t| {
            let spans = (0..SPANS_PER_TRACE).map(|i| span(t, i, repair && i % 2 == 1)).collect();
            let mut trace = Trace::new(spans, TraceIngestInfo::Datadog);
            trace.trace_id_low = t as u64 + 1;
            trace.payload.language_name = text("python");
            trace.payload.language_version = text("3.12.1");
            trace.payload.tracer_version = text("2.14.0");
            trace.payload.env = text("production");
            Event::Trace(trace)
        })
        .collect()
}

fn bench_normalizer(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("should build the runtime");
    let (control, mut outputs) = rt
        .block_on(TestComponentDriver::transform(
            DatadogTraceNormalizerConfiguration::new(),
        ))
        .expect("the normalizer should start");

    let mut group = c.benchmark_group("dd_trace_normalizer");
    group.throughput(Throughput::Elements((TRACES * SPANS_PER_TRACE) as u64));
    for (name, repair) in [("normalized", false), ("half_repaired", true)] {
        let input = traces(repair);
        group.bench_with_input(BenchmarkId::from_parameter(name), &input, |b, input| {
            b.iter_batched(
                || input.clone(),
                |events| run(&rt, &control, &mut outputs, events),
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

/// Sends `events` to the transform, and returns the buffers it forwards, so that the bench drops them untimed.
fn run(
    rt: &Runtime, control: &ComponentControl<EventsBuffer>, outputs: &mut Outputs<EventsBuffer>, events: Vec<Event>,
) -> Vec<EventsBuffer> {
    rt.block_on(async {
        control.send_events(events).await;
        let mut received = Vec::new();
        let mut count = 0;
        while count < TRACES {
            let buffer = outputs
                .default_output()
                .next()
                .await
                .expect("the normalizer should forward the traces");
            count += buffer.len();
            received.push(buffer);
        }
        // The telemetry window reports every 10 seconds: drain it, so that the transform never waits on it.
        while outputs.named_output("metrics").try_next().is_some() {}
        received
    })
}

criterion_group!(benches, bench_normalizer);
criterion_main!(benches);
