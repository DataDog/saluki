use std::collections::HashMap;

use datadog_protos::traces as proto;
use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};

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
    hostname: String,
    env: String,
    tags: HashMap<String, String>,
    agent_version: String,
    target_tps: WrappedFloat,
    error_tps: WrappedFloat,
    rare_sampler_enabled: bool,
}

impl From<&proto::AgentPayload> for AgentMetadata {
    fn from(payload: &proto::AgentPayload) -> Self {
        Self {
            hostname: payload.hostName.to_string(),
            env: payload.env.to_string(),
            tags: payload
                .tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            agent_version: payload.agentVersion.to_string(),
            target_tps: payload.targetTPS.into(),
            error_tps: payload.errorTPS.into(),
            rare_sampler_enabled: payload.rareSamplerEnabled,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct TracerMetadata {
    container_id: String,
    language_name: String,
    language_version: String,
    tracer_version: String,
    runtime_id: String,
    tags: HashMap<String, String>,
    env: String,
    hostname: String,
    app_version: String,
}

impl From<&proto::TracerPayload> for TracerMetadata {
    fn from(payload: &proto::TracerPayload) -> Self {
        Self {
            container_id: payload.containerID.to_string(),
            language_name: payload.languageName.to_string(),
            language_version: payload.languageVersion.to_string(),
            tracer_version: payload.tracerVersion.to_string(),
            runtime_id: payload.runtimeID.to_string(),
            tags: payload
                .tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            env: payload.env.to_string(),
            hostname: payload.hostname.to_string(),
            app_version: payload.appVersion.to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct TraceChunkMetadata {
    priority: i32,
    origin: String,
    tags: HashMap<String, String>,
    dropped_trace: bool,
}

impl From<&proto::TraceChunk> for TraceChunkMetadata {
    fn from(value: &proto::TraceChunk) -> Self {
        Self {
            priority: value.priority,
            origin: value.origin.to_string(),
            tags: value.tags.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            dropped_trace: value.droppedTrace,
        }
    }
}

/// A simplified span representation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Span {
    agent_metadata: AgentMetadata,
    tracer_metadata: TracerMetadata,
    trace_chunk_metadata: TraceChunkMetadata,
    service: String,
    name: String,
    resource: String,
    trace_id: u64,
    span_id: u64,
    parent_id: u64,
    start: i64,
    duration: i64,
    error: i32,
    meta: HashMap<String, String>,
    metrics: HashMap<String, WrappedFloat>,
    type_: String,
    meta_struct: HashMap<String, Vec<u8>>,
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
        self.meta.get(meta_key).map(|s| s.as_str())
    }

    /// Gets all spans from the given `AgentPayload`.
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
            service: value.service.to_string(),
            name: value.name.to_string(),
            resource: value.resource.to_string(),
            trace_id: value.traceID,
            span_id: value.spanID,
            parent_id: value.parentID,
            start: value.start,
            duration: value.duration,
            error: value.error,
            meta: value.meta.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            metrics: value
                .metrics
                .iter()
                .map(|(k, v)| (k.to_string(), (*v).into()))
                .collect(),
            type_: value.type_.to_string(),
            meta_struct: value
                .meta_struct
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_vec()))
                .collect(),
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
    attributes: HashMap<String, String>,
    tracestate: String,
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
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            tracestate: value.tracestate.to_string(),
            flags: value.flags,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SpanEvent {
    time_unix_nano: u64,
    name: String,
    attributes: HashMap<String, AttributeAnyValue>,
}

impl From<&proto::SpanEvent> for SpanEvent {
    fn from(value: &proto::SpanEvent) -> Self {
        Self {
            time_unix_nano: value.time_unix_nano,
            name: value.name.to_string(),
            attributes: value
                .attributes
                .iter()
                .map(|(k, v)| (k.to_string(), AttributeAnyValue::from(v)))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum AttributeAnyValue {
    String(String),
    Boolean(bool),
    Integer(i64),
    Double(WrappedFloat),
    Array(Vec<AttributeArrayValue>),
}

impl From<&proto::AttributeAnyValue> for AttributeAnyValue {
    fn from(value: &proto::AttributeAnyValue) -> Self {
        let maybe_proto_value_type = value.type_.enum_value().expect("unknown/invalid anyvalue type");
        match maybe_proto_value_type {
            proto::AttributeAnyValueType::STRING_VALUE => AttributeAnyValue::String(value.string_value.to_string()),
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
    String(String),
    Boolean(bool),
    Integer(i64),
    Double(WrappedFloat),
}

impl From<&proto::AttributeArrayValue> for AttributeArrayValue {
    fn from(value: &proto::AttributeArrayValue) -> Self {
        let maybe_proto_value_type = value.type_.enum_value().expect("unknown/invalid arrayvalue type");
        match maybe_proto_value_type {
            proto::AttributeArrayValueType::STRING_VALUE => AttributeArrayValue::String(value.string_value.to_string()),
            proto::AttributeArrayValueType::BOOL_VALUE => AttributeArrayValue::Boolean(value.bool_value),
            proto::AttributeArrayValueType::INT_VALUE => AttributeArrayValue::Integer(value.int_value),
            proto::AttributeArrayValueType::DOUBLE_VALUE => AttributeArrayValue::Double(value.double_value.into()),
        }
    }
}
