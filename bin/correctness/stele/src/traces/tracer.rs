use datadog_protos::traces as proto;
use datadog_protos::traces::idx as idx_proto;
use ordered_float::OrderedFloat;
use protobuf::CodedInputStream;
use saluki_common::collections::FastHashMap;
use serde::{Deserialize, Serialize};
use stringtheory::MetaString;

#[derive(Clone, Debug, Eq, PartialEq)]
struct WrappedFloat(OrderedFloat<f64>);

impl From<f64> for WrappedFloat {
    fn from(value: f64) -> Self {
        WrappedFloat(OrderedFloat(value))
    }
}

impl<'de> Deserialize<'de> for WrappedFloat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        Ok(WrappedFloat(OrderedFloat(value)))
    }
}

impl Serialize for WrappedFloat {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0 .0.serialize(serializer)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct AgentMetadata {
    hostname: MetaString,
    env: MetaString,
    tags: FastHashMap<MetaString, MetaString>,
    agent_version: MetaString,
    target_tps: WrappedFloat,
    error_tps: WrappedFloat,
    rare_sampler_enabled: bool,
}

impl From<&proto::AgentPayload> for AgentMetadata {
    fn from(payload: &proto::AgentPayload) -> Self {
        Self {
            hostname: (*payload.hostName).into(),
            env: (*payload.env).into(),
            tags: payload.tags.iter().map(|(k, v)| ((**k).into(), (**v).into())).collect(),
            agent_version: (*payload.agentVersion).into(),
            target_tps: payload.targetTPS.into(),
            error_tps: payload.errorTPS.into(),
            rare_sampler_enabled: payload.rareSamplerEnabled,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct TracerMetadata {
    container_id: MetaString,
    language_name: MetaString,
    language_version: MetaString,
    tracer_version: MetaString,
    runtime_id: MetaString,
    tags: FastHashMap<MetaString, MetaString>,
    env: MetaString,
    hostname: MetaString,
    app_version: MetaString,
}

impl From<&proto::TracerPayload> for TracerMetadata {
    fn from(payload: &proto::TracerPayload) -> Self {
        Self {
            container_id: (*payload.containerID).into(),
            language_name: (*payload.languageName).into(),
            language_version: (*payload.languageVersion).into(),
            tracer_version: (*payload.tracerVersion).into(),
            runtime_id: (*payload.runtimeID).into(),
            tags: payload.tags.iter().map(|(k, v)| ((**k).into(), (**v).into())).collect(),
            env: (*payload.env).into(),
            hostname: (*payload.hostname).into(),
            app_version: (*payload.appVersion).into(),
        }
    }
}

impl TracerMetadata {
    fn from_idx_payload(payload: &idx_proto::TracerPayload) -> Self {
        let strings = &payload.strings;
        Self {
            container_id: resolve_ref(strings, payload.containerIDRef),
            language_name: resolve_ref(strings, payload.languageNameRef),
            language_version: resolve_ref(strings, payload.languageVersionRef),
            tracer_version: resolve_ref(strings, payload.tracerVersionRef),
            runtime_id: resolve_ref(strings, payload.runtimeIDRef),
            tags: string_attrs_from_idx(&payload.attributes, strings),
            env: resolve_ref(strings, payload.envRef),
            hostname: resolve_ref(strings, payload.hostnameRef),
            app_version: resolve_ref(strings, payload.appVersionRef),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct TraceChunkMetadata {
    priority: i32,
    origin: MetaString,
    tags: FastHashMap<MetaString, MetaString>,
    dropped_trace: bool,
}

impl From<&proto::TraceChunk> for TraceChunkMetadata {
    fn from(value: &proto::TraceChunk) -> Self {
        Self {
            priority: value.priority,
            origin: (*value.origin).into(),
            tags: value.tags.iter().map(|(k, v)| ((**k).into(), (**v).into())).collect(),
            dropped_trace: value.droppedTrace,
        }
    }
}

impl TraceChunkMetadata {
    fn from_idx_chunk(chunk: &idx_proto::TraceChunk, strings: &[String]) -> Self {
        Self {
            priority: chunk.priority,
            origin: resolve_ref(strings, chunk.originRef),
            tags: string_attrs_from_idx(&chunk.attributes, strings),
            dropped_trace: chunk.droppedTrace,
        }
    }
}

/// A simplified span representation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Span {
    agent_metadata: AgentMetadata,
    tracer_metadata: TracerMetadata,
    trace_chunk_metadata: TraceChunkMetadata,
    service: MetaString,
    name: MetaString,
    resource: MetaString,
    trace_id: u64,
    span_id: u64,
    parent_id: u64,
    start: i64,
    duration: i64,
    error: i32,
    meta: FastHashMap<MetaString, MetaString>,
    metrics: FastHashMap<MetaString, WrappedFloat>,
    type_: MetaString,
    meta_struct: FastHashMap<MetaString, Vec<u8>>,
    span_links: Vec<SpanLink>,
    span_events: Vec<SpanEvent>,
}

impl Span {
    /// Returns the trace ID this span belongs to.
    pub fn trace_id(&self) -> u64 {
        self.trace_id
    }

    /// Returns the ID of this span.
    pub fn span_id(&self) -> u64 {
        self.span_id
    }

    /// Returns the value of the metadata entry of the given key, if it exists.
    pub fn get_meta_field(&self, meta_key: &str) -> Option<&str> {
        self.meta.get(meta_key).map(|s| &**s)
    }

    /// Gets all spans from the given `AgentPayload`, reading from the classic `tracerPayloads`
    /// field (proto field 5).
    pub fn get_spans_from_agent_payload(payload: &proto::AgentPayload) -> Vec<Self> {
        let agent_metadata = AgentMetadata::from(payload);

        let mut spans = Vec::new();
        for tracer_payload in payload.tracerPayloads() {
            let tracer_metadata = TracerMetadata::from(tracer_payload);

            for trace_chunk in tracer_payload.chunks() {
                let trace_chunk_metadata = TraceChunkMetadata::from(trace_chunk);

                for span in trace_chunk.spans() {
                    let span = Self::from_proto(
                        agent_metadata.clone(),
                        tracer_metadata.clone(),
                        trace_chunk_metadata.clone(),
                        span,
                    );
                    spans.push(span);
                }
            }
        }

        spans
    }

    /// Gets all spans from the raw AgentPayload bytes by decoding the idx `idxTracerPayloads`
    /// field (proto field 11).
    ///
    /// The generated `AgentPayload` struct has the wrong Rust type for field 11 (it falls back
    /// to the classic `TracerPayload` because the idx types were not in the same codegen
    /// invocation), so this function reads field 11 directly from the raw wire bytes using
    /// `CodedInputStream` and decodes each message as the correct `idx::TracerPayload` type.
    pub fn get_spans_from_idx_bytes(payload: &proto::AgentPayload, body: &[u8]) -> Vec<Self> {
        let agent_metadata = AgentMetadata::from(payload);
        let mut spans = Vec::new();

        // AgentPayload.idxTracerPayloads = field 11, wire type LEN (2) → tag = 90
        const IDX_TRACER_PAYLOADS_TAG: u32 = (11 << 3) | 2;

        let mut is = CodedInputStream::from_bytes(body);
        while let Ok(Some(tag)) = is.read_raw_tag_or_eof() {
            if tag == IDX_TRACER_PAYLOADS_TAG {
                let idx_payload: idx_proto::TracerPayload = match is.read_message() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let strings = &idx_payload.strings;
                let tracer_metadata = TracerMetadata::from_idx_payload(&idx_payload);

                for chunk in &idx_payload.chunks {
                    let trace_chunk_metadata = TraceChunkMetadata::from_idx_chunk(chunk, strings);
                    let trace_id = trace_id_low_from_bytes(&chunk.traceID);

                    for span in &chunk.spans {
                        spans.push(Self::from_idx_proto(
                            agent_metadata.clone(),
                            tracer_metadata.clone(),
                            trace_chunk_metadata.clone(),
                            span,
                            trace_id,
                            strings,
                        ));
                    }
                }
            } else {
                let _ = protobuf::rt::skip_field_for_tag(tag, &mut is);
            }
        }

        spans
    }

    fn from_proto(
        agent_metadata: AgentMetadata, tracer_metadata: TracerMetadata, trace_chunk_metadata: TraceChunkMetadata,
        value: &proto::Span,
    ) -> Self {
        let mut span_links = value.spanLinks.iter().map(SpanLink::from).collect::<Vec<_>>();
        span_links.sort_by_key(|link| (link.trace_id, link.trace_id_high, link.span_id));

        let mut span_events = value.spanEvents.iter().map(SpanEvent::from).collect::<Vec<_>>();
        span_events.sort_by_key(|event| event.time_unix_nano);

        Self {
            agent_metadata,
            tracer_metadata,
            trace_chunk_metadata,
            service: (*value.service).into(),
            name: (*value.name).into(),
            resource: (*value.resource).into(),
            trace_id: value.traceID,
            span_id: value.spanID,
            parent_id: value.parentID,
            start: value.start,
            duration: value.duration,
            error: value.error,
            meta: value.meta.iter().map(|(k, v)| ((**k).into(), (**v).into())).collect(),
            metrics: value.metrics.iter().map(|(k, v)| ((**k).into(), (*v).into())).collect(),
            type_: (*value.type_).into(),
            meta_struct: value
                .meta_struct
                .iter()
                .map(|(k, v)| ((**k).into(), v.to_vec()))
                .collect(),
            span_links,
            span_events,
        }
    }

    fn from_idx_proto(
        agent_metadata: AgentMetadata, tracer_metadata: TracerMetadata, trace_chunk_metadata: TraceChunkMetadata,
        span: &idx_proto::Span, trace_id: u64, strings: &[String],
    ) -> Self {
        let (meta, metrics, meta_struct) = split_idx_span_attributes(&span.attributes, strings);

        let mut span_links = span
            .links
            .iter()
            .map(|l| SpanLink::from_idx(l, strings))
            .collect::<Vec<_>>();
        span_links.sort_by_key(|link| (link.trace_id, link.trace_id_high, link.span_id));

        let mut span_events = span
            .events
            .iter()
            .map(|e| SpanEvent::from_idx(e, strings))
            .collect::<Vec<_>>();
        span_events.sort_by_key(|event| event.time_unix_nano);

        Self {
            agent_metadata,
            tracer_metadata,
            trace_chunk_metadata,
            service: resolve_ref(strings, span.serviceRef),
            name: resolve_ref(strings, span.nameRef),
            resource: resolve_ref(strings, span.resourceRef),
            trace_id,
            span_id: span.spanID,
            parent_id: span.parentID,
            start: span.start as i64,
            duration: span.duration as i64,
            error: i32::from(span.error),
            meta,
            metrics,
            type_: resolve_ref(strings, span.typeRef),
            meta_struct,
            span_links,
            span_events,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SpanLink {
    trace_id: u64,
    trace_id_high: u64,
    span_id: u64,
    attributes: FastHashMap<MetaString, MetaString>,
    tracestate: MetaString,
    flags: u32,
}

impl From<&proto::SpanLink> for SpanLink {
    fn from(value: &proto::SpanLink) -> Self {
        Self {
            trace_id: value.traceID,
            trace_id_high: value.traceID_high,
            span_id: value.spanID,
            attributes: value
                .attributes
                .iter()
                .map(|(k, v)| ((**k).into(), (**v).into()))
                .collect(),
            tracestate: (*value.tracestate).into(),
            flags: value.flags,
        }
    }
}

impl SpanLink {
    fn from_idx(link: &idx_proto::SpanLink, strings: &[String]) -> Self {
        let (trace_id, trace_id_high) = trace_id_parts_from_bytes(&link.traceID);
        Self {
            trace_id,
            trace_id_high,
            span_id: link.spanID,
            attributes: string_attrs_from_idx(&link.attributes, strings),
            tracestate: resolve_ref(strings, link.tracestateRef),
            flags: link.flags,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SpanEvent {
    time_unix_nano: u64,
    name: MetaString,
    attributes: FastHashMap<MetaString, AttributeAnyValue>,
}

impl From<&proto::SpanEvent> for SpanEvent {
    fn from(value: &proto::SpanEvent) -> Self {
        Self {
            time_unix_nano: value.time_unix_nano,
            name: (*value.name).into(),
            attributes: value
                .attributes
                .iter()
                .map(|(k, v)| ((**k).into(), AttributeAnyValue::from(v)))
                .collect(),
        }
    }
}

impl SpanEvent {
    fn from_idx(event: &idx_proto::SpanEvent, strings: &[String]) -> Self {
        Self {
            // The idx proto renamed time_unix_nano to `time` (same semantics).
            time_unix_nano: event.time,
            name: resolve_ref(strings, event.nameRef),
            attributes: event
                .attributes
                .iter()
                .filter_map(|(k_ref, v)| {
                    let attr = idx_anyvalue_to_event_attr(v, strings)?;
                    Some((resolve_ref(strings, *k_ref), attr))
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum AttributeAnyValue {
    String(MetaString),
    Boolean(bool),
    Integer(i64),
    Double(WrappedFloat),
    Array(Vec<AttributeArrayValue>),
}

impl From<&proto::AttributeAnyValue> for AttributeAnyValue {
    fn from(value: &proto::AttributeAnyValue) -> Self {
        let maybe_proto_value_type = value.type_.enum_value().expect("unknown/invalid anyvalue type");
        match maybe_proto_value_type {
            proto::AttributeAnyValueType::STRING_VALUE => AttributeAnyValue::String((*value.string_value).into()),
            proto::AttributeAnyValueType::BOOL_VALUE => AttributeAnyValue::Boolean(value.bool_value),
            proto::AttributeAnyValueType::INT_VALUE => AttributeAnyValue::Integer(value.int_value),
            proto::AttributeAnyValueType::DOUBLE_VALUE => AttributeAnyValue::Double(value.double_value.into()),
            proto::AttributeAnyValueType::ARRAY_VALUE => {
                let mut values = Vec::new();
                if let Some(array_value) = value.array_value.as_ref() {
                    for value in array_value.values.iter() {
                        values.push(AttributeArrayValue::from(value));
                    }
                }
                AttributeAnyValue::Array(values)
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum AttributeArrayValue {
    String(MetaString),
    Boolean(bool),
    Integer(i64),
    Double(WrappedFloat),
}

impl From<&proto::AttributeArrayValue> for AttributeArrayValue {
    fn from(value: &proto::AttributeArrayValue) -> Self {
        let maybe_proto_value_type = value.type_.enum_value().expect("unknown/invalid arrayvalue type");
        match maybe_proto_value_type {
            proto::AttributeArrayValueType::STRING_VALUE => AttributeArrayValue::String((*value.string_value).into()),
            proto::AttributeArrayValueType::BOOL_VALUE => AttributeArrayValue::Boolean(value.bool_value),
            proto::AttributeArrayValueType::INT_VALUE => AttributeArrayValue::Integer(value.int_value),
            proto::AttributeArrayValueType::DOUBLE_VALUE => AttributeArrayValue::Double(value.double_value.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// idx helpers
// ---------------------------------------------------------------------------

/// Split return type for [`split_idx_span_attributes`]: (meta, metrics, meta_struct).
type SpanAttributeSplit = (
    FastHashMap<MetaString, MetaString>,
    FastHashMap<MetaString, WrappedFloat>,
    FastHashMap<MetaString, Vec<u8>>,
);

/// Resolves a string table reference to a `MetaString`.
fn resolve_ref(strings: &[String], r: u32) -> MetaString {
    strings
        .get(r as usize)
        .map(|s| MetaString::from(s.as_str()))
        .unwrap_or_default()
}

/// Extracts the low and high 64-bit halves from a 16-byte big-endian trace ID.
///
/// Returns `(trace_id_low, trace_id_high)`. Both are zero when the byte slice is not 16 bytes.
fn trace_id_parts_from_bytes(bytes: &[u8]) -> (u64, u64) {
    if bytes.len() == 16 {
        let high = u64::from_be_bytes(bytes[..8].try_into().unwrap_or([0u8; 8]));
        let low = u64::from_be_bytes(bytes[8..].try_into().unwrap_or([0u8; 8]));
        (low, high)
    } else {
        (0, 0)
    }
}

/// Returns only the low 64 bits of a 16-byte big-endian trace ID.
fn trace_id_low_from_bytes(bytes: &[u8]) -> u64 {
    trace_id_parts_from_bytes(bytes).0
}

/// Collects only the string-valued entries from an idx attribute map.
fn string_attrs_from_idx(
    attrs: &std::collections::HashMap<u32, idx_proto::AnyValue>, strings: &[String],
) -> FastHashMap<MetaString, MetaString> {
    attrs
        .iter()
        .filter_map(|(k_ref, v)| {
            let val = match &v.value {
                Some(idx_proto::any_value::Value::StringValueRef(r)) => resolve_ref(strings, *r),
                _ => return None,
            };
            Some((resolve_ref(strings, *k_ref), val))
        })
        .collect()
}

/// Splits an idx span attribute map into the three stele maps: meta (string), metrics (float),
/// and meta_struct (bytes).
fn split_idx_span_attributes(
    attrs: &std::collections::HashMap<u32, idx_proto::AnyValue>, strings: &[String],
) -> SpanAttributeSplit {
    let mut meta = FastHashMap::default();
    let mut metrics = FastHashMap::default();
    let mut meta_struct = FastHashMap::default();

    for (k_ref, v) in attrs {
        let key = resolve_ref(strings, *k_ref);
        match &v.value {
            Some(idx_proto::any_value::Value::StringValueRef(r)) => {
                meta.insert(key, resolve_ref(strings, *r));
            }
            Some(idx_proto::any_value::Value::DoubleValue(f)) => {
                metrics.insert(key, WrappedFloat(OrderedFloat(*f)));
            }
            Some(idx_proto::any_value::Value::IntValue(i)) => {
                metrics.insert(key, WrappedFloat(OrderedFloat(*i as f64)));
            }
            Some(idx_proto::any_value::Value::BoolValue(b)) => {
                meta.insert(key, MetaString::from(if *b { "true" } else { "false" }));
            }
            Some(idx_proto::any_value::Value::BytesValue(b)) => {
                meta_struct.insert(key, b.clone());
            }
            _ => {}
        }
    }
    (meta, metrics, meta_struct)
}

/// Converts an idx `AnyValue` to a stele `AttributeAnyValue` for use in span events.
fn idx_anyvalue_to_event_attr(v: &idx_proto::AnyValue, strings: &[String]) -> Option<AttributeAnyValue> {
    match &v.value {
        Some(idx_proto::any_value::Value::StringValueRef(r)) => {
            Some(AttributeAnyValue::String(resolve_ref(strings, *r)))
        }
        Some(idx_proto::any_value::Value::BoolValue(b)) => Some(AttributeAnyValue::Boolean(*b)),
        Some(idx_proto::any_value::Value::IntValue(i)) => Some(AttributeAnyValue::Integer(*i)),
        Some(idx_proto::any_value::Value::DoubleValue(f)) => {
            Some(AttributeAnyValue::Double(WrappedFloat(OrderedFloat(*f))))
        }
        Some(idx_proto::any_value::Value::ArrayValue(arr)) => {
            let values = arr
                .values
                .iter()
                .filter_map(|inner| idx_anyvalue_to_array_attr(inner, strings))
                .collect();
            Some(AttributeAnyValue::Array(values))
        }
        _ => None,
    }
}

/// Converts an idx `AnyValue` to a stele `AttributeArrayValue` for use inside array-typed
/// span event attributes.
fn idx_anyvalue_to_array_attr(v: &idx_proto::AnyValue, strings: &[String]) -> Option<AttributeArrayValue> {
    match &v.value {
        Some(idx_proto::any_value::Value::StringValueRef(r)) => {
            Some(AttributeArrayValue::String(resolve_ref(strings, *r)))
        }
        Some(idx_proto::any_value::Value::BoolValue(b)) => Some(AttributeArrayValue::Boolean(*b)),
        Some(idx_proto::any_value::Value::IntValue(i)) => Some(AttributeArrayValue::Integer(*i)),
        Some(idx_proto::any_value::Value::DoubleValue(f)) => {
            Some(AttributeArrayValue::Double(WrappedFloat(OrderedFloat(*f))))
        }
        _ => None,
    }
}
