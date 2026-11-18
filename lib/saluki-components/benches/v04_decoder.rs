//! Benchmarks for the v0.4 trace decoders, MessagePack and JSON.
//!
//! The payloads mimic a web service's traces: a server span with a few children per chunk, each with the tags a
//! tracer typically sets. Services, names, envs and tag keys repeat across spans, as they do in real payloads. Both
//! formats carry the same spans.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rmp::encode;
use saluki_components::decoders::datadog::{decode_v04_json, decode_v04_payload};

fn write_str_map(out: &mut Vec<u8>, entries: &[(&str, String)]) {
    encode::write_map_len(out, entries.len() as u32).unwrap();
    for (key, value) in entries {
        encode::write_str(out, key).unwrap();
        encode::write_str(out, value).unwrap();
    }
}

/// One span of the benchmark payloads, written as MessagePack or as JSON.
struct BenchSpan {
    service: &'static str,
    name: &'static str,
    resource: String,
    trace_id: u64,
    span_id: u64,
    parent_id: u64,
    start: i64,
    meta: Vec<(&'static str, String)>,
    metrics: &'static [(&'static str, f64)],
    span_type: &'static str,
}

fn bench_span(chunk: u64, index: u64) -> BenchSpan {
    let (service, name, resource) = if index == 0 {
        (
            "checkout-web",
            "http.request",
            format!("GET /api/v1/orders/{}", chunk % 50),
        )
    } else {
        (
            "checkout-db",
            "postgresql.query",
            format!("SELECT id, status, total FROM orders WHERE customer_id = ? AND id = {index}"),
        )
    };
    let mut meta = vec![
        ("env", "production".to_string()),
        ("version", "2024.11.3".to_string()),
        (
            "component",
            if index == 0 { "net/http" } else { "database/sql" }.to_string(),
        ),
        ("span.kind", if index == 0 { "server" } else { "client" }.to_string()),
        ("_dd.p.tid", "6740b3a500000000".to_string()),
        ("_dd.p.dm", "-1".to_string()),
        ("language", "go".to_string()),
        ("runtime-id", "8b9a4f0e-5d5c-4c34-9a8e-0f1d2c3b4a59".to_string()),
    ];
    if index == 0 {
        meta.push(("http.method", "GET".to_string()));
        meta.push((
            "http.url",
            format!("https://shop.example.com/api/v1/orders/{}", chunk % 50),
        ));
        meta.push(("http.status_code", "200".to_string()));
    } else {
        meta.push(("db.system", "postgresql".to_string()));
        meta.push(("db.name", "orders".to_string()));
        meta.push(("out.host", "orders-db.internal".to_string()));
    }
    BenchSpan {
        service,
        name,
        resource,
        trace_id: 0x1000_0000 + chunk,
        span_id: (chunk << 16) + index + 1,
        parent_id: if index == 0 { 0 } else { (chunk << 16) + 1 },
        start: 1_700_000_000_000_000_000 + (index as i64) * 1000,
        meta,
        metrics: if index == 0 {
            &[
                ("_sampling_priority_v1", 1.0),
                ("_dd.top_level", 1.0),
                ("process_id", 4242.0),
            ]
        } else {
            &[("_dd.measured", 1.0), ("db.row_count", 3.0)]
        },
        span_type: if index == 0 { "web" } else { "sql" },
    }
}

fn write_span(out: &mut Vec<u8>, span: &BenchSpan) {
    encode::write_map_len(out, 12).unwrap();
    encode::write_str(out, "service").unwrap();
    encode::write_str(out, span.service).unwrap();
    encode::write_str(out, "name").unwrap();
    encode::write_str(out, span.name).unwrap();
    encode::write_str(out, "resource").unwrap();
    encode::write_str(out, &span.resource).unwrap();
    encode::write_str(out, "trace_id").unwrap();
    encode::write_uint(out, span.trace_id).unwrap();
    encode::write_str(out, "span_id").unwrap();
    encode::write_uint(out, span.span_id).unwrap();
    encode::write_str(out, "parent_id").unwrap();
    encode::write_uint(out, span.parent_id).unwrap();
    encode::write_str(out, "start").unwrap();
    encode::write_sint(out, span.start).unwrap();
    encode::write_str(out, "duration").unwrap();
    encode::write_sint(out, 250_000).unwrap();
    encode::write_str(out, "error").unwrap();
    encode::write_sint(out, 0).unwrap();
    encode::write_str(out, "meta").unwrap();
    write_str_map(out, &span.meta);
    encode::write_str(out, "metrics").unwrap();
    encode::write_map_len(out, span.metrics.len() as u32).unwrap();
    for (key, value) in span.metrics {
        encode::write_str(out, key).unwrap();
        encode::write_f64(out, *value).unwrap();
    }
    encode::write_str(out, "type").unwrap();
    encode::write_str(out, span.span_type).unwrap();
}

/// The same span as a tracer's JSON encoder writes it.
fn json_span(span: &BenchSpan) -> serde_json::Value {
    serde_json::json!({
        "service": span.service,
        "name": span.name,
        "resource": span.resource,
        "trace_id": span.trace_id,
        "span_id": span.span_id,
        "parent_id": span.parent_id,
        "start": span.start,
        "duration": 250_000,
        "error": 0,
        "meta": span.meta.iter().map(|(key, value)| (key.to_string(), serde_json::json!(value))).collect::<serde_json::Map<_, _>>(),
        "metrics": span.metrics.iter().map(|(key, value)| (key.to_string(), serde_json::json!(value))).collect::<serde_json::Map<_, _>>(),
        "type": span.span_type,
    })
}

fn payload(chunks: u64, spans_per_chunk: u64) -> Vec<u8> {
    let mut out = Vec::new();
    encode::write_array_len(&mut out, chunks as u32).unwrap();
    for chunk in 0..chunks {
        encode::write_array_len(&mut out, spans_per_chunk as u32).unwrap();
        for index in 0..spans_per_chunk {
            write_span(&mut out, &bench_span(chunk, index));
        }
    }
    out
}

fn json_payload(chunks: u64, spans_per_chunk: u64) -> Vec<u8> {
    let traces: Vec<Vec<serde_json::Value>> = (0..chunks)
        .map(|chunk| {
            (0..spans_per_chunk)
                .map(|index| json_span(&bench_span(chunk, index)))
                .collect()
        })
        .collect();
    serde_json::to_vec(&traces).unwrap()
}

const SHAPES: [(u64, u64); 3] = [(1, 5), (20, 5), (100, 10)];

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("v04_decoder/decode");
    for (chunks, spans_per_chunk) in SHAPES {
        let bytes = payload(chunks, spans_per_chunk);
        group.throughput(Throughput::Elements(chunks * spans_per_chunk));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{chunks}x{spans_per_chunk}")),
            &bytes,
            |b, bytes| b.iter(|| decode_v04_payload(bytes).expect("payload should decode")),
        );
    }
    group.finish();
}

fn bench_decode_json(c: &mut Criterion) {
    let mut group = c.benchmark_group("v04_decoder/decode_json");
    for (chunks, spans_per_chunk) in SHAPES {
        let bytes = json_payload(chunks, spans_per_chunk);
        group.throughput(Throughput::Elements(chunks * spans_per_chunk));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{chunks}x{spans_per_chunk}")),
            &bytes,
            |b, bytes| b.iter(|| decode_v04_json(bytes).expect("payload should decode")),
        );
    }
    group.finish();
}

criterion_group!(benches, bench_decode, bench_decode_json);
criterion_main!(benches);
