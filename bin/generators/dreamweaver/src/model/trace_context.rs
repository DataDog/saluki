//! Trace context for propagation between services.

use rand::Rng;

/// Context for trace propagation between services.
#[derive(Clone, Debug)]
pub struct TraceContext {
    /// The 128-bit trace ID (shared across all spans in the trace).
    pub trace_id: u128,

    /// The 64-bit span ID for the current span.
    pub span_id: u64,

    /// The parent span ID (None for root spans).
    pub parent_span_id: Option<u64>,

    /// The start time of this span in nanoseconds since Unix epoch.
    pub start_time_ns: u64,
}

impl TraceContext {
    /// Creates a new root trace context.
    pub fn new_root(rng: &mut impl Rng) -> Self {
        Self {
            trace_id: rng.random(),
            span_id: rng.random(),
            parent_span_id: None,
            start_time_ns: now_ns(),
        }
    }

    /// Creates a child context from this context.
    ///
    /// The child will have the same trace_id, a new span_id, and this context's
    /// span_id as its parent.
    pub fn child(&self, rng: &mut impl Rng, offset_ns: u64) -> Self {
        Self {
            trace_id: self.trace_id,
            span_id: rng.random(),
            parent_span_id: Some(self.span_id),
            start_time_ns: self.start_time_ns + offset_ns,
        }
    }

    /// Returns the trace ID as bytes (big-endian).
    pub fn trace_id_bytes(&self) -> Vec<u8> {
        self.trace_id.to_be_bytes().to_vec()
    }

    /// Returns the span ID as bytes (big-endian).
    pub fn span_id_bytes(&self) -> Vec<u8> {
        self.span_id.to_be_bytes().to_vec()
    }

    /// Returns the parent span ID as bytes (big-endian), or empty if no parent.
    pub fn parent_span_id_bytes(&self) -> Vec<u8> {
        self.parent_span_id
            .map(|id| id.to_be_bytes().to_vec())
            .unwrap_or_default()
    }
}

/// Returns the current time in nanoseconds since Unix epoch.
pub fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
