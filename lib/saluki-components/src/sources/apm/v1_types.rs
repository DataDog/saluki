use stringtheory::MetaString;

/// A chunk of spans belonging to a single trace.
#[derive(Clone, Debug, PartialEq)]
pub struct V1TraceChunk {
    /// Sampling priority for this chunk.
    pub priority: i32,
    /// Trace origin.
    pub origin: MetaString,
    /// Chunk-level attributes.
    pub attributes: Vec<V1KeyValue>,
    /// Spans contained in this chunk.
    pub spans: Vec<V1Span>,
    /// Whether this trace was dropped during sampling.
    pub dropped_trace: bool,
    /// Upper 8 bytes of the 128-bit trace ID (big-endian).
    pub trace_id_high: u64,
    /// Lower 8 bytes of the 128-bit trace ID (big-endian).
    pub trace_id_low: u64,
    /// Sampling mechanism identifier.
    pub sampling_mechanism: u32,
}

/// A single span within a trace chunk.
#[derive(Clone, Debug, PartialEq)]
pub struct V1Span {
    /// Service name.
    pub service: MetaString,
    /// Operation name.
    pub name: MetaString,
    /// Resource name.
    pub resource: MetaString,
    /// Unique identifier of this span.
    pub span_id: u64,
    /// Identifier of this span's parent, or zero if this is a root span.
    pub parent_id: u64,
    /// Start timestamp in nanoseconds since Unix epoch.
    pub start: u64,
    /// Duration in nanoseconds.
    pub duration: u64,
    /// Whether this span recorded an error.
    pub error: bool,
    /// Span-level attributes.
    pub attributes: Vec<V1KeyValue>,
    /// Span type classification (e.g. web, db, cache).
    pub span_type: MetaString,
    /// Links to spans in other traces.
    pub links: Vec<V1SpanLink>,
    /// Timestamped events associated with this span.
    pub events: Vec<V1SpanEvent>,
    /// Per-span environment override.
    pub env: MetaString,
    /// Application version.
    pub version: MetaString,
    /// Instrumentation component.
    pub component: MetaString,
    /// Span kind (OTEL values): 0=unspecified, 1=internal, 2=server, 3=client, 4=producer, 5=consumer.
    pub kind: u32,
}

/// A link from a span to another span in a different trace.
#[derive(Clone, Debug, PartialEq)]
pub struct V1SpanLink {
    /// Upper 8 bytes of the linked trace ID (big-endian).
    pub trace_id_high: u64,
    /// Lower 8 bytes of the linked trace ID (big-endian).
    pub trace_id_low: u64,
    /// Span identifier of the linked span.
    pub span_id: u64,
    /// Attributes attached to the link.
    pub attributes: Vec<V1KeyValue>,
    /// W3C tracestate value.
    pub tracestate: MetaString,
    /// W3C trace flags.
    pub flags: u32,
}

/// A timestamped event associated with a span.
#[derive(Clone, Debug, PartialEq)]
pub struct V1SpanEvent {
    /// Event timestamp in nanoseconds since Unix epoch.
    pub time_unix_nano: u64,
    /// Event name.
    pub name: MetaString,
    /// Event attributes.
    pub attributes: Vec<V1KeyValue>,
}

/// A key-value attribute entry.
#[derive(Clone, Debug, PartialEq)]
pub struct V1KeyValue {
    /// Attribute key.
    pub key: MetaString,
    /// Attribute value.
    pub value: V1AnyValue,
}

/// A typed attribute value.
#[derive(Clone, Debug, PartialEq)]
pub enum V1AnyValue {
    /// String value.
    String(MetaString),
    /// Boolean value.
    Bool(bool),
    /// 64-bit floating-point value.
    Double(f64),
    /// 64-bit signed integer value.
    Int(i64),
    /// Raw byte sequence.
    Bytes(Vec<u8>),
    /// Ordered sequence of values.
    Array(Vec<V1AnyValue>),
    /// Ordered list of key-value pairs.
    KeyValueList(Vec<V1KeyValue>),
}

/// A resolved v1 trace event.
///
/// Carries one [`V1TraceChunk`] with all string fields resolved to [`MetaString`], plus
/// payload-level metadata promoted from the originating tracer payload.
#[derive(Clone, Debug, PartialEq)]
pub struct V1Trace {
    /// The chunk of spans for one trace.
    pub chunk: V1TraceChunk,
    /// Container ID.
    pub container_id: MetaString,
    /// Tracer language name.
    pub language_name: MetaString,
    /// Tracer language version.
    pub language_version: MetaString,
    /// Tracer library version.
    pub tracer_version: MetaString,
    /// Runtime ID.
    pub runtime_id: MetaString,
    /// Environment name.
    pub env: MetaString,
    /// Hostname.
    pub hostname: MetaString,
    /// Application version.
    pub app_version: MetaString,
    /// Payload-level attributes.
    pub payload_attributes: Vec<V1KeyValue>,
    /// Per-chunk weight from the `Datadog-Client-Dropped-P0-Traces` request header,
    /// computed as `header_value / num_chunks_in_payload`. Zero if the header was absent.
    pub client_dropped_p0s_weight: f64,
}
