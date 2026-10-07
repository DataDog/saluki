//! Sans-I/O stateful logs protocol core.
//!
//! This crate owns the deterministic protocol state for stateful logs: log
//! translation, dictionary/schema state, batching, inflight payload tracking,
//! stream rotation, and retry decisions. Callers provide all I/O and scheduling
//! by executing the [`Effect`] values returned from [`StatefulLogsCore`].
//!
//! Stream-lifetime scheduling is caller-owned. When a stream opens, adapters such
//! as ADP, Vector, or the Agent must start a timer for [`SenderConfig::stream_lifetime`]
//! scoped to that stream. If it expires while that stream is still active, the
//! caller passes [`TimerKind::RotateStream`] to the core's `handle_timer` method.
//! The core does not request a lifetime timer, so callers must cancel or ignore
//! timers for streams that are no longer active.

#![deny(warnings)]

mod batch;
mod inflight;
mod machine;
mod metrics;
mod translator;
mod wire;

use std::time::Duration;

pub use self::batch::{
    BatchEncoder, BatchStrategy, DefaultBatchEncoder, EncodedPayload, PayloadId, StatefulDatum,
    StatefulDatumKind, StatefulMessage,
};
pub use self::inflight::{
    AckError, AckResult, InflightPayload, InflightQueue, PayloadRegion, SnapshotState,
    StatefulBatch,
};
pub use self::machine::{
    CoreConfig, CoreError, Effect, PushOutcome, StatefulLogsCore, StreamError, StreamId, Timer,
    TimerKind,
};
pub use self::metrics::{
    EncodedMetricBatch, LogicalMetricBatch, LogicalMetricSeries, MetricBatchEncodeError,
    MetricBatchEncoder, MetricClientEffect, MetricClientError, MetricDictionaryEvictionConfig,
    MetricDictionaryStats, MetricEffect, MetricEndpointId, MetricFailureAction, MetricOrigin,
    MetricPoint, MetricPushError, MetricResource, MetricSeriesEncoder, MetricSeriesType,
    MetricStatefulBatch, MetricStreamError, MetricStreamFailure, MetricStreamFailureKind,
    MetricTagSet, StatefulMetricsClient, StatefulMetricsCore,
};
pub use self::translator::{
    ExtractedPattern, LogRecord, NoopPatternExtractor, PatternExtractor, StatefulLogTranslator,
};
#[cfg(feature = "zstd")]
pub use self::wire::ZstdBatchCompressor;
pub use self::wire::{BatchCompressor, BatchEncodeError, NoopBatchCompressor, ProtoBatchEncoder};

/// Generated protobuf types used by the stateful logs protocol.
pub mod proto {
    /// `datadog.intake.stateful` protobuf messages.
    pub mod stateful {
        include!(concat!(env!("OUT_DIR"), "/datadog.intake.stateful.rs"));
    }
}

/// Configuration constants for the stateful logs core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SenderConfig {
    /// Maximum number of payloads allowed in a stream's inflight queue.
    ///
    /// Metrics apply it per endpoint: it bounds the payloads each endpoint's stream
    /// carries before acknowledging them or giving them back.
    pub max_inflight_payloads: usize,
    /// How long adapters should wait for outstanding acks during stream rotation.
    pub drain_timeout: Duration,
    /// Maximum lifetime callers must enforce for each active stream.
    ///
    /// The core does not schedule this timer. The caller must feed
    /// [`TimerKind::RotateStream`] back when the lifetime expires.
    pub stream_lifetime: Duration,
    /// First non-snapshot batch ID.
    pub first_payload_batch_id: u64,
    /// Reserved snapshot batch ID.
    pub snapshot_batch_id: u64,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            max_inflight_payloads: 10_000,
            drain_timeout: Duration::from_secs(5),
            stream_lifetime: Duration::from_secs(15 * 60),
            first_payload_batch_id: 1,
            snapshot_batch_id: 0,
        }
    }
}
