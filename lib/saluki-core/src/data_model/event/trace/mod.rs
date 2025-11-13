//! Traces.

use saluki_common::collections::FastHashMap;
use stringtheory::MetaString;

/// A trace event.
///
/// A trace is a collection of spans that represent a distributed trace.
#[derive(Clone, Debug, PartialEq)]
pub struct Trace {
    /// The spans that make up this trace.
    spans: Vec<Span>,
}

impl Trace {
    /// Creates a new `Trace` with the given spans.
    pub fn new(spans: Vec<Span>) -> Self {
        Self { spans }
    }

    /// Returns a reference to the spans in this trace.
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// Returns a mutable reference to the spans in this trace.
    pub fn spans_mut(&mut self) -> &mut Vec<Span> {
        &mut self.spans
    }
}

/// A span event.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Span {
    /// The name of the service associated with this span.
    pub service: MetaString,
    /// The operation name of this span.
    pub name: MetaString,
    /// The resource associated with this span.
    pub resource: MetaString,
    /// The trace identifier this span belongs to.
    pub trace_id: u64,
    /// The unique identifier of this span.
    pub span_id: u64,
    /// The identifier of this span's parent, if any.
    pub parent_id: u64,
    /// The start timestamp of this span in nanoseconds since Unix epoch.
    pub start: i64,
    /// The duration of this span in nanoseconds.
    pub duration: i64,
    /// Error flag represented as 0 (no error) or 1 (error).
    pub error: i32,
    /// String-valued tags attached to this span.
    pub meta: FastHashMap<MetaString, MetaString>,
    /// Numeric-valued tags attached to this span.
    pub metrics: FastHashMap<MetaString, f64>,
    /// Span type classification (e.g., web, db, lambda).
    pub span_type: MetaString,
    /// Structured metadata payloads.
    pub meta_struct: FastHashMap<MetaString, Vec<u8>>,
    /// Links describing relationships to other spans.
    pub span_links: Vec<SpanLink>,
    /// Events associated with this span.
    pub span_events: Vec<SpanEvent>,
}

impl Span {
    /// Creates a new `Span` with the required identifiers and names.
    pub fn new(
        service: impl Into<MetaString>, name: impl Into<MetaString>, resource: impl Into<MetaString>, trace_id: u64,
        span_id: u64,
    ) -> Self {
        Self {
            service: service.into(),
            name: name.into(),
            resource: resource.into(),
            trace_id,
            span_id,
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

    /// Sets the trace identifier.
    pub fn with_trace_id(mut self, trace_id: u64) -> Self {
        self.trace_id = trace_id;
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
    pub fn with_start(mut self, start: i64) -> Self {
        self.start = start;
        self
    }

    /// Sets the span duration.
    pub fn with_duration(mut self, duration: i64) -> Self {
        self.duration = duration;
        self
    }

    /// Sets the error flag.
    pub fn with_error(mut self, error: i32) -> Self {
        self.error = error;
        self
    }

    /// Sets the span type (e.g., web, db, lambda).
    pub fn with_span_type(mut self, span_type: impl Into<MetaString>) -> Self {
        self.span_type = span_type.into();
        self
    }

    /// Replaces the string-valued tag map.
    pub fn with_meta(mut self, meta: impl Into<Option<FastHashMap<MetaString, MetaString>>>) -> Self {
        self.meta = meta.into().unwrap_or_default();
        self
    }

    /// Replaces the numeric-valued tag map.
    pub fn with_metrics(mut self, metrics: impl Into<Option<FastHashMap<MetaString, f64>>>) -> Self {
        self.metrics = metrics.into().unwrap_or_default();
        self
    }

    /// Replaces the structured metadata map.
    pub fn with_meta_struct(mut self, meta_struct: impl Into<Option<FastHashMap<MetaString, Vec<u8>>>>) -> Self {
        self.meta_struct = meta_struct.into().unwrap_or_default();
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

    /// Returns the trace identifier.
    pub fn trace_id(&self) -> u64 {
        self.trace_id
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
    pub fn start(&self) -> i64 {
        self.start
    }

    /// Returns the span duration.
    pub fn duration(&self) -> i64 {
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

    /// Returns the string-valued tag map.
    pub fn meta(&self) -> &FastHashMap<MetaString, MetaString> {
        &self.meta
    }

    /// Returns the numeric-valued tag map.
    pub fn metrics(&self) -> &FastHashMap<MetaString, f64> {
        &self.metrics
    }

    /// Returns the structured metadata map.
    pub fn meta_struct(&self) -> &FastHashMap<MetaString, Vec<u8>> {
        &self.meta_struct
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
    pub trace_id: u64,
    /// High bits of the trace identifier when 128-bit IDs are used.
    pub trace_id_high: u64,
    /// Span identifier for the linked span.
    pub span_id: u64,
    /// Additional attributes attached to the link.
    pub attributes: FastHashMap<MetaString, MetaString>,
    /// W3C tracestate value.
    pub tracestate: MetaString,
    /// W3C trace flags where the high bit must be set when provided.
    pub flags: u32,
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
    pub fn with_attributes(mut self, attributes: impl Into<Option<FastHashMap<MetaString, MetaString>>>) -> Self {
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
    pub fn attributes(&self) -> &FastHashMap<MetaString, MetaString> {
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
    pub time_unix_nano: u64,
    /// Event name.
    pub name: MetaString,
    /// Arbitrary attributes describing the event.
    pub attributes: FastHashMap<MetaString, AttributeValue>,
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
}

/// Values supported for span and event attributes.
#[derive(Clone, Debug, PartialEq)]
pub enum AttributeValue {
    /// String attribute value.
    String(MetaString),
    /// Boolean attribute value.
    Bool(bool),
    /// Integer attribute value.
    Int(i64),
    /// Floating-point attribute value.
    Double(f64),
    /// Array attribute values.
    Array(Vec<AttributeScalarValue>),
}

/// Scalar values supported inside attribute arrays.
#[derive(Clone, Debug, PartialEq)]
pub enum AttributeScalarValue {
    /// String array value.
    String(MetaString),
    /// Boolean array value.
    Bool(bool),
    /// Integer array value.
    Int(i64),
    /// Floating-point array value.
    Double(f64),
}
