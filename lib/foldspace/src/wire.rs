use datadog_protos::stateful::{DatumSequence, StatefulBatch as ProtoStatefulBatch};
use prost::Message as _;

use crate::StatefulBatch;

/// Error returned while converting a planned batch into the protobuf envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchEncodeError {
    BatchIdTooLarge(u64),
    Marshal(String),
    Compress(String),
}

/// Compresses a serialized `DatumSequence` before it is placed in `StatefulBatch.data`.
pub trait BatchCompressor: Clone + Send + Sync + 'static {
    fn content_encoding(&self) -> Option<&'static str>;
    fn compress(&self, serialized: &[u8]) -> Result<Vec<u8>, BatchEncodeError>;
}

/// Identity compression. This matches the Agent tests that use the no-op compressor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoopBatchCompressor;

impl BatchCompressor for NoopBatchCompressor {
    fn content_encoding(&self) -> Option<&'static str> {
        None
    }

    fn compress(&self, serialized: &[u8]) -> Result<Vec<u8>, BatchEncodeError> {
        Ok(serialized.to_vec())
    }
}

/// Zstd compression using the same content-encoding token as the Agent logs pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZstdBatchCompressor {
    level: i32,
}

impl ZstdBatchCompressor {
    pub const DEFAULT_LEVEL: i32 = 3;

    pub const fn new(level: i32) -> Self {
        Self { level }
    }

    pub const fn level(&self) -> i32 {
        self.level
    }
}

impl Default for ZstdBatchCompressor {
    fn default() -> Self {
        Self::new(Self::DEFAULT_LEVEL)
    }
}

impl BatchCompressor for ZstdBatchCompressor {
    fn content_encoding(&self) -> Option<&'static str> {
        Some("zstd")
    }

    fn compress(&self, serialized: &[u8]) -> Result<Vec<u8>, BatchEncodeError> {
        zstd::stream::encode_all(serialized, self.level).map_err(|err| BatchEncodeError::Compress(err.to_string()))
    }
}

/// Converts foldspace state-machine batches into the protobuf streaming envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtoBatchEncoder<C = NoopBatchCompressor> {
    compressor: C,
}

impl Default for ProtoBatchEncoder<NoopBatchCompressor> {
    fn default() -> Self {
        Self::new(NoopBatchCompressor)
    }
}

impl<C> ProtoBatchEncoder<C>
where
    C: BatchCompressor,
{
    pub const fn new(compressor: C) -> Self {
        Self { compressor }
    }

    pub fn compressor(&self) -> &C {
        &self.compressor
    }

    pub fn content_encoding(&self) -> Option<&'static str> {
        self.compressor.content_encoding()
    }

    pub fn encode<S>(&self, batch: &StatefulBatch<S>) -> Result<ProtoStatefulBatch, BatchEncodeError> {
        let batch_id = u32::try_from(batch.batch_id).map_err(|_| BatchEncodeError::BatchIdTooLarge(batch.batch_id))?;
        let sequence = DatumSequence {
            data: batch.datums.iter().map(|datum| datum.datum().clone()).collect(),
        };

        let mut serialized = Vec::with_capacity(sequence.encoded_len());
        sequence
            .encode(&mut serialized)
            .map_err(|err| BatchEncodeError::Marshal(err.to_string()))?;
        let data = self.compressor.compress(&serialized)?;

        Ok(ProtoStatefulBatch { batch_id, data })
    }
}

#[cfg(test)]
mod tests {
    use datadog_protos::stateful::{datum, DatumSequence};
    use prost::Message as _;

    use super::*;
    use crate::StatefulDatum;

    #[test]
    fn proto_encoder_wraps_datums_in_stateful_batch_envelope() {
        let batch = StatefulBatch {
            stream: "stream",
            batch_id: 7,
            datums: vec![StatefulDatum::pattern_define(42), StatefulDatum::log()],
        };

        let encoded = ProtoBatchEncoder::default().encode(&batch).unwrap();

        assert_eq!(encoded.batch_id, 7);

        let decoded = DatumSequence::decode(encoded.data.as_slice()).unwrap();
        assert_eq!(decoded.data.len(), 2);
        assert!(matches!(
            decoded.data[0].data.as_ref(),
            Some(datum::Data::PatternDefine(define)) if define.pattern_id == 42
        ));
        assert!(matches!(decoded.data[1].data.as_ref(), Some(datum::Data::Logs(_))));
    }

    #[test]
    fn proto_encoder_rejects_batch_id_overflow() {
        let batch = StatefulBatch {
            stream: "stream",
            batch_id: u64::from(u32::MAX) + 1,
            datums: vec![],
        };

        assert_eq!(
            ProtoBatchEncoder::default().encode(&batch),
            Err(BatchEncodeError::BatchIdTooLarge(batch.batch_id))
        );
    }

    #[test]
    fn zstd_encoder_compresses_datum_sequence_payload() {
        let batch = StatefulBatch {
            stream: "stream",
            batch_id: 1,
            datums: vec![StatefulDatum::pattern_define(42), StatefulDatum::log()],
        };

        let encoded = ProtoBatchEncoder::new(ZstdBatchCompressor::default())
            .encode(&batch)
            .unwrap();
        let decompressed = zstd::stream::decode_all(encoded.data.as_slice()).unwrap();
        let decoded = DatumSequence::decode(decompressed.as_slice()).unwrap();

        assert_eq!(decoded.data.len(), 2);
    }
}
