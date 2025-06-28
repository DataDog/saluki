//! Traces.

use saluki_common::collections::FastHashMap;
use saluki_context::tags::SharedTagSet;
use stringtheory::MetaString;

/// A trace.
#[derive(Clone, Debug, PartialEq)]
pub struct Trace {
    metadata: TraceMetadata,
    tags: SharedTagSet,
    chunks: Vec<TraceChunk>,
}

/// A trace chunk.
#[derive(Clone, Debug, PartialEq)]
pub struct TraceChunk {
    priority: i32,
    origin: MetaString,
    spans: Vec<Span>,
    tags: SharedTagSet,
    dropped_trace: bool,
}

/// A span.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    service: MetaString,
    name: MetaString,
    resource: MetaString,
    type_: MetaString,
    trace_id: u64,
    span_id: u64,
    parent_id: u64,
    start: i64,
    duration: i64,
    error: i32,

    meta: FastHashMap<MetaString, MetaString>,
    meta_struct: FastHashMap<MetaString, Vec<u8>>,
    metrics: FastHashMap<MetaString, f64>,
}

/// Trace metadata.
///
/// Metadata includes all information that is not specifically related to trace itself or its spans, such as the
/// language of the application, environment, and so on.
#[derive(Clone, Debug, PartialEq)]
pub struct TraceMetadata {
    container_id: MetaString,
    language_name: MetaString,
    language_version: MetaString,
    runtime_id: MetaString,
    env: MetaString,
    hostname: MetaString,
    app_version: MetaString,
}
