//! Traces.

use std::sync::Arc;

use saluki_common::collections::FastHashMap;
use stringtheory::MetaString;

/// Typed value for attributes at every level of the trace model: span attributes,
/// span event attributes, span link attributes, and trace-level attributes.
///
/// Covers all variants carried by the V1 APM idx wire format (`RawAnyValue`).
#[derive(Clone, Debug, PartialEq)]
pub enum AttributeValue {
    /// String-valued attribute.
    String(MetaString),
    /// Boolean attribute.
    Bool(bool),
    /// Integer attribute.
    Int(i64),
    /// Floating-point attribute.
    Float(f64),
    /// Raw bytes attribute.
    Bytes(Vec<u8>),
    /// Array of attribute values (may be heterogeneous).
    Array(Vec<AttributeValue>),
    /// List of key-value pairs.
    KeyValueList(Vec<(MetaString, AttributeValue)>),
}

impl AttributeValue {
    /// Returns the inner string if this is a `String` variant.
    pub fn as_string(&self) -> Option<&MetaString> {
        if let AttributeValue::String(s) = self {
            Some(s)
        } else {
            None
        }
    }

    /// Returns the inner float if this is a `Float` variant.
    pub fn as_float(&self) -> Option<f64> {
        if let AttributeValue::Float(f) = self {
            Some(*f)
        } else {
            None
        }
    }

    /// Returns the inner bytes if this is a `Bytes` variant.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        if let AttributeValue::Bytes(b) = self {
            Some(b)
        } else {
            None
        }
    }
}

/// Payload-level metadata promoted from the tracer payload or OTLP resource.
///
/// These fields are common to all chunks within a single tracer payload and describe
/// the tracer and its environment rather than any individual trace or span.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PayloadFields {
    /// Container ID associated with the tracer.
    pub container_id: MetaString,
    /// Tracer language name (e.g. `"go"`, `"python"`).
    pub language_name: MetaString,
    /// Tracer language runtime version.
    pub language_version: MetaString,
    /// Tracer library version.
    pub tracer_version: MetaString,
    /// Tracer runtime ID.
    pub runtime_id: MetaString,
    /// Deployment environment (e.g. `"production"`, `"staging"`).
    pub env: MetaString,
    /// Hostname of the tracer host.
    pub hostname: MetaString,
    /// Application version string.
    pub app_version: MetaString,
    /// Per-chunk weight from `Datadog-Client-Dropped-P0-Traces` header. Zero if absent.
    pub client_dropped_p0s_weight: f64,
}

/// A trace event.
///
/// A trace is a collection of spans that represent a distributed trace.
#[derive(Clone, Debug, PartialEq)]
pub struct Trace {
    // ── Core fields ──────────────────────────────────────────────────────────────
    /// The spans that make up this trace.
    spans: Vec<Span>,

    // ── Unified fields (public) ──────────────────────────────────────────────────
    /// Upper 8 bytes of the 128-bit trace ID (big-endian). Zero for 64-bit-only sources.
    pub trace_id_high: u64,
    /// Lower 8 bytes of the 128-bit trace ID (big-endian).
    pub trace_id_low: u64,
    /// Trace origin string (e.g. `"lambda"`, `"rum"`).
    pub origin: MetaString,

    /// Payload-level metadata (promoted from the tracer payload or OTLP resource).
    pub payload: PayloadFields,

    /// Chunk-level or resource-level attributes (replaces `resource_tags` and
    /// `V1TraceChunk.attributes` once downstream consumers are migrated).
    pub attributes: Arc<FastHashMap<MetaString, AttributeValue>>,

    // Flat sampling fields.
    /// Sampling priority set by the tracer or a sampler.
    pub priority: Option<i32>,
    /// Whether this trace was dropped during sampling.
    pub dropped_trace: bool,
    /// Sampling mechanism identifier (see Datadog trace agent constants).
    pub sampling_mechanism: u32,
    /// Identifier of the component that made the final sampling decision.
    pub decision_maker: Option<MetaString>,
    /// Effective OTLP sampling rate (`_dd.otlp_sr`), if set.
    pub otlp_sampling_rate: Option<f64>,
}

impl Trace {
    /// Creates a new `Trace` with the given spans.
    ///
    /// All unified fields default to empty / zero. Callers should set them
    /// directly after construction.
    pub fn new(spans: Vec<Span>) -> Self {
        Self {
            spans,
            trace_id_high: 0,
            trace_id_low: 0,
            origin: MetaString::empty(),
            payload: PayloadFields::default(),
            attributes: Arc::new(FastHashMap::default()),
            priority: None,
            dropped_trace: false,
            sampling_mechanism: 0,
            decision_maker: None,
            otlp_sampling_rate: None,
        }
    }

    /// Returns a reference to the spans in this trace.
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// Returns a mutable reference to the spans in this trace.
    pub fn spans_mut(&mut self) -> &mut [Span] {
        &mut self.spans
    }

    /// Replaces the spans in this trace with the given spans.
    pub fn set_spans(&mut self, spans: Vec<Span>) {
        self.spans = spans;
    }

    /// Retains only the spans specified by the predicate.
    ///
    /// Returns the number of spans retained. If no spans match, the trace is left unchanged.
    pub fn retain_spans<F>(&mut self, mut f: F) -> usize
    where
        F: FnMut(&Trace, &Span) -> bool,
    {
        if self.spans.is_empty() {
            return 0;
        }

        let mut has_match = false;
        for span in self.spans.iter() {
            if f(self, span) {
                has_match = true;
                break;
            }
        }

        if !has_match {
            return 0;
        }

        let mut spans = std::mem::take(&mut self.spans);
        spans.retain(|span| f(self, span));
        spans.shrink_to_fit();
        let _ = std::mem::replace(&mut self.spans, spans);

        self.spans.len()
    }

    /// Remove spans only the spans specified by the predicate return true.
    pub fn remove_spans<F>(&mut self, mut f: F)
    where
        F: FnMut(&Trace, &Span) -> bool,
    {
        if self.spans.is_empty() {
            return;
        }

        let mut spans = std::mem::take(&mut self.spans);
        spans.retain(|span| !f(self, span));
        spans.shrink_to_fit();
        let _ = std::mem::replace(&mut self.spans, spans);
    }
}

/// A span event.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Span {
    /// The name of the service associated with this span.
    service: MetaString,
    /// The operation name of this span.
    name: MetaString,
    /// The resource associated with this span.
    resource: MetaString,
    /// The unique identifier of this span.
    span_id: u64,
    /// The identifier of this span's parent, if any.
    parent_id: u64,
    /// The start timestamp of this span in nanoseconds since Unix epoch.
    start: u64,
    /// The duration of this span in nanoseconds.
    duration: u64,
    /// Error flag represented as 0 (no error) or 1 (error).
    error: i32,
    /// Span type classification (for example, web, db, lambda).
    span_type: MetaString,
    /// Links describing relationships to other spans.
    span_links: Vec<SpanLink>,
    /// Events associated with this span.
    span_events: Vec<SpanEvent>,

    // ── New V1 / unified fields ──────────────────────────────────────────────────
    /// Per-span environment override (V1 path). Overrides `Trace.payload.env` when non-empty.
    pub env: MetaString,
    /// Per-span application version (V1 path).
    pub version: MetaString,
    /// Instrumentation component name (V1 path).
    pub component: MetaString,
    /// Span kind (OTEL values): 0=unspecified, 1=internal, 2=server, 3=client, 4=producer, 5=consumer.
    pub kind: u32,
    /// Typed span-level attributes (replaces `meta`, `metrics`, and `meta_struct`).
    pub attributes: FastHashMap<MetaString, AttributeValue>,
}

impl Span {
    /// Creates a new `Span` with all required fields.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        service: impl Into<MetaString>, name: impl Into<MetaString>, resource: impl Into<MetaString>,
        span_type: impl Into<MetaString>, span_id: u64, parent_id: u64, start: u64, duration: u64, error: i32,
    ) -> Self {
        Self {
            service: service.into(),
            name: name.into(),
            resource: resource.into(),
            span_type: span_type.into(),
            span_id,
            parent_id,
            start,
            duration,
            error,
            ..Self::default()
        }
    }

    /// Sets the service name.
    pub fn with_service(mut self, service: impl Into<MetaString>) -> Self {
        self.service = service.into();
        self
    }

    /// Sets the operation name.
    pub fn with_name(mut self, name: impl Into<MetaString>) -> Self {
        self.name = name.into();
        self
    }

    /// Sets the resource name.
    pub fn with_resource(mut self, resource: impl Into<MetaString>) -> Self {
        self.resource = resource.into();
        self
    }

    /// Sets the span identifier.
    pub fn with_span_id(mut self, span_id: u64) -> Self {
        self.span_id = span_id;
        self
    }

    /// Sets the parent span identifier.
    pub fn with_parent_id(mut self, parent_id: u64) -> Self {
        self.parent_id = parent_id;
        self
    }

    /// Sets the start timestamp.
    pub fn with_start(mut self, start: u64) -> Self {
        self.start = start;
        self
    }

    /// Sets the span duration.
    pub fn with_duration(mut self, duration: u64) -> Self {
        self.duration = duration;
        self
    }

    /// Sets the error flag.
    pub fn with_error(mut self, error: i32) -> Self {
        self.error = error;
        self
    }

    /// Sets the span type (for example, web, db, lambda).
    pub fn with_span_type(mut self, span_type: impl Into<MetaString>) -> Self {
        self.span_type = span_type.into();
        self
    }

    /// Replaces the span attributes map.
    pub fn with_attributes(mut self, attributes: FastHashMap<MetaString, AttributeValue>) -> Self {
        self.attributes = attributes;
        self
    }

    /// Replaces the span links collection.
    pub fn with_span_links(mut self, span_links: impl Into<Option<Vec<SpanLink>>>) -> Self {
        self.span_links = span_links.into().unwrap_or_default();
        self
    }

    /// Replaces the span events collection.
    pub fn with_span_events(mut self, span_events: impl Into<Option<Vec<SpanEvent>>>) -> Self {
        self.span_events = span_events.into().unwrap_or_default();
        self
    }

    /// Sets the per-span environment override.
    pub fn with_env(mut self, env: impl Into<MetaString>) -> Self {
        self.env = env.into();
        self
    }

    /// Sets the per-span application version.
    pub fn with_version(mut self, version: impl Into<MetaString>) -> Self {
        self.version = version.into();
        self
    }

    /// Sets the instrumentation component.
    pub fn with_component(mut self, component: impl Into<MetaString>) -> Self {
        self.component = component.into();
        self
    }

    /// Sets the span kind.
    pub fn with_kind(mut self, kind: u32) -> Self {
        self.kind = kind;
        self
    }

    /// Returns the service name.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Returns the operation name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the resource name.
    pub fn resource(&self) -> &str {
        &self.resource
    }

    /// Sets the resource name.
    pub fn set_resource(&mut self, resource: impl Into<MetaString>) {
        self.resource = resource.into();
    }

    /// Returns the span identifier.
    pub fn span_id(&self) -> u64 {
        self.span_id
    }

    /// Returns the parent span identifier.
    pub fn parent_id(&self) -> u64 {
        self.parent_id
    }

    /// Returns the start timestamp.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Returns the span duration.
    pub fn duration(&self) -> u64 {
        self.duration
    }

    /// Returns the error flag.
    pub fn error(&self) -> i32 {
        self.error
    }

    /// Returns the span type.
    pub fn span_type(&self) -> &str {
        &self.span_type
    }

    /// Returns the span links collection.
    pub fn span_links(&self) -> &[SpanLink] {
        &self.span_links
    }

    /// Returns the span events collection.
    pub fn span_events(&self) -> &[SpanEvent] {
        &self.span_events
    }
}

/// A link between spans describing a causal relationship.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SpanLink {
    /// Trace identifier for the linked span.
    trace_id: u64,
    /// High bits of the trace identifier when 128-bit IDs are used.
    trace_id_high: u64,
    /// Span identifier for the linked span.
    span_id: u64,
    /// Additional attributes attached to the link.
    attributes: FastHashMap<MetaString, AttributeValue>,
    /// W3C tracestate value.
    tracestate: MetaString,
    /// W3C trace flags where the high bit must be set when provided.
    flags: u32,
}

impl SpanLink {
    /// Creates a new span link for the provided identifiers.
    pub fn new(trace_id: u64, span_id: u64) -> Self {
        Self {
            trace_id,
            span_id,
            ..Self::default()
        }
    }

    /// Sets the trace identifier.
    pub fn with_trace_id(mut self, trace_id: u64) -> Self {
        self.trace_id = trace_id;
        self
    }

    /// Sets the high bits of the trace identifier.
    pub fn with_trace_id_high(mut self, trace_id_high: u64) -> Self {
        self.trace_id_high = trace_id_high;
        self
    }

    /// Sets the span identifier.
    pub fn with_span_id(mut self, span_id: u64) -> Self {
        self.span_id = span_id;
        self
    }

    /// Replaces the attributes map.
    pub fn with_attributes(mut self, attributes: impl Into<Option<FastHashMap<MetaString, AttributeValue>>>) -> Self {
        self.attributes = attributes.into().unwrap_or_default();
        self
    }

    /// Sets the W3C tracestate value.
    pub fn with_tracestate(mut self, tracestate: impl Into<MetaString>) -> Self {
        self.tracestate = tracestate.into();
        self
    }

    /// Sets the W3C trace flags.
    pub fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    /// Returns the trace identifier.
    pub fn trace_id(&self) -> u64 {
        self.trace_id
    }

    /// Returns the high bits of the trace identifier.
    pub fn trace_id_high(&self) -> u64 {
        self.trace_id_high
    }

    /// Returns the span identifier.
    pub fn span_id(&self) -> u64 {
        self.span_id
    }

    /// Returns the attributes map.
    pub fn attributes(&self) -> &FastHashMap<MetaString, AttributeValue> {
        &self.attributes
    }

    /// Returns the W3C tracestate value.
    pub fn tracestate(&self) -> &str {
        &self.tracestate
    }

    /// Returns the W3C trace flags.
    pub fn flags(&self) -> u32 {
        self.flags
    }
}

/// An event associated with a span.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SpanEvent {
    /// Event timestamp in nanoseconds since Unix epoch.
    time_unix_nano: u64,
    /// Event name.
    name: MetaString,
    /// Arbitrary attributes describing the event.
    attributes: FastHashMap<MetaString, AttributeValue>,
}

impl SpanEvent {
    /// Creates a new span event with the given timestamp and name.
    pub fn new(time_unix_nano: u64, name: impl Into<MetaString>) -> Self {
        Self {
            time_unix_nano,
            name: name.into(),
            ..Self::default()
        }
    }

    /// Sets the event timestamp.
    pub fn with_time_unix_nano(mut self, time_unix_nano: u64) -> Self {
        self.time_unix_nano = time_unix_nano;
        self
    }

    /// Sets the event name.
    pub fn with_name(mut self, name: impl Into<MetaString>) -> Self {
        self.name = name.into();
        self
    }

    /// Replaces the attributes map.
    pub fn with_attributes(
        mut self, attributes: impl Into<Option<FastHashMap<MetaString, AttributeValue>>>,
    ) -> Self {
        self.attributes = attributes.into().unwrap_or_default();
        self
    }

    /// Returns the event timestamp.
    pub fn time_unix_nano(&self) -> u64 {
        self.time_unix_nano
    }

    /// Returns the event name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the attributes map.
    pub fn attributes(&self) -> &FastHashMap<MetaString, AttributeValue> {
        &self.attributes
    }
}
