use std::num::NonZeroUsize;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use otlp_protos::opentelemetry::proto::{
    collector::trace::v1::ExportTraceServiceRequest,
    common::v1::{any_value::Value, AnyValue, InstrumentationScope, KeyValue},
    resource::v1::Resource,
    trace::v1::{span::SpanKind, status::StatusCode, ResourceSpans, ScopeSpans, Span, Status},
};
use prost::Message;
use saluki_components::common::otlp::{config::TracesConfig, traces::translator::OtlpTracesTranslator, Metrics};
use tokio::sync::mpsc;

fn kv(key: &str, val: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(Value::StringValue(val.to_string())),
        }),
    }
}

fn make_span(i: usize) -> Span {
    // Distribute across trace groups of 5 so translate_spans exercises its HashMap grouping.
    let mut trace_id = [0u8; 16];
    trace_id[15] = (i / 5) as u8;
    let mut span_id = [0u8; 8];
    span_id[7] = (i % 256) as u8;
    span_id[6] = (i / 256) as u8;

    Span {
        trace_id: trace_id.to_vec(),
        span_id: span_id.to_vec(),
        parent_span_id: vec![],
        name: format!("GET /api/v1/products/{}", i % 10),
        kind: if i.is_multiple_of(3) {
            SpanKind::Client
        } else {
            SpanKind::Server
        } as i32,
        start_time_unix_nano: 1_700_000_000_000_000_000 + (i as u64 * 1_000_000),
        end_time_unix_nano: 1_700_000_000_000_000_000 + (i as u64 * 1_000_000) + 5_000_000,
        attributes: vec![
            kv("http.method", "GET"),
            kv("http.route", "/api/v1/products/:id"),
            kv("http.scheme", "https"),
            kv("http.response.status_code", "200"),
            kv("net.peer.name", "product-service.internal"),
            kv("net.peer.port", "8080"),
            kv("peer.service", "product-service"),
        ],
        status: Some(Status {
            code: StatusCode::Ok as i32,
            message: String::new(),
        }),
        ..Default::default()
    }
}

fn make_resource_spans(span_count: usize) -> ResourceSpans {
    ResourceSpans {
        resource: Some(Resource {
            attributes: vec![
                kv("service.name", "api-gateway"),
                kv("deployment.environment", "production"),
                kv("service.version", "1.2.3"),
                kv("container.id", "abc123def456abc123def456"),
                kv("k8s.pod.uid", "pod-uid-12345678-abcd"),
            ],
            ..Default::default()
        }),
        scope_spans: vec![ScopeSpans {
            scope: Some(InstrumentationScope {
                name: "opentelemetry-go".to_string(),
                version: "1.21.0".to_string(),
                ..Default::default()
            }),
            spans: (0..span_count).map(make_span).collect(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn make_request_bytes(span_count: usize) -> bytes::Bytes {
    bytes::Bytes::from(
        ExportTraceServiceRequest {
            resource_spans: vec![make_resource_spans(span_count)],
        }
        .encode_to_vec(),
    )
}

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("otlp_traces/decode");
    for span_count in [10usize, 50, 100] {
        let payload = make_request_bytes(span_count);
        group.throughput(Throughput::Elements(span_count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(span_count), &payload, |b, payload| {
            b.iter(|| ExportTraceServiceRequest::decode(payload.clone()).unwrap())
        });
    }
    group.finish();
}

fn bench_translate(c: &mut Criterion) {
    let metrics = Metrics::noop();
    let mut group = c.benchmark_group("otlp_traces/translate");
    for span_count in [10usize, 50, 100] {
        let resource_spans = make_resource_spans(span_count);
        group.throughput(Throughput::Elements(span_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(span_count),
            &resource_spans,
            |b, resource_spans| {
                // Translator is reused across iterations so the string interner warms up,
                // matching production behaviour. Only the input data is cloned per iteration.
                let mut translator =
                    OtlpTracesTranslator::new(TracesConfig::default(), NonZeroUsize::new(512 * 1024).unwrap());
                b.iter_batched(
                    || resource_spans.clone(),
                    |rs| translator.translate_spans(rs, &metrics).count(),
                    BatchSize::SmallInput,
                )
            },
        );
    }
    group.finish();
}

fn bench_decode_and_translate(c: &mut Criterion) {
    let metrics = Metrics::noop();
    let mut group = c.benchmark_group("otlp_traces/decode_and_translate");
    for span_count in [10usize, 50, 100] {
        let payload = make_request_bytes(span_count);
        group.throughput(Throughput::Elements(span_count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(span_count), &payload, |b, payload| {
            let mut translator =
                OtlpTracesTranslator::new(TracesConfig::default(), NonZeroUsize::new(512 * 1024).unwrap());
            b.iter(|| {
                let request = ExportTraceServiceRequest::decode(payload.clone()).unwrap();
                let mut count = 0;
                for rs in request.resource_spans {
                    count += translator.translate_spans(rs, &metrics).count();
                }
                count
            })
        });
    }
    group.finish();
}

// Replicates the async task structure from sources/otlp/mod.rs:
//   N concurrent handler tasks --[mpsc(1024)]--> converter task --[mpsc(256)]--> sink task
// This exercises tokio channel throughput, work-stealing, and task scheduling under concurrent load.
async fn run_async_pipeline(resource_spans: &ResourceSpans, producers: usize, batches: usize) -> usize {
    let (ingest_tx, mut ingest_rx) = mpsc::channel::<ResourceSpans>(1024);
    let (sink_tx, mut sink_rx) = mpsc::channel::<usize>(256);
    let metrics = Metrics::noop();

    // Simulate concurrent gRPC handler tasks each sending a stream of trace batches.
    let mut producer_handles = Vec::new();
    for _ in 0..producers {
        let tx = ingest_tx.clone();
        let rs = resource_spans.clone();
        producer_handles.push(tokio::spawn(async move {
            for _ in 0..batches {
                tx.send(rs.clone()).await.unwrap();
            }
        }));
    }
    drop(ingest_tx);

    // Simulate run_converter: receive batches, translate, push span counts downstream.
    let converter = tokio::spawn(async move {
        let mut translator = OtlpTracesTranslator::new(TracesConfig::default(), NonZeroUsize::new(512 * 1024).unwrap());
        while let Some(rs) = ingest_rx.recv().await {
            let count = translator.translate_spans(rs, &metrics).count();
            sink_tx.send(count).await.unwrap();
        }
    });

    // Simulate downstream component draining the dispatcher output.
    let sink = tokio::spawn(async move {
        let mut total = 0usize;
        while let Some(n) = sink_rx.recv().await {
            total += n;
        }
        total
    });

    for h in producer_handles {
        h.await.unwrap();
    }
    converter.await.unwrap();
    sink.await.unwrap()
}

fn bench_async_pipeline(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("otlp_traces/async_pipeline");
    let batches = 10;
    let span_count = 50;

    for producers in [1usize, 4, 8] {
        let total_spans = producers * batches * span_count;
        let resource_spans = make_resource_spans(span_count);
        group.throughput(Throughput::Elements(total_spans as u64));
        group.bench_function(BenchmarkId::new("producers", producers), |b| {
            b.iter(|| rt.block_on(run_async_pipeline(&resource_spans, producers, batches)))
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_decode,
    bench_translate,
    bench_decode_and_translate,
    bench_async_pipeline
);
criterion_main!(benches);
