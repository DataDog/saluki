use crate::proto::stateful::{
    log_datum as datum, DictEntryDefine, DictEntryDelete, JsonSchemaDefine, JsonSchemaDelete, Log,
    LogDatum, PatternDefine, PatternDelete,
};

const LOG_EMPTY_DICT_INDEX: u64 = 1;

/// Monotonic identifier assigned to each flushed payload, spanning all senders.
///
/// This is distinct from a per-stream `batch_id`: a payload keeps one global id
/// no matter which sender (or how many, under unreliable dispatch) transmits it.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PayloadId(pub u64);

impl PayloadId {
    /// Returns the numeric payload identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A stateful datum classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatefulDatumKind {
    Log,
    PatternDefine,
    PatternDelete,
    DictEntryDefine,
    DictEntryDelete,
    JsonSchemaDefine,
    JsonSchemaDelete,
}

impl StatefulDatumKind {
    pub const fn is_state_change(&self) -> bool {
        matches!(
            self,
            Self::PatternDefine
                | Self::PatternDelete
                | Self::DictEntryDefine
                | Self::DictEntryDelete
                | Self::JsonSchemaDefine
                | Self::JsonSchemaDelete
        )
    }

    pub const fn is_delete(&self) -> bool {
        matches!(
            self,
            Self::PatternDelete | Self::DictEntryDelete | Self::JsonSchemaDelete
        )
    }
}

/// A canonical stateful datum retained for replay and snapshot planning.
#[derive(Clone, Debug, PartialEq)]
pub struct StatefulDatum {
    datum: LogDatum,
}

impl StatefulDatum {
    pub fn new(datum: LogDatum) -> Self {
        Self { datum }
    }

    pub fn datum(&self) -> &LogDatum {
        &self.datum
    }

    pub fn into_datum(self) -> LogDatum {
        self.datum
    }

    pub fn kind(&self) -> Option<StatefulDatumKind> {
        match self.datum.data.as_ref()? {
            datum::Data::Log(_) => Some(StatefulDatumKind::Log),
            datum::Data::PatternDefine(_) => Some(StatefulDatumKind::PatternDefine),
            datum::Data::PatternDelete(_) => Some(StatefulDatumKind::PatternDelete),
            datum::Data::DictEntryDefine(_) => Some(StatefulDatumKind::DictEntryDefine),
            datum::Data::DictEntryDelete(_) => Some(StatefulDatumKind::DictEntryDelete),
            datum::Data::JsonSchemaDefine(_) => Some(StatefulDatumKind::JsonSchemaDefine),
            datum::Data::JsonSchemaDelete(_) => Some(StatefulDatumKind::JsonSchemaDelete),
            datum::Data::DeltaEncodingSync(_) => None,
        }
    }

    pub fn is_state_change(&self) -> bool {
        self.kind().is_some_and(|kind| kind.is_state_change())
    }

    pub fn is_delete(&self) -> bool {
        self.kind().is_some_and(|kind| kind.is_delete())
    }

    pub fn pattern_define(pattern_id: u64) -> Self {
        Self::new(LogDatum {
            data: Some(datum::Data::PatternDefine(PatternDefine {
                pattern_id,
                ..PatternDefine::default()
            })),
        })
    }

    pub fn pattern_delete(pattern_id: u64) -> Self {
        Self::new(LogDatum {
            data: Some(datum::Data::PatternDelete(PatternDelete { pattern_id })),
        })
    }

    pub fn dict_entry_define(id: u64, value: impl Into<String>) -> Self {
        Self::new(LogDatum {
            data: Some(datum::Data::DictEntryDefine(DictEntryDefine {
                id,
                value: value.into(),
            })),
        })
    }

    pub fn dict_entry_delete(id: u64) -> Self {
        Self::new(LogDatum {
            data: Some(datum::Data::DictEntryDelete(DictEntryDelete { id })),
        })
    }

    pub fn json_schema_define(schema_id: u64) -> Self {
        Self::new(LogDatum {
            data: Some(datum::Data::JsonSchemaDefine(JsonSchemaDefine {
                schema_id,
                ..JsonSchemaDefine::default()
            })),
        })
    }

    pub fn json_schema_delete(schema_id: u64) -> Self {
        Self::new(LogDatum {
            data: Some(datum::Data::JsonSchemaDelete(JsonSchemaDelete {
                schema_id,
            })),
        })
    }

    pub fn log() -> Self {
        Self::new(LogDatum {
            data: Some(datum::Data::Log(Log::default())),
        })
    }

    pub fn flattened_log() -> Self {
        Self::log()
    }
}

/// A processed logs-agent message ready to enter stateful batching.
#[derive(Clone, Debug, PartialEq)]
pub struct StatefulMessage {
    datum: StatefulDatum,
}

impl StatefulMessage {
    pub fn new(datum: StatefulDatum) -> Self {
        Self { datum }
    }

    pub const fn datum(&self) -> &StatefulDatum {
        &self.datum
    }
}

/// Encoded payload emitted by a batch strategy and retained by inflight.
#[derive(Clone, Debug, PartialEq)]
pub struct EncodedPayload {
    pub state_changes: Vec<StatefulDatum>,
    pub wire_datums: Vec<StatefulDatum>,
}

/// Behavioral contract for stateful batch encoding.
pub trait BatchEncoder {
    fn split_state_changes(&self, datums: &[StatefulDatum]) -> Vec<StatefulDatum>;
    fn wire_datums(&self, datums: &[StatefulDatum]) -> Vec<StatefulDatum>;

    fn encode_payload(&self, datums: &[StatefulDatum]) -> EncodedPayload {
        EncodedPayload {
            state_changes: self.split_state_changes(datums),
            wire_datums: self.wire_datums(datums),
        }
    }
}

/// Default encoder behavior from the Allium `BatchEncoding` contract.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultBatchEncoder;

impl BatchEncoder for DefaultBatchEncoder {
    fn split_state_changes(&self, datums: &[StatefulDatum]) -> Vec<StatefulDatum> {
        datums
            .iter()
            .filter(|datum| datum.is_state_change())
            .cloned()
            .collect()
    }

    fn wire_datums(&self, datums: &[StatefulDatum]) -> Vec<StatefulDatum> {
        delta_encode_datums_for_wire(datums.iter().filter(|datum| !datum.is_delete()))
    }
}

fn delta_encode_datums_for_wire<'a>(
    datums: impl IntoIterator<Item = &'a StatefulDatum>,
) -> Vec<StatefulDatum> {
    let mut encoded = Vec::new();
    let mut state = WireDeltaState::default();

    for datum in datums {
        match datum.datum().data.as_ref() {
            Some(datum::Data::PatternDefine(define)) => {
                state.last_pattern_id = define.pattern_id;
                encoded.push(datum.clone());
            }
            Some(datum::Data::Log(log)) => {
                let mut log = log.clone();
                state.apply_log_delta_encoding(&mut log);
                encoded.push(StatefulDatum::new(LogDatum {
                    data: Some(datum::Data::Log(log)),
                }));
            }
            _ => encoded.push(datum.clone()),
        }
    }

    encoded
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct WireDeltaState {
    last_timestamp: i64,
    last_pattern_id: u64,
    last_tags_dict_index: u64,
    last_service_dict_id: u64,
    last_status_dict_id: u64,
    last_json_schema_dict: u64,
}

impl WireDeltaState {
    fn apply_timestamp_delta_encoding(&mut self, timestamp: &mut i64) {
        let current_timestamp = *timestamp;
        if self.last_timestamp == 0 {
            self.last_timestamp = current_timestamp;
            return;
        }

        *timestamp = current_timestamp - self.last_timestamp;
        self.last_timestamp = current_timestamp;
    }

    fn apply_log_delta_encoding(&mut self, log: &mut Log) {
        self.apply_timestamp_delta_encoding(&mut log.timestamp);
        log.status = self.delta_log_dict_index(log.status, LogDeltaField::Status);
        log.service = self.delta_log_dict_index(log.service, LogDeltaField::Service);
        log.tags = self.delta_log_dict_index(log.tags, LogDeltaField::Tags);
        log.json_schema_id =
            self.delta_log_dict_index(log.json_schema_id, LogDeltaField::JsonSchema);

        if log.raw_log.is_empty() {
            if log.pattern_id == self.last_pattern_id {
                log.pattern_id = 0;
            } else {
                self.last_pattern_id = log.pattern_id;
            }
        }
    }

    fn delta_log_dict_index(&mut self, current: u64, field: LogDeltaField) -> u64 {
        let current = if current == 0 {
            LOG_EMPTY_DICT_INDEX
        } else {
            current
        };
        let last = match field {
            LogDeltaField::Status => &mut self.last_status_dict_id,
            LogDeltaField::Service => &mut self.last_service_dict_id,
            LogDeltaField::Tags => &mut self.last_tags_dict_index,
            LogDeltaField::JsonSchema => &mut self.last_json_schema_dict,
        };

        if current == *last {
            return 0;
        }
        *last = current;
        current
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogDeltaField {
    Status,
    Service,
    Tags,
    JsonSchema,
}

/// Accumulates stateful messages until an explicit flush.
#[derive(Clone, Debug)]
pub struct BatchStrategy<E> {
    pending_messages: Vec<StatefulMessage>,
    pending_datums: Vec<StatefulDatum>,
    encoder: E,
    capacity: usize,
}

impl<E> BatchStrategy<E>
where
    E: BatchEncoder,
{
    pub fn new(encoder: E, capacity: usize) -> Self {
        Self {
            pending_messages: Vec::new(),
            pending_datums: Vec::new(),
            encoder,
            capacity,
        }
    }

    pub fn can_accept_message(&self) -> bool {
        self.pending_messages.len() < self.capacity
    }

    pub fn is_empty(&self) -> bool {
        self.pending_messages.is_empty()
    }

    // Returns the message back to the caller on rejection so it can flush and
    // retry (see `machine.rs`). Boxing the error would add a heap allocation at
    // every batch boundary purely to satisfy the lint, so allow the large Err.
    #[allow(clippy::result_large_err)]
    pub fn push(&mut self, message: StatefulMessage) -> Result<(), StatefulMessage> {
        if !self.can_accept_message() {
            return Err(message);
        }

        self.pending_datums.push(message.datum().clone());
        self.pending_messages.push(message);
        Ok(())
    }

    pub fn flush(&mut self) -> Option<EncodedPayload> {
        if self.is_empty() {
            return None;
        }

        let payload = self.encoder.encode_payload(&self.pending_datums);
        self.pending_messages.clear();
        self.pending_datums.clear();
        Some(payload)
    }

    pub fn pending_len(&self) -> usize {
        self.pending_messages.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flattened_log(
        timestamp: i64,
        status: u64,
        service: u64,
        tags: u64,
        pattern_id: u64,
        json_schema_id: u64,
    ) -> StatefulDatum {
        StatefulDatum::new(LogDatum {
            data: Some(datum::Data::Log(Log {
                timestamp,
                status,
                service,
                tags,
                pattern_id,
                json_schema_id,
                ..Log::default()
            })),
        })
    }

    fn as_flattened_log(datum: &StatefulDatum) -> &Log {
        match datum.datum().data.as_ref().unwrap() {
            datum::Data::Log(log) => log,
            other => panic!("expected Log, got {other:?}"),
        }
    }

    #[test]
    fn flush_splits_state_changes_and_omits_deletes_from_wire_datums() {
        let mut strategy = BatchStrategy::new(DefaultBatchEncoder, 8);
        strategy
            .push(StatefulMessage::new(StatefulDatum::pattern_define(1)))
            .unwrap();
        strategy
            .push(StatefulMessage::new(StatefulDatum::pattern_delete(1)))
            .unwrap();
        strategy
            .push(StatefulMessage::new(StatefulDatum::log()))
            .unwrap();

        let payload = strategy.flush().expect("payload should flush");

        assert_eq!(payload.state_changes.len(), 2);
        assert_eq!(
            payload.wire_datums,
            vec![
                StatefulDatum::pattern_define(1),
                StatefulDatum::new(LogDatum {
                    data: Some(datum::Data::Log(Log {
                        status: LOG_EMPTY_DICT_INDEX,
                        service: LOG_EMPTY_DICT_INDEX,
                        tags: LOG_EMPTY_DICT_INDEX,
                        json_schema_id: LOG_EMPTY_DICT_INDEX,
                        ..Log::default()
                    })),
                }),
            ]
        );
        assert!(strategy.is_empty());
    }

    #[test]
    fn flush_delta_encodes_flattened_log_wire_datums_within_batch() {
        let mut strategy = BatchStrategy::new(DefaultBatchEncoder, 8);
        strategy
            .push(StatefulMessage::new(flattened_log(100, 2, 3, 4, 5, 1)))
            .unwrap();
        strategy
            .push(StatefulMessage::new(flattened_log(112, 2, 3, 4, 5, 1)))
            .unwrap();

        let payload = strategy.flush().expect("payload should flush");

        let first = as_flattened_log(&payload.wire_datums[0]);
        assert_eq!(first.timestamp, 100);
        assert_eq!(first.status, 2);
        assert_eq!(first.service, 3);
        assert_eq!(first.tags, 4);
        assert_eq!(first.pattern_id, 5);
        assert_eq!(first.json_schema_id, 1);

        let second = as_flattened_log(&payload.wire_datums[1]);
        assert_eq!(second.timestamp, 12);
        assert_eq!(second.status, 0);
        assert_eq!(second.service, 0);
        assert_eq!(second.tags, 0);
        assert_eq!(second.pattern_id, 0);
        assert_eq!(second.json_schema_id, 0);
    }
}
