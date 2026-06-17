use std::collections::VecDeque;

use datadog_protos::stateful::datum;

use crate::{EncodedPayload, SenderConfig, StatefulDatum};

/// Region of an inflight payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadRegion {
    Unsent,
    SentUnacked,
    Acked,
}

/// Payload retained by the worker until it is acknowledged.
#[derive(Clone, Debug, PartialEq)]
pub struct InflightPayload {
    pub payload: EncodedPayload,
    pub region: PayloadRegion,
    pub batch_id: Option<u64>,
}

impl InflightPayload {
    fn unsent(payload: EncodedPayload) -> Self {
        Self {
            payload,
            region: PayloadRegion::Unsent,
            batch_id: None,
        }
    }
}

/// Server-side state known to exist on a stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SnapshotState {
    datums: Vec<StatefulDatum>,
}

impl SnapshotState {
    pub fn new(datums: Vec<StatefulDatum>) -> Self {
        let mut state = Self::default();
        state.apply_state_changes(&datums);
        state
    }

    pub fn datums(&self) -> &[StatefulDatum] {
        &self.datums
    }

    pub fn is_empty(&self) -> bool {
        self.datums.is_empty()
    }

    pub fn apply_state_changes(&mut self, changes: &[StatefulDatum]) {
        for change in changes {
            if !change.is_state_change() {
                continue;
            }

            if let Some(delete_key) = StateKey::from_delete(change) {
                self.remove_matching_define(&delete_key);
            } else if !change.is_delete() {
                self.upsert(change.clone());
            }
        }
    }

    pub fn missing_from(&self, known: &Self) -> Vec<StatefulDatum> {
        self.datums
            .iter()
            .filter(|datum| !known.datums.contains(datum))
            .cloned()
            .collect()
    }

    fn upsert(&mut self, datum: StatefulDatum) {
        if let Some(key) = StateKey::from_define(&datum) {
            self.remove_matching_define(&key);
        }
        self.datums.push(datum);
    }

    fn remove_matching_define(&mut self, key: &StateKey) {
        self.datums
            .retain(|datum| StateKey::from_define(datum).as_ref() != Some(key));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum StateKey {
    Pattern(u64),
    DictEntry(u64),
    JsonSchema(u64),
}

impl StateKey {
    fn from_define(datum: &StatefulDatum) -> Option<Self> {
        match datum.datum().data.as_ref()? {
            datum::Data::PatternDefine(define) => Some(Self::Pattern(define.pattern_id)),
            datum::Data::DictEntryDefine(define) => Some(Self::DictEntry(define.id)),
            datum::Data::JsonSchemaDefine(define) => Some(Self::JsonSchema(define.schema_id)),
            _ => None,
        }
    }

    fn from_delete(datum: &StatefulDatum) -> Option<Self> {
        match datum.datum().data.as_ref()? {
            datum::Data::PatternDelete(delete) => Some(Self::Pattern(delete.pattern_id)),
            datum::Data::DictEntryDelete(delete) => Some(Self::DictEntry(delete.id)),
            datum::Data::JsonSchemaDelete(delete) => Some(Self::JsonSchema(delete.schema_id)),
            _ => None,
        }
    }
}

/// A stateful batch ready for transport.
#[derive(Clone, Debug, PartialEq)]
pub struct StatefulBatch<S> {
    pub stream: S,
    pub batch_id: u64,
    pub datums: Vec<StatefulDatum>,
}

impl<S> StatefulBatch<S> {
    pub fn is_snapshot(&self, config: &SenderConfig) -> bool {
        self.batch_id == config.snapshot_batch_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AckError {
    NoUnackedPayloads,
    UnexpectedBatchId { expected: u64, actual: u64 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AckResult {
    pub batch_id: u64,
    pub payload: EncodedPayload,
}

/// Ordered inflight queue for one stream worker.
#[derive(Clone, Debug)]
pub struct InflightQueue {
    capacity: usize,
    payloads: VecDeque<InflightPayload>,
    snapshot: SnapshotState,
    stream_known: SnapshotState,
    next_batch_id: u64,
    expected_ack_batch_id: u64,
}

impl InflightQueue {
    pub fn new(config: &SenderConfig) -> Self {
        Self {
            capacity: config.max_inflight_payloads,
            payloads: VecDeque::new(),
            snapshot: SnapshotState::default(),
            stream_known: SnapshotState::default(),
            next_batch_id: config.first_payload_batch_id,
            expected_ack_batch_id: config.first_payload_batch_id,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn total_count(&self) -> usize {
        self.payloads.len()
    }

    pub fn has_capacity(&self) -> bool {
        self.total_count() < self.capacity
    }

    pub fn has_unsent(&self) -> bool {
        self.payloads
            .iter()
            .any(|payload| payload.region == PayloadRegion::Unsent)
    }

    pub fn has_unacked(&self) -> bool {
        self.payloads
            .iter()
            .any(|payload| payload.region == PayloadRegion::SentUnacked)
    }

    pub fn sent_unacked_count(&self) -> usize {
        self.payloads
            .iter()
            .filter(|payload| payload.region == PayloadRegion::SentUnacked)
            .count()
    }

    pub fn snapshot(&self) -> &SnapshotState {
        &self.snapshot
    }

    pub fn stream_known(&self) -> &SnapshotState {
        &self.stream_known
    }

    pub fn next_batch_id(&self) -> u64 {
        self.next_batch_id
    }

    pub fn expected_ack_batch_id(&self) -> u64 {
        self.expected_ack_batch_id
    }

    pub fn push_unsent(&mut self, payload: EncodedPayload) -> Result<(), EncodedPayload> {
        if !self.has_capacity() {
            return Err(payload);
        }

        self.payloads.push_back(InflightPayload::unsent(payload));
        Ok(())
    }

    pub fn reset_for_new_stream(&mut self, config: &SenderConfig) {
        for payload in &mut self.payloads {
            if payload.region == PayloadRegion::SentUnacked {
                payload.region = PayloadRegion::Unsent;
                payload.batch_id = None;
            }
        }
        self.stream_known = SnapshotState::default();
        self.next_batch_id = config.first_payload_batch_id;
        self.expected_ack_batch_id = config.first_payload_batch_id;
    }

    pub fn snapshot_batch<S>(&mut self, stream: S, config: &SenderConfig) -> Option<StatefulBatch<S>>
    where
        S: Clone,
    {
        if self.snapshot.is_empty() {
            return None;
        }

        self.stream_known = self.snapshot.clone();
        Some(StatefulBatch {
            stream,
            batch_id: config.snapshot_batch_id,
            datums: self.snapshot.datums().to_vec(),
        })
    }

    pub fn next_payload_batch<S>(&mut self, stream: S) -> Option<StatefulBatch<S>>
    where
        S: Clone,
    {
        let index = self
            .payloads
            .iter()
            .position(|payload| payload.region == PayloadRegion::Unsent)?;

        let missing_snapshot = self.snapshot.missing_from(&self.stream_known);
        let payload_datums = self.payloads[index].payload.wire_datums.clone();
        let mut planned_datums = Vec::with_capacity(missing_snapshot.len() + payload_datums.len());
        planned_datums.extend(missing_snapshot);
        planned_datums.extend(payload_datums);

        let batch_id = self.next_batch_id;
        self.next_batch_id += 1;
        self.stream_known.apply_state_changes(&planned_datums);

        let payload = &mut self.payloads[index];
        payload.region = PayloadRegion::SentUnacked;
        payload.batch_id = Some(batch_id);

        Some(StatefulBatch {
            stream,
            batch_id,
            datums: planned_datums,
        })
    }

    pub fn ack_expected(&mut self, batch_id: u64) -> Result<AckResult, AckError> {
        if !self.has_unacked() {
            return Err(AckError::NoUnackedPayloads);
        }
        if batch_id != self.expected_ack_batch_id {
            return Err(AckError::UnexpectedBatchId {
                expected: self.expected_ack_batch_id,
                actual: batch_id,
            });
        }

        let index = self
            .payloads
            .iter()
            .position(|payload| payload.region == PayloadRegion::SentUnacked)
            .expect("has_unacked requires a sent-unacked payload");
        let payload = self.payloads.remove(index).expect("payload index should be valid");

        self.expected_ack_batch_id += 1;
        self.snapshot.apply_state_changes(&payload.payload.state_changes);

        Ok(AckResult {
            batch_id,
            payload: payload.payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EncodedPayload, StatefulDatum};

    fn payload(label: &'static [u8]) -> EncodedPayload {
        let _ = label;
        EncodedPayload {
            state_changes: vec![],
            wire_datums: vec![StatefulDatum::log()],
        }
    }

    #[test]
    fn capacity_is_bounded() {
        let config = SenderConfig {
            max_inflight_payloads: 1,
            ..SenderConfig::default()
        };
        let mut queue = InflightQueue::new(&config);
        assert!(queue.push_unsent(payload(b"one")).is_ok());
        assert!(queue.push_unsent(payload(b"two")).is_err());
        assert_eq!(queue.total_count(), 1);
    }

    #[test]
    fn payload_batches_start_at_one_and_ack_in_order() {
        let config = SenderConfig::default();
        let mut queue = InflightQueue::new(&config);
        queue.push_unsent(payload(b"one")).unwrap();

        let batch = queue.next_payload_batch("stream").expect("batch should exist");
        assert_eq!(batch.batch_id, 1);
        assert_eq!(queue.next_batch_id(), 2);

        let ack = queue.ack_expected(1).expect("ack should match");
        assert_eq!(ack.batch_id, 1);
        assert_eq!(queue.expected_ack_batch_id(), 2);
        assert_eq!(queue.total_count(), 0);
    }

    #[test]
    fn ack_mismatch_is_rejected() {
        let mut queue = InflightQueue::new(&SenderConfig::default());
        queue.push_unsent(payload(b"one")).unwrap();
        queue.next_payload_batch("stream").unwrap();

        assert_eq!(
            queue.ack_expected(2),
            Err(AckError::UnexpectedBatchId { expected: 1, actual: 2 })
        );
    }

    #[test]
    fn new_stream_resets_unacked_payloads_to_unsent() {
        let config = SenderConfig::default();
        let mut queue = InflightQueue::new(&config);
        queue.push_unsent(payload(b"one")).unwrap();
        queue.next_payload_batch("old").unwrap();

        queue.reset_for_new_stream(&config);

        let batch = queue.next_payload_batch("new").expect("payload should replay");
        assert_eq!(batch.stream, "new");
        assert_eq!(batch.batch_id, 1);
    }

    #[test]
    fn snapshot_batch_id_is_reserved_zero() {
        let config = SenderConfig::default();
        let mut queue = InflightQueue::new(&config);
        queue.snapshot.apply_state_changes(&[StatefulDatum::pattern_define(1)]);

        let batch = queue.snapshot_batch("stream", &config).expect("snapshot should exist");

        assert!(batch.is_snapshot(&config));
        assert_eq!(batch.batch_id, 0);
        assert_eq!(queue.stream_known().datums().len(), 1);
    }

    #[test]
    fn snapshot_define_updates_replace_existing_state_by_id() {
        let mut snapshot = SnapshotState::default();

        snapshot.apply_state_changes(&[StatefulDatum::pattern_define(1)]);
        snapshot.apply_state_changes(&[StatefulDatum::pattern_define(1)]);

        assert_eq!(snapshot.datums().len(), 1);
    }

    #[test]
    fn snapshot_deletes_remove_matching_define_by_id() {
        let mut snapshot = SnapshotState::default();

        snapshot.apply_state_changes(&[StatefulDatum::pattern_define(1), StatefulDatum::pattern_define(2)]);
        snapshot.apply_state_changes(&[StatefulDatum::pattern_delete(1)]);

        assert_eq!(snapshot.datums(), &[StatefulDatum::pattern_define(2)]);
    }
}
