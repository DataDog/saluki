//! Stateful metrics protocol support.
//!
//! The metrics core sends every payload to each configured endpoint over one shared
//! dictionary, retaining complete logical series until every endpoint acknowledges
//! them. Encoding is deliberately delayed until a stream is open and has an
//! available in-flight slot, so callers can leave logical retry batches compressed
//! on disk throughout an outage. Callers can also accumulate partial batches with
//! `push_batch` and signal `flush` from their own idle timer; the core performs no timing.

mod client;
mod dictionary;
mod encoding;
mod eviction_policy;
mod machine;
mod model;
mod retention;
mod rule_store;

pub use self::client::{MetricClientEffect, MetricClientError, StatefulMetricsClient};
pub use self::encoding::{
    EncodedMetricBatch, MetricBatchEncodeError, MetricBatchEncoder, MetricSeriesEncoder,
    MetricStatefulBatch,
};
pub use self::machine::{
    MetricEffect, MetricEndpointId, MetricFailureAction, MetricPushError, MetricStreamError,
    MetricStreamFailure, MetricStreamFailureKind, StatefulMetricsCore,
};
pub use self::model::{
    LogicalMetricBatch, LogicalMetricSeries, MetricOrigin, MetricPoint, MetricResource,
    MetricSeriesType, MetricTagSet,
};

pub use self::eviction_policy::EvictionConfig as MetricDictionaryEvictionConfig;
pub use self::retention::MetricDictionaryStats;
