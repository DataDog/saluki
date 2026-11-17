//! Tests for the v0.5 decoder.
//!
//! The primary test is a table-driven run over `testdata/v05_cases.json`, whose expected values are what the
//! trace-agent's receiver decodes from each body (see `test/trace-agent-oracle/`). The other tests port the
//! reference's tests of `UnmarshalMsgDictionary` (`pkg/proto/pbgo/trace/idx/span_v05_test.go` and
//! `pkg/proto/pbgo/trace/decoder_v05_test.go`), and bound what a decode allocates.

use proptest::prelude::*;
use saluki_core::data_model::event::trace::{AttributeValue, Span};

use super::{decode_v05_payload, Dictionary};
use crate::decoders::datadog::json::tests::check_fixture;
use crate::decoders::datadog::v04::tests::{alloc_tracking, encode_value, s, string_attr, V};
use crate::decoders::datadog::{read, DecodeError, DecodedV1};

const FIXTURE_CASES: &str = include_str!("../testdata/v05_cases.json");

/// A span with the given string references, IDs, `meta` and `metrics`.
fn span(refs: [u64; 4], ids: [u64; 3], meta: &[(u64, u64)], metrics: &[(u64, f64)]) -> V {
    let [service, name, resource, span_type] = refs;
    let [trace_id, span_id, parent_id] = ids;
    V::Arr(vec![
        V::Uint(service),
        V::Uint(name),
        V::Uint(resource),
        V::Uint(trace_id),
        V::Uint(span_id),
        V::Uint(parent_id),
        V::Int(123),
        V::Int(456),
        V::Int(1),
        V::Map(meta.iter().map(|&(k, v)| (V::Uint(k), V::Uint(v))).collect()),
        V::Map(metrics.iter().map(|&(k, v)| (V::Uint(k), V::F64(v))).collect()),
        V::Uint(span_type),
    ])
}

fn payload(dictionary: &[&str], chunks: Vec<Vec<V>>) -> Vec<u8> {
    encode_value(&V::Arr(vec![
        V::Arr(dictionary.iter().map(|value| s(value)).collect()),
        V::Arr(chunks.into_iter().map(V::Arr).collect()),
    ]))
}

fn decode(bytes: &[u8]) -> DecodedV1 {
    decode_v05_payload(bytes).expect("payload should decode")
}

#[test]
fn fixture_cases_decode_as_the_reference_decodes_them() {
    check_fixture(FIXTURE_CASES, |_, body| decode_v05_payload(body), false);
}

const REFERENCE_DICTIONARY: &[&str] = &[
    "baggage",
    "item",
    "elasticsearch.version",
    "7.0",
    "my-name",
    "X",
    "my-service",
    "my-resource",
    "_dd.sampling_rate_whatever",
    "value whatever",
    "sql",
    "env",
    "some-env",
];

#[test]
fn reference_span_decodes() {
    // TestUnmarshalMsgDictionary.
    let bytes = payload(
        REFERENCE_DICTIONARY,
        vec![vec![span(
            [6, 4, 7, 10],
            [1, 2, 3],
            &[(8, 9), (0, 1), (2, 3), (11, 12)],
            &[(5, 1.2)],
        )]],
    );
    let decoded = decode(&bytes);
    assert_eq!(decoded.traces.len(), 1);
    let trace = &decoded.traces[0];
    let span = &trace.spans()[0];
    assert_eq!(span.service(), "my-service");
    assert_eq!(span.name(), "my-name");
    assert_eq!(span.resource(), "my-resource");
    assert_eq!(span.span_type(), "sql");
    assert_eq!((span.span_id(), span.parent_id()), (2, 3));
    assert_eq!((span.start(), span.duration(), span.error()), (123, 456, 1));
    assert_eq!(span.attributes.get("baggage"), Some(&string_attr("item")));
    assert_eq!(span.attributes.get("elasticsearch.version"), Some(&string_attr("7.0")));
    assert_eq!(
        span.attributes.get("_dd.sampling_rate_whatever"),
        Some(&string_attr("value whatever"))
    );
    assert_eq!(span.attributes.get("X"), Some(&AttributeValue::Float(1.2)));
    assert_eq!(span.attributes.get("_dd.convertedv1"), Some(&string_attr("v05")));
    assert_eq!((trace.trace_id_high, trace.trace_id_low), (0, 1));
    assert_eq!(trace.payload.env, "some-env");
}

#[test]
fn dictionary_is_rebuilt_as_the_reference_string_table() {
    // TestBuildStringTable: the table, and where the first string moved to.
    let cases: &[(&[&str], &[&str], u32)] = &[
        (&[], &[], 0),
        (
            &["", "service", "name", "resource"],
            &["", "service", "name", "resource"],
            0,
        ),
        (
            &["service", "name", "", "resource"],
            &["", "name", "service", "resource"],
            2,
        ),
        (
            &["service", "name", "resource", ""],
            &["", "name", "resource", "service"],
            3,
        ),
        (
            &["service", "name", "resource"],
            &["", "name", "resource", "service"],
            3,
        ),
        (&["service"], &["", "service"], 1),
        (&[""], &[""], 0),
        (&["service", "", "name", ""], &["", "service", "name", ""], 1),
    ];
    for (input, table, zero_ref) in cases {
        let bytes = encode_value(&V::Arr(input.iter().map(|value| s(value)).collect()));
        let dictionary = Dictionary::read(&mut bytes.as_slice()).unwrap();
        let strings: Vec<&str> = dictionary.strings.iter().map(|value| value.as_ref()).collect();
        assert_eq!(&strings, table, "{input:?}");
        assert_eq!(dictionary.zero_ref, *zero_ref, "{input:?}");
        assert_eq!(dictionary.wire_len as usize, input.len(), "{input:?}");
    }

    // A reference to 0 reads the moved string, and so does a reference to where the empty string was.
    let bytes = payload(
        &["service", "name", "", "resource"],
        vec![vec![span([0, 2, 1, 3], [0; 3], &[], &[])]],
    );
    let decoded = decode(&bytes);
    let span = &decoded.traces[0].spans()[0];
    assert_eq!(
        (span.service(), span.name(), span.resource(), span.span_type()),
        ("service", "service", "name", "resource")
    );
}

#[test]
fn reference_size_limits_and_string_refs_are_enforced() {
    // TestUnmarshalMsgDictionaryLimitsSize: Go's "too long payload".
    let limited: [&[u8]; 4] = [
        b"\x92\xdd\xff\xff\xff\xff",
        b"\x96\x97\xa40000\xa6000000\xa6000000\xa6000000\xa6000000\xa6000000\xa6000000\x96\x94\x9c\x00\x00\x0000\xd100000\xdf0000",
        b"\x90\x90\xddJ\xdc\x00A",
        b"\x9b\x91\xa20c\xdd\xe12]\x08\xf60\x9a\x9a\x9a",
    ];
    for bytes in limited {
        let error = decode_v05_payload(bytes).unwrap_err();
        assert!(
            matches!(error, DecodeError::OversizeHeader { .. }),
            "{bytes:?}: {error:?}"
        );
    }

    // TestUnmarshalMsgDictionaryRejectsInvalidStringRef.
    let error = decode_v05_payload(b"\x91\x90\x91\x91\x9c\x0000000000\x80\x800").unwrap_err();
    assert!(
        matches!(error, DecodeError::DictionaryIndexOutOfRange { index: 0 }),
        "{error:?}"
    );
    assert_eq!(error.to_string(), "dictionary index 0 out of range");
}

#[test]
fn small_inputs_with_huge_counts_allocate_little() {
    // TestUnmarshalMsgDictionaryBoundsAllocationsByInput, with its 1 MiB ceiling.
    const HUGE: u32 = 25_000_000;
    const MAX_COLLECTION: u32 = read::V0_MAX_COLLECTION_SIZE;
    let array = |n: u32| {
        let mut out = vec![0xdd];
        out.extend_from_slice(&n.to_be_bytes());
        out
    };
    let dictionary = [&[0x92][..], &array(HUGE)].concat();
    let traces = [&[0x92, 0x90][..], &array(MAX_COLLECTION)].concat();
    let spans = [&[0x92, 0x90, 0x91][..], &array(MAX_COLLECTION)].concat();
    // A 12-element span with references and zeros up to `meta`, whose map declares more entries than remain.
    let mut meta = vec![0x92, 0x91, 0xa0, 0x91, 0x91, 0x9c];
    meta.extend_from_slice(&[0x00; 9]);
    meta.push(0xdf);
    meta.extend_from_slice(&MAX_COLLECTION.to_be_bytes());
    // One byte per declared entry passes the header check but not the two-values-per-entry check.
    let half_backed = [&meta[..], &vec![0xc0; MAX_COLLECTION as usize]].concat();

    for (name, bytes) in [
        ("dictionary", dictionary),
        ("traces", traces),
        ("spans", spans),
        ("meta", meta),
        ("meta half backed", half_backed),
    ] {
        let (result, peak) = alloc_tracking::peak_during(|| decode_v05_payload(&bytes));
        assert!(result.is_err(), "{name}");
        assert!(peak < 1 << 20, "{name}: peak {peak} bytes for {} bytes", bytes.len());
    }
}

#[test]
fn one_large_string_referenced_from_many_spans_is_held_once() {
    // A 256 KiB string, referenced by every string field and a meta entry of 1,000 spans: about 20 KB of references
    // that must not become 6,000 copies of the string.
    const SPANS: u64 = 1000;
    let large = "x".repeat(256 << 10);
    let refs = [1, 1, 1, 1];
    let chunks = vec![(0..SPANS).map(|i| span(refs, [1, i + 1, 0], &[(1, 1)], &[])).collect()];
    let bytes = payload(&["", &large], chunks);
    assert!(bytes.len() < large.len() + 32 * 1024);

    let (decoded, peak) = alloc_tracking::peak_during(|| decode(&bytes));
    let spans = decoded.traces[0].spans();
    assert_eq!(spans.len(), SPANS as usize);
    let first = spans[0].service().as_ptr();
    for span in spans {
        for value in [span.service(), span.name(), span.resource(), span.span_type()] {
            assert_eq!(value.as_ptr(), first);
        }
        let (key, value) = span
            .attributes
            .iter()
            .find(|(key, _)| key.len() == large.len())
            .expect("the meta entry");
        let AttributeValue::String(value) = value else {
            panic!("a string meta value");
        };
        assert_eq!((key.as_ptr(), value.as_ptr()), (first, first));
    }
    // The string once, plus each span with its two attributes.
    let bound = large.len() + SPANS as usize * (std::mem::size_of::<Span>() + 512);
    assert!(peak <= bound, "peak {peak} bytes (bound {bound})");
}

#[test]
fn short_input_is_distinguishable() {
    let full = payload(
        REFERENCE_DICTIONARY,
        vec![vec![span(
            [6, 4, 7, 10],
            [1, 2, 3],
            &[(8, 9), (0, 1), (2, 3), (11, 12)],
            &[(5, 1.2)],
        )]],
    );
    let cases: &[(&str, &[u8])] = &[
        ("empty payload", &[]),
        ("outer array declares more elements than bytes remain", &[0x92, 0x90]),
        ("truncated dictionary string", &[0x92, 0x91, 0xa5, b'a', b'b']),
        (
            "trace list declares more chunks than bytes remain",
            &[0x92, 0x91, 0xa0, 0x93, 0x90],
        ),
        (
            "truncated string reference",
            &[0x92, 0x91, 0xa0, 0x91, 0x91, 0x9c, 0xcd, 0x00],
        ),
        ("truncated full payload", &full[..full.len() - 3]),
    ];
    for (name, bytes) in cases {
        let error = decode_v05_payload(bytes).unwrap_err();
        assert!(matches!(error, DecodeError::ShortBytes { .. }), "{name}: {error:?}");
    }

    // Every strict prefix of a valid payload fails to decode.
    for len in 0..full.len() {
        assert!(decode_v05_payload(&full[..len]).is_err(), "prefix of {len} bytes");
    }
}

#[test]
fn only_the_first_span_of_the_payload_is_tagged_converted() {
    let bytes = payload(
        &["", "s"],
        vec![
            vec![],
            vec![span([1; 4], [1, 1, 0], &[], &[]), span([1; 4], [1, 2, 0], &[], &[])],
            vec![span([1; 4], [2, 3, 0], &[], &[])],
        ],
    );
    let decoded = decode(&bytes);
    let tagged: Vec<bool> = decoded
        .traces
        .iter()
        .flat_map(|trace| trace.spans())
        .map(|span| span.attributes.contains_key("_dd.convertedv1"))
        .collect();
    assert_eq!(tagged, [true, false, false]);
    // A span without a `span.kind` is unspecified, not internal as on v0.4.
    assert_eq!(decoded.traces[1].spans()[0].kind, 0);
}

fn span_strategy(dictionary_len: u64) -> impl Strategy<Value = V> {
    let reference = 0..dictionary_len;
    (
        prop::array::uniform4(reference.clone()),
        prop::array::uniform3(any::<u64>()),
        prop::collection::vec((reference.clone(), reference.clone()), 0..4),
        prop::collection::vec((reference, any::<f64>()), 0..4),
    )
        .prop_map(|(refs, ids, meta, metrics)| span(refs, ids, &meta, &metrics))
}

fn payload_strategy() -> impl Strategy<Value = (Vec<String>, Vec<Vec<V>>)> {
    prop::collection::vec("[a-z._]{0,40}", 1..12).prop_flat_map(|dictionary| {
        let spans = span_strategy(dictionary.len() as u64);
        (
            Just(dictionary),
            prop::collection::vec(prop::collection::vec(spans, 0..4), 0..4),
        )
    })
}

proptest! {
    #[test]
    fn property_test_arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
        // Any outcome is fine as long as decoding returns. Each decoded chunk takes at least one byte.
        if let Ok(decoded) = decode_v05_payload(&bytes) {
            prop_assert!(decoded.traces.len() <= bytes.len());
        }
    }

    /// Mutated and truncated payloads decode or fail, without panicking.
    ///
    /// The allocation bound checked here is a loose sanity ceiling, not a regression guard:
    /// `small_inputs_with_huge_counts_allocate_little` and `one_large_string_referenced_from_many_spans_is_held_once`
    /// guard the allocation behavior.
    #[test]
    fn property_test_mutated_full_payloads_never_panic(
        (dictionary, chunks) in payload_strategy(),
        truncate_at in any::<prop::sample::Index>(),
        truncate in any::<bool>(),
        flips in prop::collection::vec((any::<prop::sample::Index>(), 1u8..=255), 0..6),
    ) {
        let dictionary: Vec<&str> = dictionary.iter().map(String::as_str).collect();
        let bytes = payload(&dictionary, chunks.clone());
        let decoded = decode_v05_payload(&bytes);
        prop_assert!(decoded.is_ok(), "the unmutated payload must decode: {:?}", decoded.err());
        let decoded = decoded.unwrap();
        prop_assert_eq!(decoded.traces.len(), chunks.len());
        for (trace, chunk) in decoded.traces.iter().zip(&chunks) {
            prop_assert_eq!(trace.spans().len(), chunk.len());
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
        let (_, peak) = alloc_tracking::peak_during(|| decode_v05_payload(&mutated));
        let bound = 2 * read::MAX_PREALLOCATED_ELEMENTS * std::mem::size_of::<Span>() + 2048 * mutated.len();
        prop_assert!(peak <= bound, "peak {} bytes for {} bytes (bound {})", peak, mutated.len(), bound);
    }
}
