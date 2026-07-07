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
//!
//! # Missing
//!
//! - Incrementally compressed blocks. This is a centerpiece of the implementation on the Agent side,
//!   but we do this in a single shot as part of this initial implementation.

mod payload;
mod telemetry;

pub(crate) use datadog_agent_metrics_v3::V3EncoderStats;
pub use datadog_agent_metrics_v3::{V3EncodedMetrics, V3MetricType, V3Writer};
pub(super) use payload::{V3EncodedRequest, V3PayloadLimits, V3PayloadRequest};
pub(crate) use telemetry::{V3PayloadSplitReason, V3SerializerTelemetry};
