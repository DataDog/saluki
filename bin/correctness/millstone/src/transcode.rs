//! Transcoding of generated v0.4 trace payloads into the other Datadog trace intake formats.
//!
//! lading generates only v0.4 MessagePack (`[][]Span`) and v1.0 trace payloads. Tracing libraries also send v0.5
//! (dictionary-encoded MessagePack), v0.7 (a MessagePack `TracerPayload`), JSON `[][]Span` (v0.2 to v0.4), and JSON
//! `[]Span` (`/v0.1/spans` and `/spans`). Each of those carries the same spans as v0.4, so we transcode the generated
//! v0.4 payloads into them instead of generating them separately: every format then gets the same deterministic
//! corpus.

use std::collections::BTreeMap;

use bytes::Bytes;
use saluki_error::{generic_error, GenericError};
use serde::{Deserialize, Serialize};

/// Span metric from which the trace-agent reads a v0.4 trace's sampling priority.
const SAMPLING_PRIORITY_KEY: &str = "_sampling_priority_v1";

/// Chunk priority meaning "no priority set", as the trace-agent's sampler marks it.
const PRIORITY_NONE: i32 = -128;

/// Trace intake format to transcode generated v0.4 payloads into.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum TraceEncoding {
    /// v0.5: `[dictionary, [][]Span]`, where each span is a 12-element array of dictionary references and values.
    #[serde(rename = "v0.5")]
    V05,

    /// v0.7: a MessagePack `TracerPayload` whose chunks carry the spans and the trace's sampling priority.
    #[serde(rename = "v0.7")]
    V07,

    /// JSON `[][]Span`, as accepted by `/v0.2/traces`, `/v0.3/traces` and `/v0.4/traces`.
    #[serde(rename = "json")]
    Json,

    /// JSON `[]Span`, as accepted by `/v0.1/spans` and `/spans`: every trace's spans in one flat list.
    #[serde(rename = "spans_json")]
    SpansJson,
}

/// A v0.4 span, as lading generates it.
///
/// Unknown fields are rejected so that transcoding fails loudly, rather than dropping data, if lading starts
/// generating fields that aren't carried over (for example, `meta_struct`).
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Span {
    service: String,
    name: String,
    resource: String,
    trace_id: u64,
    span_id: u64,
    parent_id: u64,
    start: i64,
    duration: i64,
    error: i32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    meta: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    metrics: BTreeMap<String, f64>,
    #[serde(rename = "type")]
    span_type: String,
}

#[derive(Serialize)]
struct TracerPayload {
    chunks: Vec<TraceChunk>,
}

#[derive(Serialize)]
struct TraceChunk {
    priority: i32,
    spans: Vec<Span>,
}

/// Transcodes a v0.4 MessagePack payload into `encoding`.
///
/// An empty payload, which generators return when nothing fits, stays empty.
///
/// # Errors
///
/// If the payload isn't a v0.4 payload of the spans lading generates, or can't be encoded, an error is returned.
pub fn transcode_v04(payload: &[u8], encoding: TraceEncoding) -> Result<Bytes, GenericError> {
    if payload.is_empty() {
        return Ok(Bytes::new());
    }
    let traces: Vec<Vec<Span>> = rmp_serde::from_slice(payload)
        .map_err(|e| generic_error!("Failed to decode generated v0.4 trace payload: {}", e))?;

    let encoded = match encoding {
        TraceEncoding::V05 => encode_v05(&traces),
        TraceEncoding::V07 => encode_v07(traces)?,
        TraceEncoding::Json => serde_json::to_vec(&traces)?,
        TraceEncoding::SpansJson => serde_json::to_vec(&traces.into_iter().flatten().collect::<Vec<_>>())?,
    };
    Ok(Bytes::from(encoded))
}

fn encode_v07(traces: Vec<Vec<Span>>) -> Result<Vec<u8>, GenericError> {
    let chunks = traces
        .into_iter()
        .map(|spans| {
            // lading sets the priority on the root span; v0.7 carries it on the chunk instead.
            let priority = spans
                .iter()
                .find_map(|span| span.metrics.get(SAMPLING_PRIORITY_KEY))
                .map_or(PRIORITY_NONE, |priority| *priority as i32);
            TraceChunk { priority, spans }
        })
        .collect();
    let mut out = Vec::new();
    TracerPayload { chunks }
        .serialize(&mut rmp_serde::Serializer::new(&mut out).with_struct_map())
        .map_err(|e| generic_error!("Failed to encode v0.7 trace payload: {}", e))?;
    Ok(out)
}

fn encode_v05(traces: &[Vec<Span>]) -> Vec<u8> {
    let mut dictionary = Dictionary::default();
    let mut body = Vec::new();
    write_array_len(&mut body, traces.len());
    for trace in traces {
        write_array_len(&mut body, trace.len());
        for span in trace {
            write_array_len(&mut body, 12);
            for s in [&span.service, &span.name, &span.resource] {
                write_ref(&mut body, dictionary.intern(s));
            }
            rmp::encode::write_uint(&mut body, span.trace_id).expect("Vec writes are infallible");
            rmp::encode::write_uint(&mut body, span.span_id).expect("Vec writes are infallible");
            rmp::encode::write_uint(&mut body, span.parent_id).expect("Vec writes are infallible");
            rmp::encode::write_sint(&mut body, span.start).expect("Vec writes are infallible");
            rmp::encode::write_sint(&mut body, span.duration).expect("Vec writes are infallible");
            rmp::encode::write_sint(&mut body, i64::from(span.error)).expect("Vec writes are infallible");
            write_map_len(&mut body, span.meta.len());
            for (key, value) in &span.meta {
                write_ref(&mut body, dictionary.intern(key));
                write_ref(&mut body, dictionary.intern(value));
            }
            write_map_len(&mut body, span.metrics.len());
            for (key, value) in &span.metrics {
                write_ref(&mut body, dictionary.intern(key));
                rmp::encode::write_f64(&mut body, *value).expect("Vec writes are infallible");
            }
            write_ref(&mut body, dictionary.intern(&span.span_type));
        }
    }

    let mut out = Vec::with_capacity(body.len() + dictionary.bytes + 16);
    write_array_len(&mut out, 2);
    write_array_len(&mut out, dictionary.strings.len());
    for s in &dictionary.strings {
        rmp::encode::write_str(&mut out, s).expect("Vec writes are infallible");
    }
    out.extend_from_slice(&body);
    out
}

/// The v0.5 string dictionary, in order of first use.
#[derive(Default)]
struct Dictionary {
    strings: Vec<String>,
    indexes: BTreeMap<String, u32>,
    bytes: usize,
}

impl Dictionary {
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(index) = self.indexes.get(s) {
            return *index;
        }
        let index = u32::try_from(self.strings.len()).expect("fewer than 2^32 distinct strings");
        self.strings.push(s.to_string());
        self.indexes.insert(s.to_string(), index);
        self.bytes += s.len() + 5;
        index
    }
}

fn write_ref(out: &mut Vec<u8>, index: u32) {
    rmp::encode::write_uint(out, u64::from(index)).expect("Vec writes are infallible");
}

fn write_array_len(out: &mut Vec<u8>, len: usize) {
    rmp::encode::write_array_len(out, u32::try_from(len).expect("fewer than 2^32 elements"))
        .expect("Vec writes are infallible");
}

fn write_map_len(out: &mut Vec<u8>, len: usize) {
    rmp::encode::write_map_len(out, u32::try_from(len).expect("fewer than 2^32 entries"))
        .expect("Vec writes are infallible");
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;

    fn span(trace_id: u64, span_id: u64, priority: Option<f64>) -> Span {
        Span {
            service: "svc".to_string(),
            name: "op".to_string(),
            resource: "res".to_string(),
            trace_id,
            span_id,
            parent_id: 0,
            start: 1_700_000_000_000_000_000,
            duration: 42,
            error: 1,
            meta: BTreeMap::from([("env".to_string(), "svc".to_string())]),
            metrics: priority
                .map(|priority| BTreeMap::from([(SAMPLING_PRIORITY_KEY.to_string(), priority)]))
                .unwrap_or_default(),
            span_type: "web".to_string(),
        }
    }

    fn v04(traces: &[Vec<Span>]) -> Vec<u8> {
        let mut out = Vec::new();
        traces
            .serialize(&mut rmp_serde::Serializer::new(&mut out).with_struct_map())
            .unwrap();
        out
    }

    fn sample() -> Vec<u8> {
        v04(&[vec![span(1, 1, Some(2.0)), span(1, 2, None)], vec![span(3, 3, None)]])
    }

    #[test]
    fn empty_payloads_stay_empty() {
        for encoding in [
            TraceEncoding::V05,
            TraceEncoding::V07,
            TraceEncoding::Json,
            TraceEncoding::SpansJson,
        ] {
            assert!(transcode_v04(&[], encoding).unwrap().is_empty());
        }
    }

    #[test]
    fn json_keeps_the_traces_and_spans_lists_flatten_them() {
        let traces: Value = serde_json::from_slice(&transcode_v04(&sample(), TraceEncoding::Json).unwrap()).unwrap();
        assert_eq!(traces.as_array().unwrap().len(), 2);
        assert_eq!(
            traces[0][0],
            json!({
                "service": "svc", "name": "op", "resource": "res", "trace_id": 1, "span_id": 1, "parent_id": 0,
                "start": 1_700_000_000_000_000_000_i64, "duration": 42, "error": 1, "meta": {"env": "svc"},
                "metrics": {SAMPLING_PRIORITY_KEY: 2.0}, "type": "web",
            })
        );

        let spans: Value =
            serde_json::from_slice(&transcode_v04(&sample(), TraceEncoding::SpansJson).unwrap()).unwrap();
        let ids: Vec<_> = spans.as_array().unwrap().iter().map(|s| s["span_id"].clone()).collect();
        assert_eq!(ids, [json!(1), json!(2), json!(3)]);
    }

    #[test]
    fn v05_references_a_dictionary_of_first_use() {
        let bytes = transcode_v04(&sample(), TraceEncoding::V05).unwrap();
        let value: rmpv::Value = rmpv::decode::read_value(&mut &bytes[..]).unwrap();
        let dictionary: Vec<&str> = value[0]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert_eq!(dictionary, ["svc", "op", "res", "env", SAMPLING_PRIORITY_KEY, "web"]);
        let first = value[1][0][0].as_array().unwrap();
        assert_eq!(first.len(), 12);
        assert_eq!(first[0].as_u64(), Some(0));
        assert_eq!(first[3].as_u64(), Some(1));
        assert_eq!(first[6].as_i64(), Some(1_700_000_000_000_000_000));
        assert_eq!(first[9].as_map().unwrap()[0].0.as_u64(), Some(3));
        assert_eq!(first[10].as_map().unwrap()[0].1.as_f64(), Some(2.0));
        assert_eq!(first[11].as_u64(), Some(5));
        assert_eq!(value[1][1].as_array().unwrap().len(), 1);
    }

    #[test]
    fn v07_carries_the_root_priority_on_each_chunk() {
        let bytes = transcode_v04(&sample(), TraceEncoding::V07).unwrap();
        let value: rmpv::Value = rmpv::decode::read_value(&mut &bytes[..]).unwrap();
        let chunks = value["chunks"].as_array().unwrap();
        assert_eq!(chunks[0]["priority"].as_i64(), Some(2));
        assert_eq!(chunks[0]["spans"].as_array().unwrap().len(), 2);
        assert_eq!(chunks[0]["spans"][1]["span_id"].as_u64(), Some(2));
        assert_eq!(chunks[1]["priority"].as_i64(), Some(i64::from(PRIORITY_NONE)));
    }

    #[test]
    fn unknown_span_fields_are_rejected() {
        let span = rmpv::Value::Map(vec![
            ("span_id".into(), 1.into()),
            ("meta_struct".into(), rmpv::Value::Map(vec![])),
        ]);
        let payload = rmpv::Value::Array(vec![rmpv::Value::Array(vec![span])]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &payload).unwrap();
        assert!(transcode_v04(&bytes, TraceEncoding::Json).is_err());
    }
}
