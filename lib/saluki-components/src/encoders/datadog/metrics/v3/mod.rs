//! V3 columnar metrics payload encoder.
//!
//! This module implements the V3 columnar format for Datadog metrics payloads. Unlike the V2
//! row-based protobuf format where each metric is a complete message, V3 uses a columnar layout
//! with dictionary-based string deduplication for efficient encoding.
//!
//! The key differences from V2:
//! - Dictionary deduplication for metric names, tags, resources, and origin info
//! - Delta encoding for index arrays to reduce payload size
//! - Batch encoding - all metrics must be collected before serialization
//! - Separate value columns for different numeric types (sint64, float32, float64)

mod interner;
mod serializer;
mod types;
mod writer;

pub use serializer::serialize_v3_payload;
pub use types::V3MetricType;
pub use writer::V3Writer;
