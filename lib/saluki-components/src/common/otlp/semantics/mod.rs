//! Semantic attribute accessors for OTLP and converted spans.

pub mod accessor;

pub use accessor::{DdSpanAccessor, OtelSpanAccessor, OtlpAttributesAccessor};
