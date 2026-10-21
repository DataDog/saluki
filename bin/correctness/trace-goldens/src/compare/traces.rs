//! Semantic comparison of trace payloads (`AgentPayload` with idx tracer payloads).

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use datadog_protos::traces::{
    idx::{self, any_value::Value as ProtoValue},
    AgentPayload,
};
use protobuf::{EnumOrUnknown, MessageField};
use saluki_error::{generic_error, GenericError};
use serde::Deserialize;

use super::{
    protojson::{base64_bytes, opt_base64_bytes, opt_i64_str, u64_str},
    CompareOptions, Mismatches,
};

// --- Golden protojson model -------------------------------------------------------------------------------------

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct GoldenAgentPayload {
    host_name: String,
    env: String,
    tracer_payloads: Vec<serde_json::Value>,
    tags: BTreeMap<String, String>,
    agent_version: String,
    #[serde(rename = "targetTPS")]
    target_tps: f64,
    #[serde(rename = "errorTPS")]
    error_tps: f64,
    rare_sampler_enabled: bool,
    idx_tracer_payloads: Vec<GoldenTracerPayload>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct GoldenTracerPayload {
    strings: Vec<String>,
    #[serde(rename = "containerIDRef")]
    container_id_ref: u32,
    language_name_ref: u32,
    language_version_ref: u32,
    tracer_version_ref: u32,
    #[serde(rename = "runtimeIDRef")]
    runtime_id_ref: u32,
    env_ref: u32,
    hostname_ref: u32,
    app_version_ref: u32,
    attributes: BTreeMap<u32, GoldenAnyValue>,
    chunks: Vec<GoldenChunk>,
    /// Container debug information; ignored.
    #[serde(rename = "containerDebug")]
    _container_debug: serde_json::Value,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct GoldenChunk {
    priority: i32,
    origin_ref: u32,
    attributes: BTreeMap<u32, GoldenAnyValue>,
    spans: Vec<GoldenSpan>,
    dropped_trace: bool,
    #[serde(rename = "traceID", deserialize_with = "base64_bytes")]
    trace_id: Vec<u8>,
    sampling_mechanism: u32,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct GoldenSpan {
    service_ref: u32,
    name_ref: u32,
    resource_ref: u32,
    #[serde(rename = "spanID", deserialize_with = "u64_str")]
    span_id: u64,
    #[serde(rename = "parentID", deserialize_with = "u64_str")]
    parent_id: u64,
    /// Normalized start placeholder: `"<start:N>"`.
    start: String,
    #[serde(deserialize_with = "u64_str")]
    duration: u64,
    error: bool,
    attributes: BTreeMap<u32, GoldenAnyValue>,
    type_ref: u32,
    links: Vec<GoldenLink>,
    events: Vec<GoldenEvent>,
    env_ref: u32,
    version_ref: u32,
    component_ref: u32,
    kind: String,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct GoldenLink {
    #[serde(rename = "traceID", deserialize_with = "base64_bytes")]
    trace_id: Vec<u8>,
    #[serde(rename = "spanID", deserialize_with = "u64_str")]
    span_id: u64,
    attributes: BTreeMap<u32, GoldenAnyValue>,
    tracestate_ref: u32,
    flags: u32,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct GoldenEvent {
    #[serde(deserialize_with = "u64_str")]
    time: u64,
    name_ref: u32,
    attributes: BTreeMap<u32, GoldenAnyValue>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct GoldenAnyValue {
    string_value_ref: Option<u32>,
    bool_value: Option<bool>,
    double_value: Option<f64>,
    #[serde(deserialize_with = "opt_i64_str")]
    int_value: Option<i64>,
    #[serde(deserialize_with = "opt_base64_bytes")]
    bytes_value: Option<Vec<u8>>,
    array_value: Option<GoldenArrayValue>,
    key_value_list: Option<GoldenKeyValueList>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct GoldenArrayValue {
    values: Vec<GoldenAnyValue>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct GoldenKeyValueList {
    key_values: Vec<GoldenKeyValue>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct GoldenKeyValue {
    key: u32,
    value: GoldenAnyValue,
}

const SPAN_KINDS: [(&str, idx::SpanKind); 6] = [
    ("SPAN_KIND_UNSPECIFIED", idx::SpanKind::SPAN_KIND_UNSPECIFIED),
    ("SPAN_KIND_INTERNAL", idx::SpanKind::SPAN_KIND_INTERNAL),
    ("SPAN_KIND_SERVER", idx::SpanKind::SPAN_KIND_SERVER),
    ("SPAN_KIND_CLIENT", idx::SpanKind::SPAN_KIND_CLIENT),
    ("SPAN_KIND_PRODUCER", idx::SpanKind::SPAN_KIND_PRODUCER),
    ("SPAN_KIND_CONSUMER", idx::SpanKind::SPAN_KIND_CONSUMER),
];

/// Parses a golden span start placeholder (`"<start:N>"`) into its offset.
fn parse_start_placeholder(s: &str) -> Result<i64, GenericError> {
    s.strip_prefix("<start:")
        .and_then(|rest| rest.strip_suffix('>'))
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| generic_error!("Invalid golden span start '{}'; expected '<start:N>'.", s))
}

fn golden_attributes(attrs: BTreeMap<u32, GoldenAnyValue>) -> Result<std::collections::HashMap<u32, idx::AnyValue>, GenericError> {
    attrs
        .into_iter()
        .map(|(k, v)| Ok((k, v.into_proto()?)))
        .collect()
}

impl GoldenAnyValue {
    fn into_proto(self) -> Result<idx::AnyValue, GenericError> {
        let mut variants = Vec::new();
        if let Some(v) = self.string_value_ref {
            variants.push(ProtoValue::StringValueRef(v));
        }
        if let Some(v) = self.bool_value {
            variants.push(ProtoValue::BoolValue(v));
        }
        if let Some(v) = self.double_value {
            variants.push(ProtoValue::DoubleValue(v));
        }
        if let Some(v) = self.int_value {
            variants.push(ProtoValue::IntValue(v));
        }
        if let Some(v) = self.bytes_value {
            variants.push(ProtoValue::BytesValue(v));
        }
        if let Some(v) = self.array_value {
            let mut array = idx::ArrayValue::new();
            array.values = v
                .values
                .into_iter()
                .map(GoldenAnyValue::into_proto)
                .collect::<Result<_, _>>()?;
            variants.push(ProtoValue::ArrayValue(array));
        }
        if let Some(v) = self.key_value_list {
            let mut list = idx::KeyValueList::new();
            for kv in v.key_values {
                let mut entry = idx::KeyValue::new();
                entry.key = kv.key;
                entry.value = MessageField::some(kv.value.into_proto()?);
                list.keyValues.push(entry);
            }
            variants.push(ProtoValue::KeyValueList(list));
        }
        if variants.len() > 1 {
            return Err(generic_error!("Golden AnyValue sets more than one value."));
        }
        let mut value = idx::AnyValue::new();
        value.value = variants.pop();
        Ok(value)
    }
}

impl GoldenAgentPayload {
    /// Converts the golden protojson model into the protobuf message.
    ///
    /// Span starts carry the already-normalized golden offset, bit-cast from `i64` to `u64`.
    pub(super) fn into_proto(self) -> Result<AgentPayload, GenericError> {
        let mut p = AgentPayload::new();
        p.hostName = self.host_name;
        p.env = self.env;
        p.tracerPayloads = vec![Default::default(); self.tracer_payloads.len()];
        p.tags = self.tags.into_iter().collect();
        p.agentVersion = self.agent_version;
        p.targetTPS = self.target_tps;
        p.errorTPS = self.error_tps;
        p.rareSamplerEnabled = self.rare_sampler_enabled;
        for tp in self.idx_tracer_payloads {
            let mut out = idx::TracerPayload::new();
            out.strings = tp.strings;
            out.containerIDRef = tp.container_id_ref;
            out.languageNameRef = tp.language_name_ref;
            out.languageVersionRef = tp.language_version_ref;
            out.tracerVersionRef = tp.tracer_version_ref;
            out.runtimeIDRef = tp.runtime_id_ref;
            out.envRef = tp.env_ref;
            out.hostnameRef = tp.hostname_ref;
            out.appVersionRef = tp.app_version_ref;
            out.attributes = golden_attributes(tp.attributes)?;
            for c in tp.chunks {
                let mut chunk = idx::TraceChunk::new();
                chunk.priority = c.priority;
                chunk.originRef = c.origin_ref;
                chunk.attributes = golden_attributes(c.attributes)?;
                chunk.droppedTrace = c.dropped_trace;
                chunk.traceID = c.trace_id;
                chunk.samplingMechanism = c.sampling_mechanism;
                for s in c.spans {
                    let mut span = idx::Span::new();
                    span.serviceRef = s.service_ref;
                    span.nameRef = s.name_ref;
                    span.resourceRef = s.resource_ref;
                    span.spanID = s.span_id;
                    span.parentID = s.parent_id;
                    span.start = parse_start_placeholder(&s.start)? as u64;
                    span.duration = s.duration;
                    span.error = s.error;
                    span.attributes = golden_attributes(s.attributes)?;
                    span.typeRef = s.type_ref;
                    span.envRef = s.env_ref;
                    span.versionRef = s.version_ref;
                    span.componentRef = s.component_ref;
                    let kind = SPAN_KINDS
                        .iter()
                        .find(|(name, _)| *name == s.kind)
                        .map(|(_, k)| *k)
                        .ok_or_else(|| generic_error!("Unknown golden span kind '{}'.", s.kind))?;
                    span.kind = EnumOrUnknown::new(kind);
                    for l in s.links {
                        let mut link = idx::SpanLink::new();
                        link.traceID = l.trace_id;
                        link.spanID = l.span_id;
                        link.attributes = golden_attributes(l.attributes)?;
                        link.tracestateRef = l.tracestate_ref;
                        link.flags = l.flags;
                        span.links.push(link);
                    }
                    for e in s.events {
                        let mut event = idx::SpanEvent::new();
                        event.time = e.time;
                        event.nameRef = e.name_ref;
                        event.attributes = golden_attributes(e.attributes)?;
                        span.events.push(event);
                    }
                    chunk.spans.push(span);
                }
                out.chunks.push(chunk);
            }
            p.idxTracerPayloads.push(out);
        }
        Ok(p)
    }
}

// --- Semantic model ---------------------------------------------------------------------------------------------

/// A totally ordered `f64`, so that values can live in sorted collections.
#[derive(Clone, Copy, PartialEq)]
struct F64(f64);

impl Eq for F64 {}

impl PartialOrd for F64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for F64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl fmt::Debug for F64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An attribute value with every string ref resolved.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Value {
    Unset,
    String(String),
    Bool(bool),
    Double(F64),
    Int(i64),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    KeyValueList(Vec<(String, Value)>),
}

type Attributes = BTreeMap<String, Value>;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Link {
    trace_id: (u64, u64),
    span_id: u64,
    attributes: Attributes,
    tracestate: String,
    flags: u32,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Event {
    /// Absolute event time, compared exactly (the Go goldens don't normalize it).
    time: u64,
    name: String,
    attributes: Attributes,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Span {
    span_id: u64,
    parent_id: u64,
    service: String,
    name: String,
    resource: String,
    span_type: String,
    env: String,
    version: String,
    component: String,
    /// Start offset from the payload build clock, rounded.
    start_offset: i64,
    duration: u64,
    error: bool,
    kind: i32,
    attributes: Attributes,
    links: Vec<Link>,
    events: Vec<Event>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TracerMeta {
    lang: String,
    lang_version: String,
    tracer_version: String,
    env: String,
    hostname: String,
    app_version: String,
    container_id: String,
    runtime_id: String,
    attributes: Attributes,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Chunk {
    tracer: TracerMeta,
    priority: i32,
    origin: String,
    attributes: Attributes,
    dropped_trace: bool,
    sampling_mechanism: u32,
    trace_id: (u64, u64),
    /// Spans, sorted by span ID.
    spans: Vec<Span>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct AgentMeta {
    host_name: String,
    env: String,
    agent_version: String,
    target_tps: F64,
    error_tps: F64,
    rare_sampler_enabled: bool,
    tags: BTreeMap<String, String>,
    /// Number of classic (non-idx) tracer payloads; expected to be `0`.
    classic_tracer_payloads: usize,
}

/// The semantic form of a set of trace payloads.
///
/// Every string table ref is resolved, so ref numbering and string table order don't matter. Span starts are
/// normalized to rounded offsets from the payload build clock. Chunks are kept as a sorted multiset, each carrying its
/// tracer payload metadata, so chunk order is ignored and chunks may be regrouped across tracer payloads or requests.
/// Agent-level metadata is kept as a set of the distinct values seen across payloads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TraceSummary {
    agents: BTreeSet<AgentMeta>,
    chunks: Vec<Chunk>,
}

/// Normalizes an absolute span start to a rounded offset from `epoch`.
///
/// This reproduces the Go harness formula exactly: `off = start - epoch; off = (off + R/2) / R * R`, using
/// truncating integer division. Because truncation rounds toward zero, negative offsets don't round to the nearest
/// multiple: both `-2s` and `-1.99s` normalize to `-1.9s`. Matching that quirk is what makes actual output comparable
/// with the goldens.
pub fn normalize_start(start: u64, epoch_unix_nanos: u64, rounding_ns: i64) -> i64 {
    let off = (start as i64).wrapping_sub(epoch_unix_nanos as i64);
    (off + rounding_ns / 2) / rounding_ns * rounding_ns
}

/// Splits a big-endian trace ID into its high and low halves. Shorter IDs are right-aligned, so an empty ID equals
/// sixteen zero bytes; longer IDs keep their last 16 bytes.
fn split_trace_id(id: &[u8]) -> (u64, u64) {
    let mut buf = [0u8; 16];
    if id.len() >= 16 {
        buf.copy_from_slice(&id[id.len() - 16..]);
    } else {
        buf[16 - id.len()..].copy_from_slice(id);
    }
    let mut high = [0u8; 8];
    let mut low = [0u8; 8];
    high.copy_from_slice(&buf[..8]);
    low.copy_from_slice(&buf[8..]);
    (u64::from_be_bytes(high), u64::from_be_bytes(low))
}

struct Resolver<'a> {
    strings: &'a [String],
    path: String,
    errors: &'a mut Mismatches,
}

impl Resolver<'_> {
    fn string(&mut self, field: &str, r: u32) -> String {
        match self.strings.get(r as usize) {
            Some(s) => s.clone(),
            None => {
                self.errors.push(
                    format!("{}.{}", self.path, field),
                    format!("a string table ref below {}", self.strings.len()),
                    format!("ref {}", r),
                );
                String::new()
            }
        }
    }

    fn value(&mut self, v: &idx::AnyValue) -> Value {
        match &v.value {
            None => Value::Unset,
            Some(ProtoValue::StringValueRef(r)) => Value::String(self.string("stringValueRef", *r)),
            Some(ProtoValue::BoolValue(b)) => Value::Bool(*b),
            Some(ProtoValue::DoubleValue(d)) => Value::Double(F64(*d)),
            Some(ProtoValue::IntValue(i)) => Value::Int(*i),
            Some(ProtoValue::BytesValue(b)) => Value::Bytes(b.clone()),
            Some(ProtoValue::ArrayValue(a)) => Value::Array(a.values.iter().map(|v| self.value(v)).collect()),
            Some(ProtoValue::KeyValueList(l)) => Value::KeyValueList(
                l.keyValues
                    .iter()
                    .map(|kv| {
                        let key = self.string("keyValueList.key", kv.key);
                        let value = kv.value.as_ref().map_or(Value::Unset, |v| self.value(v));
                        (key, value)
                    })
                    .collect(),
            ),
            Some(_) => Value::Unset,
        }
    }

    fn attributes(&mut self, attrs: &std::collections::HashMap<u32, idx::AnyValue>) -> Attributes {
        attrs
            .iter()
            .map(|(k, v)| (self.string("attributes.key", *k), self.value(v)))
            .collect()
    }
}

impl TraceSummary {
    fn build(payloads: &[AgentPayload], start: impl Fn(u64) -> i64, opts: &CompareOptions) -> Result<Self, Mismatches> {
        let mut errors = Mismatches::default();
        let mut summary = Self::default();
        for (pi, p) in payloads.iter().enumerate() {
            summary.agents.insert(AgentMeta {
                host_name: p.hostName.clone(),
                env: p.env.clone(),
                agent_version: if opts.ignore_agent_version {
                    String::new()
                } else {
                    p.agentVersion.clone()
                },
                target_tps: F64(p.targetTPS),
                error_tps: F64(p.errorTPS),
                rare_sampler_enabled: p.rareSamplerEnabled,
                tags: p.tags.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                classic_tracer_payloads: p.tracerPayloads.len(),
            });
            for (ti, tp) in p.idxTracerPayloads.iter().enumerate() {
                let mut r = Resolver {
                    strings: &tp.strings,
                    path: format!("payload[{}].idxTracerPayloads[{}]", pi, ti),
                    errors: &mut errors,
                };
                let tracer = TracerMeta {
                    lang: r.string("languageNameRef", tp.languageNameRef),
                    lang_version: r.string("languageVersionRef", tp.languageVersionRef),
                    tracer_version: r.string("tracerVersionRef", tp.tracerVersionRef),
                    env: r.string("envRef", tp.envRef),
                    hostname: r.string("hostnameRef", tp.hostnameRef),
                    app_version: r.string("appVersionRef", tp.appVersionRef),
                    container_id: r.string("containerIDRef", tp.containerIDRef),
                    runtime_id: r.string("runtimeIDRef", tp.runtimeIDRef),
                    attributes: r.attributes(&tp.attributes),
                };
                for c in &tp.chunks {
                    let mut spans: Vec<Span> = c
                        .spans
                        .iter()
                        .map(|s| Span {
                            span_id: s.spanID,
                            parent_id: s.parentID,
                            service: r.string("serviceRef", s.serviceRef),
                            name: r.string("nameRef", s.nameRef),
                            resource: r.string("resourceRef", s.resourceRef),
                            span_type: r.string("typeRef", s.typeRef),
                            env: r.string("envRef", s.envRef),
                            version: r.string("versionRef", s.versionRef),
                            component: r.string("componentRef", s.componentRef),
                            start_offset: start(s.start),
                            duration: s.duration,
                            error: s.error,
                            kind: s.kind.value(),
                            attributes: r.attributes(&s.attributes),
                            links: s
                                .links
                                .iter()
                                .map(|l| Link {
                                    trace_id: split_trace_id(&l.traceID),
                                    span_id: l.spanID,
                                    attributes: r.attributes(&l.attributes),
                                    tracestate: r.string("tracestateRef", l.tracestateRef),
                                    flags: l.flags,
                                })
                                .collect(),
                            events: s
                                .events
                                .iter()
                                .map(|e| Event {
                                    time: e.time,
                                    name: r.string("event.nameRef", e.nameRef),
                                    attributes: r.attributes(&e.attributes),
                                })
                                .collect(),
                        })
                        .collect();
                    spans.sort();
                    summary.chunks.push(Chunk {
                        tracer: tracer.clone(),
                        priority: c.priority,
                        origin: r.string("originRef", c.originRef),
                        attributes: r.attributes(&c.attributes),
                        dropped_trace: c.droppedTrace,
                        sampling_mechanism: c.samplingMechanism,
                        trace_id: split_trace_id(&c.traceID),
                        spans,
                    });
                }
            }
        }
        summary.chunks.sort();
        errors.into_result().map(|()| summary)
    }

    /// Builds a summary from actual trace payloads.
    ///
    /// `epoch_unix_nanos` is the clock the input payload was built against; span starts are normalized to offsets
    /// from it (see [`normalize_start`]).
    ///
    /// # Errors
    ///
    /// Returns a mismatch for every string table ref that is out of range.
    pub fn from_payloads(
        payloads: &[AgentPayload], epoch_unix_nanos: u64, opts: &CompareOptions,
    ) -> Result<Self, Mismatches> {
        let rounding = opts.start_rounding_ns;
        Self::build(payloads, |s| normalize_start(s, epoch_unix_nanos, rounding), opts)
    }

    pub(super) fn from_golden_payloads(
        goldens: Vec<GoldenAgentPayload>, opts: &CompareOptions,
    ) -> Result<Self, GenericError> {
        let payloads = goldens
            .into_iter()
            .map(GoldenAgentPayload::into_proto)
            .collect::<Result<Vec<_>, _>>()?;
        // Golden starts already hold the normalized offset.
        Self::build(&payloads, |s| s as i64, opts).map_err(|e| generic_error!("Invalid golden trace payload: {}", e))
    }

    /// Returns the number of trace chunks in the summary.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

fn span_ids(c: &Chunk) -> Vec<u64> {
    c.spans.iter().map(|s| s.span_id).collect()
}

fn diff_attributes(out: &mut Mismatches, path: &str, e: &Attributes, a: &Attributes) {
    for (k, ev) in e {
        out.check(format!("{}.attributes[{:?}]", path, k), &Some(ev), &a.get(k));
    }
    for (k, av) in a {
        if !e.contains_key(k) {
            out.check(format!("{}.attributes[{:?}]", path, k), &None, &Some(av));
        }
    }
}

fn diff_chunk(out: &mut Mismatches, path: &str, e: &Chunk, a: &Chunk) {
    let t = format!("{}.tracer", path);
    out.check(format!("{}.lang", t), &e.tracer.lang, &a.tracer.lang);
    out.check(format!("{}.lang_version", t), &e.tracer.lang_version, &a.tracer.lang_version);
    out.check(format!("{}.tracer_version", t), &e.tracer.tracer_version, &a.tracer.tracer_version);
    out.check(format!("{}.env", t), &e.tracer.env, &a.tracer.env);
    out.check(format!("{}.hostname", t), &e.tracer.hostname, &a.tracer.hostname);
    out.check(format!("{}.app_version", t), &e.tracer.app_version, &a.tracer.app_version);
    out.check(format!("{}.container_id", t), &e.tracer.container_id, &a.tracer.container_id);
    out.check(format!("{}.runtime_id", t), &e.tracer.runtime_id, &a.tracer.runtime_id);
    diff_attributes(out, &t, &e.tracer.attributes, &a.tracer.attributes);
    out.check(format!("{}.priority", path), &e.priority, &a.priority);
    out.check(format!("{}.origin", path), &e.origin, &a.origin);
    diff_attributes(out, path, &e.attributes, &a.attributes);
    out.check(format!("{}.dropped_trace", path), &e.dropped_trace, &a.dropped_trace);
    out.check(
        format!("{}.sampling_mechanism", path),
        &e.sampling_mechanism,
        &a.sampling_mechanism,
    );
    out.check(format!("{}.trace_id", path), &e.trace_id, &a.trace_id);

    for es in &e.spans {
        let sp = format!("{}.span[{}]", path, es.span_id);
        let Some(s) = a.spans.iter().find(|s| s.span_id == es.span_id) else {
            out.push(sp, "span present", "span missing");
            continue;
        };
        out.check(format!("{}.parent_id", sp), &es.parent_id, &s.parent_id);
        out.check(format!("{}.service", sp), &es.service, &s.service);
        out.check(format!("{}.name", sp), &es.name, &s.name);
        out.check(format!("{}.resource", sp), &es.resource, &s.resource);
        out.check(format!("{}.type", sp), &es.span_type, &s.span_type);
        out.check(format!("{}.env", sp), &es.env, &s.env);
        out.check(format!("{}.version", sp), &es.version, &s.version);
        out.check(format!("{}.component", sp), &es.component, &s.component);
        out.check(format!("{}.start_offset", sp), &es.start_offset, &s.start_offset);
        out.check(format!("{}.duration", sp), &es.duration, &s.duration);
        out.check(format!("{}.error", sp), &es.error, &s.error);
        out.check(format!("{}.kind", sp), &es.kind, &s.kind);
        diff_attributes(out, &sp, &es.attributes, &s.attributes);
        out.check(format!("{}.links", sp), &es.links, &s.links);
        out.check(format!("{}.events", sp), &es.events, &s.events);
    }
    for s in &a.spans {
        if !e.spans.iter().any(|es| es.span_id == s.span_id) {
            out.push(format!("{}.span[{}]", path, s.span_id), "no span", "unexpected span");
        }
    }
}

/// Compares two trace summaries.
///
/// Agent-level metadata must match as a set. Chunks must match exactly as a multiset; an unmatched expected chunk is
/// diffed field by field against an unmatched actual chunk with the same (or, failing that, overlapping) span IDs,
/// and any others are reported as missing or unexpected.
///
/// # Errors
///
/// Returns every difference found.
pub fn compare_traces(expected: &TraceSummary, actual: &TraceSummary, opts: &CompareOptions) -> Result<(), Mismatches> {
    let mut out = Mismatches::default();

    let strip = |agents: &BTreeSet<AgentMeta>| -> BTreeSet<AgentMeta> {
        agents
            .iter()
            .cloned()
            .map(|mut a| {
                if opts.ignore_agent_version {
                    a.agent_version.clear();
                }
                a
            })
            .collect()
    };
    let (ea, aa) = (strip(&expected.agents), strip(&actual.agents));
    if ea != aa {
        if let (1, 1) = (ea.len(), aa.len()) {
            let (e, a) = (ea.first().unwrap(), aa.first().unwrap());
            out.check("agent.host_name", &e.host_name, &a.host_name);
            out.check("agent.env", &e.env, &a.env);
            out.check("agent.agent_version", &e.agent_version, &a.agent_version);
            out.check("agent.target_tps", &e.target_tps, &a.target_tps);
            out.check("agent.error_tps", &e.error_tps, &a.error_tps);
            out.check("agent.rare_sampler_enabled", &e.rare_sampler_enabled, &a.rare_sampler_enabled);
            out.check("agent.tags", &e.tags, &a.tags);
            out.check(
                "agent.classic_tracer_payloads",
                &e.classic_tracer_payloads,
                &a.classic_tracer_payloads,
            );
        } else {
            out.check("agent", &ea, &aa);
        }
    }

    // Remove exact matches first, then pair the remainder by span IDs for a field-level diff.
    let mut remaining: Vec<&Chunk> = actual.chunks.iter().collect();
    let mut unmatched = Vec::new();
    for e in &expected.chunks {
        match remaining.iter().position(|a| *a == e) {
            Some(i) => {
                remaining.remove(i);
            }
            None => unmatched.push(e),
        }
    }
    for e in unmatched {
        let path = format!("chunk{:?}", span_ids(e));
        // Prefer a chunk with identical span IDs, then one sharing any span ID.
        let ids = span_ids(e);
        let position = remaining
            .iter()
            .position(|a| span_ids(a) == ids)
            .or_else(|| remaining.iter().position(|a| span_ids(a).iter().any(|id| ids.contains(id))));
        match position {
            Some(i) => {
                let a = remaining.remove(i);
                diff_chunk(&mut out, &path, e, a);
            }
            None => out.push(path, "chunk present", "chunk missing"),
        }
    }
    for a in remaining {
        out.push(format!("chunk{:?}", span_ids(a)), "no chunk", "unexpected chunk");
    }
    out.into_result()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{compare::WireGolden, golden_wire_path};

    pub(crate) const EPOCH: u64 = 1_700_000_000_123_456_789;

    #[derive(Default)]
    struct Strings(Vec<String>);

    impl Strings {
        fn r(&mut self, s: &str) -> u32 {
            if let Some(i) = self.0.iter().position(|x| x == s) {
                return i as u32;
            }
            self.0.push(s.to_string());
            (self.0.len() - 1) as u32
        }
    }

    fn string_value(r: u32) -> idx::AnyValue {
        let mut v = idx::AnyValue::new();
        v.value = Some(ProtoValue::StringValueRef(r));
        v
    }

    fn double_value(d: f64) -> idx::AnyValue {
        let mut v = idx::AnyValue::new();
        v.value = Some(ProtoValue::DoubleValue(d));
        v
    }

    /// Hand-builds the trace payload the `default` case should produce, with a string table in insertion order
    /// (unlike the sorted golden table) and absolute span starts relative to `EPOCH`.
    pub(crate) fn default_agent_payload() -> AgentPayload {
        let mut st = Strings::default();
        // Interleave some strings so the table order differs from the golden's sorted order.
        st.r("");
        st.r("web.request");
        st.r("prod");

        let mut root = idx::Span::new();
        root.spanID = 1;
        root.serviceRef = st.r("web");
        root.nameRef = st.r("web.request");
        root.resourceRef = st.r("GET /home");
        root.typeRef = st.r("web");
        root.start = EPOCH - 2_000_000_000;
        root.duration = 100_000_000;
        root.attributes.insert(st.r("http.method"), string_value(st.r("GET")));
        root.attributes.insert(st.r("http.status_code"), string_value(st.r("200")));
        root.attributes.insert(st.r("http.url"), string_value(st.r("http://example.com/home")));
        root.attributes.insert(st.r("_sampling_priority_rate_v1"), double_value(1.0));
        root.attributes.insert(st.r("_top_level"), double_value(1.0));

        let mut child = idx::Span::new();
        child.spanID = 2;
        child.parentID = 1;
        child.serviceRef = st.r("web-db");
        child.nameRef = st.r("sql.query");
        child.resourceRef = st.r("SELECT * FROM users");
        child.typeRef = st.r("sql");
        child.start = EPOCH - 1_990_000_000;
        child.duration = 50_000_000;
        child.attributes.insert(st.r("db.system"), string_value(st.r("postgres")));
        child.attributes.insert(st.r("_top_level"), double_value(1.0));

        let mut chunk = idx::TraceChunk::new();
        chunk.priority = 1;
        // Spans out of order: ordering must not matter.
        chunk.spans = vec![child, root];
        chunk.traceID = vec![0; 16];

        let mut tp = idx::TracerPayload::new();
        tp.languageNameRef = st.r("python");
        tp.languageVersionRef = st.r("3.12.0");
        tp.tracerVersionRef = st.r("2.0.0");
        tp.envRef = st.r("prod");
        tp.appVersionRef = st.r("1.2.3");
        tp.chunks.push(chunk);
        tp.strings = st.0;

        let mut p = AgentPayload::new();
        p.hostName = "agent-host".into();
        p.env = "none".into();
        p.agentVersion = "7.99.0".into();
        p.targetTPS = 10.0;
        p.errorTPS = 10.0;
        p.idxTracerPayloads.push(tp);
        p
    }

    fn golden(case: &str) -> TraceSummary {
        let wire = WireGolden::from_json(&std::fs::read_to_string(golden_wire_path(case)).unwrap()).unwrap();
        wire.trace_summary(&CompareOptions::default()).unwrap()
    }

    fn compare_default(p: AgentPayload) -> Result<(), Mismatches> {
        let opts = CompareOptions::default();
        let actual = TraceSummary::from_payloads(&[p], EPOCH, &opts).unwrap();
        compare_traces(&golden("default"), &actual, &opts)
    }

    fn expect_mismatch(result: Result<(), Mismatches>, needle: &str) {
        let err = result.expect_err("comparison should fail");
        assert!(
            err.0.iter().any(|m| m.to_string().contains(needle)),
            "no mismatch containing {:?} in:\n{}",
            needle,
            err
        );
    }

    #[test]
    fn every_wire_golden_trace_payload_parses() {
        for case in crate::cases::load_bundled_cases().unwrap() {
            let summary = golden(&case.name);
            // Neither of these cases sends a trace request.
            if case.name == "user_drop" || case.name == "no_priority" {
                assert_eq!(summary.chunk_count(), 0);
            } else {
                assert!(summary.chunk_count() > 0, "case {}", case.name);
            }
        }
    }

    #[test]
    fn default_case_proto_matches_golden() {
        compare_default(default_agent_payload()).unwrap();
    }

    #[test]
    fn changed_attribute_value_is_reported() {
        let mut p = default_agent_payload();
        let tp = &mut p.idxTracerPayloads[0];
        tp.strings.push("POST".into());
        let post = (tp.strings.len() - 1) as u32;
        let method = tp.strings.iter().position(|s| s == "http.method").unwrap() as u32;
        let root = tp.chunks[0].spans.iter_mut().find(|s| s.spanID == 1).unwrap();
        root.attributes.insert(method, string_value(post));
        expect_mismatch(compare_default(p), "span[1].attributes[\"http.method\"]");
    }

    #[test]
    fn missing_span_is_reported() {
        let mut p = default_agent_payload();
        p.idxTracerPayloads[0].chunks[0].spans.retain(|s| s.spanID != 2);
        expect_mismatch(compare_default(p), "span[2]: expected span present, got span missing");
    }

    #[test]
    fn shifted_start_is_reported() {
        let mut p = default_agent_payload();
        p.idxTracerPayloads[0].chunks[0].spans[0].start += 200_000_000;
        expect_mismatch(compare_default(p), "start_offset");
    }

    #[test]
    fn changed_agent_metadata_is_reported() {
        let mut p = default_agent_payload();
        p.targetTPS = 5.0;
        expect_mismatch(compare_default(p), "agent.target_tps");
    }

    #[test]
    fn out_of_range_ref_is_reported() {
        let mut p = default_agent_payload();
        p.idxTracerPayloads[0].chunks[0].spans[0].nameRef = 9999;
        let err = TraceSummary::from_payloads(&[p], EPOCH, &CompareOptions::default()).unwrap_err();
        assert!(err.to_string().contains("nameRef"), "{}", err);
    }

    #[test]
    fn start_rounding_matches_go() {
        let r = 100_000_000;
        for (offset, expected) in [
            (-2_000_000_000i64, -1_900_000_000i64),
            (-1_990_000_000, -1_900_000_000),
            (40_000_000, 0),
            (-140_000_000, 0),
            (160_000_000, 200_000_000),
        ] {
            let start = (EPOCH as i64 + offset) as u64;
            assert_eq!(normalize_start(start, EPOCH, r), expected, "offset {}", offset);
        }
    }

    #[test]
    fn trace_ids_are_right_aligned() {
        assert_eq!(split_trace_id(&[]), (0, 0));
        assert_eq!(split_trace_id(&[0; 16]), (0, 0));
        assert_eq!(split_trace_id(&[1]), (0, 1));
    }
}
