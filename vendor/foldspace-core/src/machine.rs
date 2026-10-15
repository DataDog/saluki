use std::time::Duration;

use crate::{
    BatchEncoder, BatchStrategy, DefaultBatchEncoder, EncodedPayload, InflightQueue, LogRecord,
    MetricDictionaryEvictionConfig, NoopPatternExtractor, PatternExtractor, SenderConfig,
    StatefulBatch, StatefulLogTranslator,
};

/// Stream-local identifier assigned by the sans-I/O core.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StreamId(u64);

impl StreamId {
    pub(crate) const fn from_raw(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric stream identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Configuration for the sans-I/O stateful logs and metrics cores.
#[derive(Clone, Debug, PartialEq)]
pub struct CoreConfig {
    /// Sender protocol configuration.
    pub sender: SenderConfig,
    /// Number of log stateful messages or buffered metric series before automatic flush.
    ///
    /// Metrics buffer on every `push_batch` and also support an explicit `flush`.
    /// For metrics, zero is treated as one and an input batch can exceed the threshold.
    pub batch_capacity: usize,
    /// Local metrics dictionary eviction; `None` (the default) disables it. Logs ignore this policy.
    pub metrics_dictionary_eviction: Option<MetricDictionaryEvictionConfig>,
    /// Number of endpoints a metrics core sends every payload to, sharing one dictionary.
    /// Zero is treated as one. Logs ignore this setting.
    pub metrics_endpoints: usize,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            sender: SenderConfig::default(),
            batch_capacity: 512,
            metrics_dictionary_eviction: None,
            metrics_endpoints: 1,
        }
    }
}

/// Timer requested by the core.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timer {
    /// Timer kind.
    pub kind: TimerKind,
    /// Duration after which the caller should feed the timer back.
    pub after: Duration,
}

/// Timer kind requested by the core.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerKind {
    /// Reopen a failed stream.
    ///
    /// The caller chooses the reconnect delay using its own backoff policy.
    Reconnect,
    /// Rotate the active stream after its caller-managed lifetime expires.
    ///
    /// Callers schedule this from [`SenderConfig::stream_lifetime`] when a stream opens
    /// and ignore the timer if that stream is no longer active.
    RotateStream,
    /// Finish draining an old stream.
    DrainExpired,
}

/// Stream error reported by an adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamError {
    /// Human-readable error detail.
    pub message: String,
}

impl StreamError {
    /// Creates a stream error from a displayable value.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Effects emitted by the sans-I/O core for the caller to execute.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Open a new stream with the given stream ID.
    OpenStream { stream_id: StreamId },
    /// Send a protocol batch on an already opened stream.
    SendBatch { batch: StatefulBatch<StreamId> },
    /// Close the currently active stream.
    CloseStream { stream_id: StreamId },
    /// Schedule a reconnect using the caller's backoff policy.
    ScheduleReconnect,
    /// Report a protocol error to the caller.
    ReportError { error: CoreError },
}

/// Error reported by the sans-I/O core.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoreError {
    /// The caller reported an event for a stale or unknown stream.
    UnexpectedStream {
        expected: Option<StreamId>,
        actual: StreamId,
    },
    /// The server acknowledged a batch ID that was not expected.
    AckMismatch { expected: u64, actual: u64 },
    /// The server acknowledged a payload when none was outstanding.
    AckWithoutInflightPayload,
    /// The batch capacity was zero, so no message can ever be queued.
    BatchCapacityZero,
    /// The inflight queue was full.
    InflightQueueFull,
    /// The adapter reported a stream error.
    StreamFailed(StreamError),
}

/// Result of pushing a log into the core.
#[derive(Clone, Debug, PartialEq)]
pub struct PushOutcome {
    /// Effects emitted while processing the log.
    pub effects: Vec<Effect>,
    /// Whether the push triggered a flush.
    pub flushed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamState {
    Disconnected,
    Connecting(StreamId),
    Open(StreamId),
}

/// Sans-I/O stateful logs protocol state machine.
#[derive(Clone, Debug)]
pub struct StatefulLogsCore<E = DefaultBatchEncoder, P = NoopPatternExtractor> {
    config: CoreConfig,
    stream_state: StreamState,
    next_stream_id: u64,
    translator: StatefulLogTranslator<P>,
    batcher: BatchStrategy<E>,
    inflight: InflightQueue,
}

impl Default for StatefulLogsCore<DefaultBatchEncoder> {
    fn default() -> Self {
        Self::new(CoreConfig::default())
    }
}

impl StatefulLogsCore<DefaultBatchEncoder> {
    /// Creates a core state machine with the default batch encoder.
    pub fn new(config: CoreConfig) -> Self {
        Self::with_encoder(config, DefaultBatchEncoder)
    }
}

impl<E> StatefulLogsCore<E, NoopPatternExtractor>
where
    E: BatchEncoder,
{
    /// Creates a core state machine with a custom batch encoder and raw-log
    /// (noop) pattern extraction.
    pub fn with_encoder(config: CoreConfig, encoder: E) -> Self {
        Self::with_encoder_and_extractor(config, encoder, NoopPatternExtractor)
    }
}

impl<P> StatefulLogsCore<DefaultBatchEncoder, P>
where
    P: PatternExtractor,
{
    /// Creates a core state machine with the default batch encoder and a custom
    /// pattern extractor.
    pub fn with_pattern_extractor(config: CoreConfig, pattern_extractor: P) -> Self {
        Self::with_encoder_and_extractor(config, DefaultBatchEncoder, pattern_extractor)
    }
}

impl<E, P> StatefulLogsCore<E, P>
where
    E: BatchEncoder,
    P: PatternExtractor,
{
    /// Creates a core state machine with a custom batch encoder and pattern extractor.
    pub fn with_encoder_and_extractor(
        config: CoreConfig,
        encoder: E,
        pattern_extractor: P,
    ) -> Self {
        let inflight = InflightQueue::new(&config.sender);
        let batcher = BatchStrategy::new(encoder, config.batch_capacity);
        Self {
            config,
            stream_state: StreamState::Disconnected,
            next_stream_id: 1,
            translator: StatefulLogTranslator::with_pattern_extractor(pattern_extractor),
            batcher,
            inflight,
        }
    }

    /// Starts the core by requesting a stream if none is active.
    pub fn start(&mut self) -> Vec<Effect> {
        self.open_stream_if_needed()
    }

    /// Pushes a log record into the state machine.
    pub fn push_log(&mut self, log: &LogRecord) -> PushOutcome {
        let mut effects = Vec::new();
        let mut flushed = false;

        for message in self.translator.translate(log) {
            if let Err(message) = self.batcher.push(message) {
                flushed |= self.flush_into(&mut effects);
                if self.batcher.push(message).is_err() {
                    effects.push(Effect::ReportError {
                        error: CoreError::BatchCapacityZero,
                    });
                }
            }
        }

        if !self.batcher.can_accept_message() {
            flushed |= self.flush_into(&mut effects);
        }

        PushOutcome { effects, flushed }
    }

    /// Flushes pending log messages into an inflight payload.
    pub fn flush(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.flush_into(&mut effects);
        effects
    }

    /// Handles notification that a requested stream opened successfully.
    pub fn handle_stream_opened(&mut self, stream_id: StreamId) -> Vec<Effect> {
        match self.stream_state {
            StreamState::Connecting(expected) if expected == stream_id => {
                self.stream_state = StreamState::Open(stream_id);
                self.inflight.reset_for_new_stream(&self.config.sender);
                let mut effects = Vec::new();
                if let Some(batch) = self.inflight.snapshot_batch(stream_id, &self.config.sender) {
                    effects.push(Effect::SendBatch { batch });
                }
                self.send_ready_batches(&mut effects);
                effects
            }
            StreamState::Connecting(expected) | StreamState::Open(expected) => {
                vec![Effect::ReportError {
                    error: CoreError::UnexpectedStream {
                        expected: Some(expected),
                        actual: stream_id,
                    },
                }]
            }
            StreamState::Disconnected => vec![Effect::ReportError {
                error: CoreError::UnexpectedStream {
                    expected: None,
                    actual: stream_id,
                },
            }],
        }
    }

    /// Handles a server batch acknowledgement.
    pub fn handle_ack(&mut self, stream_id: StreamId, batch_id: u64) -> Vec<Effect> {
        if let Some(error) = self.validate_open_stream(stream_id) {
            return vec![Effect::ReportError { error }];
        }

        if batch_id == self.config.sender.snapshot_batch_id {
            let mut effects = Vec::new();
            self.send_ready_batches(&mut effects);
            return effects;
        }

        match self.inflight.ack_expected(batch_id) {
            Ok(_) => {
                let mut effects = Vec::new();
                self.send_ready_batches(&mut effects);
                effects
            }
            Err(crate::AckError::NoUnackedPayloads) => vec![Effect::ReportError {
                error: CoreError::AckWithoutInflightPayload,
            }],
            Err(crate::AckError::UnexpectedBatchId { expected, actual }) => {
                vec![Effect::ReportError {
                    error: CoreError::AckMismatch { expected, actual },
                }]
            }
        }
    }

    /// Handles a stream failure reported by the caller.
    pub fn handle_stream_error(&mut self, stream_id: StreamId, error: StreamError) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(validation_error) = self.validate_current_stream(stream_id) {
            effects.push(Effect::ReportError {
                error: validation_error,
            });
            return effects;
        }

        self.stream_state = StreamState::Disconnected;
        self.inflight.reset_for_new_stream(&self.config.sender);
        effects.push(Effect::ReportError {
            error: CoreError::StreamFailed(error),
        });
        effects.push(Effect::ScheduleReconnect);
        effects
    }

    /// Handles a previously scheduled timer.
    pub fn handle_timer(&mut self, timer: TimerKind) -> Vec<Effect> {
        match timer {
            TimerKind::Reconnect => self.open_stream_if_needed(),
            TimerKind::RotateStream => self.rotate_stream(),
            TimerKind::DrainExpired => self.force_close_current_stream(),
        }
    }

    /// Returns the current stream ID, if one is active or connecting.
    pub const fn current_stream_id(&self) -> Option<StreamId> {
        match self.stream_state {
            StreamState::Disconnected => None,
            StreamState::Connecting(stream_id) | StreamState::Open(stream_id) => Some(stream_id),
        }
    }

    /// Returns the number of pending, unsent, and unacked payloads.
    pub fn inflight_len(&self) -> usize {
        self.inflight.total_count()
    }

    fn flush_into(&mut self, effects: &mut Vec<Effect>) -> bool {
        let Some(payload) = self.batcher.flush() else {
            return false;
        };

        if self.enqueue_payload(payload, effects) {
            self.send_ready_batches(effects);
        }
        true
    }

    fn enqueue_payload(&mut self, payload: EncodedPayload, effects: &mut Vec<Effect>) -> bool {
        if self.inflight.push_unsent(payload).is_err() {
            effects.push(Effect::ReportError {
                error: CoreError::InflightQueueFull,
            });
            return false;
        }
        true
    }

    fn send_ready_batches(&mut self, effects: &mut Vec<Effect>) {
        let StreamState::Open(stream_id) = self.stream_state else {
            return;
        };

        while let Some(batch) = self.inflight.next_payload_batch(stream_id) {
            effects.push(Effect::SendBatch { batch });
        }
    }

    fn open_stream_if_needed(&mut self) -> Vec<Effect> {
        if !matches!(self.stream_state, StreamState::Disconnected) {
            return Vec::new();
        }

        let stream_id = StreamId(self.next_stream_id);
        self.next_stream_id += 1;
        self.stream_state = StreamState::Connecting(stream_id);
        vec![Effect::OpenStream { stream_id }]
    }

    fn rotate_stream(&mut self) -> Vec<Effect> {
        let mut effects = self.force_close_current_stream();
        effects.extend(self.open_stream_if_needed());
        effects
    }

    fn force_close_current_stream(&mut self) -> Vec<Effect> {
        let Some(stream_id) = self.current_stream_id() else {
            return Vec::new();
        };
        self.stream_state = StreamState::Disconnected;
        self.inflight.reset_for_new_stream(&self.config.sender);
        vec![Effect::CloseStream { stream_id }]
    }

    fn validate_open_stream(&self, stream_id: StreamId) -> Option<CoreError> {
        match self.stream_state {
            StreamState::Open(expected) if expected == stream_id => None,
            StreamState::Open(expected) | StreamState::Connecting(expected) => {
                Some(CoreError::UnexpectedStream {
                    expected: Some(expected),
                    actual: stream_id,
                })
            }
            StreamState::Disconnected => Some(CoreError::UnexpectedStream {
                expected: None,
                actual: stream_id,
            }),
        }
    }

    fn validate_current_stream(&self, stream_id: StreamId) -> Option<CoreError> {
        match self.current_stream_id() {
            Some(expected) if expected == stream_id => None,
            expected => Some(CoreError::UnexpectedStream {
                expected,
                actual: stream_id,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::stateful::log_datum as datum;

    fn log(message: &str, timestamp: i64) -> LogRecord {
        let mut record = LogRecord::new(message.as_bytes().to_vec(), timestamp);
        record.status = Some("info".to_string());
        record.service = Some("checkout".to_string());
        record
    }

    #[test]
    fn start_requests_stream_without_io() {
        let mut core = StatefulLogsCore::default();
        let effects = core.start();

        assert_eq!(
            effects,
            vec![Effect::OpenStream {
                stream_id: StreamId(1)
            }]
        );
        assert_eq!(core.current_stream_id(), Some(StreamId(1)));
    }

    #[test]
    fn push_log_flushes_when_batch_capacity_is_reached() {
        let mut core = StatefulLogsCore::new(CoreConfig {
            batch_capacity: 3,
            ..CoreConfig::default()
        });
        let stream_id = match core.start().pop().unwrap() {
            Effect::OpenStream { stream_id } => stream_id,
            effect => panic!("unexpected effect: {effect:?}"),
        };
        assert!(core.handle_stream_opened(stream_id).is_empty());

        let outcome = core.push_log(&log("hello", 1_700_000_000_000));
        assert!(outcome.flushed);
        assert_eq!(outcome.effects.len(), 1);
        let Effect::SendBatch { batch } = &outcome.effects[0] else {
            panic!("expected send batch");
        };
        assert_eq!(batch.stream, stream_id);
        assert_eq!(batch.batch_id, 1);
        assert!(batch
            .datums
            .iter()
            .any(|datum| matches!(datum.datum().data.as_ref(), Some(datum::Data::Log(_)))));
    }

    #[test]
    fn ack_removes_acknowledged_payload_from_inflight_queue() {
        let mut core = StatefulLogsCore::new(CoreConfig {
            batch_capacity: 3,
            ..CoreConfig::default()
        });
        let stream_id = match core.start().pop().unwrap() {
            Effect::OpenStream { stream_id } => stream_id,
            _ => unreachable!(),
        };
        core.handle_stream_opened(stream_id);

        let first = core.push_log(&log("one", 1));
        let second = core.push_log(&log("two", 2));
        let second_flush = core.flush();
        assert_eq!(first.effects.len(), 1);
        assert!(second.effects.is_empty());
        assert_eq!(second_flush.len(), 1);
        assert_eq!(core.inflight_len(), 2);

        let effects = core.handle_ack(stream_id, 1);
        assert!(effects.is_empty());
        assert_eq!(core.inflight_len(), 1);
    }

    #[test]
    fn stream_error_resets_unacked_payloads_and_requests_reconnect() {
        let mut core = StatefulLogsCore::new(CoreConfig {
            batch_capacity: 3,
            ..CoreConfig::default()
        });
        let stream_id = match core.start().pop().unwrap() {
            Effect::OpenStream { stream_id } => stream_id,
            _ => unreachable!(),
        };
        core.handle_stream_opened(stream_id);
        core.push_log(&log("one", 1));

        let effects = core.handle_stream_error(stream_id, StreamError::new("unavailable"));
        assert!(matches!(effects[0], Effect::ReportError { .. }));
        assert_eq!(effects[1], Effect::ScheduleReconnect);

        let effects = core.handle_timer(TimerKind::Reconnect);
        assert_eq!(
            effects,
            vec![Effect::OpenStream {
                stream_id: StreamId(2)
            }]
        );

        let effects = core.handle_stream_opened(StreamId(2));
        assert_eq!(effects.len(), 1);
        let Effect::SendBatch { batch } = &effects[0] else {
            panic!("expected replayed payload");
        };
        assert_eq!(batch.batch_id, 1);
    }

    #[test]
    fn stale_stream_events_report_errors() {
        let mut core = StatefulLogsCore::default();
        core.start();

        assert_eq!(
            core.handle_stream_opened(StreamId(99)),
            vec![Effect::ReportError {
                error: CoreError::UnexpectedStream {
                    expected: Some(StreamId(1)),
                    actual: StreamId(99)
                }
            }]
        );
    }
}
