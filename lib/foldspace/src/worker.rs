use std::time::{Duration, Instant};

use crate::{
    AckError, AckResult, EncodedPayload, Endpoint, InflightQueue, SenderConfig, StatefulBatch, StreamTransport,
    TransportError,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerState {
    Disconnected,
    Connecting,
    Active,
    Draining,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackoffState {
    error_count: u32,
}

impl BackoffState {
    pub const fn new() -> Self {
        Self { error_count: 0 }
    }

    pub const fn error_count(&self) -> u32 {
        self.error_count
    }

    fn record_error(&mut self) {
        self.error_count = self.error_count.saturating_add(1);
    }

    fn record_recovery(&mut self) {
        self.error_count = 0;
    }

    fn delay(&self) -> Duration {
        let shift = self.error_count.saturating_sub(1).min(6);
        Duration::from_millis(100 * (1_u64 << shift))
    }
}

impl Default for BackoffState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerError {
    InvalidState {
        expected: &'static str,
        actual: WorkerState,
    },
    InflightFull,
    NoCurrentStream,
    AckMismatch(AckError),
    Transport(TransportError),
}

#[derive(Clone, Debug, PartialEq)]
pub enum WorkerAckOutcome {
    Acked(AckResult),
    IgnoredSnapshot,
    IgnoredStale,
    Drained(AckResult),
}

/// Stateful stream worker for one logs pipeline.
#[derive(Clone, Debug)]
pub struct StreamWorker<S> {
    endpoint: Endpoint,
    state: WorkerState,
    current_stream: Option<S>,
    inflight: InflightQueue,
    backoff: BackoffState,
    stream_lifetime_deadline: Option<Instant>,
    drain_deadline: Option<Instant>,
    backoff_deadline: Option<Instant>,
    config: SenderConfig,
}

impl<S> StreamWorker<S>
where
    S: Clone + Eq,
{
    pub fn new(endpoint: Endpoint, config: SenderConfig) -> Self {
        Self {
            endpoint,
            state: WorkerState::Disconnected,
            current_stream: None,
            inflight: InflightQueue::new(&config),
            backoff: BackoffState::default(),
            stream_lifetime_deadline: None,
            drain_deadline: None,
            backoff_deadline: None,
            config,
        }
    }

    pub const fn state(&self) -> WorkerState {
        self.state
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn current_stream(&self) -> Option<&S> {
        self.current_stream.as_ref()
    }

    pub fn inflight(&self) -> &InflightQueue {
        &self.inflight
    }

    pub fn backoff(&self) -> &BackoffState {
        &self.backoff
    }

    pub const fn stream_lifetime_deadline(&self) -> Option<Instant> {
        self.stream_lifetime_deadline
    }

    pub const fn drain_deadline(&self) -> Option<Instant> {
        self.drain_deadline
    }

    pub const fn backoff_deadline(&self) -> Option<Instant> {
        self.backoff_deadline
    }

    pub fn may_accept_payload(&self) -> bool {
        self.state != WorkerState::Stopped && self.inflight.has_capacity()
    }

    pub fn may_send_payload(&self) -> bool {
        self.state == WorkerState::Active && self.inflight.has_unsent()
    }

    pub fn start(&mut self) -> Result<(), WorkerError> {
        if self.state != WorkerState::Disconnected {
            return Err(WorkerError::InvalidState {
                expected: "disconnected",
                actual: self.state,
            });
        }
        self.state = WorkerState::Connecting;
        Ok(())
    }

    pub async fn create_stream<T>(
        &mut self, transport: &T, now: Instant,
    ) -> Result<Option<StatefulBatch<S>>, WorkerError>
    where
        T: StreamTransport<Stream = S> + Sync,
        S: Send + Sync + 'static,
    {
        if self.state != WorkerState::Connecting {
            return Err(WorkerError::InvalidState {
                expected: "connecting",
                actual: self.state,
            });
        }

        match transport.create_stream(&self.endpoint).await {
            Ok(stream) => Ok(self.stream_creation_succeeded(stream, now)),
            Err(err) => {
                self.stream_creation_failed(now);
                Err(WorkerError::Transport(err))
            }
        }
    }

    pub fn stream_creation_succeeded(&mut self, stream: S, now: Instant) -> Option<StatefulBatch<S>> {
        self.current_stream = Some(stream.clone());
        self.state = WorkerState::Active;
        self.stream_lifetime_deadline = Some(now + self.config.stream_lifetime);
        self.drain_deadline = None;
        self.backoff_deadline = None;
        self.inflight.reset_for_new_stream(&self.config);
        self.inflight.snapshot_batch(stream, &self.config)
    }

    pub fn stream_creation_failed(&mut self, now: Instant) {
        self.rotate_after_failure(now);
    }

    pub fn backoff_elapsed(&mut self, now: Instant) -> bool {
        if self.state != WorkerState::Disconnected {
            return false;
        }
        let Some(deadline) = self.backoff_deadline else {
            return false;
        };
        if deadline > now {
            return false;
        }

        self.state = WorkerState::Connecting;
        self.backoff_deadline = None;
        true
    }

    pub fn enqueue_payload(&mut self, payload: EncodedPayload) -> Result<(), WorkerError> {
        if !self.may_accept_payload() {
            return Err(WorkerError::InflightFull);
        }
        self.inflight
            .push_unsent(payload)
            .map_err(|_| WorkerError::InflightFull)
    }

    pub fn next_payload_batch(&mut self) -> Result<Option<StatefulBatch<S>>, WorkerError> {
        if self.state != WorkerState::Active {
            return Ok(None);
        }
        let Some(stream) = self.current_stream.clone() else {
            return Err(WorkerError::NoCurrentStream);
        };
        Ok(self.inflight.next_payload_batch(stream))
    }

    pub async fn send_next<T>(&mut self, transport: &T, now: Instant) -> Result<Option<StatefulBatch<S>>, WorkerError>
    where
        T: StreamTransport<Stream = S> + Sync,
        S: Send + Sync + 'static,
    {
        let Some(batch) = self.next_payload_batch()? else {
            return Ok(None);
        };

        if let Err(err) = transport.send_batch(batch.clone()).await {
            self.stream_failure(&batch.stream, now);
            return Err(WorkerError::Transport(err));
        }

        Ok(Some(batch))
    }

    pub fn handle_ack(&mut self, stream: &S, batch_id: u64, now: Instant) -> Result<WorkerAckOutcome, WorkerError> {
        if self.current_stream.as_ref() != Some(stream) {
            return Ok(WorkerAckOutcome::IgnoredStale);
        }

        if batch_id == self.config.snapshot_batch_id {
            return Ok(WorkerAckOutcome::IgnoredSnapshot);
        }

        let ack = match self.inflight.ack_expected(batch_id) {
            Ok(ack) => ack,
            Err(err) => {
                self.rotate_after_failure(now);
                return Err(WorkerError::AckMismatch(err));
            }
        };

        if batch_id == self.config.first_payload_batch_id {
            self.backoff.record_recovery();
        }

        if self.state == WorkerState::Draining && !self.inflight.has_unacked() {
            self.current_stream = None;
            self.drain_deadline = None;
            self.stream_lifetime_deadline = None;
            self.state = WorkerState::Connecting;
            return Ok(WorkerAckOutcome::Drained(ack));
        }

        Ok(WorkerAckOutcome::Acked(ack))
    }

    pub fn stream_lifetime_elapsed(&mut self, now: Instant) -> bool {
        if self.state != WorkerState::Active {
            return false;
        }
        let Some(deadline) = self.stream_lifetime_deadline else {
            return false;
        };
        if deadline > now {
            return false;
        }

        if self.inflight.has_unacked() {
            self.state = WorkerState::Draining;
            self.drain_deadline = Some(now + self.config.drain_timeout);
        } else {
            self.current_stream = None;
            self.stream_lifetime_deadline = None;
            self.state = WorkerState::Connecting;
        }
        true
    }

    pub fn drain_elapsed(&mut self, now: Instant) -> bool {
        if self.state != WorkerState::Draining {
            return false;
        }
        let Some(deadline) = self.drain_deadline else {
            return false;
        };
        if deadline > now {
            return false;
        }

        self.current_stream = None;
        self.drain_deadline = None;
        self.stream_lifetime_deadline = None;
        self.state = WorkerState::Connecting;
        true
    }

    pub fn stream_failure(&mut self, stream: &S, now: Instant) -> bool {
        if !matches!(self.state, WorkerState::Active | WorkerState::Draining) {
            return false;
        }
        if self.current_stream.as_ref() != Some(stream) {
            return false;
        }

        self.rotate_after_failure(now);
        true
    }

    pub fn stop(&mut self) {
        self.state = WorkerState::Stopped;
        self.current_stream = None;
        self.stream_lifetime_deadline = None;
        self.drain_deadline = None;
        self.backoff_deadline = None;
    }

    fn rotate_after_failure(&mut self, now: Instant) {
        self.current_stream = None;
        self.stream_lifetime_deadline = None;
        self.drain_deadline = None;
        self.state = WorkerState::Disconnected;
        self.backoff.record_error();
        self.backoff_deadline = Some(now + self.backoff.delay());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::{EncodedPayload, StatefulDatum};

    fn payload(label: &'static [u8]) -> EncodedPayload {
        let _ = label;
        EncodedPayload {
            state_changes: vec![],
            wire_datums: vec![StatefulDatum::log()],
        }
    }

    #[derive(Default)]
    struct MockTransport {
        sent: Arc<Mutex<Vec<StatefulBatch<u64>>>>,
    }

    #[async_trait]
    impl StreamTransport for MockTransport {
        type Stream = u64;

        async fn create_stream(&self, _endpoint: &Endpoint) -> Result<Self::Stream, TransportError> {
            Ok(42)
        }

        async fn send_batch(&self, batch: StatefulBatch<Self::Stream>) -> Result<(), TransportError> {
            self.sent.lock().unwrap().push(batch);
            Ok(())
        }

        async fn receive_ack(&self, _stream: &Self::Stream) -> Result<u64, TransportError> {
            Ok(1)
        }
    }

    #[test]
    fn start_moves_disconnected_worker_to_connecting() {
        let mut worker = StreamWorker::<u64>::new(Endpoint::new("grpc://intake"), SenderConfig::default());
        worker.start().unwrap();
        assert_eq!(worker.state(), WorkerState::Connecting);
    }

    #[test]
    fn stream_creation_resets_sent_payloads_and_prepares_snapshot() {
        let now = Instant::now();
        let mut worker = StreamWorker::<u64>::new(Endpoint::new("grpc://intake"), SenderConfig::default());
        worker.enqueue_payload(payload(b"one")).unwrap();
        worker.start().unwrap();
        worker.stream_creation_succeeded(1, now);
        worker.next_payload_batch().unwrap().unwrap();

        worker.stream_failure(&1, now);
        assert_eq!(worker.state(), WorkerState::Disconnected);
        worker.start().unwrap();
        worker.stream_creation_succeeded(2, now);
        let replay = worker.next_payload_batch().unwrap().unwrap();

        assert_eq!(replay.stream, 2);
        assert_eq!(replay.batch_id, 1);
    }

    #[test]
    fn expected_ack_is_accepted_and_first_payload_recovers_backoff() {
        let now = Instant::now();
        let mut worker = StreamWorker::<u64>::new(Endpoint::new("grpc://intake"), SenderConfig::default());
        worker.stream_creation_failed(now);
        assert_eq!(worker.backoff().error_count(), 1);
        worker.backoff_elapsed(worker.backoff_deadline().unwrap());
        worker.stream_creation_succeeded(1, now);
        worker.enqueue_payload(payload(b"one")).unwrap();
        worker.next_payload_batch().unwrap().unwrap();

        let ack = worker.handle_ack(&1, 1, now).unwrap();

        assert!(matches!(ack, WorkerAckOutcome::Acked(_)));
        assert_eq!(worker.backoff().error_count(), 0);
        assert_eq!(worker.inflight().total_count(), 0);
    }

    #[test]
    fn snapshot_ack_and_stale_ack_are_ignored() {
        let now = Instant::now();
        let mut worker = StreamWorker::<u64>::new(Endpoint::new("grpc://intake"), SenderConfig::default());
        worker.stream_creation_succeeded(1, now);

        assert_eq!(worker.handle_ack(&1, 0, now), Ok(WorkerAckOutcome::IgnoredSnapshot));
        assert_eq!(worker.handle_ack(&2, 1, now), Ok(WorkerAckOutcome::IgnoredStale));
    }

    #[test]
    fn ack_mismatch_rotates_with_failure() {
        let now = Instant::now();
        let mut worker = StreamWorker::<u64>::new(Endpoint::new("grpc://intake"), SenderConfig::default());
        worker.stream_creation_succeeded(1, now);
        worker.enqueue_payload(payload(b"one")).unwrap();
        worker.next_payload_batch().unwrap().unwrap();

        let err = worker.handle_ack(&1, 2, now).unwrap_err();

        assert!(matches!(err, WorkerError::AckMismatch(_)));
        assert_eq!(worker.state(), WorkerState::Disconnected);
        assert_eq!(worker.backoff().error_count(), 1);
        assert!(worker.backoff_deadline().is_some());
    }

    #[test]
    fn stream_lifetime_with_unacked_enters_draining_and_ack_completes_rotation() {
        let now = Instant::now();
        let config = SenderConfig {
            stream_lifetime: Duration::from_secs(1),
            ..SenderConfig::default()
        };
        let mut worker = StreamWorker::<u64>::new(Endpoint::new("grpc://intake"), config);
        worker.stream_creation_succeeded(1, now);
        worker.enqueue_payload(payload(b"one")).unwrap();
        worker.next_payload_batch().unwrap().unwrap();

        assert!(worker.stream_lifetime_elapsed(now + Duration::from_secs(2)));
        assert_eq!(worker.state(), WorkerState::Draining);

        let ack = worker.handle_ack(&1, 1, now).unwrap();

        assert!(matches!(ack, WorkerAckOutcome::Drained(_)));
        assert_eq!(worker.state(), WorkerState::Connecting);
        assert!(worker.current_stream().is_none());
    }

    #[tokio::test]
    async fn transport_send_path_uses_stream_transport_contract() {
        let now = Instant::now();
        let transport = MockTransport::default();
        let mut worker = StreamWorker::<u64>::new(Endpoint::new("grpc://intake"), SenderConfig::default());
        worker.start().unwrap();
        worker.create_stream(&transport, now).await.unwrap();
        worker.enqueue_payload(payload(b"one")).unwrap();

        let sent = worker.send_next(&transport, now).await.unwrap().unwrap();

        assert_eq!(sent.batch_id, 1);
        assert_eq!(transport.sent.lock().unwrap().len(), 1);
    }
}
