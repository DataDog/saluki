//! MessagePack encoding of materialized cases in the v1.0 trace wire format.
//!
//! The encoder mirrors `InternalTracerPayload.MarshalMsg` (and the chunk and span equivalents) in the Go agent:
//!
//! - Structs are maps keyed by `u32` field number, written in ascending field order. Fields holding their zero value
//!   are omitted, and the map length counts only the emitted fields.
//! - Strings are streamed: the first occurrence is written inline as a MessagePack string, and later occurrences as
//!   the `u32` index of that first occurrence. Indexes start at `1`; the empty string is always index `0`.
//! - Attribute maps are flat arrays with three entries per attribute: key (streaming string), `u32` value type
//!   (`1` string, `3` double), and the value.

use std::collections::{BTreeMap, HashMap};

use rmp::encode;

use super::{AttributeSpec, MaterializedCase, MaterializedChunk, MaterializedPayload, MaterializedSpan};

/// Request path of the v1.0 traces endpoint.
pub const V1_TRACES_PATH: &str = "/v1.0/traces";

/// Writes to a `Vec<u8>` can't fail; this collapses `rmp`'s `Result` into a panic that never happens in practice.
fn infallible<T, E: std::fmt::Debug>(r: Result<T, E>) {
    r.expect("writing MessagePack to a Vec should not fail");
}

#[derive(Default)]
struct Encoder {
    buf: Vec<u8>,
    indexes: HashMap<String, u32>,
}

impl Encoder {
    fn uint(&mut self, v: u64) {
        infallible(encode::write_uint(&mut self.buf, v));
    }

    fn field(&mut self, number: u32) {
        self.uint(u64::from(number));
    }

    fn map_len(&mut self, len: u32) {
        infallible(encode::write_map_len(&mut self.buf, len));
    }

    fn array_len(&mut self, len: u32) {
        infallible(encode::write_array_len(&mut self.buf, len));
    }

    fn streaming_str(&mut self, s: &str) {
        if s.is_empty() {
            self.uint(0);
        } else if let Some(&index) = self.indexes.get(s) {
            self.uint(u64::from(index));
        } else {
            infallible(encode::write_str(&mut self.buf, s));
            let next = self.indexes.len() as u32 + 1;
            self.indexes.insert(s.to_string(), next);
        }
    }

    fn attributes(&mut self, attributes: &BTreeMap<String, AttributeSpec>) {
        self.array_len(attributes.len() as u32 * 3);
        for (key, value) in attributes {
            self.streaming_str(key);
            match value {
                AttributeSpec::String(s) => {
                    self.uint(1);
                    self.streaming_str(s);
                }
                AttributeSpec::Double(d) => {
                    self.uint(3);
                    infallible(encode::write_f64(&mut self.buf, *d));
                }
            }
        }
    }

    fn payload(&mut self, p: &MaterializedPayload) {
        let strs = [
            (3, &p.lang),
            (4, &p.lang_version),
            (5, &p.tracer_version),
            (7, &p.env),
            (8, &p.hostname),
            (9, &p.app_version),
        ];
        let len = strs.iter().filter(|(_, s)| !s.is_empty()).count() + usize::from(!p.chunks.is_empty());
        self.map_len(len as u32);
        for (number, s) in strs {
            if !s.is_empty() {
                self.field(number);
                self.streaming_str(s);
            }
        }
        if !p.chunks.is_empty() {
            self.field(11);
            self.array_len(p.chunks.len() as u32);
            for chunk in &p.chunks {
                self.chunk(chunk);
            }
        }
    }

    fn chunk(&mut self, c: &MaterializedChunk) {
        let len = usize::from(c.priority != 0)
            + usize::from(!c.origin.is_empty())
            + usize::from(!c.spans.is_empty())
            + usize::from(c.dropped_trace);
        self.map_len(len as u32);
        if c.priority != 0 {
            self.field(1);
            infallible(encode::write_sint(&mut self.buf, i64::from(c.priority)));
        }
        if !c.origin.is_empty() {
            self.field(2);
            self.streaming_str(&c.origin);
        }
        if !c.spans.is_empty() {
            self.field(4);
            self.array_len(c.spans.len() as u32);
            for span in &c.spans {
                self.span(span);
            }
        }
        if c.dropped_trace {
            self.field(5);
            infallible(encode::write_bool(&mut self.buf, true));
        }
    }

    fn span(&mut self, s: &MaterializedSpan) {
        let len = [
            !s.service.is_empty(),
            !s.name.is_empty(),
            !s.resource.is_empty(),
            s.span_id != 0,
            s.parent_id != 0,
            s.start != 0,
            s.duration != 0,
            s.error,
            !s.attributes.is_empty(),
            !s.span_type.is_empty(),
            s.kind != 0,
        ]
        .iter()
        .filter(|present| **present)
        .count();
        self.map_len(len as u32);
        for (number, value) in [(1, &s.service), (2, &s.name), (3, &s.resource)] {
            if !value.is_empty() {
                self.field(number);
                self.streaming_str(value);
            }
        }
        for (number, value) in [(4, s.span_id), (5, s.parent_id), (6, s.start), (7, s.duration)] {
            if value != 0 {
                self.field(number);
                self.uint(value);
            }
        }
        if s.error {
            self.field(8);
            infallible(encode::write_bool(&mut self.buf, true));
        }
        if !s.attributes.is_empty() {
            self.field(9);
            self.attributes(&s.attributes);
        }
        if !s.span_type.is_empty() {
            self.field(10);
            self.streaming_str(&s.span_type);
        }
        if s.kind != 0 {
            self.field(16);
            self.uint(u64::from(s.kind));
        }
    }
}

/// An HTTP request for the v1.0 traces endpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct V1Request {
    /// Request path (always `/v1.0/traces`).
    pub path: &'static str,

    /// Request headers, with case overrides applied.
    pub headers: Vec<(String, String)>,

    /// MessagePack-encoded tracer payload.
    pub body: Vec<u8>,
}

impl MaterializedCase {
    /// Encodes the payload as a v1.0 MessagePack body.
    pub fn encode(&self) -> Vec<u8> {
        let mut encoder = Encoder::default();
        encoder.payload(&self.payload);
        encoder.buf
    }

    /// Builds the `/v1.0/traces` request for this case.
    ///
    /// The default headers match the Go harness: `Content-Type: application/msgpack`, `X-Datadog-Trace-Count` set to
    /// the chunk count, `Datadog-Meta-Lang: python`, `Datadog-Meta-Lang-Version: 3.12.0`, and
    /// `Datadog-Meta-Tracer-Version: 2.0.0`. The case's own headers then override them, matching names
    /// case-insensitively.
    pub fn request(&self) -> V1Request {
        let mut headers: Vec<(String, String)> = vec![
            ("Content-Type".into(), "application/msgpack".into()),
            ("X-Datadog-Trace-Count".into(), self.payload.chunks.len().to_string()),
            ("Datadog-Meta-Lang".into(), "python".into()),
            ("Datadog-Meta-Lang-Version".into(), "3.12.0".into()),
            ("Datadog-Meta-Tracer-Version".into(), "2.0.0".into()),
        ];
        for (name, value) in &self.headers {
            match headers.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
                Some(existing) => existing.1 = value.clone(),
                None => headers.push((name.clone(), value.clone())),
            }
        }
        V1Request {
            path: V1_TRACES_PATH,
            headers,
            body: self.encode(),
        }
    }
}

#[cfg(test)]
mod tests {
    use saluki_components::decoders::datadog::decode_v1_payload;
    use saluki_core::data_model::event::trace::AttributeValue;

    use crate::cases::{load_bundled_cases, MaterializedCase};

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn bundled(name: &str) -> MaterializedCase {
        load_bundled_cases()
            .unwrap()
            .into_iter()
            .find(|c| c.name == name)
            .unwrap()
            .materialize(NOW)
            .unwrap()
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn request_has_default_headers() {
        let req = bundled("two_chunks").request();
        assert_eq!(req.path, "/v1.0/traces");
        assert_eq!(header(&req.headers, "content-type"), Some("application/msgpack"));
        assert_eq!(header(&req.headers, "X-Datadog-Trace-Count"), Some("2"));
        assert_eq!(header(&req.headers, "Datadog-Meta-Lang"), Some("python"));
        assert_eq!(header(&req.headers, "Datadog-Meta-Lang-Version"), Some("3.12.0"));
        assert_eq!(header(&req.headers, "Datadog-Meta-Tracer-Version"), Some("2.0.0"));
    }

    #[test]
    fn case_headers_are_added_and_override_case_insensitively() {
        let mut case = bundled("container_process_tags");
        let req = case.request();
        assert_eq!(
            header(&req.headers, "Datadog-Container-ID"),
            Some("abcdef0123456789abcdef0123456789")
        );
        assert_eq!(
            header(&req.headers, "X-Datadog-Process-Tags"),
            Some("env:golden,team:apm")
        );

        case.headers.insert("content-type".into(), "application/other".into());
        let req = case.request();
        assert_eq!(header(&req.headers, "Content-Type"), Some("application/other"));
        assert_eq!(
            req.headers
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                .count(),
            1
        );
    }

    #[test]
    fn every_bundled_case_round_trips_through_decoder() {
        for case in load_bundled_cases().unwrap() {
            let m = case.materialize(NOW).unwrap();
            let traces = decode_v1_payload(&m.encode()).unwrap_or_else(|e| panic!("case {}: {}", m.name, e));
            assert_eq!(traces.len(), m.payload.chunks.len(), "case {}", m.name);

            for (trace, chunk) in traces.iter().zip(&m.payload.chunks) {
                let ctx = &m.name;
                let p = &trace.payload;
                assert_eq!(&*p.language_name, m.payload.lang, "case {ctx}");
                assert_eq!(&*p.language_version, m.payload.lang_version, "case {ctx}");
                assert_eq!(&*p.tracer_version, m.payload.tracer_version, "case {ctx}");
                assert_eq!(&*p.app_version, m.payload.app_version, "case {ctx}");
                assert_eq!(&*p.env, m.payload.env, "case {ctx}");
                assert_eq!(&*p.hostname, m.payload.hostname, "case {ctx}");
                assert_eq!(&*p.container_id, "", "case {ctx}");

                let expected_priority = (chunk.priority != 0).then_some(chunk.priority);
                assert_eq!(trace.priority, expected_priority, "case {ctx}");
                assert_eq!(&*trace.origin, chunk.origin, "case {ctx}");
                assert_eq!(trace.dropped_trace, chunk.dropped_trace, "case {ctx}");

                assert_eq!(trace.spans().len(), chunk.spans.len(), "case {ctx}");
                for (span, s) in trace.spans().iter().zip(&chunk.spans) {
                    assert_eq!(span.service(), s.service, "case {ctx}");
                    assert_eq!(span.name(), s.name, "case {ctx}");
                    assert_eq!(span.resource(), s.resource, "case {ctx}");
                    assert_eq!(span.span_type(), s.span_type, "case {ctx}");
                    assert_eq!(span.span_id(), s.span_id, "case {ctx}");
                    assert_eq!(span.parent_id(), s.parent_id, "case {ctx}");
                    assert_eq!(span.start(), s.start, "case {ctx}");
                    assert_eq!(span.duration(), s.duration, "case {ctx}");
                    assert_eq!(span.error(), i32::from(s.error), "case {ctx}");
                    assert_eq!(span.kind, s.kind, "case {ctx}");
                    assert_eq!(span.attributes.len(), s.attributes.len(), "case {ctx}");
                    for (key, value) in &s.attributes {
                        let actual = span.attributes.get(key.as_str());
                        match (value, actual) {
                            (super::AttributeSpec::String(e), Some(AttributeValue::String(a))) => {
                                assert_eq!(&**a, e, "case {ctx} attr {key}")
                            }
                            (super::AttributeSpec::Double(e), Some(AttributeValue::Float(a))) => {
                                assert_eq!(a, e, "case {ctx} attr {key}")
                            }
                            (e, a) => panic!("case {ctx} attr {key}: expected {e:?}, got {a:?}"),
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn no_priority_omits_priority_field() {
        let m = bundled("no_priority");
        assert_eq!(m.payload.chunks[0].priority, 0);
        let traces = decode_v1_payload(&m.encode()).unwrap();
        assert_eq!(traces[0].priority, None);
    }
}
