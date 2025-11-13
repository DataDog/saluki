//! Traces.

use datadog_protos::trace::Span;

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
