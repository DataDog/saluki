//! Tests for the v0.7 decoder.
//!
//! The primary test is a table-driven run over `testdata/v07_cases.json`, whose expected values are what the
//! trace-agent's receiver decodes from each body (see `test/trace-agent-oracle/`). The others bound what a decode
//! allocates, and check that no input panics.

use proptest::prelude::*;
use saluki_core::data_model::event::trace::Span;

use super::{decode_v07_payload, SpanMessage};
use crate::decoders::datadog::json::tests::check_fixture;
use crate::decoders::datadog::read;
use crate::decoders::datadog::v04::tests::{alloc_tracking, encode_value, s, V};

const FIXTURE_CASES: &str = include_str!("../testdata/v07_cases.json");

#[test]
fn fixture_cases_decode_as_the_reference_decodes_them() {
    check_fixture(FIXTURE_CASES, |_, body| decode_v07_payload(body), false);
}

fn obj(fields: Vec<(&str, V)>) -> V {
    V::Map(fields.into_iter().map(|(key, value)| (s(key), value)).collect())
}

fn payload(fields: Vec<(&str, V)>, chunks: Vec<V>) -> Vec<u8> {
    let mut fields = fields;
    fields.push(("chunks", V::Arr(chunks)));
    encode_value(&obj(fields))
}

fn chunk(spans: Vec<V>) -> V {
    obj(vec![("spans", V::Arr(spans))])
}

#[test]
fn payload_fields_are_shared_by_every_trace() {
    let hostname = "host-".repeat(20);
    let spans = |id| vec![obj(vec![("trace_id", V::Uint(id)), ("span_id", V::Uint(id))])];
    let bytes = payload(vec![("hostname", s(&hostname))], vec![chunk(spans(1)), chunk(spans(2))]);

    let decoded = decode_v07_payload(&bytes).unwrap();
    assert_eq!(decoded.traces.len(), 2);
    assert_eq!(decoded.traces[1].payload.hostname, hostname.as_str());
    assert_eq!(
        decoded.traces[0].payload.hostname.as_ptr(),
        decoded.traces[1].payload.hostname.as_ptr()
    );
}

#[test]
fn small_inputs_with_huge_counts_allocate_little() {
    let array = |n: u32| [&[0xdd][..], &n.to_be_bytes()].concat();
    let map = |n: u32| [&[0xdf][..], &n.to_be_bytes()].concat();
    let limit = read::MSGP_MAX_COLLECTION_SIZE;
    let prefix = |key: &str| {
        let mut out = vec![0x81];
        out.extend(encode_value(&s(key)));
        out
    };
    let in_span = |field: &str| {
        [
            &prefix("chunks")[..],
            &[0x91],
            &prefix("spans")[..],
            &[0x91],
            &prefix(field)[..],
        ]
        .concat()
    };

    for (name, bytes) in [
        ("chunks", [&prefix("chunks")[..], &array(limit)].concat()),
        ("payload tags", [&prefix("tags")[..], &map(limit)].concat()),
        ("payload fields", map(limit)),
        (
            "spans",
            [&prefix("chunks")[..], &[0x91], &prefix("spans")[..], &array(limit)].concat(),
        ),
        ("meta", [&in_span("meta")[..], &map(limit)].concat()),
        ("meta_struct", [&in_span("meta_struct")[..], &map(10 << 20)].concat()),
        (
            "meta_struct value",
            [
                &in_span("meta_struct")[..],
                &[0x81, 0xa1, b'k', 0xc6],
                &(10u32 << 20).to_be_bytes(),
            ]
            .concat(),
        ),
        ("span links", [&in_span("span_links")[..], &array(limit)].concat()),
        ("span events", [&in_span("span_events")[..], &array(limit)].concat()),
    ] {
        let (result, peak) = alloc_tracking::peak_during(|| decode_v07_payload(&bytes));
        assert!(result.is_err(), "{name}");
        assert!(peak < 1 << 20, "{name}: peak {peak} bytes for {} bytes", bytes.len());
    }
}

#[test]
fn nil_and_empty_elements_allocate_in_proportion_to_their_bytes() {
    // 100,000 spans of one byte each: `nil`, which costs one pointer until the payload is decoded, and an empty map,
    // which costs a span as the reference's `new(Span)` does.
    const SPANS: usize = 100_000;
    for (name, element, per_element) in [
        ("nil", 0xc0, 2 * std::mem::size_of::<usize>()),
        (
            "empty",
            0x80,
            std::mem::size_of::<SpanMessage>() + std::mem::size_of::<Span>() + 2 * std::mem::size_of::<usize>(),
        ),
    ] {
        let mut bytes = vec![0x81];
        bytes.extend(encode_value(&s("chunks")));
        bytes.push(0x91);
        bytes.push(0x81);
        bytes.extend(encode_value(&s("spans")));
        bytes.push(0xdd);
        bytes.extend_from_slice(&(SPANS as u32).to_be_bytes());
        bytes.extend(std::iter::repeat_n(element, SPANS));

        let (result, peak) = alloc_tracking::peak_during(|| decode_v07_payload(&bytes));
        let decoded = result.unwrap();
        assert_eq!(
            decoded.traces.iter().map(|trace| trace.spans().len()).sum::<usize>(),
            if element == 0xc0 { 0 } else { SPANS }
        );
        let bound = SPANS * per_element + (1 << 20);
        assert!(
            peak <= bound,
            "{name}: peak {peak} bytes for {} bytes (bound {bound})",
            bytes.len()
        );
    }
}

fn span_strategy() -> impl Strategy<Value = V> {
    (
        "[a-z._]{0,20}",
        any::<u64>(),
        any::<u64>(),
        prop::collection::vec(("[a-z._]{1,12}", "[a-z0-9]{0,12}"), 0..4),
        prop::collection::vec(("[a-z._]{1,12}", any::<f64>()), 0..3),
        0usize..3,
        0usize..3,
    )
        .prop_map(|(service, trace_id, span_id, meta, metrics, links, events)| {
            obj(vec![
                ("service", s(&service)),
                ("name", s("op")),
                ("trace_id", V::Uint(trace_id)),
                ("span_id", V::Uint(span_id)),
                ("start", V::Int(1)),
                ("duration", V::Int(2)),
                (
                    "meta",
                    V::Map(meta.iter().map(|(key, value)| (s(key), s(value))).collect()),
                ),
                (
                    "metrics",
                    V::Map(metrics.iter().map(|(key, value)| (s(key), V::F64(*value))).collect()),
                ),
                (
                    "span_links",
                    V::Arr(
                        (0..links)
                            .map(|i| obj(vec![("trace_id", V::Uint(i as u64)), ("tracestate", s("dd=s:1"))]))
                            .collect(),
                    ),
                ),
                (
                    "span_events",
                    V::Arr(
                        (0..events)
                            .map(|i| {
                                obj(vec![
                                    ("time_unix_nano", V::Uint(i as u64)),
                                    ("name", s("event")),
                                    (
                                        "attributes",
                                        obj(vec![("k", obj(vec![("type", V::Int(2)), ("int_value", V::Int(1))]))]),
                                    ),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ])
        })
}

proptest! {
    #[test]
    fn property_test_arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
        // Any outcome is fine as long as decoding returns. Each decoded chunk takes at least one byte.
        if let Ok(decoded) = decode_v07_payload(&bytes) {
            prop_assert!(decoded.traces.len() <= bytes.len());
        }
    }

    /// Mutated and truncated payloads decode or fail, without panicking.
    ///
    /// The allocation bound checked here is a loose sanity ceiling, not a regression guard:
    /// `small_inputs_with_huge_counts_allocate_little` and `nil_and_empty_elements_allocate_in_proportion_to_their_bytes`
    /// guard the allocation behavior.
    #[test]
    fn property_test_mutated_full_payloads_never_panic(
        chunks in prop::collection::vec(prop::collection::vec(span_strategy(), 0..4), 0..4),
        truncate_at in any::<prop::sample::Index>(),
        truncate in any::<bool>(),
        flips in prop::collection::vec((any::<prop::sample::Index>(), 1u8..=255), 0..6),
    ) {
        let fields = vec![("env", s("prod")), ("language_name", s("python")), ("tags", obj(vec![("k", s("v"))]))];
        let bytes = payload(fields, chunks.iter().cloned().map(chunk).collect());
        let decoded = decode_v07_payload(&bytes);
        prop_assert!(decoded.is_ok(), "the unmutated payload must decode: {:?}", decoded.err());
        let decoded = decoded.unwrap();
        // Chunks without spans are dropped.
        let kept: Vec<_> = chunks.iter().filter(|spans| !spans.is_empty()).collect();
        prop_assert_eq!(decoded.traces.len(), kept.len());
        for (trace, spans) in decoded.traces.iter().zip(kept) {
            prop_assert_eq!(trace.spans().len(), spans.len());
            prop_assert_eq!(trace.payload.language_name.as_ref(), "python");
        }

        let mut mutated = bytes;
        for (index, mask) in &flips {
            let i = index.index(mutated.len());
            mutated[i] ^= mask;
        }
        if truncate {
            mutated.truncate(truncate_at.index(mutated.len() + 1));
        }
        // Any outcome is fine. The peak is only checked against a loose ceiling, to catch a runaway allocation.
        let (_, peak) = alloc_tracking::peak_during(|| decode_v07_payload(&mutated));
        let bound = 2 * read::MAX_PREALLOCATED_ELEMENTS * std::mem::size_of::<SpanMessage>() + 2048 * mutated.len();
        prop_assert!(peak <= bound, "peak {} bytes for {} bytes (bound {})", peak, mutated.len(), bound);
    }
}
