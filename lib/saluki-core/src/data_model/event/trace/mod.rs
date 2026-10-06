//! Traces.

use std::sync::Arc;

use libdd_trace_model::{TraceText, ValueMap, ValueTypes};
use saluki_common::collections::FastHashMap;
use stringtheory::MetaString;

/// Type marker selecting concrete types for `libdd_trace_model::ValueTypes`
pub enum SalukiValues {}

impl ValueTypes for SalukiValues {
    type Text = MetaString;
    type Bytes = Vec<u8>;
    type Array = Vec<AttributeValue>;
    type Map = Vec<(MetaString, AttributeValue)>;
}

/// The libdd-trace-model AttributeValue using Saluki's concrete types
pub type AttributeValue = libdd_trace_model::AttributeValue<SalukiValues>;

impl ValueMap<SalukiValues> for Vec<(MetaString, AttributeValue)> {
    fn try_get(&self, key: &str) -> Option<&AttributeValue> {
        self.as_slice().iter().find(|(k, _)| k.as_ref() == key).map(|(_, v)| v)
    }

    fn iter<'a>(&'a self) -> impl Iterator<Item = (&'a str, &'a AttributeValue)>
    where
        SalukiValues: 'a,
    {
        self.as_slice().iter().map(|(k, v)| (k.as_str(), v))
    }
}

/// Convenience methods for the libdd-trace-model AttributeValue type
pub trait AttributeValueExt {
    /// Returns the inner string if this is a `String` variant.
    fn as_string(&self) -> Option<&MetaString>;
    /// Returns the inner float if this is a `Float` variant.
    ///
    /// Returns `Some` only when the stored variant is `Float`. For numeric semantic tags
    /// where the source may store an integer (for example, sampling priority, `_sample_rate`,
    /// `_top_level`), use [`as_num`][AttributeValue::as_num] instead.
    fn as_float(&self) -> Option<f64>;
    /// Returns the inner bool if this is a `Bool` variant.
    fn as_bool(&self) -> Option<bool>;
    /// Returns the inner integer if this is an `Int` variant.
    fn as_int(&self) -> Option<i64>;
    /// Returns a numeric value as `f64` for either `Float` or `Int` variants.
    ///
    /// Use this when the caller only needs a number and doesn't care whether
    /// the stored type is integral or floating-point (for example, sampling priority).
    fn as_num(&self) -> Option<f64>;
    /// Returns the inner bytes if this is a `Bytes` variant.
    fn as_bytes(&self) -> Option<&[u8]>;
}

impl AttributeValueExt for AttributeValue {
    fn as_string(&self) -> Option<&MetaString> {
        if let AttributeValue::String(s) = self {
            Some(s)
        } else {
            None
        }
    }

    fn as_float(&self) -> Option<f64> {
        if let AttributeValue::Float(f) = self {
            Some(*f)
        } else {
            None
        }
    }

    fn as_bool(&self) -> Option<bool> {
        if let AttributeValue::Bool(b) = self {
            Some(*b)
        } else {
            None
        }
    }

    fn as_int(&self) -> Option<i64> {
        if let AttributeValue::Int(i) = self {
            Some(*i)
        } else {
            None
        }
    }

    fn as_num(&self) -> Option<f64> {
        match self {
            AttributeValue::Float(f) => Some(*f),
            AttributeValue::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    fn as_bytes(&self) -> Option<&[u8]> {
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
    /// Tracer language name (for example, `"go"`, `"python"`).
    pub language_name: MetaString,
    /// Tracer language runtime version.
    pub language_version: MetaString,
    /// Tracer library version.
    pub tracer_version: MetaString,
    /// Tracer runtime ID.
    pub runtime_id: MetaString,
    /// Deployment environment (for example, `"production"`, `"staging"`).
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
    /// The spans that make up this trace.
    spans: Vec<Span>,
    /// Upper 8 bytes of the 128-bit trace ID (big-endian). Zero for 64-bit-only sources.
    pub trace_id_high: u64,
    /// Lower 8 bytes of the 128-bit trace ID (big-endian).
    pub trace_id_low: u64,
    /// Trace origin string (for example, `"lambda"`, `"rum"`).
    pub origin: MetaString,
    /// Payload-level metadata (promoted from the tracer payload or OTLP resource).
    pub payload: PayloadFields,
    /// Trace chunk-level or resource-level attributes.
    pub attributes: Arc<FastHashMap<MetaString, AttributeValue>>,
    /// Sampling priority set by the tracer or a sampler.
    pub priority: Option<i32>,
    /// Whether this trace was dropped during sampling.
    pub dropped_trace: bool,
    /// The mechanism by which the sampling decision was made.
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
    /// Per-span environment override. Overrides `Trace.payload.env` when non-empty.
    pub env: MetaString,
    /// Per-span application version.
    pub version: MetaString,
    /// Instrumentation component name.
    pub component: MetaString,
    /// Span kind (OTel values): 0=unspecified, 1=internal, 2=server, 3=client, 4=producer, 5=consumer.
    pub kind: u32,
    /// Typed span-level attributes.
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
    pub fn with_attributes(mut self, attributes: impl Into<Option<FastHashMap<MetaString, AttributeValue>>>) -> Self {
        self.attributes = attributes.into().unwrap_or_default();
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

    /// Returns a mutable reference to the span events collection.
    pub fn span_events_mut(&mut self) -> &mut Vec<SpanEvent> {
        &mut self.span_events
    }
}

impl libdd_trace_model::Span for Span {
    fn service(&self) -> &str {
        self.service.as_str()
    }

    fn resource(&self) -> &str {
        Span::resource(self)
    }

    fn r#type(&self) -> &str {
        self.span_type.as_str()
    }

    fn set_service(&mut self, value: impl Into<Self::Text>) {
        self.service = value.into();
    }

    fn set_resource(&mut self, value: impl Into<Self::Text>) {
        self.resource = value.into();
    }
}

impl libdd_trace_model::Attributes for Span {
    type Text = MetaString;
    type Bytes = Vec<u8>;
    type Values = SalukiValues;

    fn attribute(&self, key: &str) -> Option<&libdd_trace_model::Value<Self>> {
        self.attributes.get(key)
    }

    fn retain_attributes(&mut self, f: impl FnMut(&Self::Text, &mut libdd_trace_model::Value<Self>) -> bool) {
        self.attributes.retain(f);
    }

    fn attribute_mut(&mut self, key: &str) -> Option<&mut libdd_trace_model::Value<Self>> {
        self.attributes.get_mut(key)
    }

    fn set_attribute(&mut self, key: impl Into<Self::Text>, value: libdd_trace_model::Value<Self>) {
        self.attributes.insert(key.into(), value);
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
    pub fn with_attributes(mut self, attributes: impl Into<Option<FastHashMap<MetaString, AttributeValue>>>) -> Self {
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

    /// Returns a mutable reference to the attributes map.
    pub fn attributes_mut(&mut self) -> &mut FastHashMap<MetaString, AttributeValue> {
        &mut self.attributes
    }
}
