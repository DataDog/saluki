use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    mem::take,
    time::Instant,
};

use crate::{proto::stateful::MetricDatum, CoreConfig, PayloadId, StreamId, Timer, TimerKind};

use super::{
    encoding::{EncodedMetricBatch, MetricSeriesEncoder, MetricStatefulBatch},
    retention::DefinitionKey,
    rule_store::{MetricRuleStore, MetricStateChange},
    LogicalMetricBatch, MetricDictionaryStats,
};

/// Transport-neutral classification for a stateful metrics stream failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricStreamFailureKind {
    /// The stream or backend is temporarily unavailable.
    Unavailable,
    /// A send, receive, or acknowledgement deadline expired.
    DeadlineExceeded,
    /// The stream exhausted a temporary resource or its allowed lifetime.
    ResourceExhausted,
    /// The server rejected the payload or protocol state permanently.
    InvalidArgument,
    /// The stream credentials are invalid or expired.
    Unauthenticated,
    /// Stateful delivery is not available for this destination.
    FailedPrecondition,
    /// The stream's acknowledgements violated the protocol, such as an out-of-order ACK.
    ///
    /// The core reports this kind itself. It does not correspond to a gRPC status, so adapters
    /// should not map transport errors to it.
    ProtocolViolation,
}

impl MetricStreamFailureKind {
    /// Returns the caller action required for this failure classification.
    pub const fn action(self) -> MetricFailureAction {
        match self {
            Self::Unavailable | Self::DeadlineExceeded | Self::ResourceExhausted => {
                MetricFailureAction::RetryWithBackoff
            }
            Self::InvalidArgument => MetricFailureAction::DoNotRetry,
            Self::Unauthenticated => MetricFailureAction::WaitForCredentials,
            Self::FailedPrecondition | Self::ProtocolViolation => {
                MetricFailureAction::UseStatelessDelivery
            }
        }
    }
}

/// Action the client must take after a classified stream failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricFailureAction {
    /// Hold the endpoint's returned batches and resubmit them with
    /// [`StatefulMetricsCore::send_batch_to`] once it can accept them again. After a stream
    /// failure, also reconnect the endpoint after the requested backoff.
    RetryWithBackoff,
    /// Do not retry the endpoint's returned batches automatically.
    DoNotRetry,
    /// Keep the endpoint's returned batches outside Foldspace until credentials are updated.
    WaitForCredentials,
    /// Submit the endpoint's returned batches through stateless delivery.
    UseStatelessDelivery,
}

/// Classified stream failure reported by a metrics transport adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricStreamFailure {
    kind: MetricStreamFailureKind,
    message: String,
}

impl MetricStreamFailure {
    /// Creates a classified stream failure.
    pub fn new(kind: MetricStreamFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Returns the transport-neutral failure classification.
    pub const fn kind(&self) -> MetricStreamFailureKind {
        self.kind
    }

    /// Returns the human-readable failure detail.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Transfers ownership of the classification and message to the caller.
    pub fn into_parts(self) -> (MetricStreamFailureKind, String) {
        (self.kind, self.message)
    }
}

/// Identifies one of a core's endpoints by its index in configuration order.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MetricEndpointId(pub usize);

impl MetricEndpointId {
    /// Returns the endpoint's index in configuration order.
    pub const fn get(self) -> usize {
        self.0
    }
}

/// Effects emitted by the sans-I/O stateful metrics core.
#[derive(Clone, Debug, PartialEq)]
pub enum MetricEffect {
    /// Open an initial or replacement stream to an endpoint.
    OpenStream {
        endpoint: MetricEndpointId,
        stream_id: StreamId,
    },
    /// Send a metric datum batch on an open stream.
    SendBatch {
        batch: MetricStatefulBatch<StreamId>,
    },
    /// Close a stream that is no longer usable.
    CloseStream { stream_id: StreamId },
    /// Report a classified failure on one endpoint's stream.
    ///
    /// `unacknowledged` carries the endpoint's copy of every batch its stream had not
    /// acknowledged, oldest first; the core no longer holds them for that endpoint. A retryable
    /// failure also schedules a reconnect. Any other failure suspends the endpoint.
    StreamFailed {
        endpoint: MetricEndpointId,
        failure: MetricStreamFailure,
        action: MetricFailureAction,
        unacknowledged: Vec<LogicalMetricBatch>,
    },
    /// Return an endpoint's copy of complete logical batches it cannot carry now.
    ///
    /// A flush returns each endpoint's copy that is not open, or whose inflight window is full.
    /// A drain timeout or destination reset returns the batches the closed stream had not
    /// acknowledged. `action` is [`MetricFailureAction::RetryWithBackoff`] unless the endpoint
    /// is suspended, in which case it is the action of the failure that suspended it.
    ReturnUnacknowledged {
        endpoint: MetricEndpointId,
        action: MetricFailureAction,
        batches: Vec<LogicalMetricBatch>,
    },
    /// Return the unsent partial batch once every endpoint is suspended.
    ReturnBuffered { batch: LogicalMetricBatch },
    /// Schedule a reconnect for a failed stream using the caller's backoff policy.
    ScheduleReconnect { stream_id: StreamId },
    /// Schedule a stream-scoped timer and feed it back through [`StatefulMetricsCore::handle_timer`].
    ScheduleTimer { stream_id: StreamId, timer: Timer },
    /// Report a protocol or adapter error.
    ReportError { error: MetricStreamError },
}

/// A rejected logical-batch push.
#[derive(Clone, Debug, PartialEq)]
pub enum MetricPushError {
    /// No target endpoint has an open stream, so encoding was not attempted.
    StreamUnavailable(LogicalMetricBatch),
    /// Every open target endpoint already has `max_inflight_payloads` unacknowledged
    /// payloads, so encoding was not attempted.
    InflightWindowFull(LogicalMetricBatch),
    /// The batch contains no series after input normalization.
    EmptyBatch(LogicalMetricBatch),
    /// The caller named an endpoint the core was not configured with.
    UnknownEndpoint(MetricEndpointId, LogicalMetricBatch),
}

impl MetricPushError {
    /// Returns ownership of the logical batch to the caller.
    pub fn into_batch(self) -> LogicalMetricBatch {
        match self {
            Self::StreamUnavailable(batch)
            | Self::InflightWindowFull(batch)
            | Self::EmptyBatch(batch)
            | Self::UnknownEndpoint(_, batch) => batch,
        }
    }
}

/// Error reported by the stateful metrics core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetricStreamError {
    /// The caller reported an event for a stale or unknown stream, or one whose state does
    /// not admit the event.
    UnexpectedStream { stream_id: StreamId },
    /// The caller named an endpoint the core was not configured with.
    UnknownEndpoint(MetricEndpointId),
    /// The server acknowledged a batch other than the FIFO head.
    AckMismatch { expected: u64, actual: u64 },
    /// The server acknowledged a data batch when none was outstanding.
    AckWithoutInflightBatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamState {
    /// No stream is active; the client may open or reconnect one.
    Disconnected,
    /// No stream is active; stateful delivery waits for caller intervention after a failure
    /// that required this action.
    Suspended(MetricFailureAction),
    Connecting(StreamId),
    Open(StreamId),
    Draining(StreamId),
}

#[derive(Debug)]
struct RetainedBatch {
    logical: LogicalMetricBatch,
    // Endpoints whose current stream carried this payload and has not acknowledged it.
    unsettled: usize,
}

#[derive(Clone, Copy, Debug)]
struct SentPayload {
    batch_id: u64,
    payload_id: PayloadId,
}

#[derive(Debug)]
struct Endpoint {
    state: StreamState,
    // Identifies the failed stream whose reconnect timer is currently valid.
    pending_reconnect: Option<StreamId>,
    // Next batch ID assigned within the current stream.
    next_batch_id: u64,
    // Sent on the current stream, in acknowledgement order.
    outstanding: VecDeque<SentPayload>,
    // Definitions the current stream has carried, so its receiver holds them.
    sent: BTreeSet<DefinitionKey>,
}

impl Endpoint {
    fn new(first_batch_id: u64) -> Self {
        Self {
            state: StreamState::Disconnected,
            pending_reconnect: None,
            next_batch_id: first_batch_id,
            outstanding: VecDeque::new(),
            sent: BTreeSet::new(),
        }
    }

    const fn stream_id(&self) -> Option<StreamId> {
        match self.state {
            StreamState::Disconnected | StreamState::Suspended(_) => None,
            StreamState::Connecting(stream_id)
            | StreamState::Open(stream_id)
            | StreamState::Draining(stream_id) => Some(stream_id),
        }
    }

    const fn is_open(&self) -> bool {
        matches!(self.state, StreamState::Open(_))
    }

    /// Whether the endpoint's stream is open with room in its inflight window.
    fn accepts(&self, max_inflight_payloads: usize) -> bool {
        self.is_open() && self.outstanding.len() < max_inflight_payloads
    }

    /// The action that accompanies batches this endpoint returns instead of carrying.
    const fn return_action(&self) -> MetricFailureAction {
        match self.state {
            StreamState::Suspended(action) => action,
            _ => MetricFailureAction::RetryWithBackoff,
        }
    }

    /// Ends the current stream's view of the endpoint and returns the payloads it had not
    /// acknowledged, oldest first.
    fn end_stream(&mut self, state: StreamState) -> Vec<PayloadId> {
        self.state = state;
        self.pending_reconnect = None;
        self.sent.clear();
        self.outstanding
            .drain(..)
            .map(|sent| sent.payload_id)
            .collect()
    }
}

/// Sans-I/O state machine that sends every metrics payload to every configured endpoint.
///
/// One dictionary and one rule store serve all endpoints; endpoints never own dictionary
/// state. Each endpoint has its own stream lifecycle, stream-local batch IDs, ordered
/// acknowledgement FIFO, and the set of definitions its current stream has carried. A
/// payload is encoded once, its new definitions are sealed into the rule store, and each open
/// endpoint is sent the definitions the payload references that its stream lacks, followed by
/// the shared series datum. A replacement stream therefore needs no snapshot: it receives
/// definitions as payloads first reference them.
///
/// The core holds a payload's logical batch only while some endpoint's current stream has
/// carried it without acknowledging it, and each endpoint's inflight window is bounded on its
/// own. Anything an endpoint cannot carry is returned to the caller as that endpoint's copy:
/// the unacknowledged batches of a stream that fails, times out draining, or is reset, and
/// each flushed batch while the endpoint is not open or its window is full. The caller owns
/// retrying them. For [`MetricFailureAction::RetryWithBackoff`] it resubmits them with
/// [`Self::send_batch_to`], which re-encodes against the current dictionary for that endpoint
/// alone. A non-retryable failure or protocol violation suspends only that endpoint.
///
/// The caller owns active-stream lifetime scheduling. For each successfully opened stream,
/// it must schedule [`crate::SenderConfig::stream_lifetime`] and pass
/// its [`StreamId`] with [`TimerKind::RotateStream`] to [`Self::handle_timer`] when the timer
/// expires. The core ignores timers for stale streams.
#[derive(Debug)]
pub struct StatefulMetricsCore {
    config: CoreConfig,
    // Monotonically identifies streams across endpoints so stale events can be rejected.
    next_stream_id: u64,
    // Interns series and tracks retention for every endpoint.
    encoder: MetricSeriesEncoder,
    // Live definitions keyed by definition, as of the last seal.
    head: MetricRuleStore,
    // Next global payload ID; never reset, so it orders payloads across streams.
    next_payload_id: u64,
    // Logical batches some endpoint's current stream has not acknowledged.
    retained: BTreeMap<PayloadId, RetainedBatch>,
    endpoints: Vec<Endpoint>,
    // Accepted series awaiting a size-triggered or caller-triggered flush.
    buffered: LogicalMetricBatch,
    // Number of logical batches actually encoded; used for diagnostics.
    encoding_count: u64,
}

impl Default for StatefulMetricsCore {
    fn default() -> Self {
        Self::new(CoreConfig::default())
    }
}

impl StatefulMetricsCore {
    /// Creates an empty metrics core with [`CoreConfig::metrics_endpoints`] endpoints.
    pub fn new(config: CoreConfig) -> Self {
        let endpoints = (0..config.metrics_endpoints.max(1))
            .map(|_| Endpoint::new(config.sender.first_payload_batch_id))
            .collect();
        Self {
            config,
            next_stream_id: 1,
            encoder: MetricSeriesEncoder::default(),
            head: MetricRuleStore::default(),
            next_payload_id: 0,
            retained: BTreeMap::new(),
            endpoints,
            buffered: LogicalMetricBatch::default(),
            encoding_count: 0,
        }
    }

    /// Starts the core by requesting a stream for every endpoint without one.
    pub fn start(&mut self) -> Vec<MetricEffect> {
        (0..self.endpoints.len())
            .filter_map(|index| self.open_stream_if_needed(index))
            .collect()
    }

    /// Returns the number of configured endpoints.
    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    /// Returns an endpoint's current stream ID, if one is connecting, open, or draining.
    pub fn current_stream_id(&self, endpoint: MetricEndpointId) -> Option<StreamId> {
        self.endpoints.get(endpoint.0).and_then(Endpoint::stream_id)
    }

    /// Returns whether some endpoint is open with room in its inflight window, so the core can
    /// accept more series for the next payload.
    pub fn has_send_capacity(&self) -> bool {
        let max = self.config.sender.max_inflight_payloads;
        self.endpoints.iter().any(|endpoint| endpoint.accepts(max))
    }

    /// Returns whether an endpoint is open with room in its inflight window, so
    /// [`Self::send_batch_to`] would accept a batch for it.
    pub fn endpoint_has_send_capacity(&self, endpoint: MetricEndpointId) -> bool {
        self.endpoints
            .get(endpoint.0)
            .is_some_and(|state| state.accepts(self.config.sender.max_inflight_payloads))
    }

    /// Returns the number of logical batches some endpoint's current stream has not
    /// acknowledged.
    pub fn inflight_len(&self) -> usize {
        self.retained.len()
    }

    /// Returns live dictionary usage, shared by every endpoint.
    pub fn dictionary_stats(&self) -> MetricDictionaryStats {
        self.encoder.dictionary_stats()
    }

    /// Returns the number of logical batches encoded by this core, including resubmissions.
    pub const fn encoding_count(&self) -> u64 {
        self.encoding_count
    }

    /// Handles notification that a requested stream opened successfully.
    ///
    /// The stream starts a fresh batch-ID sequence and is assumed to hold no dictionary
    /// state, so each payload it carries is preceded by the definitions it references. Nothing
    /// is sent on opening; the caller resubmits batches it holds for the endpoint with
    /// [`Self::send_batch_to`]. Only an unexpected stream produces an effect, a
    /// [`MetricEffect::ReportError`].
    ///
    /// The caller must also start the caller-owned stream-lifetime timer. This method intentionally
    /// does not return a [`MetricEffect::ScheduleTimer`] for it.
    pub fn handle_stream_opened(&mut self, stream_id: StreamId) -> Vec<MetricEffect> {
        let Some(index) = self.locate(stream_id) else {
            return unexpected_stream(stream_id);
        };
        let endpoint = &mut self.endpoints[index];
        if endpoint.state != StreamState::Connecting(stream_id) {
            return unexpected_stream(stream_id);
        }
        debug_assert!(endpoint.outstanding.is_empty() && endpoint.sent.is_empty());
        endpoint.state = StreamState::Open(stream_id);
        endpoint.next_batch_id = self.config.sender.first_payload_batch_id;
        Vec::new()
    }

    /// Returns the number of accepted series awaiting a flush.
    pub fn buffered_series_len(&self) -> usize {
        self.buffered.series().len()
    }

    /// Buffers logical series until the caller invokes `flush()` or the
    /// series count reaches `CoreConfig::batch_capacity`, triggering an automatic flush.
    ///
    /// The capacity is a series-count threshold, not a byte limit; one input batch can exceed it.
    /// `now` is the caller's monotonic reading, used when this push triggers encoding.
    /// Zero is treated as one. Rejected input is returned unchanged, leaving earlier buffered
    /// series intact. No dictionary state or batch ID is assigned until a flush.
    #[allow(clippy::result_large_err)]
    pub fn push_batch(
        &mut self,
        logical: LogicalMetricBatch,
        now: Instant,
    ) -> Result<Vec<MetricEffect>, MetricPushError> {
        let logical = self.check_send_capacity(logical)?;
        if logical.is_empty() {
            return Err(MetricPushError::EmptyBatch(logical));
        }
        self.buffered.append(logical);
        if self.buffered_series_len() >= self.config.batch_capacity.max(1) {
            self.flush(now)
        } else {
            Ok(Vec::new())
        }
    }

    /// Flushes the partial batch in response to a caller-owned idle timer or shutdown signal.
    ///
    /// The batch is encoded once and sent to every open endpoint with room in its inflight
    /// window. Every other endpoint gets its copy back through
    /// [`MetricEffect::ReturnUnacknowledged`].
    ///
    /// Empty flushes perform dictionary maintenance without emitting a payload.
    /// `now` is supplied by the caller; this method reads no clock and schedules no timer. On rejection,
    /// the error transfers the complete unsent batch to the caller, clearing the partial buffer.
    #[allow(clippy::result_large_err)]
    pub fn flush(&mut self, now: Instant) -> Result<Vec<MetricEffect>, MetricPushError> {
        if self.buffered.is_empty() {
            let evicted = match &self.config.metrics_dictionary_eviction {
                Some(config) => self.encoder.maintain(now, config),
                None => Vec::new(),
            };
            self.forget(evicted);
            return Ok(Vec::new());
        }
        let logical = take(&mut self.buffered);
        let logical = self.check_send_capacity(logical)?;

        let encoded = Self::encode_and_seal(
            &mut self.encoder,
            &mut self.head,
            &mut self.encoding_count,
            &logical,
            now,
        );
        let payload_id = self.next_payload_id();
        let max = self.config.sender.max_inflight_payloads;
        let mut effects = Vec::new();
        let mut unsettled = 0;
        for index in 0..self.endpoints.len() {
            let endpoint = &self.endpoints[index];
            match endpoint.state {
                StreamState::Open(stream_id) if endpoint.accepts(max) => {
                    unsettled += 1;
                    effects.push(self.send_encoded(index, stream_id, payload_id, &encoded));
                }
                _ => effects.push(MetricEffect::ReturnUnacknowledged {
                    endpoint: MetricEndpointId(index),
                    action: endpoint.return_action(),
                    batches: vec![logical.clone()],
                }),
            }
        }
        self.retained
            .insert(payload_id, RetainedBatch { logical, unsettled });
        self.evict_after(&encoded);
        Ok(effects)
    }

    /// Encodes a batch and sends it to one endpoint only, as its own payload.
    ///
    /// This is how the caller resubmits batches an endpoint returned with
    /// [`MetricFailureAction::RetryWithBackoff`]. The batch is re-encoded against the current
    /// dictionary, so values evicted since it was first sent are defined again under fresh
    /// IDs. The partial buffer and the other endpoints are untouched. Resubmitted batches may
    /// interleave with newer flushes; ordering across payloads is not preserved.
    ///
    /// The endpoint must be open with room in its inflight window. Rejection transfers the
    /// batch back unchanged without encoding it.
    #[allow(clippy::result_large_err)]
    pub fn send_batch_to(
        &mut self,
        endpoint: MetricEndpointId,
        logical: LogicalMetricBatch,
        now: Instant,
    ) -> Result<Vec<MetricEffect>, MetricPushError> {
        let index = endpoint.0;
        let Some(state) = self.endpoints.get(index) else {
            return Err(MetricPushError::UnknownEndpoint(endpoint, logical));
        };
        let StreamState::Open(stream_id) = state.state else {
            return Err(MetricPushError::StreamUnavailable(logical));
        };
        if !state.accepts(self.config.sender.max_inflight_payloads) {
            return Err(MetricPushError::InflightWindowFull(logical));
        }
        if logical.is_empty() {
            return Err(MetricPushError::EmptyBatch(logical));
        }

        let encoded = Self::encode_and_seal(
            &mut self.encoder,
            &mut self.head,
            &mut self.encoding_count,
            &logical,
            now,
        );
        let payload_id = self.next_payload_id();
        let effect = self.send_encoded(index, stream_id, payload_id, &encoded);
        self.retained.insert(
            payload_id,
            RetainedBatch {
                logical,
                unsettled: 1,
            },
        );
        self.evict_after(&encoded);
        Ok(vec![effect])
    }

    /// Applies an ordered server acknowledgement on one endpoint's stream.
    ///
    /// The payload's logical batch is released once every endpoint has settled it. An
    /// acknowledgement with no outstanding batch, or one that does not match the endpoint's
    /// oldest outstanding batch, suspends that endpoint and reports a
    /// [`MetricStreamFailureKind::ProtocolViolation`] failure.
    pub fn handle_ack(&mut self, stream_id: StreamId, batch_id: u64) -> Vec<MetricEffect> {
        let Some(index) = self.locate(stream_id) else {
            return unexpected_stream(stream_id);
        };
        let endpoint = &self.endpoints[index];
        if !matches!(
            endpoint.state,
            StreamState::Open(_) | StreamState::Draining(_)
        ) {
            return unexpected_stream(stream_id);
        }
        let Some(front) = endpoint.outstanding.front().copied() else {
            return self.suspend_on_violation(
                index,
                stream_id,
                MetricStreamError::AckWithoutInflightBatch,
            );
        };
        if batch_id != front.batch_id {
            return self.suspend_on_violation(
                index,
                stream_id,
                MetricStreamError::AckMismatch {
                    expected: front.batch_id,
                    actual: batch_id,
                },
            );
        }

        let endpoint = &mut self.endpoints[index];
        endpoint.outstanding.pop_front();
        let drained =
            matches!(endpoint.state, StreamState::Draining(_)) && endpoint.outstanding.is_empty();
        self.settle(front.payload_id);
        if drained {
            self.finish_stream_rotation(index)
        } else {
            Vec::new()
        }
    }

    /// Closes a failed stream and applies its classification to that endpoint only.
    pub fn handle_stream_error(
        &mut self,
        stream_id: StreamId,
        failure: MetricStreamFailure,
    ) -> Vec<MetricEffect> {
        let Some(index) = self.locate(stream_id) else {
            return unexpected_stream(stream_id);
        };
        let action = failure.kind().action();
        let retry = action == MetricFailureAction::RetryWithBackoff;
        let unacknowledged = self.end_stream(
            index,
            if retry {
                StreamState::Disconnected
            } else {
                StreamState::Suspended(action)
            },
        );
        let mut effects = vec![
            MetricEffect::CloseStream { stream_id },
            MetricEffect::StreamFailed {
                endpoint: MetricEndpointId(index),
                failure,
                action,
                unacknowledged,
            },
        ];
        if retry {
            self.endpoints[index].pending_reconnect = Some(stream_id);
            effects.push(MetricEffect::ScheduleReconnect { stream_id });
        } else {
            self.return_buffered_if_undeliverable(&mut effects);
        }
        effects
    }

    /// Handles a stream-scoped timer expiry.
    ///
    /// Reconnect scheduling is requested through [`MetricEffect::ScheduleReconnect`], with the
    /// delay selected by the caller. Drain timers are requested through
    /// [`MetricEffect::ScheduleTimer`]. The caller schedules [`TimerKind::RotateStream`] from
    /// [`crate::SenderConfig::stream_lifetime`] when a stream opens.
    pub fn handle_timer(&mut self, stream_id: StreamId, timer: TimerKind) -> Vec<MetricEffect> {
        match timer {
            TimerKind::Reconnect => self
                .endpoints
                .iter()
                .position(|endpoint| endpoint.pending_reconnect == Some(stream_id))
                .and_then(|index| self.open_stream_if_needed(index))
                .into_iter()
                .collect(),
            TimerKind::RotateStream => self.begin_stream_rotation(stream_id),
            TimerKind::DrainExpired => match self.locate(stream_id) {
                Some(index) if self.endpoints[index].state == StreamState::Draining(stream_id) => {
                    self.finish_stream_rotation(index)
                }
                _ => Vec::new(),
            },
        }
    }

    /// Replaces an endpoint's stream after its identity, such as its address or API key,
    /// changes.
    ///
    /// The endpoint's unacknowledged batches come back through
    /// [`MetricEffect::ReturnUnacknowledged`], and a replacement stream that is assumed to hold
    /// nothing is opened. The shared dictionary and the partial buffer are kept: no endpoint
    /// relies on another's stream state. This also resumes a suspended endpoint.
    pub fn reset_destination_state(&mut self, endpoint: MetricEndpointId) -> Vec<MetricEffect> {
        let mut effects = match self.close_endpoint(endpoint) {
            Ok(effects) => effects,
            Err(error) => return vec![MetricEffect::ReportError { error }],
        };
        effects.extend(self.open_stream_if_needed(endpoint.0));
        effects
    }

    /// Closes every stream and transfers all logical work to the caller without opening
    /// replacements.
    ///
    /// Each endpoint's unacknowledged batches come back through
    /// [`MetricEffect::ReturnUnacknowledged`] and the partial buffer through
    /// [`MetricEffect::ReturnBuffered`]. Afterwards the core holds no batches;
    /// [`Self::start`] opens new streams for every endpoint, including suspended ones.
    pub fn shutdown(&mut self) -> Vec<MetricEffect> {
        let mut effects: Vec<_> = (0..self.endpoints.len())
            .flat_map(|index| {
                self.close_endpoint(MetricEndpointId(index))
                    .expect("every index names a configured endpoint")
            })
            .collect();
        if !self.buffered.is_empty() {
            effects.push(MetricEffect::ReturnBuffered {
                batch: take(&mut self.buffered),
            });
        }
        debug_assert!(self.retained.is_empty());
        effects
    }

    /// Closes an endpoint's stream, leaving it disconnected, and returns its unacknowledged
    /// batches for the caller to retry.
    fn close_endpoint(
        &mut self,
        endpoint: MetricEndpointId,
    ) -> Result<Vec<MetricEffect>, MetricStreamError> {
        let current_stream = self
            .endpoints
            .get(endpoint.0)
            .ok_or(MetricStreamError::UnknownEndpoint(endpoint))?
            .stream_id();
        let batches = self.end_stream(endpoint.0, StreamState::Disconnected);
        let mut effects = Vec::new();
        if let Some(stream_id) = current_stream {
            effects.push(MetricEffect::CloseStream { stream_id });
        }
        if !batches.is_empty() {
            effects.push(MetricEffect::ReturnUnacknowledged {
                endpoint,
                action: MetricFailureAction::RetryWithBackoff,
                batches,
            });
        }
        Ok(effects)
    }

    #[allow(clippy::result_large_err)]
    fn check_send_capacity(
        &self,
        logical: LogicalMetricBatch,
    ) -> Result<LogicalMetricBatch, MetricPushError> {
        if !self.endpoints.iter().any(Endpoint::is_open) {
            return Err(MetricPushError::StreamUnavailable(logical));
        }
        if !self.has_send_capacity() {
            return Err(MetricPushError::InflightWindowFull(logical));
        }
        Ok(logical)
    }

    fn next_payload_id(&mut self) -> PayloadId {
        let payload_id = PayloadId(self.next_payload_id);
        self.next_payload_id += 1;
        payload_id
    }

    /// Runs size-driven eviction after a payload has been dispatched.
    fn evict_after(&mut self, encoded: &EncodedMetricBatch) {
        let evicted = match &self.config.metrics_dictionary_eviction {
            Some(config) => self.encoder.evict(config),
            None => Vec::new(),
        };
        debug_assert!(
            evicted
                .iter()
                .all(|key| encoded.references().binary_search(key).is_err()),
            "eviction must not remove a definition the payload it follows references"
        );
        self.forget(evicted);
    }

    /// Encodes against the shared dictionary and seals the definitions the encoding introduced.
    fn encode_and_seal(
        encoder: &mut MetricSeriesEncoder,
        head: &mut MetricRuleStore,
        encoding_count: &mut u64,
        logical: &LogicalMetricBatch,
        now: Instant,
    ) -> EncodedMetricBatch {
        let encoded = encoder.encode(logical, now);
        *encoding_count += 1;
        head.seal(
            &encoded
                .seal_manifest()
                .iter()
                .cloned()
                .map(MetricStateChange::Define)
                .collect::<Vec<_>>(),
        );
        encoded
    }

    /// Sends a payload on an open stream, preceded by the definitions it references that the
    /// stream has not carried.
    fn send_encoded(
        &mut self,
        index: usize,
        stream_id: StreamId,
        payload_id: PayloadId,
        encoded: &EncodedMetricBatch,
    ) -> MetricEffect {
        let endpoint = &mut self.endpoints[index];
        let mut datums: Vec<MetricDatum> = self
            .head
            .definitions_for(encoded.references(), &endpoint.sent)
            .cloned()
            .collect();
        endpoint.sent.extend(datums.iter().map(DefinitionKey::of));
        datums.extend(encoded.series().cloned());
        let batch_id = endpoint.next_batch_id;
        endpoint.next_batch_id += 1;
        endpoint.outstanding.push_back(SentPayload {
            batch_id,
            payload_id,
        });
        MetricEffect::SendBatch {
            batch: MetricStatefulBatch {
                stream: stream_id,
                batch_id,
                datums,
            },
        }
    }

    /// Removes evicted definitions from the rule store and every stream's sent set. Their IDs
    /// are never reissued, so no later payload can reference them.
    fn forget(&mut self, evicted: Vec<DefinitionKey>) {
        if evicted.is_empty() {
            return;
        }
        for endpoint in &mut self.endpoints {
            for key in &evicted {
                endpoint.sent.remove(key);
            }
        }
        self.head.seal(
            &evicted
                .into_iter()
                .map(MetricStateChange::Evict)
                .collect::<Vec<_>>(),
        );
    }

    /// Records that one endpoint acknowledged a payload.
    fn settle(&mut self, payload_id: PayloadId) {
        let retained = self
            .retained
            .get_mut(&payload_id)
            .expect("an unsettled payload is retained");
        retained.unsettled -= 1;
        if retained.unsettled == 0 {
            self.retained.remove(&payload_id);
        }
    }

    /// Records that one endpoint gave a payload back, returning that endpoint's copy.
    fn give_back(&mut self, payload_id: PayloadId) -> LogicalMetricBatch {
        let retained = self
            .retained
            .get_mut(&payload_id)
            .expect("an unsettled payload is retained");
        retained.unsettled -= 1;
        if retained.unsettled == 0 {
            self.retained
                .remove(&payload_id)
                .expect("checked above")
                .logical
        } else {
            retained.logical.clone()
        }
    }

    /// Ends an endpoint's stream and returns its copy of every payload the stream had not
    /// acknowledged, oldest first.
    fn end_stream(&mut self, index: usize, state: StreamState) -> Vec<LogicalMetricBatch> {
        self.endpoints[index]
            .end_stream(state)
            .into_iter()
            .map(|payload_id| self.give_back(payload_id))
            .collect()
    }

    fn return_buffered_if_undeliverable(&mut self, effects: &mut Vec<MetricEffect>) {
        let undeliverable = self
            .endpoints
            .iter()
            .all(|endpoint| matches!(endpoint.state, StreamState::Suspended(_)));
        if undeliverable && !self.buffered.is_empty() {
            effects.push(MetricEffect::ReturnBuffered {
                batch: take(&mut self.buffered),
            });
        }
    }

    fn open_stream_if_needed(&mut self, index: usize) -> Option<MetricEffect> {
        let endpoint = &mut self.endpoints[index];
        if endpoint.state != StreamState::Disconnected {
            return None;
        }
        let stream_id = StreamId::from_raw(self.next_stream_id);
        self.next_stream_id += 1;
        endpoint.pending_reconnect = None;
        endpoint.state = StreamState::Connecting(stream_id);
        Some(MetricEffect::OpenStream {
            endpoint: MetricEndpointId(index),
            stream_id,
        })
    }

    /// The endpoint whose current stream is `stream_id`.
    fn locate(&self, stream_id: StreamId) -> Option<usize> {
        self.endpoints
            .iter()
            .position(|endpoint| endpoint.stream_id() == Some(stream_id))
    }

    fn begin_stream_rotation(&mut self, stream_id: StreamId) -> Vec<MetricEffect> {
        let Some(index) = self.locate(stream_id) else {
            return Vec::new();
        };
        let endpoint = &mut self.endpoints[index];
        if endpoint.state != StreamState::Open(stream_id) {
            return Vec::new();
        }
        if endpoint.outstanding.is_empty() {
            return self.finish_stream_rotation(index);
        }
        endpoint.state = StreamState::Draining(stream_id);
        vec![MetricEffect::ScheduleTimer {
            stream_id,
            timer: Timer {
                kind: TimerKind::DrainExpired,
                after: self.config.sender.drain_timeout,
            },
        }]
    }

    fn finish_stream_rotation(&mut self, index: usize) -> Vec<MetricEffect> {
        let mut effects = self
            .close_endpoint(MetricEndpointId(index))
            .expect("a located endpoint is configured");
        effects.extend(self.open_stream_if_needed(index));
        effects
    }

    fn suspend_on_violation(
        &mut self,
        index: usize,
        stream_id: StreamId,
        error: MetricStreamError,
    ) -> Vec<MetricEffect> {
        let failure = MetricStreamFailure::new(
            MetricStreamFailureKind::ProtocolViolation,
            protocol_violation_message(&error),
        );
        let action = failure.kind().action();
        let unacknowledged = self.end_stream(index, StreamState::Suspended(action));
        let mut effects = vec![
            MetricEffect::CloseStream { stream_id },
            MetricEffect::StreamFailed {
                endpoint: MetricEndpointId(index),
                failure,
                action,
                unacknowledged,
            },
        ];
        self.return_buffered_if_undeliverable(&mut effects);
        effects.push(MetricEffect::ReportError { error });
        effects
    }
}

fn unexpected_stream(stream_id: StreamId) -> Vec<MetricEffect> {
    vec![MetricEffect::ReportError {
        error: MetricStreamError::UnexpectedStream { stream_id },
    }]
}

fn protocol_violation_message(error: &MetricStreamError) -> String {
    match error {
        MetricStreamError::AckMismatch { expected, actual } => {
            format!("received acknowledgement for batch {actual}, expected batch {expected}")
        }
        MetricStreamError::AckWithoutInflightBatch => {
            "received a data acknowledgement with no batch inflight".to_string()
        }
        MetricStreamError::UnexpectedStream { .. } | MetricStreamError::UnknownEndpoint(_) => {
            format!("{error:?}")
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        proto::stateful::metric_datum as datum, LogicalMetricSeries,
        MetricDictionaryEvictionConfig, MetricOrigin, MetricPoint, MetricResource,
        MetricSeriesType, MetricTagSet, SenderConfig,
    };
    use std::time::Duration;

    use super::*;

    const A: MetricEndpointId = MetricEndpointId(0);
    const B: MetricEndpointId = MetricEndpointId(1);

    fn batch(name: &str, value: f64) -> LogicalMetricBatch {
        LogicalMetricBatch::new(vec![LogicalMetricSeries::new(
            name,
            MetricSeriesType::Gauge,
            vec![MetricPoint::new(1, value)],
        )])
    }

    fn complete_batch(name: &str, value: f64) -> LogicalMetricBatch {
        LogicalMetricBatch::new(vec![LogicalMetricSeries::new(
            name,
            MetricSeriesType::Rate,
            vec![MetricPoint::new(1, value)],
        )
        .with_tags(MetricTagSet {
            prefix: vec!["env:prod".to_string()],
            values: vec!["service:api".to_string()],
        })
        .with_resources(vec![MetricResource::new("host", "web-1")])
        .with_interval(10)
        .with_source_type_name("nginx".to_string())
        .with_origin(MetricOrigin::new(1, 2, 3))
        .with_no_index(true)])
    }

    fn core_with(endpoints: usize) -> StatefulMetricsCore {
        StatefulMetricsCore::new(CoreConfig {
            metrics_endpoints: endpoints,
            ..CoreConfig::default()
        })
    }

    fn eviction_core(endpoints: usize) -> StatefulMetricsCore {
        StatefulMetricsCore::new(CoreConfig {
            metrics_endpoints: endpoints,
            metrics_dictionary_eviction: Some(MetricDictionaryEvictionConfig {
                max_item_count: 4,
                high_watermark: 1.0,
                low_watermark: 0.5,
                grace_period: Duration::ZERO,
                stale_after: Duration::ZERO,
                ..MetricDictionaryEvictionConfig::default()
            }),
            ..CoreConfig::default()
        })
    }

    /// Opens every stream the effects request and returns their IDs in order.
    fn open_requested(core: &mut StatefulMetricsCore, effects: &[MetricEffect]) -> Vec<StreamId> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                MetricEffect::OpenStream { stream_id, .. } => Some(*stream_id),
                _ => None,
            })
            .inspect(|stream_id| {
                assert!(core.handle_stream_opened(*stream_id).is_empty());
            })
            .collect()
    }

    fn open_all(core: &mut StatefulMetricsCore) -> Vec<StreamId> {
        let effects = core.start();
        open_requested(core, &effects)
    }

    fn open(core: &mut StatefulMetricsCore) -> StreamId {
        let streams = open_all(core);
        assert_eq!(streams.len(), 1);
        streams[0]
    }

    fn reopen(core: &mut StatefulMetricsCore, failed: StreamId) -> (StreamId, Vec<MetricEffect>) {
        let effects = core.handle_timer(failed, TimerKind::Reconnect);
        let [MetricEffect::OpenStream { stream_id, .. }] = effects.as_slice() else {
            panic!("reconnect should open a stream, got {effects:?}");
        };
        (*stream_id, core.handle_stream_opened(*stream_id))
    }

    fn push_and_flush(
        core: &mut StatefulMetricsCore,
        logical: LogicalMetricBatch,
    ) -> Result<Vec<MetricEffect>, MetricPushError> {
        let mut effects = core.push_batch(logical, Instant::now())?;
        effects.extend(core.flush(Instant::now())?);
        Ok(effects)
    }

    fn sends(effects: &[MetricEffect]) -> Vec<&MetricStatefulBatch<StreamId>> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                MetricEffect::SendBatch { batch } => Some(batch),
                _ => None,
            })
            .collect()
    }

    fn sent_batch(effects: &[MetricEffect]) -> &MetricStatefulBatch<StreamId> {
        sends(effects)
            .first()
            .copied()
            .expect("core should send a batch")
    }

    fn definition_keys(batch: &MetricStatefulBatch<StreamId>) -> Vec<DefinitionKey> {
        batch
            .datums
            .iter()
            .filter(|datum| !matches!(datum.data, Some(datum::Data::MetricSeriesBatch(_))))
            .map(DefinitionKey::of)
            .collect()
    }

    fn retryable(message: &str) -> MetricStreamFailure {
        MetricStreamFailure::new(MetricStreamFailureKind::Unavailable, message)
    }

    fn assert_head_matches_dictionary(core: &StatefulMetricsCore) {
        assert_eq!(
            core.head.keys().collect::<Vec<_>>(),
            core.encoder.retained_keys(),
            "rule store keys must match the shared dictionary"
        );
    }

    fn protocol_violation(
        endpoint: MetricEndpointId,
        message: &str,
        unacknowledged: Vec<LogicalMetricBatch>,
    ) -> MetricEffect {
        MetricEffect::StreamFailed {
            endpoint,
            failure: MetricStreamFailure::new(MetricStreamFailureKind::ProtocolViolation, message),
            action: MetricFailureAction::UseStatelessDelivery,
            unacknowledged,
        }
    }

    fn assert_classified_failure(
        kind: MetricStreamFailureKind,
        expected_action: MetricFailureAction,
    ) {
        let mut core = StatefulMetricsCore::default();
        let stream_id = open(&mut core);
        push_and_flush(&mut core, complete_batch("acked", 0.5)).unwrap();
        assert!(core.handle_ack(stream_id, 1).is_empty());
        let first = complete_batch("first", 1.25);
        let second = complete_batch("second", 2.5);
        push_and_flush(&mut core, first.clone()).unwrap();
        push_and_flush(&mut core, second.clone()).unwrap();

        let mut effects = core.handle_stream_error(
            stream_id,
            MetricStreamFailure::new(kind, "classified failure"),
        );
        assert_eq!(effects.remove(0), MetricEffect::CloseStream { stream_id });
        let MetricEffect::StreamFailed {
            endpoint,
            failure,
            action,
            unacknowledged,
        } = effects.remove(0)
        else {
            panic!("classified failure effect should follow the close");
        };
        assert_eq!(endpoint, A);
        assert_eq!(failure.into_parts(), (kind, "classified failure".into()));
        assert_eq!(action, expected_action);
        assert_eq!(core.current_stream_id(A), None);
        assert_eq!(unacknowledged, vec![first.clone(), second]);
        assert_eq!(core.inflight_len(), 0);
        assert_head_matches_dictionary(&core);

        if action == MetricFailureAction::RetryWithBackoff {
            assert_eq!(effects, vec![MetricEffect::ScheduleReconnect { stream_id }]);
            let (replacement, opened) = reopen(&mut core, stream_id);
            assert!(opened.is_empty(), "the core replays nothing itself");
            let resent = core.send_batch_to(A, first, Instant::now()).unwrap();
            let resent = sent_batch(&resent);
            assert_eq!((resent.stream, resent.batch_id), (replacement, 1));
            assert!(
                !definition_keys(resent).is_empty(),
                "a fresh stream receives what the resubmission references"
            );
        } else {
            assert!(effects.is_empty());
            assert!(core
                .handle_timer(stream_id, TimerKind::Reconnect)
                .is_empty());
            assert!(core.start().is_empty());
        }
    }

    #[test]
    fn multiple_payloads_are_inflight_and_acknowledged_in_order() {
        let mut core = StatefulMetricsCore::default();
        let stream_id = open(&mut core);

        assert_eq!(
            sent_batch(&push_and_flush(&mut core, batch("one", 1.0)).unwrap()).batch_id,
            1
        );
        assert_eq!(
            sent_batch(&push_and_flush(&mut core, batch("two", 2.0)).unwrap()).batch_id,
            2
        );
        assert_eq!(core.inflight_len(), 2);

        assert!(core.handle_ack(stream_id, 1).is_empty());
        assert_eq!(core.inflight_len(), 1);
        assert!(core.handle_ack(stream_id, 2).is_empty());
        assert_eq!(core.inflight_len(), 0);
    }

    #[test]
    fn out_of_order_ack_suspends_endpoint_and_returns_its_batches() {
        let mut core = StatefulMetricsCore::default();
        let stream_id = open(&mut core);
        let first = batch("one", 1.0);
        let second = batch("two", 2.0);
        push_and_flush(&mut core, first.clone()).unwrap();
        push_and_flush(&mut core, second.clone()).unwrap();

        assert_eq!(
            core.handle_ack(stream_id, 2),
            vec![
                MetricEffect::CloseStream { stream_id },
                protocol_violation(
                    A,
                    "received acknowledgement for batch 2, expected batch 1",
                    vec![first, second],
                ),
                MetricEffect::ReportError {
                    error: MetricStreamError::AckMismatch {
                        expected: 1,
                        actual: 2,
                    },
                }
            ]
        );
        assert_eq!(core.current_stream_id(A), None);
        assert_eq!(core.inflight_len(), 0);
        assert!(core
            .handle_timer(stream_id, TimerKind::Reconnect)
            .is_empty());
        assert!(core.start().is_empty());
        assert!(matches!(
            core.reset_destination_state(A).as_slice(),
            [MetricEffect::OpenStream { endpoint: A, .. }]
        ));
    }

    #[test]
    fn ack_without_inflight_batch_suspends_stream() {
        let mut core = StatefulMetricsCore::default();
        let stream_id = open(&mut core);

        assert_eq!(
            core.handle_ack(stream_id, 1),
            vec![
                MetricEffect::CloseStream { stream_id },
                protocol_violation(
                    A,
                    "received a data acknowledgement with no batch inflight",
                    Vec::new(),
                ),
                MetricEffect::ReportError {
                    error: MetricStreamError::AckWithoutInflightBatch,
                }
            ]
        );
        assert_eq!(core.current_stream_id(A), None);
        assert!(core.start().is_empty());
    }

    #[test]
    fn out_of_order_ack_while_draining_suspends_stream() {
        let mut core = StatefulMetricsCore::default();
        let stream_id = open(&mut core);
        let logical = batch("one", 1.0);
        push_and_flush(&mut core, logical.clone()).unwrap();
        core.handle_timer(stream_id, TimerKind::RotateStream);

        assert_eq!(
            core.handle_ack(stream_id, 2),
            vec![
                MetricEffect::CloseStream { stream_id },
                protocol_violation(
                    A,
                    "received acknowledgement for batch 2, expected batch 1",
                    vec![logical],
                ),
                MetricEffect::ReportError {
                    error: MetricStreamError::AckMismatch {
                        expected: 1,
                        actual: 2,
                    },
                }
            ]
        );
        assert!(core
            .handle_timer(stream_id, TimerKind::DrainExpired)
            .is_empty());
        assert!(core.start().is_empty());
    }

    #[test]
    fn rotation_waits_for_outstanding_acknowledgements() {
        let mut core = StatefulMetricsCore::default();
        let stream_id = open(&mut core);
        push_and_flush(&mut core, batch("one", 1.0)).unwrap();

        assert_eq!(
            core.handle_timer(stream_id, TimerKind::RotateStream),
            vec![MetricEffect::ScheduleTimer {
                stream_id,
                timer: Timer {
                    kind: TimerKind::DrainExpired,
                    after: CoreConfig::default().sender.drain_timeout,
                },
            }]
        );
        assert_eq!(core.current_stream_id(A), Some(stream_id));
        assert!(!core.has_send_capacity());
        let unsent = batch("two", 2.0);
        assert_eq!(
            push_and_flush(&mut core, unsent.clone())
                .unwrap_err()
                .into_batch(),
            unsent
        );

        let effects = core.handle_ack(stream_id, 1);
        assert_eq!(effects[0], MetricEffect::CloseStream { stream_id });
        let MetricEffect::OpenStream {
            endpoint: A,
            stream_id: replacement,
        } = effects[1]
        else {
            panic!("replacement stream should open after draining");
        };
        assert_ne!(replacement, stream_id);
        assert_eq!(core.current_stream_id(A), Some(replacement));
        assert_eq!(core.inflight_len(), 0);
        assert!(core
            .handle_timer(stream_id, TimerKind::DrainExpired)
            .is_empty());
    }

    #[test]
    fn stale_drain_timer_does_not_close_replacement_stream() {
        let mut core = StatefulMetricsCore::default();
        let first_stream = open(&mut core);
        push_and_flush(&mut core, batch("one", 1.0)).unwrap();
        let stale_timer = core
            .handle_timer(first_stream, TimerKind::RotateStream)
            .remove(0);

        let effects = core.handle_ack(first_stream, 1);
        let replacement = open_requested(&mut core, &effects)[0];
        push_and_flush(&mut core, batch("two", 2.0)).unwrap();
        core.handle_timer(replacement, TimerKind::RotateStream);

        let MetricEffect::ScheduleTimer { stream_id, timer } = stale_timer else {
            panic!("stream rotation should schedule a drain timer");
        };
        assert!(core.handle_timer(stream_id, timer.kind).is_empty());
        assert_eq!(core.current_stream_id(A), Some(replacement));
    }

    #[test]
    fn stale_reconnect_timer_does_not_open_replacement_stream() {
        let mut core = StatefulMetricsCore::default();
        let first_stream = open(&mut core);
        core.handle_stream_error(first_stream, retryable("first failure"));
        let (second_stream, _) = reopen(&mut core, first_stream);
        core.handle_stream_error(second_stream, retryable("second failure"));

        assert!(core
            .handle_timer(first_stream, TimerKind::Reconnect)
            .is_empty());
        assert_eq!(core.current_stream_id(A), None);
        assert!(matches!(
            core.handle_timer(second_stream, TimerKind::Reconnect)
                .as_slice(),
            [MetricEffect::OpenStream { .. }]
        ));
    }

    /// A drain that times out hands its unacknowledged batches back for retry. Resubmitted on
    /// the replacement, a batch carries the same datums because the new stream lacks them all.
    #[test]
    fn drain_timeout_returns_unacknowledged_batches_for_retry() {
        let mut core = StatefulMetricsCore::default();
        let stream_id = open(&mut core);
        let logical = complete_batch("one", 1.0);
        let first = sent_batch(&push_and_flush(&mut core, logical.clone()).unwrap()).clone();
        core.handle_timer(stream_id, TimerKind::RotateStream);

        let effects = core.handle_timer(stream_id, TimerKind::DrainExpired);
        assert_eq!(effects[0], MetricEffect::CloseStream { stream_id });
        assert_eq!(
            effects[1],
            MetricEffect::ReturnUnacknowledged {
                endpoint: A,
                action: MetricFailureAction::RetryWithBackoff,
                batches: vec![logical.clone()],
            }
        );
        let MetricEffect::OpenStream {
            stream_id: replacement,
            ..
        } = effects[2]
        else {
            panic!("replacement stream should open after drain timeout");
        };
        assert_eq!(effects.len(), 3);
        assert_eq!(core.inflight_len(), 0);

        assert!(core.handle_stream_opened(replacement).is_empty());
        let resent = core.send_batch_to(A, logical, Instant::now()).unwrap();
        let resent = sent_batch(&resent);
        assert_eq!((resent.stream, resent.batch_id), (replacement, 1));
        assert_eq!(resent.datums, first.datums);
        assert!(core.handle_ack(replacement, 1).is_empty());
        assert_eq!(core.inflight_len(), 0);
    }

    #[test]
    fn rotation_without_outstanding_batches_replaces_stream_immediately() {
        let mut core = StatefulMetricsCore::default();
        let stream_id = open(&mut core);

        let effects = core.handle_timer(stream_id, TimerKind::RotateStream);

        assert_eq!(effects[0], MetricEffect::CloseStream { stream_id });
        let MetricEffect::OpenStream {
            stream_id: replacement,
            ..
        } = effects[1]
        else {
            panic!("replacement stream should open immediately");
        };
        assert_ne!(replacement, stream_id);
        assert_eq!(core.current_stream_id(A), Some(replacement));
    }

    #[test]
    fn unavailable_returns_batches_and_reconnects() {
        assert_classified_failure(
            MetricStreamFailureKind::Unavailable,
            MetricFailureAction::RetryWithBackoff,
        );
    }

    #[test]
    fn deadline_exceeded_returns_batches_and_reconnects() {
        assert_classified_failure(
            MetricStreamFailureKind::DeadlineExceeded,
            MetricFailureAction::RetryWithBackoff,
        );
    }

    #[test]
    fn resource_exhausted_returns_batches_and_reconnects() {
        assert_classified_failure(
            MetricStreamFailureKind::ResourceExhausted,
            MetricFailureAction::RetryWithBackoff,
        );
    }

    #[test]
    fn invalid_argument_returns_batches_without_reconnecting() {
        assert_classified_failure(
            MetricStreamFailureKind::InvalidArgument,
            MetricFailureAction::DoNotRetry,
        );
    }

    #[test]
    fn unauthenticated_returns_batches_and_waits_for_credentials() {
        assert_classified_failure(
            MetricStreamFailureKind::Unauthenticated,
            MetricFailureAction::WaitForCredentials,
        );
    }

    #[test]
    fn failed_precondition_returns_batches_for_stateless_delivery() {
        assert_classified_failure(
            MetricStreamFailureKind::FailedPrecondition,
            MetricFailureAction::UseStatelessDelivery,
        );
    }

    #[test]
    fn protocol_violation_returns_batches_for_stateless_delivery() {
        assert_classified_failure(
            MetricStreamFailureKind::ProtocolViolation,
            MetricFailureAction::UseStatelessDelivery,
        );
    }

    #[test]
    fn destination_reset_resumes_delivery_after_credentials_change() {
        let mut core = StatefulMetricsCore::default();
        let stream_id = open(&mut core);
        core.handle_stream_error(
            stream_id,
            MetricStreamFailure::new(
                MetricStreamFailureKind::Unauthenticated,
                "expired credentials",
            ),
        );

        assert!(core.start().is_empty());
        assert!(matches!(
            core.reset_destination_state(A).as_slice(),
            [MetricEffect::OpenStream { endpoint: A, .. }]
        ));
    }

    /// A payload whose definitions an earlier, acknowledged payload already carried is
    /// resubmitted on a fresh stream with those definitions, because the new receiver has none.
    #[test]
    fn resubmission_carries_definitions_the_new_stream_lacks() {
        let mut core = StatefulMetricsCore::default();
        let first_stream = open(&mut core);
        let logical = complete_batch("shared", 3.5);
        let introduced = definition_keys(sent_batch(
            &push_and_flush(&mut core, logical.clone()).unwrap(),
        ));
        core.handle_ack(first_stream, 1);
        let reuse = push_and_flush(&mut core, logical.clone()).unwrap();
        assert!(definition_keys(sent_batch(&reuse)).is_empty());
        let effects = core.handle_stream_error(first_stream, retryable("reset"));
        assert!(
            matches!(&effects[1], MetricEffect::StreamFailed { unacknowledged, .. }
            if *unacknowledged == vec![logical.clone()])
        );

        reopen(&mut core, first_stream);
        let resent = core.send_batch_to(A, logical, Instant::now()).unwrap();
        let resent = sent_batch(&resent);
        assert_eq!(resent.batch_id, 1);
        let mut expected = introduced;
        expected.sort();
        assert_eq!(definition_keys(resent), expected);
        assert!(matches!(
            resent.datums.last().unwrap().data,
            Some(datum::Data::MetricSeriesBatch(_))
        ));
    }

    #[test]
    fn disconnected_push_does_not_encode() {
        let mut core = StatefulMetricsCore::default();
        let expected = complete_batch("queued", 1.0);
        let mut logical = expected.clone();

        for _ in 0..10_000 {
            logical = push_and_flush(&mut core, logical).unwrap_err().into_batch();
        }

        assert_eq!(logical, expected);
        assert_eq!(core.encoding_count(), 0);
    }

    /// A destination reset closes the stream, returns its unacknowledged batches for retry,
    /// and opens a replacement; the partial buffer stays put.
    #[test]
    fn destination_reset_returns_unacknowledged_batches() {
        let logical = complete_batch("private", 1.0);
        let mut core = StatefulMetricsCore::default();
        let stream_id = open(&mut core);
        push_and_flush(&mut core, logical.clone()).unwrap();
        assert!(core.handle_ack(stream_id, 1).is_empty());
        push_and_flush(&mut core, logical.clone()).unwrap();
        let pending = batch("pending", 2.0);
        core.push_batch(pending, Instant::now()).unwrap();

        let effects = core.reset_destination_state(A);
        assert_eq!(effects[0], MetricEffect::CloseStream { stream_id });
        assert_eq!(
            effects[1],
            MetricEffect::ReturnUnacknowledged {
                endpoint: A,
                action: MetricFailureAction::RetryWithBackoff,
                batches: vec![logical.clone()],
            }
        );
        let MetricEffect::OpenStream {
            endpoint: A,
            stream_id: replacement,
        } = effects[2]
        else {
            panic!("replacement destination should open a stream");
        };
        assert_eq!(effects.len(), 3);
        assert_eq!(core.buffered_series_len(), 1);
        assert_eq!(core.inflight_len(), 0);
        assert!(core.handle_stream_opened(replacement).is_empty());
        let resent = core.send_batch_to(A, logical, Instant::now()).unwrap();
        assert!(!definition_keys(sent_batch(&resent)).is_empty());
    }

    /// Shutdown closes every stream and hands back each endpoint's unacknowledged batches and
    /// the partial buffer, leaving the core empty until it is started again.
    #[test]
    fn shutdown_transfers_all_logical_work_without_reopening() {
        let mut core = core_with(2);
        let streams = open_all(&mut core);
        let sent = batch("sent", 1.0);
        push_and_flush(&mut core, sent.clone()).unwrap();
        assert!(core.handle_ack(streams[0], 1).is_empty());
        let pending = batch("pending", 2.0);
        core.push_batch(pending.clone(), Instant::now()).unwrap();

        assert_eq!(
            core.shutdown(),
            vec![
                MetricEffect::CloseStream {
                    stream_id: streams[0]
                },
                MetricEffect::CloseStream {
                    stream_id: streams[1]
                },
                MetricEffect::ReturnUnacknowledged {
                    endpoint: B,
                    action: MetricFailureAction::RetryWithBackoff,
                    batches: vec![sent],
                },
                MetricEffect::ReturnBuffered { batch: pending },
            ]
        );
        assert_eq!(core.inflight_len(), 0);
        assert_eq!(core.buffered_series_len(), 0);
        assert_eq!(core.current_stream_id(A), None);
        assert!(core.handle_ack(streams[1], 1).iter().all(|effect| matches!(
            effect,
            MetricEffect::ReportError {
                error: MetricStreamError::UnexpectedStream { .. }
            }
        )));
        assert_eq!(open_all(&mut core).len(), 2);
    }

    #[test]
    fn reset_of_an_unknown_endpoint_is_reported() {
        let mut core = StatefulMetricsCore::default();
        assert_eq!(
            core.reset_destination_state(B),
            vec![MetricEffect::ReportError {
                error: MetricStreamError::UnknownEndpoint(B),
            }]
        );
    }

    #[test]
    fn bounded_window_rejects_without_encoding() {
        let mut core = StatefulMetricsCore::new(CoreConfig {
            sender: SenderConfig {
                max_inflight_payloads: 1,
                ..SenderConfig::default()
            },
            ..CoreConfig::default()
        });
        open(&mut core);
        push_and_flush(&mut core, batch("one", 1.0)).unwrap();
        let encoding_count = core.encoding_count();
        let second = batch("two", 2.0);

        let error = push_and_flush(&mut core, second.clone()).unwrap_err();

        assert_eq!(error.into_batch(), second);
        assert_eq!(core.encoding_count(), encoding_count);
    }

    #[test]
    fn consecutive_pushes_accumulate_until_explicit_flush_in_input_order() {
        let mut core = StatefulMetricsCore::default();
        let stream = open(&mut core);
        let first = batch("first", 1.0);
        let second = batch("second", 2.0);
        core.push_batch(first.clone(), Instant::now()).unwrap();
        assert!(core
            .push_batch(second.clone(), Instant::now())
            .unwrap()
            .is_empty());
        assert_eq!(core.buffered_series_len(), 2);
        assert_eq!(core.inflight_len(), 0);
        assert_eq!(core.encoding_count(), 0);
        let effects = core.flush(Instant::now()).unwrap();
        assert_eq!(sent_batch(&effects).batch_id, 1);
        assert_eq!(core.inflight_len(), 1);
        assert_eq!(core.buffered_series_len(), 0);
        let effects = core.handle_stream_error(
            stream,
            MetricStreamFailure::new(MetricStreamFailureKind::InvalidArgument, "rejected"),
        );
        let MetricEffect::StreamFailed { unacknowledged, .. } = &effects[1] else {
            panic!("expected failed batch");
        };
        let mut series = first.into_series();
        series.extend(second.into_series());
        assert_eq!(*unacknowledged, vec![LogicalMetricBatch::new(series)]);
    }

    #[test]
    fn buffering_rejections_preserve_ownership_and_encoder_state() {
        let mut core = StatefulMetricsCore::new(CoreConfig {
            sender: SenderConfig {
                max_inflight_payloads: 1,
                ..SenderConfig::default()
            },
            ..CoreConfig::default()
        });
        let input = batch("input", 1.0);
        assert!(
            matches!(core.push_batch(input.clone(), Instant::now()), Err(MetricPushError::StreamUnavailable(returned)) if returned == input)
        );
        open(&mut core);
        core.push_batch(input.clone(), Instant::now()).unwrap();
        let invalid = batch("", 2.0);
        assert!(
            matches!(core.push_batch(invalid.clone(), Instant::now()), Err(MetricPushError::EmptyBatch(returned)) if returned == invalid)
        );
        assert_eq!(core.buffered_series_len(), 1);
        assert_eq!(core.encoding_count(), 0);
        core.flush(Instant::now()).unwrap();
        assert!(
            matches!(core.push_batch(input.clone(), Instant::now()), Err(MetricPushError::InflightWindowFull(returned)) if returned == input)
        );
        assert_eq!(core.buffered_series_len(), 0);
        assert_eq!(core.encoding_count(), 1);
    }

    #[test]
    fn zero_capacity_and_oversized_inputs_flush_on_admission() {
        for capacity in [0, 1, 2] {
            let mut core = StatefulMetricsCore::new(CoreConfig {
                batch_capacity: capacity,
                ..CoreConfig::default()
            });
            open(&mut core);
            let mut series = batch("first", 1.0).into_series();
            series.extend(batch("second", 2.0).into_series());
            series.extend(batch("third", 3.0).into_series());
            let effects = core
                .push_batch(LogicalMetricBatch::new(series), Instant::now())
                .unwrap();
            assert_eq!(sent_batch(&effects).batch_id, 1);
            assert_eq!(core.buffered_series_len(), 0);
            assert_eq!(core.inflight_len(), 1);
        }
    }

    #[test]
    fn zero_endpoints_is_treated_as_one() {
        let mut core = core_with(0);
        assert_eq!(core.endpoint_count(), 1);
        open(&mut core);
        assert!(core.has_send_capacity());
    }

    /// Rotation keeps the partial buffer; it flushes onto the replacement stream.
    #[test]
    fn rotation_keeps_the_partial_batch_for_the_replacement() {
        let mut core = StatefulMetricsCore::default();
        let stream = open(&mut core);
        let pending = batch("unsent", 2.0);
        core.push_batch(pending, Instant::now()).unwrap();

        let effects = core.handle_timer(stream, TimerKind::RotateStream);
        assert!(matches!(
            effects.as_slice(),
            [
                MetricEffect::CloseStream { .. },
                MetricEffect::OpenStream { .. }
            ]
        ));
        assert_eq!(core.buffered_series_len(), 1);
        let replacement = open_requested(&mut core, &effects)[0];
        let effects = core.flush(Instant::now()).unwrap();
        assert_eq!(sent_batch(&effects).stream, replacement);
    }

    #[test]
    fn flush_during_drain_returns_unsent_ownership_once() {
        let mut core = StatefulMetricsCore::default();
        let stream = open(&mut core);
        core.push_batch(batch("sent", 1.0), Instant::now()).unwrap();
        core.flush(Instant::now()).unwrap();
        let pending = batch("unsent", 2.0);
        core.push_batch(pending.clone(), Instant::now()).unwrap();
        core.handle_timer(stream, TimerKind::RotateStream);
        assert!(
            matches!(core.flush(Instant::now()), Err(MetricPushError::StreamUnavailable(batch)) if batch == pending)
        );
        assert_eq!(core.buffered_series_len(), 0);
        assert!(core.flush(Instant::now()).unwrap().is_empty());
        let effects = core.handle_timer(stream, TimerKind::DrainExpired);
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, MetricEffect::ReturnBuffered { .. })));
    }

    #[test]
    fn invalid_ack_returns_unsent_data_after_unacknowledged_data() {
        let mut core = StatefulMetricsCore::default();
        let stream = open(&mut core);
        let sent = batch("sent", 1.0);
        let pending = batch("unsent", 2.0);
        core.push_batch(sent.clone(), Instant::now()).unwrap();
        core.flush(Instant::now()).unwrap();
        core.push_batch(pending.clone(), Instant::now()).unwrap();

        let effects = core.handle_ack(stream, 9);
        assert!(matches!(&effects[1], MetricEffect::StreamFailed {
            failure,
            action: MetricFailureAction::UseStatelessDelivery,
            unacknowledged,
            ..
        } if failure.kind() == MetricStreamFailureKind::ProtocolViolation
            && *unacknowledged == vec![sent]));
        assert!(matches!(&effects[2], MetricEffect::ReturnBuffered { batch } if *batch == pending));
        assert!(matches!(
            &effects[3],
            MetricEffect::ReportError {
                error: MetricStreamError::AckMismatch { .. }
            }
        ));
        assert_eq!(core.buffered_series_len(), 0);
        assert_eq!(core.inflight_len(), 0);
    }

    /// A retryable failure keeps buffered input in the core instead of returning it.
    #[test]
    fn buffered_input_survives_a_retryable_failure() {
        let mut core = StatefulMetricsCore::default();
        let stream = open(&mut core);
        core.push_batch(batch("never_sent", 1.0), Instant::now())
            .unwrap();
        let effects = core.handle_stream_error(stream, retryable("retry"));
        assert!(matches!(effects.as_slice(), [
            MetricEffect::CloseStream { .. },
            MetricEffect::StreamFailed { unacknowledged, .. },
            MetricEffect::ScheduleReconnect { .. },
        ] if unacknowledged.is_empty()));
        assert_eq!(core.buffered_series_len(), 1);

        let (_, opened) = reopen(&mut core, stream);
        assert!(opened.is_empty());
        let effects = core.flush(Instant::now()).unwrap();
        assert_eq!(sent_batch(&effects).batch_id, 1);
        assert_eq!(core.encoding_count(), 1);
    }

    #[test]
    fn stale_stream_events_leave_partial_batch_intact() {
        let mut core = StatefulMetricsCore::default();
        let old_stream = open(&mut core);
        let effects = core.reset_destination_state(A);
        let current = open_requested(&mut core, &effects)[0];
        core.push_batch(batch("pending", 1.0), Instant::now())
            .unwrap();
        assert_eq!(
            core.handle_stream_error(old_stream, retryable("stale")),
            vec![MetricEffect::ReportError {
                error: MetricStreamError::UnexpectedStream {
                    stream_id: old_stream
                },
            }]
        );
        core.handle_ack(old_stream, 1);
        core.handle_timer(old_stream, TimerKind::RotateStream);
        assert_eq!(core.buffered_series_len(), 1);
        assert_eq!(core.encoding_count(), 0);
        let effects = core.flush(Instant::now()).unwrap();
        assert_eq!(sent_batch(&effects).stream, current);
    }

    #[test]
    fn validated_batches_keep_fifo_and_threshold_semantics_through_failure() {
        let mut core = StatefulMetricsCore::new(CoreConfig {
            batch_capacity: 2,
            ..CoreConfig::default()
        });
        let stream = open(&mut core);
        let raw_series = vec![
            LogicalMetricSeries::new("", MetricSeriesType::Gauge, vec![MetricPoint::new(1, 9.0)]),
            LogicalMetricSeries::new(
                "first",
                MetricSeriesType::Gauge,
                vec![MetricPoint::new(1, 1.0)],
            ),
            LogicalMetricSeries::new("empty", MetricSeriesType::Gauge, Vec::new()),
        ];
        let first: LogicalMetricBatch =
            serde_json::from_value(serde_json::json!({ "series": raw_series })).unwrap();
        assert!(core
            .push_batch(first.clone(), Instant::now())
            .unwrap()
            .is_empty());
        assert_eq!(core.buffered_series_len(), 1);
        assert_eq!(core.encoding_count(), 0);

        let second = LogicalMetricBatch::new(vec![LogicalMetricSeries::new(
            "second",
            MetricSeriesType::Gauge,
            vec![MetricPoint::new(2, f64::NAN), MetricPoint::new(2, 2.0)],
        )]);
        let effects = core.push_batch(second.clone(), Instant::now()).unwrap();
        assert_eq!(sent_batch(&effects).batch_id, 1);
        assert_eq!(core.buffered_series_len(), 0);
        assert_eq!(core.inflight_len(), 1);
        assert_eq!(core.encoding_count(), 1);

        let pending = batch("third", 3.0);
        assert!(core
            .push_batch(pending.clone(), Instant::now())
            .unwrap()
            .is_empty());
        let effects = core.handle_stream_error(
            stream,
            MetricStreamFailure::new(MetricStreamFailureKind::InvalidArgument, "rejected"),
        );
        let mut series = first.into_series();
        series.extend(second.into_series());
        let expected = LogicalMetricBatch::new(series);
        assert!(
            matches!(&effects[1], MetricEffect::StreamFailed { unacknowledged, .. } if *unacknowledged == vec![expected])
        );
        assert!(matches!(&effects[2], MetricEffect::ReturnBuffered { batch } if *batch == pending));
        assert_eq!(core.buffered_series_len(), 0);
    }

    /// The store is written only when a payload is encoded: buffered series are not
    /// interned, and a flush seals exactly the definitions it introduced.
    #[test]
    fn rule_store_reflects_only_flushed_payloads() {
        let mut core = StatefulMetricsCore::default();
        open(&mut core);
        assert!(core
            .push_batch(complete_batch("requests", 1.0), Instant::now())
            .unwrap()
            .is_empty());
        assert_eq!(core.head.keys().count(), 0);

        let effects = core.flush(Instant::now()).unwrap();
        let mut sent = definition_keys(sent_batch(&effects));
        assert!(!sent.is_empty());
        sent.sort();
        assert_eq!(core.head.keys().collect::<Vec<_>>(), sent);
        assert_head_matches_dictionary(&core);
    }

    /// An empty flush still evicts, and those evictions reach the store.
    #[test]
    fn idle_flush_evictions_reach_the_rule_store() {
        let mut core = eviction_core(1);
        let stream_id = open(&mut core);
        push_and_flush(&mut core, complete_batch("requests", 1.0)).unwrap();
        assert!(core.handle_ack(stream_id, 1).is_empty());
        let before = core.head.keys().count();

        assert!(core.flush(Instant::now()).unwrap().is_empty());

        assert!(core.head.keys().count() < before);
        assert_head_matches_dictionary(&core);
        assert!(core.endpoints[0].sent.len() <= core.head.keys().count());
    }

    /// Eviction no longer waits for acknowledgements: once a payload has been dispatched,
    /// its definitions are eligible even while it is outstanding, and every path that ends a
    /// stream leaves the store matching the dictionary.
    #[test]
    fn rule_store_tracks_the_dictionary_through_stream_ends() {
        let mut core = eviction_core(1);
        let mut stream_id = open(&mut core);

        for name in ["a", "b", "c"] {
            push_and_flush(&mut core, complete_batch(name, 1.0)).unwrap();
            assert_head_matches_dictionary(&core);
        }

        let effects = core.handle_stream_error(stream_id, retryable("retry"));
        let MetricEffect::StreamFailed { unacknowledged, .. } = &effects[1] else {
            panic!("a retryable failure returns the stream's batches");
        };
        assert_eq!(unacknowledged.len(), 3);
        assert_head_matches_dictionary(&core);
        (stream_id, _) = reopen(&mut core, stream_id);
        for logical in unacknowledged.clone() {
            core.send_batch_to(A, logical, Instant::now()).unwrap();
            assert_head_matches_dictionary(&core);
        }

        core.handle_ack(stream_id, 99);
        assert_head_matches_dictionary(&core);

        let effects = core.reset_destination_state(A);
        open_requested(&mut core, &effects);
        assert_head_matches_dictionary(&core);
    }

    /// A batch returned for a down endpoint can outlive its definitions: by the time the
    /// endpoint reconnects they may be evicted. Resubmission re-encodes it, defining those
    /// values again under fresh IDs, and the batch still carries its full closure.
    #[test]
    fn resubmission_after_eviction_redefines_values_under_fresh_ids() {
        let mut core = eviction_core(2);
        let streams = open_all(&mut core);
        core.handle_stream_error(streams[1], retryable("b down"));

        let p = complete_batch("p", 1.0);
        let effects = push_and_flush(&mut core, p.clone()).unwrap();
        let original = definition_keys(sent_batch(&effects));
        assert!(effects.contains(&MetricEffect::ReturnUnacknowledged {
            endpoint: B,
            action: MetricFailureAction::RetryWithBackoff,
            batches: vec![p.clone()],
        }));
        assert!(core.handle_ack(streams[0], 1).is_empty());
        push_and_flush(&mut core, batch("q", 2.0)).unwrap();
        assert!(
            original
                .iter()
                .any(|key| !core.head.keys().any(|live| live == *key)),
            "the first payload's definitions should have been evicted"
        );

        reopen(&mut core, streams[1]);
        let resent = core.send_batch_to(B, p, Instant::now()).unwrap();
        let redefined = definition_keys(sent_batch(&resent));
        assert!(redefined.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(redefined.iter().any(|key| !original.contains(key)));
        assert_head_matches_dictionary(&core);
    }

    /// Payload IDs are global: they keep increasing across streams while batch IDs restart.
    #[test]
    fn payload_ids_increase_across_streams() {
        let mut core = StatefulMetricsCore::default();
        let stream_id = open(&mut core);
        push_and_flush(&mut core, batch("one", 1.0)).unwrap();
        push_and_flush(&mut core, batch("two", 2.0)).unwrap();
        assert_eq!(
            core.retained.keys().copied().collect::<Vec<_>>(),
            vec![PayloadId(0), PayloadId(1)]
        );

        core.handle_stream_error(stream_id, retryable("retry"));
        reopen(&mut core, stream_id);
        push_and_flush(&mut core, batch("three", 3.0)).unwrap();

        let sent = core.endpoints[0].outstanding.back().unwrap();
        assert_eq!(sent.payload_id, PayloadId(2));
        assert_eq!(sent.batch_id, 1);
    }

    /// One encoding serves every open endpoint, and the payload is released only once each
    /// has acknowledged it.
    #[test]
    fn every_endpoint_receives_each_payload_from_one_encoding() {
        let mut core = core_with(2);
        let streams = open_all(&mut core);
        assert_eq!(streams.len(), 2);

        let effects = push_and_flush(&mut core, complete_batch("shared", 1.0)).unwrap();
        let sent = sends(&effects);
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent.iter().map(|batch| batch.stream).collect::<Vec<_>>(),
            streams
        );
        assert_eq!(sent[0].datums, sent[1].datums);
        assert_eq!(core.encoding_count(), 1);
        assert_eq!(core.inflight_len(), 1);

        assert!(core.handle_ack(streams[0], 1).is_empty());
        assert_eq!(core.inflight_len(), 1);
        assert!(core.handle_ack(streams[1], 1).is_empty());
        assert_eq!(core.inflight_len(), 0);
    }

    /// One endpoint's retryable failure leaves the others sending. It returns what its stream
    /// had not acknowledged, and its copy of each later flush comes straight back, so the core
    /// holds nothing for it. Resubmitted after it reconnects, each batch is sent only the
    /// definitions the new stream still lacks.
    #[test]
    fn retryable_failure_on_one_endpoint_does_not_disturb_the_others() {
        let mut core = core_with(2);
        let streams = open_all(&mut core);
        let p1 = complete_batch("p1", 1.0);
        push_and_flush(&mut core, p1.clone()).unwrap();

        assert_eq!(
            core.handle_stream_error(streams[1], retryable("b down")),
            vec![
                MetricEffect::CloseStream {
                    stream_id: streams[1]
                },
                MetricEffect::StreamFailed {
                    endpoint: B,
                    failure: retryable("b down"),
                    action: MetricFailureAction::RetryWithBackoff,
                    unacknowledged: vec![p1.clone()],
                },
                MetricEffect::ScheduleReconnect {
                    stream_id: streams[1]
                },
            ]
        );
        assert!(core.has_send_capacity());
        assert!(!core.endpoint_has_send_capacity(B));
        let p2 = complete_batch("p2", 2.0);
        let effects = push_and_flush(&mut core, p2.clone()).unwrap();
        let sent = sends(&effects);
        assert_eq!(sent.len(), 1);
        assert_eq!((sent[0].stream, sent[0].batch_id), (streams[0], 2));
        assert!(effects.contains(&MetricEffect::ReturnUnacknowledged {
            endpoint: B,
            action: MetricFailureAction::RetryWithBackoff,
            batches: vec![p2.clone()],
        }));
        assert!(core.handle_ack(streams[0], 1).is_empty());
        assert!(core.handle_ack(streams[0], 2).is_empty());
        assert_eq!(core.inflight_len(), 0, "the core keeps nothing for B");

        let (replacement, opened) = reopen(&mut core, streams[1]);
        assert!(opened.is_empty());
        assert!(core.endpoint_has_send_capacity(B));
        let first = core.send_batch_to(B, p1, Instant::now()).unwrap();
        let second = core.send_batch_to(B, p2, Instant::now()).unwrap();
        let (first, second) = (sent_batch(&first), sent_batch(&second));
        assert_eq!(
            [
                (first.stream, first.batch_id),
                (second.stream, second.batch_id)
            ],
            [(replacement, 1), (replacement, 2)]
        );
        let first = definition_keys(first);
        assert!(definition_keys(second)
            .iter()
            .all(|key| !first.contains(key)));
        assert_eq!(core.encoding_count(), 4);
        assert!(core.handle_ack(replacement, 1).is_empty());
        assert!(core.handle_ack(replacement, 2).is_empty());
        assert_eq!(core.inflight_len(), 0);
    }

    /// Resubmission targets one endpoint and leaves the partial buffer alone.
    #[test]
    fn send_batch_to_reaches_only_its_endpoint() {
        let mut core = core_with(2);
        let streams = open_all(&mut core);
        let pending = batch("pending", 2.0);
        core.push_batch(pending, Instant::now()).unwrap();

        let effects = core
            .send_batch_to(B, batch("retry", 1.0), Instant::now())
            .unwrap();
        let [MetricEffect::SendBatch { batch }] = effects.as_slice() else {
            panic!("resubmission sends one batch, got {effects:?}");
        };
        assert_eq!((batch.stream, batch.batch_id), (streams[1], 1));
        assert_eq!(core.buffered_series_len(), 1);
        assert_eq!(core.inflight_len(), 1);

        let effects = core.flush(Instant::now()).unwrap();
        assert_eq!(
            sends(&effects)
                .iter()
                .map(|batch| (batch.stream, batch.batch_id))
                .collect::<Vec<_>>(),
            [(streams[0], 1), (streams[1], 2)]
        );
    }

    /// Resubmission rejects without encoding and returns the batch unchanged.
    #[test]
    fn send_batch_to_rejections_return_the_batch() {
        let mut core = StatefulMetricsCore::new(CoreConfig {
            metrics_endpoints: 2,
            sender: SenderConfig {
                max_inflight_payloads: 1,
                ..SenderConfig::default()
            },
            ..CoreConfig::default()
        });
        let input = batch("retry", 1.0);
        let now = Instant::now();
        assert_eq!(
            core.send_batch_to(A, input.clone(), now),
            Err(MetricPushError::StreamUnavailable(input.clone()))
        );
        let streams = open_all(&mut core);
        assert_eq!(
            core.send_batch_to(MetricEndpointId(2), input.clone(), now),
            Err(MetricPushError::UnknownEndpoint(
                MetricEndpointId(2),
                input.clone()
            ))
        );
        let empty = LogicalMetricBatch::default();
        assert_eq!(
            core.send_batch_to(A, empty.clone(), now),
            Err(MetricPushError::EmptyBatch(empty))
        );
        core.send_batch_to(A, input.clone(), now).unwrap();
        assert_eq!(
            core.send_batch_to(A, input.clone(), now),
            Err(MetricPushError::InflightWindowFull(input.clone()))
        );
        assert_eq!(core.encoding_count(), 1);
        let effects = core.send_batch_to(B, input, now).unwrap();
        assert_eq!(sent_batch(&effects).stream, streams[1]);
    }

    /// A stream receives a definition when a payload first references it, not as a snapshot
    /// of everything the dictionary holds.
    #[test]
    fn a_new_stream_receives_definitions_lazily() {
        let mut core = core_with(2);
        let streams = open_all(&mut core);
        let x = push_and_flush(&mut core, complete_batch("x", 1.0)).unwrap();
        let x_keys = definition_keys(sent_batch(&x));
        core.handle_ack(streams[0], 1);
        core.handle_ack(streams[1], 1);
        core.handle_stream_error(streams[1], retryable("b down"));
        let (replacement, opened) = reopen(&mut core, streams[1]);
        assert!(opened.is_empty(), "nothing is pushed to a fresh stream");

        let effects = push_and_flush(&mut core, batch("y", 2.0)).unwrap();
        let sent = sends(&effects);
        let to_a = sent
            .iter()
            .find(|batch| batch.stream == streams[0])
            .unwrap();
        let to_b = sent
            .iter()
            .find(|batch| batch.stream == replacement)
            .unwrap();
        assert_eq!(definition_keys(to_a), [DefinitionKey::Name(2)]);
        assert_eq!(definition_keys(to_b), [DefinitionKey::Name(2)]);
        assert!(definition_keys(to_b)
            .iter()
            .all(|key| !x_keys.contains(key)));
        assert_eq!(to_a.datums.last(), to_b.datums.last());
    }

    /// A permanent failure suspends one endpoint and returns only its copies. Later payloads
    /// still go to the healthy endpoint, and the suspended one's copy comes straight back.
    #[test]
    fn permanent_failure_on_one_endpoint_returns_only_its_copies() {
        let mut core = core_with(2);
        let streams = open_all(&mut core);
        let p1 = complete_batch("p1", 1.0);
        push_and_flush(&mut core, p1.clone()).unwrap();
        core.push_batch(batch("pending", 3.0), Instant::now())
            .unwrap();

        let effects = core.handle_stream_error(
            streams[1],
            MetricStreamFailure::new(MetricStreamFailureKind::InvalidArgument, "rejected"),
        );
        assert!(matches!(effects.as_slice(), [
            MetricEffect::CloseStream { .. },
            MetricEffect::StreamFailed { endpoint: B, unacknowledged, .. },
        ] if *unacknowledged == vec![p1.clone()]));
        assert_eq!(core.inflight_len(), 1, "A still owes an acknowledgement");
        assert_eq!(core.buffered_series_len(), 1);

        core.flush(Instant::now()).unwrap();
        let p2 = batch("p2", 2.0);
        let effects = push_and_flush(&mut core, p2.clone()).unwrap();
        assert_eq!(sends(&effects).len(), 1);
        assert!(effects.contains(&MetricEffect::ReturnUnacknowledged {
            endpoint: B,
            action: MetricFailureAction::DoNotRetry,
            batches: vec![p2],
        }));
        for batch_id in 1..=3 {
            assert!(core.handle_ack(streams[0], batch_id).is_empty());
        }
        assert_eq!(core.inflight_len(), 0);

        let effects = core.reset_destination_state(B);
        let replacement = open_requested(&mut core, &effects)[0];
        assert_eq!(core.current_stream_id(B), Some(replacement));
    }

    /// The partial buffer is handed back only once no endpoint can deliver it.
    #[test]
    fn buffered_input_returns_only_when_every_endpoint_is_suspended() {
        let mut core = core_with(2);
        let streams = open_all(&mut core);
        let pending = batch("pending", 1.0);
        core.push_batch(pending.clone(), Instant::now()).unwrap();
        let rejected =
            || MetricStreamFailure::new(MetricStreamFailureKind::InvalidArgument, "rejected");

        let effects = core.handle_stream_error(streams[0], rejected());
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, MetricEffect::ReturnBuffered { .. })));
        let effects = core.handle_stream_error(streams[1], rejected());
        assert_eq!(
            effects.last(),
            Some(&MetricEffect::ReturnBuffered { batch: pending })
        );
    }

    /// Each endpoint's inflight window is bounded on its own. An endpoint that falls behind
    /// gets its copy of later flushes back instead of holding up the others; the core rejects
    /// input only once every open endpoint is full.
    #[test]
    fn inflight_window_is_per_endpoint() {
        let mut core = StatefulMetricsCore::new(CoreConfig {
            metrics_endpoints: 2,
            sender: SenderConfig {
                max_inflight_payloads: 1,
                ..SenderConfig::default()
            },
            ..CoreConfig::default()
        });
        let streams = open_all(&mut core);
        push_and_flush(&mut core, batch("one", 1.0)).unwrap();
        assert!(core.handle_ack(streams[0], 1).is_empty());
        assert!(core.has_send_capacity());
        assert!(!core.endpoint_has_send_capacity(B));

        let two = batch("two", 2.0);
        let effects = push_and_flush(&mut core, two.clone()).unwrap();
        assert_eq!(sent_batch(&effects).stream, streams[0]);
        assert!(effects.contains(&MetricEffect::ReturnUnacknowledged {
            endpoint: B,
            action: MetricFailureAction::RetryWithBackoff,
            batches: vec![two],
        }));
        assert!(!core.has_send_capacity());
        assert!(matches!(
            push_and_flush(&mut core, batch("three", 3.0)),
            Err(MetricPushError::InflightWindowFull(_))
        ));
        assert!(core.handle_ack(streams[1], 1).is_empty());
        assert!(core.has_send_capacity());
        assert_eq!(core.inflight_len(), 1);
    }

    /// An acknowledgement error on one endpoint suspends only that endpoint.
    #[test]
    fn protocol_violation_on_one_endpoint_spares_the_others() {
        let mut core = core_with(2);
        let streams = open_all(&mut core);
        let logical = batch("one", 1.0);
        push_and_flush(&mut core, logical.clone()).unwrap();

        let effects = core.handle_ack(streams[1], 7);
        assert_eq!(
            effects[1],
            protocol_violation(
                B,
                "received acknowledgement for batch 7, expected batch 1",
                vec![logical],
            )
        );
        assert_eq!(core.current_stream_id(A), Some(streams[0]));
        assert!(core.has_send_capacity());
        assert!(core.handle_ack(streams[0], 1).is_empty());
        assert_eq!(core.inflight_len(), 0);
    }

    /// Rotating one endpoint returns only that endpoint's outstanding batches.
    #[test]
    fn rotation_is_per_endpoint() {
        let mut core = core_with(2);
        let streams = open_all(&mut core);
        let one = batch("one", 1.0);
        push_and_flush(&mut core, one.clone()).unwrap();
        assert!(core.handle_ack(streams[0], 1).is_empty());

        core.handle_timer(streams[0], TimerKind::RotateStream);
        let effects = core.handle_timer(streams[1], TimerKind::RotateStream);
        assert!(matches!(
            effects.as_slice(),
            [MetricEffect::ScheduleTimer { .. }]
        ));
        let effects = core.handle_timer(streams[1], TimerKind::DrainExpired);
        assert!(matches!(
            effects.as_slice(),
            [
                MetricEffect::CloseStream { .. },
                MetricEffect::ReturnUnacknowledged { endpoint: B, batches, .. },
                MetricEffect::OpenStream { endpoint: B, .. },
            ] if *batches == vec![one]
        ));
        assert!(core.current_stream_id(A).is_some());
        assert_eq!(core.inflight_len(), 0);
    }
}
