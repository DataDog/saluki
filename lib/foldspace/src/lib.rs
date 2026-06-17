//! Stateful logs sender primitives for the Agent Data Plane.
//!
//! This crate intentionally starts below the Saluki topology layer. It models
//! the sender, batching, inflight, and stream lifecycle behavior distilled in
//! `pkg/logs/sender/grpc/grpc_sender.allium` so a tonic/protobuf adapter can be
//! added without baking transport details into the state machine.

mod batch;
mod inflight;
mod runner;
mod sender;
mod translator;
mod transport;
mod wire;
mod worker;

use std::time::Duration;

pub use self::batch::{
    BatchEncoder, BatchStrategy, DefaultBatchEncoder, EncodedPayload, StatefulDatum, StatefulDatumKind, StatefulMessage,
};
pub use self::inflight::{
    AckError, AckResult, InflightPayload, InflightQueue, PayloadRegion, SnapshotState, StatefulBatch,
};
pub use self::runner::{RunnerError, StreamWorkerRunner, StreamWorkerRunnerHandle};
pub use self::sender::{Endpoint, EndpointSet, GrpcSender, SenderError, SenderState};
pub use self::translator::{
    ExtractedPattern, LogRecord, NoopPatternExtractor, PatternExtractor, StatefulLogTranslator,
};
pub use self::transport::{StreamTransport, TonicStatefulTransport, TonicStream, TransportError};
pub use self::wire::{BatchCompressor, BatchEncodeError, NoopBatchCompressor, ProtoBatchEncoder, ZstdBatchCompressor};
pub use self::worker::{BackoffState, StreamWorker, WorkerAckOutcome, WorkerError, WorkerState};

/// Configuration constants for the stateful gRPC logs sender.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SenderConfig {
    /// Maximum number of payloads allowed in a worker's inflight queue.
    pub max_inflight_payloads: usize,
    /// Timeout used by the eventual transport adapter while creating streams.
    pub connection_timeout: Duration,
    /// How long a worker waits for outstanding acks during stream rotation.
    pub drain_timeout: Duration,
    /// Maximum lifetime of an active stream before rotation is requested.
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
            connection_timeout: Duration::from_secs(10),
            drain_timeout: Duration::from_secs(5),
            stream_lifetime: Duration::from_secs(15 * 60),
            first_payload_batch_id: 1,
            snapshot_batch_id: 0,
        }
    }
}
