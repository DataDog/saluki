use std::time::Instant;

use crate::{
    proto::stateful::StatefulBatch as ProtoStatefulBatch, BatchCompressor, CoreConfig,
    NoopBatchCompressor, StreamId, Timer, TimerKind,
};

use super::{
    LogicalMetricBatch, MetricBatchEncoder, MetricDictionaryStats, MetricEffect, MetricEndpointId,
    MetricFailureAction, MetricPushError, MetricStreamError, MetricStreamFailure,
    MetricStreamFailureKind, StatefulMetricsCore,
};

/// Transport-ready effects emitted by [`StatefulMetricsClient`].
#[derive(Clone, Debug, PartialEq)]
pub enum MetricClientEffect {
    /// Open an initial or replacement stream to an endpoint.
    OpenStream {
        endpoint: MetricEndpointId,
        stream_id: StreamId,
    },
    /// Send a serialized and compressed payload on an open stream.
    SendPayload {
        stream_id: StreamId,
        payload: ProtoStatefulBatch,
    },
    /// Close a stream that is no longer usable.
    CloseStream { stream_id: StreamId },
    /// Report a classified failure on one endpoint's stream. See [`MetricEffect::StreamFailed`].
    StreamFailed {
        endpoint: MetricEndpointId,
        failure: MetricStreamFailure,
        action: MetricFailureAction,
        unacknowledged: Vec<LogicalMetricBatch>,
    },
    /// Return an endpoint's copy of complete logical batches it cannot carry now. See
    /// [`MetricEffect::ReturnUnacknowledged`].
    ReturnUnacknowledged {
        endpoint: MetricEndpointId,
        action: MetricFailureAction,
        batches: Vec<LogicalMetricBatch>,
    },
    /// Return the unsent partial batch once every endpoint is suspended, or at shutdown.
    ReturnBuffered { batch: LogicalMetricBatch },
    /// Schedule a reconnect for a failed stream using the caller's backoff policy.
    ScheduleReconnect { stream_id: StreamId },
    /// Schedule a stream-scoped timer and feed it back through [`StatefulMetricsClient::handle_timer`].
    ScheduleTimer { stream_id: StreamId, timer: Timer },
    /// Report a protocol state error.
    ReportError { error: MetricStreamError },
}

/// Error returned while accepting a logical metric batch.
#[derive(Clone, Debug, PartialEq)]
pub enum MetricClientError {
    /// The state machine rejected the logical batch without changing its state.
    Push(MetricPushError),
}

/// Transport-facing stateful metrics client.
///
/// This type combines the sans-I/O state machine with wire serialization and compression.
/// Callers submit logical batches and execute transport-ready [`MetricClientEffect`] values;
/// they never need to connect [`StatefulMetricsCore`] to [`MetricBatchEncoder`] themselves.
///
/// A batch that fails wire encoding fails only the stream it was planned for, as a
/// [`MetricStreamFailureKind::Unavailable`] failure: the effects returned in its place close
/// that stream, return its unacknowledged batches including this one, and schedule its
/// reconnect.
#[derive(Debug)]
pub struct StatefulMetricsClient<C = NoopBatchCompressor> {
    core: StatefulMetricsCore,
    batch_encoder: MetricBatchEncoder<C>,
}

impl Default for StatefulMetricsClient<NoopBatchCompressor> {
    fn default() -> Self {
        Self::new(CoreConfig::default(), NoopBatchCompressor)
    }
}

impl<C> StatefulMetricsClient<C>
where
    C: BatchCompressor,
{
    /// Creates a client with the supplied state-machine configuration and compressor.
    pub fn new(config: CoreConfig, compressor: C) -> Self {
        Self {
            core: StatefulMetricsCore::new(config),
            batch_encoder: MetricBatchEncoder::new(compressor),
        }
    }

    /// Creates a client from separately constructed protocol components.
    pub const fn from_parts(
        core: StatefulMetricsCore,
        batch_encoder: MetricBatchEncoder<C>,
    ) -> Self {
        Self {
            core,
            batch_encoder,
        }
    }

    /// Transfers ownership of the underlying protocol components to the caller.
    pub fn into_parts(self) -> (StatefulMetricsCore, MetricBatchEncoder<C>) {
        (self.core, self.batch_encoder)
    }

    /// Returns the content-encoding token for the configured compressor.
    pub fn content_encoding(&self) -> Option<&'static str> {
        self.batch_encoder.content_encoding()
    }

    /// Returns the number of configured endpoints.
    pub fn endpoint_count(&self) -> usize {
        self.core.endpoint_count()
    }

    /// Returns an endpoint's current stream ID, if one is connecting, open, or draining.
    pub fn current_stream_id(&self, endpoint: MetricEndpointId) -> Option<StreamId> {
        self.core.current_stream_id(endpoint)
    }

    /// Returns whether some endpoint can accept more series for the next payload.
    pub fn has_send_capacity(&self) -> bool {
        self.core.has_send_capacity()
    }

    /// Returns whether [`Self::send_batch_to`] would accept a batch for an endpoint.
    pub fn endpoint_has_send_capacity(&self, endpoint: MetricEndpointId) -> bool {
        self.core.endpoint_has_send_capacity(endpoint)
    }

    /// Returns the number of logical batches some endpoint's current stream has not
    /// acknowledged.
    pub fn inflight_len(&self) -> usize {
        self.core.inflight_len()
    }

    /// Returns live dictionary usage, shared by every endpoint.
    pub fn dictionary_stats(&self) -> MetricDictionaryStats {
        self.core.dictionary_stats()
    }

    /// Returns the number of logical batches statefully encoded by this client, including
    /// resubmissions.
    pub const fn encoding_count(&self) -> u64 {
        self.core.encoding_count()
    }

    /// Starts the client by requesting a stream for every endpoint without one.
    pub fn start(&mut self) -> Vec<MetricClientEffect> {
        let effects = self.core.start();
        self.prepare_effects(effects)
    }

    /// Marks a requested stream as open. See [`StatefulMetricsCore::handle_stream_opened`].
    pub fn handle_stream_opened(&mut self, stream_id: StreamId) -> Vec<MetricClientEffect> {
        let effects = self.core.handle_stream_opened(stream_id);
        self.prepare_effects(effects)
    }

    /// Returns the number of accepted series awaiting a flush.
    pub fn buffered_series_len(&self) -> usize {
        self.core.buffered_series_len()
    }

    /// Buffers series until the configured series-count threshold or a caller-triggered flush.
    ///
    /// `now` is the caller's monotonic reading, used when encoding starts.
    /// See [`StatefulMetricsCore::push_batch`] for admission and ownership rules.
    #[allow(clippy::result_large_err)]
    pub fn push_batch(
        &mut self,
        logical: LogicalMetricBatch,
        now: Instant,
    ) -> Result<Vec<MetricClientEffect>, MetricClientError> {
        let effects = self
            .core
            .push_batch(logical, now)
            .map_err(MetricClientError::Push)?;
        Ok(self.prepare_effects(effects))
    }

    /// Handles the caller's flush signal and returns a transport-ready payload per open endpoint.
    ///
    /// ADP owns the idle timer and calls this when it expires or when draining input at shutdown.
    /// An empty flush emits nothing but can evict dictionary entries using the supplied `now`.
    /// Rejection returns ownership through `MetricClientError::Push`.
    #[allow(clippy::result_large_err)]
    pub fn flush(&mut self, now: Instant) -> Result<Vec<MetricClientEffect>, MetricClientError> {
        let effects = self.core.flush(now).map_err(MetricClientError::Push)?;
        Ok(self.prepare_effects(effects))
    }

    /// Resubmits a returned batch to one endpoint as its own transport-ready payload.
    ///
    /// See [`StatefulMetricsCore::send_batch_to`] for admission and ownership rules.
    #[allow(clippy::result_large_err)]
    pub fn send_batch_to(
        &mut self,
        endpoint: MetricEndpointId,
        logical: LogicalMetricBatch,
        now: Instant,
    ) -> Result<Vec<MetricClientEffect>, MetricClientError> {
        let effects = self
            .core
            .send_batch_to(endpoint, logical, now)
            .map_err(MetricClientError::Push)?;
        Ok(self.prepare_effects(effects))
    }

    /// Applies an ordered server acknowledgement.
    pub fn handle_ack(&mut self, stream_id: StreamId, batch_id: u64) -> Vec<MetricClientEffect> {
        let effects = self.core.handle_ack(stream_id, batch_id);
        self.prepare_effects(effects)
    }

    /// Applies a classified stream failure and returns transport-ready recovery effects.
    pub fn handle_stream_error(
        &mut self,
        stream_id: StreamId,
        failure: MetricStreamFailure,
    ) -> Vec<MetricClientEffect> {
        let effects = self.core.handle_stream_error(stream_id, failure);
        self.prepare_effects(effects)
    }

    /// Handles a stream-scoped timer expiry.
    pub fn handle_timer(
        &mut self,
        stream_id: StreamId,
        timer: TimerKind,
    ) -> Vec<MetricClientEffect> {
        let effects = self.core.handle_timer(stream_id, timer);
        self.prepare_effects(effects)
    }

    /// Replaces an endpoint's stream after its destination identity changes.
    pub fn reset_destination_state(
        &mut self,
        endpoint: MetricEndpointId,
    ) -> Vec<MetricClientEffect> {
        let effects = self.core.reset_destination_state(endpoint);
        self.prepare_effects(effects)
    }

    /// Closes every stream and transfers all logical work to the caller. See
    /// [`StatefulMetricsCore::shutdown`].
    pub fn shutdown(&mut self) -> Vec<MetricClientEffect> {
        let effects = self.core.shutdown();
        self.prepare_effects(effects)
    }

    fn prepare_effects(&mut self, effects: Vec<MetricEffect>) -> Vec<MetricClientEffect> {
        let mut prepared = Vec::with_capacity(effects.len());
        let mut failed = Vec::new();
        for effect in effects {
            let effect = match effect {
                MetricEffect::SendBatch { batch } => {
                    let stream_id = batch.stream;
                    if failed.contains(&stream_id) {
                        continue;
                    }
                    match self.batch_encoder.encode(&batch) {
                        Ok(payload) => MetricClientEffect::SendPayload { stream_id, payload },
                        Err(error) => {
                            failed.push(stream_id);
                            prepared.retain(|effect| {
                                !matches!(effect, MetricClientEffect::SendPayload { stream_id: sent, .. } if *sent == stream_id)
                            });
                            let failure = MetricStreamFailure::new(
                                MetricStreamFailureKind::Unavailable,
                                format!("local batch encoding failed: {error:?}"),
                            );
                            prepared.extend(
                                self.core
                                    .handle_stream_error(stream_id, failure)
                                    .into_iter()
                                    .map(convert),
                            );
                            continue;
                        }
                    }
                }
                other => convert(other),
            };
            prepared.push(effect);
        }
        prepared
    }
}

/// Converts an effect that carries no batch to send.
fn convert(effect: MetricEffect) -> MetricClientEffect {
    match effect {
        MetricEffect::OpenStream {
            endpoint,
            stream_id,
        } => MetricClientEffect::OpenStream {
            endpoint,
            stream_id,
        },
        MetricEffect::SendBatch { .. } => {
            unreachable!("send batches are wire-encoded by the client")
        }
        MetricEffect::CloseStream { stream_id } => MetricClientEffect::CloseStream { stream_id },
        MetricEffect::StreamFailed {
            endpoint,
            failure,
            action,
            unacknowledged,
        } => MetricClientEffect::StreamFailed {
            endpoint,
            failure,
            action,
            unacknowledged,
        },
        MetricEffect::ReturnUnacknowledged {
            endpoint,
            action,
            batches,
        } => MetricClientEffect::ReturnUnacknowledged {
            endpoint,
            action,
            batches,
        },
        MetricEffect::ReturnBuffered { batch } => MetricClientEffect::ReturnBuffered { batch },
        MetricEffect::ScheduleReconnect { stream_id } => {
            MetricClientEffect::ScheduleReconnect { stream_id }
        }
        MetricEffect::ScheduleTimer { stream_id, timer } => {
            MetricClientEffect::ScheduleTimer { stream_id, timer }
        }
        MetricEffect::ReportError { error } => MetricClientEffect::ReportError { error },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use prost::Message as _;

    use crate::{
        proto::stateful::{metric_datum as datum, MetricDatumSequence},
        BatchEncodeError, LogicalMetricSeries, MetricPoint, MetricSeriesType,
    };

    use super::*;

    #[derive(Clone, Copy, Debug)]
    struct FailingCompressor;

    impl BatchCompressor for FailingCompressor {
        fn content_encoding(&self) -> Option<&'static str> {
            None
        }

        fn compress(&self, _serialized: &[u8]) -> Result<Vec<u8>, BatchEncodeError> {
            Err(BatchEncodeError::Compress("compression failed".to_string()))
        }
    }

    /// Fails exactly the `fail_at`th compression and passes the rest through.
    #[derive(Clone, Debug, Default)]
    struct FailOnce {
        calls: Arc<AtomicUsize>,
        fail_at: usize,
    }

    impl FailOnce {
        fn at(fail_at: usize) -> Self {
            Self {
                fail_at,
                ..Self::default()
            }
        }
    }

    impl BatchCompressor for FailOnce {
        fn content_encoding(&self) -> Option<&'static str> {
            None
        }

        fn compress(&self, serialized: &[u8]) -> Result<Vec<u8>, BatchEncodeError> {
            if self.calls.fetch_add(1, Ordering::Relaxed) == self.fail_at {
                Err(BatchEncodeError::Compress("compression failed".to_string()))
            } else {
                Ok(serialized.to_vec())
            }
        }
    }

    fn logical_batch() -> LogicalMetricBatch {
        LogicalMetricBatch::new(vec![LogicalMetricSeries::new(
            "requests",
            MetricSeriesType::Gauge,
            vec![MetricPoint::new(1, 2.5)],
        )])
    }

    fn open_all<C>(client: &mut StatefulMetricsClient<C>) -> Vec<StreamId>
    where
        C: BatchCompressor,
    {
        client
            .start()
            .into_iter()
            .map(|effect| {
                let MetricClientEffect::OpenStream { stream_id, .. } = effect else {
                    panic!("client should request streams");
                };
                assert!(client.handle_stream_opened(stream_id).is_empty());
                stream_id
            })
            .collect()
    }

    fn open<C>(client: &mut StatefulMetricsClient<C>) -> StreamId
    where
        C: BatchCompressor,
    {
        open_all(client)[0]
    }

    fn two_endpoints() -> CoreConfig {
        CoreConfig {
            metrics_endpoints: 2,
            ..CoreConfig::default()
        }
    }

    #[test]
    fn push_batch_buffers_until_flush_returns_a_transport_ready_payload() {
        let mut client = StatefulMetricsClient::default();
        let stream_id = open(&mut client);

        assert!(client
            .push_batch(logical_batch(), Instant::now())
            .unwrap()
            .is_empty());
        let effects = client.flush(Instant::now()).unwrap();
        let [MetricClientEffect::SendPayload {
            stream_id: actual_stream_id,
            payload,
        }] = effects.as_slice()
        else {
            panic!("accepted batch should produce one transport payload");
        };

        assert_eq!(*actual_stream_id, stream_id);
        assert_eq!(payload.batch_id, 1);
        let sequence = MetricDatumSequence::decode(payload.data.as_slice()).unwrap();
        assert!(matches!(
            sequence.data.last().unwrap().data,
            Some(datum::Data::MetricSeriesBatch(_))
        ));
    }

    /// An encoding failure fails only its stream and returns the batch for retry, which then
    /// succeeds on the replacement stream.
    #[test]
    fn encoding_failure_fails_the_stream_and_returns_the_batch() {
        let mut client = StatefulMetricsClient::new(CoreConfig::default(), FailOnce::at(0));
        let stream_id = open(&mut client);
        client.push_batch(logical_batch(), Instant::now()).unwrap();

        let effects = client.flush(Instant::now()).unwrap();
        let [MetricClientEffect::CloseStream { stream_id: closed }, MetricClientEffect::StreamFailed {
            failure,
            action: MetricFailureAction::RetryWithBackoff,
            unacknowledged,
            ..
        }, MetricClientEffect::ScheduleReconnect { .. }] = effects.as_slice()
        else {
            panic!("encoding failure should fail the stream, got {effects:?}");
        };
        assert_eq!(*closed, stream_id);
        assert!(failure.message().contains("compression failed"));
        assert_eq!(*unacknowledged, vec![logical_batch()]);
        assert_eq!(client.inflight_len(), 0);

        let effects = client.handle_timer(stream_id, TimerKind::Reconnect);
        let [MetricClientEffect::OpenStream {
            stream_id: replacement,
            ..
        }] = effects.as_slice()
        else {
            panic!("reconnect should open a stream");
        };
        assert!(client.handle_stream_opened(*replacement).is_empty());
        let resent = client
            .send_batch_to(MetricEndpointId(0), logical_batch(), Instant::now())
            .unwrap();
        assert!(
            matches!(resent.as_slice(), [MetricClientEffect::SendPayload { stream_id, payload }]
                if stream_id == replacement && payload.batch_id == 1)
        );
    }

    /// One endpoint's encoding failure does not drop the payload planned for another.
    #[test]
    fn encoding_failure_on_one_endpoint_keeps_the_others_payloads() {
        let mut client = StatefulMetricsClient::new(two_endpoints(), FailOnce::at(1));
        let streams = open_all(&mut client);
        client.push_batch(logical_batch(), Instant::now()).unwrap();

        let effects = client.flush(Instant::now()).unwrap();
        assert!(matches!(effects.as_slice(), [
            MetricClientEffect::SendPayload { stream_id: first, .. },
            MetricClientEffect::CloseStream { stream_id: second },
            MetricClientEffect::StreamFailed { endpoint: MetricEndpointId(1), unacknowledged, .. },
            MetricClientEffect::ScheduleReconnect { .. },
        ] if *first == streams[0] && *second == streams[1] && *unacknowledged == vec![logical_batch()]));
        assert_eq!(client.inflight_len(), 1);
        assert!(client.handle_ack(streams[0], 1).is_empty());
        assert_eq!(client.inflight_len(), 0);
    }

    /// A batch that never encodes keeps coming back to the caller instead of being lost.
    #[test]
    fn persistent_encoding_failure_keeps_returning_the_batch() {
        let mut client = StatefulMetricsClient::new(CoreConfig::default(), FailingCompressor);
        let stream_id = open(&mut client);
        client.push_batch(logical_batch(), Instant::now()).unwrap();
        let effects = client.flush(Instant::now()).unwrap();
        let returned = |effects: &[MetricClientEffect]| {
            matches!(effects, [
                MetricClientEffect::CloseStream { .. },
                MetricClientEffect::StreamFailed { unacknowledged, .. },
                MetricClientEffect::ScheduleReconnect { .. }
            ] if *unacknowledged == vec![logical_batch()])
        };
        assert!(returned(&effects));
        assert_eq!(client.buffered_series_len(), 0);
        assert_eq!(client.inflight_len(), 0);
        let effects = client.handle_timer(stream_id, TimerKind::Reconnect);
        let [MetricClientEffect::OpenStream {
            stream_id: replacement,
            ..
        }] = effects.as_slice()
        else {
            panic!("reconnect should open a stream");
        };
        assert!(client.handle_stream_opened(*replacement).is_empty());
        let effects = client
            .send_batch_to(MetricEndpointId(0), logical_batch(), Instant::now())
            .unwrap();
        assert!(returned(&effects));
        assert_eq!(client.inflight_len(), 0);
    }

    #[test]
    fn idle_flush_emits_partial_payload_once_without_waiting_for_another_metric() {
        let mut client = StatefulMetricsClient::default();
        open(&mut client);
        let logical = logical_batch();

        assert!(client
            .push_batch(logical.clone(), Instant::now())
            .unwrap()
            .is_empty());
        assert_eq!(client.buffered_series_len(), 1);
        assert_eq!(client.inflight_len(), 0);
        assert_eq!(client.encoding_count(), 0);
        let effects = client.flush(Instant::now()).unwrap();
        let [MetricClientEffect::SendPayload { payload, .. }] = effects.as_slice() else {
            panic!("idle flush must emit the partial payload");
        };
        assert_eq!(payload.batch_id, 1);
        let sequence = MetricDatumSequence::decode(payload.data.as_slice()).unwrap();
        assert!(matches!(
            sequence.data.last().unwrap().data,
            Some(datum::Data::MetricSeriesBatch(_))
        ));
        assert_eq!(client.buffered_series_len(), 0);
        assert_eq!(client.inflight_len(), 1);
        assert_eq!(client.encoding_count(), 1);
        let mut threshold_client = StatefulMetricsClient::new(
            CoreConfig {
                batch_capacity: 1,
                ..CoreConfig::default()
            },
            NoopBatchCompressor,
        );
        open(&mut threshold_client);
        assert_eq!(
            effects,
            threshold_client
                .push_batch(logical, Instant::now())
                .unwrap()
        );
        assert!(client.flush(Instant::now()).unwrap().is_empty());
        let stream = client.current_stream_id(MetricEndpointId(0)).unwrap();
        assert!(client.handle_ack(stream, 1).is_empty());
        assert_eq!(client.inflight_len(), 0);
    }

    #[test]
    fn size_flush_coalesces_inputs_and_retains_complete_logical_ownership() {
        let mut client = StatefulMetricsClient::new(
            CoreConfig {
                batch_capacity: 2,
                ..CoreConfig::default()
            },
            NoopBatchCompressor,
        );
        let stream = open(&mut client);
        let first = logical_batch();
        let second = LogicalMetricBatch::new(vec![LogicalMetricSeries::new(
            "latency",
            MetricSeriesType::Gauge,
            vec![MetricPoint::new(2, 5.0)],
        )]);
        assert!(client
            .push_batch(first.clone(), Instant::now())
            .unwrap()
            .is_empty());
        let effects = client.push_batch(second.clone(), Instant::now()).unwrap();
        assert!(matches!(
            effects.as_slice(),
            [MetricClientEffect::SendPayload { .. }]
        ));
        assert_eq!(client.inflight_len(), 1);
        assert_eq!(client.buffered_series_len(), 0);
        assert!(client.flush(Instant::now()).unwrap().is_empty());

        let effects = client.handle_stream_error(
            stream,
            MetricStreamFailure::new(MetricStreamFailureKind::InvalidArgument, "rejected"),
        );
        let MetricClientEffect::StreamFailed { unacknowledged, .. } = &effects[1] else {
            panic!("expected failed batch ownership");
        };
        let mut expected = first.into_series();
        expected.extend(second.into_series());
        assert_eq!(*unacknowledged, vec![LogicalMetricBatch::new(expected)]);
    }

    #[test]
    fn empty_flush_does_not_open_a_stream_or_consume_a_batch_id() {
        let mut client = StatefulMetricsClient::default();
        assert!(client.flush(Instant::now()).unwrap().is_empty());
        assert_eq!(client.current_stream_id(MetricEndpointId(0)), None);
        open(&mut client);
        assert!(client.flush(Instant::now()).unwrap().is_empty());
        client.push_batch(logical_batch(), Instant::now()).unwrap();
        let effects = client.flush(Instant::now()).unwrap();
        assert!(
            matches!(effects.as_slice(), [MetricClientEffect::SendPayload { payload, .. }] if payload.batch_id == 1)
        );
    }

    #[test]
    fn failure_returns_unsent_buffer_after_inflight_without_abandoning_it() {
        let mut client = StatefulMetricsClient::default();
        let stream = open(&mut client);
        client.push_batch(logical_batch(), Instant::now()).unwrap();
        client.flush(Instant::now()).unwrap();
        let pending = logical_batch();
        client.push_batch(pending.clone(), Instant::now()).unwrap();
        let effects = client.handle_stream_error(
            stream,
            MetricStreamFailure::new(MetricStreamFailureKind::InvalidArgument, "bad sent batch"),
        );
        assert!(matches!(effects.as_slice(), [
            MetricClientEffect::CloseStream { .. },
            MetricClientEffect::StreamFailed { action: MetricFailureAction::DoNotRetry, unacknowledged, .. },
            MetricClientEffect::ReturnBuffered { batch },
        ] if unacknowledged.len() == 1 && *batch == pending));
        assert_eq!(client.buffered_series_len(), 0);
        assert!(client.flush(Instant::now()).unwrap().is_empty());
    }
}
