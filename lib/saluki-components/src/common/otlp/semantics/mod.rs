//! Semantic attribute accessors for OTLP and converted spans.

pub mod accessor;

pub(crate) use accessor::{DdSpanAccessor, OtelSpanAccessor, OtlpAttributesAccessor};
