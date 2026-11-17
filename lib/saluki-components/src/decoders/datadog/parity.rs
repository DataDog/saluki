//! Go/Rust parity fuzzing of the trace decoders.
//!
//! Each test generates payloads for one decoder, decodes each with the decoder and with the trace-agent's own receiver
//! code, and fails on the first payload whose results differ, shrunk to a minimal one. The trace-agent side is the
//! oracle of `test/trace-agent-oracle/`, built as a Go test binary and driven over its stdin and stdout
//! (`ORACLE_STREAM`). The payloads are the fixtures' bodies and payloads built by the decoders' property-test
//! strategies, either as they are or mutated: bytes flipped, overwritten, inserted or removed, and the end cut off.
//!
//! The tests are ignored, as they need the oracle: `test/trace-agent-oracle/fuzz.sh` builds it and runs them. See
//! docs/development/testing.md.

use std::cell::RefCell;
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use base64::{engine::general_purpose, Engine as _};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestRng, TestRunner};
use serde_json::{json, Value};

use super::json::tests::{classify, dump, json_strategy, parse_doubles, sort_chunks};
use super::v04::tests::{encode_value, span_strategy as v04_span_strategy, V};
use super::{
    decode_v01_json, decode_v04_json, decode_v04_payload, decode_v05_payload, decode_v07_payload, decode_v1_payload,
    DecodeError, DecodedV1,
};

/// A running oracle.
struct Oracle {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Oracle {
    /// Starts the oracle binary that `TRACE_AGENT_ORACLE` names.
    fn start() -> Self {
        let binary = std::env::var("TRACE_AGENT_ORACLE")
            .expect("TRACE_AGENT_ORACLE must name the oracle test binary: run test/trace-agent-oracle/fuzz.sh");
        let mut child = Command::new(binary)
            .args(["-test.run", "^TestOracle$"])
            .env("ORACLE_STREAM", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the oracle starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout }
    }

    /// Decodes `body` as the trace-agent decodes a request to the endpoint of `version` with `content_type`.
    fn decode(&mut self, version: &str, content_type: Option<&str>, body: &[u8]) -> Value {
        let case = json!({
            "name": "fuzz",
            "version": version,
            "content_type": content_type,
            "body_b64": general_purpose::STANDARD.encode(body),
        });
        writeln!(self.stdin, "{case}").expect("the oracle reads its input");
        self.stdin.flush().unwrap();
        let mut line = String::new();
        loop {
            line.clear();
            assert!(self.stdout.read_line(&mut line).unwrap() > 0, "the oracle exited");
            if let Some(result) = line.strip_prefix("ORACLE ") {
                return serde_json::from_str(result).unwrap();
            }
        }
    }
}

impl Drop for Oracle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One decoder under test, and how to reach the same code in the trace-agent.
struct Target {
    version: &'static str,
    content_type: Option<&'static str>,
    decode: fn(&[u8]) -> Result<DecodedV1, DecodeError>,
    /// The trace-agent groups v0.1 spans into chunks through a map, so their order is random.
    any_chunk_order: bool,
}

/// Compares the decoder's result with the oracle's.
fn compare(target: &Target, body: &[u8], oracle: &Value) -> Result<(), String> {
    let actual = (target.decode)(body);
    match (oracle.get("payload"), oracle.get("error"), actual) {
        // The trace-agent has no bound on the strings references resolve to; this decoder's is a deliberate
        // deviation (see the module documentation of `decoders::datadog`).
        (_, _, Err(DecodeError::ResolvedStringsTooLarge { .. })) => Ok(()),
        // The v1.0 decoder bounds a chunk or span list's count by the bytes that remain, at a plausible minimum size
        // per element, so that one-byte elements can't each cost a decoded chunk or span. The trace-agent only checks
        // that a byte remains per element: another deliberate deviation, which only degenerate payloads meet.
        (_, _, Err(DecodeError::ImplausibleHeaderCount { .. })) => Ok(()),
        (Some(expected), None, Ok(decoded)) => {
            let mut expected = expected.clone();
            parse_doubles(&mut expected);
            drop_valueless(&mut expected);
            normalize_trace_ids(&mut expected);
            let mut actual = dump(&decoded);
            drop_duplicates(&mut expected, &mut actual);
            // Without traces, nothing of the payload is kept, so there is nothing to compare.
            if decoded.traces.is_empty() && expected["chunks"].as_array().is_some_and(Vec::is_empty) {
                return Ok(());
            }
            if target.any_chunk_order {
                sort_chunks(&mut expected);
                sort_chunks(&mut actual);
            }
            if actual == expected {
                Ok(())
            } else {
                Err(format!("decoded differently\nrust: {actual}\ngo:   {expected}"))
            }
        }
        (None, Some(error), Ok(decoded)) => Err(format!("go failed with {error}, rust decoded {}", dump(&decoded))),
        (Some(expected), None, Err(error)) => Err(format!("rust failed with {error:?}, go decoded {expected}")),
        (None, Some(expected), Err(error)) => {
            let (tag, reason) = classify(&error);
            if expected["tag"] == tag && expected["reason"] == reason {
                Ok(())
            } else {
                Err(format!(
                    "rust failed with {tag}/{reason} ({error:?}), go with {expected}"
                ))
            }
        }
        _ => Err(format!("unexpected oracle result {oracle}")),
    }
}

/// Brings the trace-agent's v1.0 trace IDs to 16 bytes, as its `normalizeTraceChunkV1` does before anything reads
/// them: a shorter one right-aligned, a longer one cut to its last 16 bytes.
///
/// The trace-agent decodes a v1.0 trace ID, of a chunk or a span link, as the bytes sent, and the decoders as the
/// 128-bit value that the trace-agent's normalizer makes of them.
fn normalize_trace_ids(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(id)) = map.get_mut("trace_id") {
                *id = if id.len() < 32 {
                    format!("{id:0>32}")
                } else {
                    id[id.len() - 32..].to_string()
                };
            }
            map.values_mut().for_each(normalize_trace_ids);
        }
        Value::Array(values) => values.iter_mut().for_each(normalize_trace_ids),
        _ => {}
    }
}

/// Removes the attributes and array elements that the trace-agent keeps without a value.
///
/// The trace-agent keeps an attribute whose v0.4 `AnyValue` has an unknown `type` as an `AnyValue` without a value,
/// which reaches the intake as an attribute without a value. The decoders' attribute model has no such value, so they
/// drop the attribute: a deviation in representation, which only payloads with an invalid `type` meet.
fn drop_valueless(value: &mut Value) {
    let valueless = |value: &Value| value.get("unknown").is_some_and(|kind| kind == "<nil>");
    match value {
        Value::Object(map) => {
            map.retain(|_, value| !valueless(value));
            map.values_mut().for_each(drop_valueless);
        }
        Value::Array(values) => {
            values.retain(|value| !valueless(value));
            values.iter_mut().for_each(drop_valueless);
        }
        _ => {}
    }
}

/// Removes, from both renderings, the attributes that the oracle rendered as duplicated.
///
/// The trace-agent keys v0.5 attributes by dictionary index, so a dictionary holding the same text twice gives a span
/// two attributes of the same name, which the oracle can't order. The decoders keep the last one in wire order, as
/// for any repeated key.
fn drop_duplicates(expected: &mut Value, actual: &mut Value) {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            let duplicated: Vec<String> = expected
                .iter()
                .filter(|(_, value)| value.get("duplicate").is_some())
                .map(|(key, _)| key.clone())
                .collect();
            for key in duplicated {
                expected.remove(&key);
                actual.remove(&key);
            }
            for (key, value) in expected.iter_mut() {
                if let Some(other) = actual.get_mut(key) {
                    drop_duplicates(value, other);
                }
            }
        }
        (Value::Array(expected), Value::Array(actual)) => {
            for (value, other) in expected.iter_mut().zip(actual.iter_mut()) {
                drop_duplicates(value, other);
            }
        }
        _ => {}
    }
}

/// Bodies of the cases of a fixture, by their `body`, `body_b64` or `payload_base64`.
fn fixture_bodies(fixture: &str) -> Vec<Vec<u8>> {
    let fixture: Value = serde_json::from_str(fixture).unwrap();
    fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(
            |case| match (&case["body"], &case["body_b64"], &case["payload_base64"]) {
                (Value::String(body), _, _) => body.as_bytes().to_vec(),
                (_, Value::String(body), _) | (_, _, Value::String(body)) => {
                    general_purpose::STANDARD.decode(body).unwrap()
                }
                _ => panic!("a fixture case without a body"),
            },
        )
        .collect()
}

/// Mutations of a body: each flips, overwrites, inserts or removes a byte at an index.
fn mutate(body: BoxedStrategy<Vec<u8>>) -> BoxedStrategy<Vec<u8>> {
    let edits = prop::collection::vec((0u8..4, any::<prop::sample::Index>(), any::<u8>()), 0..4);
    let mutated = (body, edits, prop::option::of(any::<prop::sample::Index>())).prop_map(|(mut body, edits, cut)| {
        for (kind, index, byte) in edits {
            let at = index.index(body.len() + 1);
            match kind {
                0 if at < body.len() => body[at] ^= byte.max(1),
                1 if at < body.len() => body[at] = byte,
                2 => body.insert(at, byte),
                3 if at < body.len() => {
                    body.remove(at);
                }
                _ => {}
            }
        }
        if let Some(cut) = cut {
            body.truncate(cut.index(body.len() + 1));
        }
        body
    });
    mutated.boxed()
}

/// Fuzzes `target` with `bodies`, mutated or not, for `PARITY_CASES` cases (1,000 by default).
///
/// With `PARITY_BODY` set to a base64 body, such as one a failure printed, compares only that body instead. The run's
/// seed is printed, and taken from `PARITY_SEED` when it's set.
fn fuzz(target: Target, bodies: BoxedStrategy<Vec<u8>>) {
    let cases = std::env::var("PARITY_CASES").map_or(1000, |cases| cases.parse().unwrap());
    let oracle = RefCell::new(Oracle::start());
    if let Some(body) = std::env::var("PARITY_BODY").ok().filter(|body| !body.is_empty()) {
        let body = general_purpose::STANDARD.decode(body.trim()).unwrap();
        let expected = oracle.borrow_mut().decode(target.version, target.content_type, &body);
        if let Err(diff) = compare(&target, &body, &expected) {
            panic!("{} differs from the trace-agent: {diff}", target.version);
        }
        return;
    }
    // Printed on every run, so that a run can be repeated with `PARITY_SEED`, given the same oracle.
    let seed = std::env::var("PARITY_SEED")
        .ok()
        .filter(|seed| !seed.is_empty())
        .map_or_else(
            || RandomState::new().build_hasher().finish(),
            |seed| seed.parse().expect("PARITY_SEED is a u64"),
        );
    println!("{}: PARITY_SEED={seed}", target.version);
    let config = Config {
        cases,
        failure_persistence: None,
        ..Config::default()
    };
    let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &seed.to_le_bytes().repeat(4));
    let mut runner = TestRunner::new_with_rng(config, rng);
    let inputs = prop_oneof![1 => bodies.clone(), 2 => mutate(bodies)];
    let result = runner.run(&inputs, |body| {
        let expected = oracle.borrow_mut().decode(target.version, target.content_type, &body);
        if expected.get("panic").is_some() {
            // A panic in the trace-agent is its own bug, not a difference to fix here.
            eprintln!(
                "the trace-agent panicked on {}: {expected}",
                general_purpose::STANDARD.encode(&body)
            );
            return Ok(());
        }
        let mut result = compare(&target, &body, &expected);
        // The trace-agent's chunk order is random, and so is the payload's env or hostname when several chunks carry
        // one: the decoder must produce one of the trace-agent's results, so ask again for another.
        for _ in 0..(if target.any_chunk_order { 32 } else { 0 }) {
            if result.is_ok() {
                break;
            }
            let expected = oracle.borrow_mut().decode(target.version, target.content_type, &body);
            result = compare(&target, &body, &expected);
        }
        result.map_err(|diff| {
            TestCaseError::fail(format!(
                "{diff}\nbody (base64): {}",
                general_purpose::STANDARD.encode(&body)
            ))
        })
    });
    if let Err(error) = result {
        panic!(
            "{} differs from the trace-agent (PARITY_SEED={seed}): {error}",
            target.version
        );
    }
}

fn seeds(fixture: &str) -> BoxedStrategy<Vec<u8>> {
    prop::sample::select(fixture_bodies(fixture)).boxed()
}

const MSGPACK: Option<&str> = Some("application/msgpack");
const JSON: Option<&str> = Some("application/json");

#[test]
#[ignore = "needs the trace-agent oracle: run test/trace-agent-oracle/fuzz.sh"]
fn parity_v1() {
    let target = Target {
        version: "v1.0",
        content_type: MSGPACK,
        decode: decode_v1_payload,
        any_chunk_order: false,
    };
    let bodies = prop_oneof![
        seeds(include_str!("testdata/v1_decoder_cases.json")),
        v1_payload_strategy()
    ];
    fuzz(target, bodies.boxed());
}

/// A v1.0 string sent as a reference, possibly out of range, or as a literal.
#[derive(Clone, Debug)]
enum V1String {
    Ref(u32),
    Literal(Vec<u8>),
}

/// Strings that collide: the same text, the empty string, and different bytes that repair to the same text.
fn v1_text() -> impl Strategy<Value = Vec<u8>> {
    prop::sample::select(vec![
        b"a".to_vec(),
        b"b".to_vec(),
        b"".to_vec(),
        b"checkout".to_vec(),
        b"a\xff".to_vec(),
        b"a\xfe".to_vec(),
    ])
}

fn v1_string() -> impl Strategy<Value = V1String> {
    prop_oneof![
        (0u32..10).prop_map(V1String::Ref),
        v1_text().prop_map(V1String::Literal)
    ]
}

fn write_v1_string(buf: &mut Vec<u8>, value: &V1String) {
    match value {
        V1String::Ref(index) => rmp::encode::write_uint(buf, u64::from(*index)).map(|_| ()).unwrap(),
        V1String::Literal(bytes) => {
            rmp::encode::write_str_len(buf, bytes.len() as u32).unwrap();
            buf.extend_from_slice(bytes);
        }
    }
}

/// v1.0 payloads whose string table holds duplicates, whose literals repeat strings, and whose chunks may have no
/// priority or `PriorityNone`: the cases where the reference's string table and priority are easy to get wrong.
fn v1_payload_strategy() -> BoxedStrategy<Vec<u8>> {
    let span = (v1_string(), v1_string(), any::<u64>());
    let chunk = (
        prop::option::of(prop::sample::select(vec![-128i64, 0, 1, 2])),
        any::<u8>(),
        prop::collection::vec(span, 1..4),
    );
    (
        prop::collection::vec(v1_text(), 0..6),
        prop::collection::vec(chunk, 1..3),
    )
        .prop_map(|(table, chunks)| {
            let mut buf = Vec::new();
            rmp::encode::write_map_len(&mut buf, 2).unwrap();
            rmp::encode::write_uint(&mut buf, 1).unwrap(); // string table
            rmp::encode::write_array_len(&mut buf, table.len() as u32).unwrap();
            for entry in &table {
                write_v1_string(&mut buf, &V1String::Literal(entry.clone()));
            }
            rmp::encode::write_uint(&mut buf, 11).unwrap(); // chunks
            rmp::encode::write_array_len(&mut buf, chunks.len() as u32).unwrap();
            for (priority, trace_id, spans) in &chunks {
                rmp::encode::write_map_len(&mut buf, 2 + u32::from(priority.is_some())).unwrap();
                if let Some(priority) = priority {
                    rmp::encode::write_uint(&mut buf, 1).unwrap();
                    rmp::encode::write_sint(&mut buf, *priority).unwrap();
                }
                rmp::encode::write_uint(&mut buf, 6).unwrap(); // traceID
                rmp::encode::write_bin(&mut buf, &[*trace_id; 16]).unwrap();
                rmp::encode::write_uint(&mut buf, 4).unwrap(); // spans
                rmp::encode::write_array_len(&mut buf, spans.len() as u32).unwrap();
                for (service, name, span_id) in spans {
                    rmp::encode::write_map_len(&mut buf, 4).unwrap();
                    rmp::encode::write_uint(&mut buf, 1).unwrap(); // service
                    write_v1_string(&mut buf, service);
                    rmp::encode::write_uint(&mut buf, 2).unwrap(); // name
                    write_v1_string(&mut buf, name);
                    rmp::encode::write_uint(&mut buf, 3).unwrap(); // resource, long enough for the size bounds
                    rmp::encode::write_str(&mut buf, "GET /checkout/cart").unwrap();
                    rmp::encode::write_uint(&mut buf, 4).unwrap(); // spanID
                    rmp::encode::write_u64(&mut buf, *span_id).unwrap();
                }
            }
            buf
        })
        .boxed()
}

#[test]
#[ignore = "needs the trace-agent oracle: run test/trace-agent-oracle/fuzz.sh"]
fn parity_v04_msgpack() {
    let target = Target {
        version: "v0.4",
        content_type: MSGPACK,
        decode: decode_v04_payload,
        any_chunk_order: false,
    };
    let generated = prop::collection::vec(prop::collection::vec(v04_span_strategy(), 0..4), 0..4)
        .prop_map(|chunks| encode_value(&V::Arr(chunks.into_iter().map(V::Arr).collect())));
    fuzz(target, generated.boxed());
}

#[test]
#[ignore = "needs the trace-agent oracle: run test/trace-agent-oracle/fuzz.sh"]
fn parity_v04_json() {
    let target = Target {
        version: "v0.4",
        content_type: JSON,
        decode: decode_v04_json,
        any_chunk_order: false,
    };
    let generated = prop::collection::vec(prop::collection::vec(json_strategy(), 0..4), 0..4)
        .prop_map(|traces| serde_json::to_vec(&traces).unwrap());
    let bodies = prop_oneof![seeds(include_str!("testdata/v04_json_cases.json")), generated];
    fuzz(target, bodies.boxed());
}

#[test]
#[ignore = "needs the trace-agent oracle: run test/trace-agent-oracle/fuzz.sh"]
fn parity_v01() {
    let target = Target {
        version: "v0.1",
        content_type: JSON,
        decode: decode_v01_json,
        any_chunk_order: true,
    };
    let generated = prop::collection::vec(json_strategy(), 0..6).prop_map(|spans| serde_json::to_vec(&spans).unwrap());
    let bodies = prop_oneof![seeds(include_str!("testdata/v01_json_cases.json")), generated];
    fuzz(target, bodies.boxed());
}

#[test]
#[ignore = "needs the trace-agent oracle: run test/trace-agent-oracle/fuzz.sh"]
fn parity_v05() {
    let target = Target {
        version: "v0.5",
        content_type: MSGPACK,
        decode: decode_v05_payload,
        any_chunk_order: false,
    };
    let generated = super::v05::tests::payload_strategy().prop_map(|(dictionary, chunks)| {
        let dictionary: Vec<&str> = dictionary.iter().map(String::as_str).collect();
        super::v05::tests::payload(&dictionary, chunks)
    });
    let bodies = prop_oneof![seeds(include_str!("testdata/v05_cases.json")), generated];
    fuzz(target, bodies.boxed());
}

#[test]
#[ignore = "needs the trace-agent oracle: run test/trace-agent-oracle/fuzz.sh"]
fn parity_v07() {
    use super::v07::tests::{chunk, payload, span_strategy};

    let target = Target {
        version: "v0.7",
        content_type: MSGPACK,
        decode: decode_v07_payload,
        any_chunk_order: false,
    };
    let generated = prop::collection::vec(prop::collection::vec(span_strategy(), 0..4), 0..4).prop_map(|chunks| {
        let fields = vec![
            ("env", V::Str("prod".to_string())),
            ("hostname", V::Str("web-1".to_string())),
        ];
        payload(fields, chunks.into_iter().map(chunk).collect())
    });
    let bodies = prop_oneof![seeds(include_str!("testdata/v07_cases.json")), generated];
    fuzz(target, bodies.boxed());
}
