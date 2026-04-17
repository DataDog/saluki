use std::num::NonZeroUsize;

use async_trait::async_trait;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use otlp_protos::opentelemetry::proto::{
    collector::trace::v1::{
        trace_service_client::TraceServiceClient,
        trace_service_server::{TraceService, TraceServiceServer},
        ExportTraceServiceRequest, ExportTraceServiceResponse,
    },
    common::v1::{any_value::Value, AnyValue, InstrumentationScope, KeyValue},
    resource::v1::Resource,
    trace::v1::{span::SpanKind, status::StatusCode, ResourceSpans, ScopeSpans, Span, Status},
};
use prost::Message;
use saluki_components::common::otlp::{config::TracesConfig, traces::translator::OtlpTracesTranslator, Metrics};
use tokio::sync::mpsc;
use tonic::{transport::Server, Request, Response, Status as TonicStatus};

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

// Minimal gRPC TraceService that mirrors the real source handler:
//   tonic decode → encode_to_vec → prost decode → MPSC send → response
// This is the same re-encode/re-decode round-trip the real OtlpSource does.
struct BenchTraceHandler {
    tx: mpsc::Sender<ResourceSpans>,
}

#[async_trait]
impl TraceService for BenchTraceHandler {
    async fn export(
        &self, request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, TonicStatus> {
        // Mirror sources/otlp/mod.rs: re-encode then re-decode to raw bytes,
        // then send each ResourceSpans through the MPSC channel.
        let raw = request.into_inner().encode_to_vec();
        let decoded =
            ExportTraceServiceRequest::decode(raw.as_slice()).map_err(|e| TonicStatus::internal(e.to_string()))?;
        for rs in decoded.resource_spans {
            self.tx
                .send(rs)
                .await
                .map_err(|_| TonicStatus::internal("channel closed"))?;
        }
        Ok(Response::new(ExportTraceServiceResponse { partial_success: None }))
    }
}

fn bench_grpc_ingest(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    let span_count = 50;
    let requests_per_iter = 100;
    let request_payload = ExportTraceServiceRequest {
        resource_spans: vec![make_resource_spans(span_count)],
    };

    // Bind to a random loopback port, start the gRPC server.
    let (addr, rx) = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel::<ResourceSpans>(1024);
        let incoming = tonic::transport::server::TcpIncoming::from(listener);
        tokio::spawn(
            Server::builder()
                .add_service(TraceServiceServer::new(BenchTraceHandler { tx }))
                .serve_with_incoming(incoming),
        );
        (addr, rx)
    });

    // Drain received ResourceSpans and translate them — keeps the ingest channel
    // from filling up and mirrors the converter task in the real pipeline.
    rt.spawn(async move {
        let metrics = Metrics::noop();
        let mut translator = OtlpTracesTranslator::new(TracesConfig::default(), NonZeroUsize::new(512 * 1024).unwrap());
        let mut rx = rx;
        while let Some(rs) = rx.recv().await {
            let _ = translator.translate_spans(rs, &metrics).count();
        }
    });

    // Connect a single client — matches `parallel_connections: 1` in the SMP config.
    let channel = rt
        .block_on(
            tonic::transport::Channel::from_shared(format!("http://{}", addr))
                .unwrap()
                .connect(),
        )
        .unwrap();
    let mut client: TraceServiceClient<tonic::transport::Channel> = TraceServiceClient::new(channel);

    let total_spans = requests_per_iter * span_count;
    let mut group = c.benchmark_group("otlp_traces/grpc_ingest");
    group.throughput(Throughput::Elements(total_spans as u64));

    // Sequential requests over one connection — closest to Lading's single-connection model.
    group.bench_function("sequential", |b| {
        b.iter(|| {
            rt.block_on(async {
                for _ in 0..requests_per_iter {
                    client.export(Request::new(request_payload.clone())).await.unwrap();
                }
            })
        })
    });

    // Concurrent requests multiplexed over the same HTTP/2 connection.
    group.bench_function("concurrent", |b| {
        b.iter(|| {
            rt.block_on(async {
                let futs: Vec<_> = (0..requests_per_iter)
                    .map(|_| {
                        let mut c = client.clone();
                        let req = request_payload.clone();
                        async move { c.export(Request::new(req)).await.unwrap() }
                    })
                    .collect();
                futures::future::join_all(futs).await;
            })
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_decode,
    bench_translate,
    bench_decode_and_translate,
    bench_async_pipeline,
    bench_grpc_ingest
);
criterion_main!(benches);
