//! Benchmarks for the v0.4 trace decoder.
//!
//! The payloads mimic a web service's traces: a server span with a few children per chunk, each with the tags a
//! tracer typically sets. Services, names, envs and tag keys repeat across spans, as they do in real payloads.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rmp::encode;
use saluki_components::decoders::datadog::decode_v04_payload;

fn write_str_map(out: &mut Vec<u8>, entries: &[(&str, String)]) {
    encode::write_map_len(out, entries.len() as u32).unwrap();
    for (key, value) in entries {
        encode::write_str(out, key).unwrap();
        encode::write_str(out, value).unwrap();
    }
}

fn write_span(out: &mut Vec<u8>, chunk: u64, index: u64) {
    let trace_id = 0x1000_0000 + chunk;
    let span_id = (chunk << 16) + index + 1;
    let parent_id = if index == 0 { 0 } else { (chunk << 16) + 1 };
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

    encode::write_map_len(out, 12).unwrap();
    encode::write_str(out, "service").unwrap();
    encode::write_str(out, service).unwrap();
    encode::write_str(out, "name").unwrap();
    encode::write_str(out, name).unwrap();
    encode::write_str(out, "resource").unwrap();
    encode::write_str(out, &resource).unwrap();
    encode::write_str(out, "trace_id").unwrap();
    encode::write_uint(out, trace_id).unwrap();
    encode::write_str(out, "span_id").unwrap();
    encode::write_uint(out, span_id).unwrap();
    encode::write_str(out, "parent_id").unwrap();
    encode::write_uint(out, parent_id).unwrap();
    encode::write_str(out, "start").unwrap();
    encode::write_sint(out, 1_700_000_000_000_000_000 + (index as i64) * 1000).unwrap();
    encode::write_str(out, "duration").unwrap();
    encode::write_sint(out, 250_000).unwrap();
    encode::write_str(out, "error").unwrap();
    encode::write_sint(out, 0).unwrap();
    encode::write_str(out, "meta").unwrap();
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
    write_str_map(out, &meta);
    encode::write_str(out, "metrics").unwrap();
    let metrics: &[(&str, f64)] = if index == 0 {
        &[
            ("_sampling_priority_v1", 1.0),
            ("_dd.top_level", 1.0),
            ("process_id", 4242.0),
        ]
    } else {
        &[("_dd.measured", 1.0), ("db.row_count", 3.0)]
    };
    encode::write_map_len(out, metrics.len() as u32).unwrap();
    for (key, value) in metrics {
        encode::write_str(out, key).unwrap();
        encode::write_f64(out, *value).unwrap();
    }
    encode::write_str(out, "type").unwrap();
    encode::write_str(out, if index == 0 { "web" } else { "sql" }).unwrap();
}

fn payload(chunks: u64, spans_per_chunk: u64) -> Vec<u8> {
    let mut out = Vec::new();
    encode::write_array_len(&mut out, chunks as u32).unwrap();
    for chunk in 0..chunks {
        encode::write_array_len(&mut out, spans_per_chunk as u32).unwrap();
        for index in 0..spans_per_chunk {
            write_span(&mut out, chunk, index);
        }
    }
    out
}

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("v04_decoder/decode");
    for (chunks, spans_per_chunk) in [(1, 5), (20, 5), (100, 10)] {
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

criterion_group!(benches, bench_decode);
criterion_main!(benches);
