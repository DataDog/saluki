use std::{collections::BTreeSet, time::Instant};

use prost::{encoding::message as protobuf_message, Message as _};

use crate::{
    proto::stateful::{
        metric_datum as datum, MetricDatum, MetricSeriesBatch, StatefulBatch as ProtoStatefulBatch,
    },
    BatchCompressor, NoopBatchCompressor,
};

use super::{
    dictionary::{MetricDictionary, SeriesReferences},
    retention::DefinitionKey,
    LogicalMetricBatch, LogicalMetricSeries, MetricDictionaryEvictionConfig, MetricDictionaryStats,
};

const FLAG_NO_INDEX: u64 = 0x100;
const FLAG_HAS_UNIT: u64 = 0x200;
const VALUE_TYPE_ZERO: u64 = 0x00;
const VALUE_TYPE_SINT64: u64 = 0x10;
const VALUE_TYPE_FLOAT32: u64 = 0x20;
const VALUE_TYPE_FLOAT64: u64 = 0x30;
const F32_INT_MAX: i64 = 1 << 24;
const VARINT_MAX_INTEGER: i64 = 1 << 48;
const METRIC_DATUM_SEQUENCE_DATA_TAG: u32 = 1;

/// A logical metric batch after stateful dictionary and column encoding.
#[derive(Clone, Debug, PartialEq)]
pub struct EncodedMetricBatch {
    datums: Vec<MetricDatum>,
    definition_count: usize,
    point_count: usize,
    references: Vec<DefinitionKey>,
}

impl EncodedMetricBatch {
    /// Returns dictionary definitions introduced by this batch.
    pub fn definitions(&self) -> &[MetricDatum] {
        &self.datums[..self.definition_count]
    }

    /// Every definition the series datum depends on, including dependencies of
    /// composites, in dependency order. A stream must have received each of these
    /// before it can decode the series datum.
    pub(super) fn references(&self) -> &[DefinitionKey] {
        &self.references
    }

    /// The series datum, shared by every stream that sends this batch.
    pub(super) fn series(&self) -> Option<&MetricDatum> {
        self.datums[self.definition_count..].first()
    }

    /// Returns all wire datums, with definitions before the series datum.
    pub fn datums(&self) -> &[MetricDatum] {
        &self.datums
    }

    /// Returns the number of metric points encoded in this batch.
    pub const fn point_count(&self) -> usize {
        self.point_count
    }

    /// Consumes this batch and returns its wire datums.
    pub fn into_datums(self) -> Vec<MetricDatum> {
        self.datums
    }
}

/// Dictionary and column encoder for logical metric series.
#[derive(Clone, Debug, Default)]
pub struct MetricSeriesEncoder {
    dictionary: MetricDictionary,
}

impl MetricSeriesEncoder {
    /// Evicts by the policy and returns the removed keys.
    pub(super) fn evict(&mut self, config: &MetricDictionaryEvictionConfig) -> Vec<DefinitionKey> {
        self.dictionary.evict(config)
    }

    /// Evicts with no payload in progress and returns the removed keys.
    pub(super) fn maintain(
        &mut self,
        now: Instant,
        config: &MetricDictionaryEvictionConfig,
    ) -> Vec<DefinitionKey> {
        self.dictionary.begin_batch(now);
        self.dictionary.evict(config)
    }

    /// The definitions in `references` that `sent` lacks, in dependency order, as the
    /// dictionary holds them now.
    pub(super) fn definitions_for<'a>(
        &'a self,
        references: &'a [DefinitionKey],
        sent: &'a BTreeSet<DefinitionKey>,
    ) -> impl Iterator<Item = &'a MetricDatum> + 'a {
        self.dictionary.definitions_for(references, sent)
    }

    #[cfg(test)]
    pub(super) fn retained_keys(&self) -> Vec<DefinitionKey> {
        self.dictionary.retained_keys()
    }

    /// Returns current dictionary usage.
    pub fn dictionary_stats(&self) -> MetricDictionaryStats {
        self.dictionary.stats()
    }

    /// Seeds this encoder with definitions in dependency order, using caller time.
    pub fn apply_definitions(&mut self, definitions: &[MetricDatum], now: Instant) {
        self.dictionary.apply_definitions(definitions, now);
    }

    /// Encodes one logical batch and records dictionary usage at the caller's monotonic time.
    pub fn encode(&mut self, batch: &LogicalMetricBatch, now: Instant) -> EncodedMetricBatch {
        encode_logical_batch(&mut self.dictionary, batch, now)
    }
}

/// A stateful metrics batch ready to be serialized for transport.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricStatefulBatch<S> {
    /// Stream selected by the sans-I/O state machine.
    pub stream: S,
    /// Stream-local ordered batch identifier.
    pub batch_id: u64,
    /// Metric datums in define-before-reference order.
    pub datums: Vec<MetricDatum>,
}

/// Error returned while converting metric datums into the transport envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetricBatchEncodeError {
    /// The stream batch ID cannot be represented by the wire protocol.
    BatchIdTooLarge(u64),
    /// Outer compression failed.
    Compress(String),
}

/// Serializes and outer-compresses metric datum sequences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetricBatchEncoder<C = NoopBatchCompressor> {
    compressor: C,
}

impl Default for MetricBatchEncoder<NoopBatchCompressor> {
    fn default() -> Self {
        Self::new(NoopBatchCompressor)
    }
}

impl<C> MetricBatchEncoder<C>
where
    C: BatchCompressor,
{
    /// Creates an encoder using the supplied outer compressor.
    pub const fn new(compressor: C) -> Self {
        Self { compressor }
    }

    /// Returns the content-encoding token for the configured compressor.
    pub fn content_encoding(&self) -> Option<&'static str> {
        self.compressor.content_encoding()
    }

    /// Converts a planned metric batch into the shared transport envelope.
    pub fn encode<S>(
        &self,
        batch: &MetricStatefulBatch<S>,
    ) -> Result<ProtoStatefulBatch, MetricBatchEncodeError> {
        let batch_id = u32::try_from(batch.batch_id)
            .map_err(|_| MetricBatchEncodeError::BatchIdTooLarge(batch.batch_id))?;
        let serialized = encode_metric_datum_sequence(&batch.datums);
        let data = self
            .compressor
            .compress(&serialized)
            .map_err(|error| match error {
                crate::BatchEncodeError::Compress(message) => {
                    MetricBatchEncodeError::Compress(message)
                }
                other => MetricBatchEncodeError::Compress(format!("{other:?}")),
            })?;

        Ok(ProtoStatefulBatch { batch_id, data })
    }
}

fn encode_metric_datum_sequence(datums: &[MetricDatum]) -> Vec<u8> {
    let encoded_len =
        protobuf_message::encoded_len_repeated(METRIC_DATUM_SEQUENCE_DATA_TAG, datums);
    let mut serialized = Vec::with_capacity(encoded_len);
    protobuf_message::encode_repeated(METRIC_DATUM_SEQUENCE_DATA_TAG, datums, &mut serialized);
    serialized
}

fn encode_logical_batch(
    dictionary: &mut MetricDictionary,
    batch: &LogicalMetricBatch,
    now: Instant,
) -> EncodedMetricBatch {
    dictionary.begin_batch(now);
    let mut definitions = Vec::new();
    let mut referenced_series = Vec::with_capacity(batch.series().len());
    let mut point_count = 0;
    for series in batch.series() {
        point_count += series.points().len();
        let references = dictionary.intern_series(series, &mut definitions);
        referenced_series.push((series, references));
    }
    let metric_data = encode_metric_data(&referenced_series);
    let definition_count = definitions.len();
    let mut datums = definitions;
    if !referenced_series.is_empty() {
        datums.push(MetricDatum {
            data: Some(datum::Data::MetricSeriesBatch(MetricSeriesBatch {
                metric_data,
            })),
        });
    }

    EncodedMetricBatch {
        datums,
        definition_count,
        point_count,
        references: dictionary.references().collect(),
    }
}

fn encode_metric_data(series: &[(&LogicalMetricSeries, SeriesReferences)]) -> Vec<u8> {
    let mut columns = MetricDataColumns::default();
    for (series, references) in series {
        let value_type = value_type_for_series(series);
        let mut metric_type = series.metric_type().as_u64() | value_type;
        if series.no_index() {
            metric_type |= FLAG_NO_INDEX;
        }
        if references.unit != 0 {
            metric_type |= FLAG_HAS_UNIT;
            columns.unit_refs.push(references.unit as i64);
        }

        columns.types.push(metric_type);
        columns.name_refs.push(references.name as i64);
        columns.tagset_refs.push(references.tags as i64);
        columns.resources_refs.push(references.resources as i64);
        columns.intervals.push(series.interval());
        columns.num_points.push(series.points().len() as u64);
        columns
            .source_type_name_refs
            .push(references.source_type_name as i64);
        columns.origin_info_refs.push(references.origin as i64);

        for point in series.points() {
            columns.timestamps.push(point.timestamp);
            match value_type {
                VALUE_TYPE_ZERO => {}
                VALUE_TYPE_SINT64 => columns.vals_sint64.push(point.value as i64),
                VALUE_TYPE_FLOAT32 => columns.vals_float32.push(point.value as f32),
                VALUE_TYPE_FLOAT64 => columns.vals_float64.push(point.value),
                _ => unreachable!("value type is selected from protocol constants"),
            }
        }
    }

    delta_encode(&mut columns.name_refs);
    delta_encode(&mut columns.tagset_refs);
    delta_encode(&mut columns.resources_refs);
    delta_encode(&mut columns.source_type_name_refs);
    delta_encode(&mut columns.origin_info_refs);
    delta_encode(&mut columns.unit_refs);
    delta_encode(&mut columns.timestamps);
    columns.encode_to_vec()
}

fn value_type_for_series(series: &LogicalMetricSeries) -> u64 {
    let kind = series
        .points()
        .iter()
        .map(|point| PointKind::for_value(point.value))
        .fold(PointKind::Zero, PointKind::union);
    match kind {
        PointKind::Zero => VALUE_TYPE_ZERO,
        PointKind::Int24 | PointKind::Int48 => VALUE_TYPE_SINT64,
        PointKind::Float32 => VALUE_TYPE_FLOAT32,
        PointKind::Float64 => VALUE_TYPE_FLOAT64,
    }
}

fn delta_encode(values: &mut [i64]) {
    for index in (1..values.len()).rev() {
        values[index] -= values[index - 1];
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PointKind {
    Zero,
    Int24,
    Int48,
    Float32,
    Float64,
}

impl PointKind {
    fn for_value(value: f64) -> Self {
        if value == 0.0 {
            return Self::Zero;
        }

        let integer = value as i64;
        if (-VARINT_MAX_INTEGER..VARINT_MAX_INTEGER).contains(&integer) && integer as f64 == value {
            if (-F32_INT_MAX..=F32_INT_MAX).contains(&integer) {
                return Self::Int24;
            }
            return Self::Int48;
        }
        if value as f32 as f64 == value {
            return Self::Float32;
        }
        Self::Float64
    }

    fn union(self, other: Self) -> Self {
        match (self, other) {
            (Self::Int48, Self::Float32) | (Self::Float32, Self::Int48) => Self::Float64,
            _ => self.max(other),
        }
    }
}

#[derive(Clone, PartialEq, prost::Message)]
struct MetricDataColumns {
    #[prost(uint64, repeated, packed = "true", tag = "10")]
    types: Vec<u64>,
    #[prost(sint64, repeated, packed = "true", tag = "11")]
    name_refs: Vec<i64>,
    #[prost(sint64, repeated, packed = "true", tag = "12")]
    tagset_refs: Vec<i64>,
    #[prost(sint64, repeated, packed = "true", tag = "13")]
    resources_refs: Vec<i64>,
    #[prost(uint64, repeated, packed = "true", tag = "14")]
    intervals: Vec<u64>,
    #[prost(uint64, repeated, packed = "true", tag = "15")]
    num_points: Vec<u64>,
    #[prost(sint64, repeated, packed = "true", tag = "16")]
    timestamps: Vec<i64>,
    #[prost(sint64, repeated, packed = "true", tag = "17")]
    vals_sint64: Vec<i64>,
    #[prost(float, repeated, packed = "true", tag = "18")]
    vals_float32: Vec<f32>,
    #[prost(double, repeated, packed = "true", tag = "19")]
    vals_float64: Vec<f64>,
    #[prost(sint64, repeated, packed = "true", tag = "23")]
    source_type_name_refs: Vec<i64>,
    #[prost(sint64, repeated, packed = "true", tag = "24")]
    origin_info_refs: Vec<i64>,
    #[prost(sint64, repeated, packed = "true", tag = "26")]
    unit_refs: Vec<i64>,
}

#[cfg(test)]
mod tests {
    use crate::{
        proto::stateful::metric_datum as datum, BatchCompressor, BatchEncodeError,
        LogicalMetricSeries, MetricOrigin, MetricPoint, MetricResource, MetricSeriesType,
        MetricTagSet,
    };
    #[cfg(feature = "zstd")]
    use crate::{proto::stateful::MetricDatumSequence, ZstdBatchCompressor};

    use super::*;

    #[derive(Clone, Copy)]
    struct FailingCompressor;

    impl BatchCompressor for FailingCompressor {
        fn content_encoding(&self) -> Option<&'static str> {
            None
        }

        fn compress(&self, _serialized: &[u8]) -> Result<Vec<u8>, BatchEncodeError> {
            Err(BatchEncodeError::Compress("compression failed".to_string()))
        }
    }

    fn complete_series() -> LogicalMetricSeries {
        LogicalMetricSeries::new(
            "requests",
            MetricSeriesType::Rate,
            vec![MetricPoint::new(100, 1.5), MetricPoint::new(110, 2.5)],
        )
        .with_tags(MetricTagSet {
            prefix: vec!["env:prod".to_string()],
            values: vec!["service:api".to_string()],
        })
        .with_resources(vec![MetricResource::new("host", "web-1")])
        .with_interval(10)
        .with_unit("request")
        .with_source_type_name("nginx".to_string())
        .with_origin(MetricOrigin::new(1, 2, 3))
        .with_no_index(true)
    }

    #[test]
    fn series_batch_references_every_dictionary_kind() {
        let mut dictionary = MetricDictionary::default();
        let encoded = encode_logical_batch(
            &mut dictionary,
            &LogicalMetricBatch::new(vec![complete_series()]),
            Instant::now(),
        );
        let datum::Data::MetricSeriesBatch(batch) =
            encoded.datums().last().unwrap().data.as_ref().unwrap()
        else {
            panic!("last datum should be the series batch");
        };
        let columns = MetricDataColumns::decode(batch.metric_data.as_slice()).unwrap();

        assert_eq!(
            encoded.definitions(),
            &encoded.datums()[..encoded.definitions().len()]
        );
        assert_eq!(
            columns.types,
            vec![
                MetricSeriesType::Rate.as_u64()
                    | VALUE_TYPE_FLOAT32
                    | FLAG_NO_INDEX
                    | FLAG_HAS_UNIT
            ]
        );
        assert_eq!(columns.name_refs, vec![1]);
        assert_eq!(columns.tagset_refs, vec![2]);
        assert_eq!(columns.resources_refs, vec![1]);
        assert_eq!(columns.source_type_name_refs, vec![1]);
        assert_eq!(columns.origin_info_refs, vec![1]);
        assert_eq!(columns.unit_refs, vec![1]);
        assert_eq!(columns.intervals, vec![10]);
        assert_eq!(columns.num_points, vec![2]);
        assert_eq!(columns.timestamps, vec![100, 10]);
        assert_eq!(columns.vals_float32, vec![1.5, 2.5]);
    }

    #[test]
    fn second_batch_reuses_state_without_definitions() {
        let logical = LogicalMetricBatch::new(vec![complete_series()]);
        let mut dictionary = MetricDictionary::default();
        let first = encode_logical_batch(&mut dictionary, &logical, Instant::now());
        let second = encode_logical_batch(&mut dictionary, &logical, Instant::now());

        assert!(!first.definitions().is_empty());
        assert!(second.definitions().is_empty());
        let mut introduced: Vec<_> = first.definitions().iter().map(DefinitionKey::of).collect();
        introduced.sort();
        assert_eq!(first.references(), introduced);
        assert_eq!(
            second.references(),
            first.references(),
            "a batch references its full closure even when it introduces nothing"
        );
        assert_eq!(second.series(), second.datums().last());
        assert!(matches!(
            second.datums()[0].data,
            Some(datum::Data::MetricSeriesBatch(_))
        ));
    }

    #[test]
    fn empty_batch_emits_no_dictionary_or_series_data() {
        let mut encoder = MetricSeriesEncoder::default();

        let encoded = encoder.encode(&LogicalMetricBatch::default(), Instant::now());

        assert!(encoded.definitions().is_empty());
        assert!(encoded.datums().is_empty());
        assert_eq!(encoded.point_count(), 0);
    }

    #[test]
    fn unit_metadata_is_preserved() {
        let encoded = MetricSeriesEncoder::default().encode(
            &LogicalMetricBatch::new(vec![complete_series()]),
            Instant::now(),
        );
        let unit = encoded
            .definitions()
            .iter()
            .find_map(|datum| match datum.data.as_ref() {
                Some(datum::Data::MetricUnitDefine(define)) => Some(define),
                _ => None,
            })
            .unwrap();
        let datum::Data::MetricSeriesBatch(batch) =
            encoded.datums().last().unwrap().data.as_ref().unwrap()
        else {
            panic!("last datum should be the series batch");
        };
        let columns = MetricDataColumns::decode(batch.metric_data.as_slice()).unwrap();

        assert_eq!(unit.value, "request");
        assert_eq!(columns.unit_refs, vec![unit.id as i64]);
        assert_ne!(columns.types[0] & FLAG_HAS_UNIT, 0);
    }

    #[test]
    fn unitless_series_omits_unit_reference() {
        let series = complete_series().with_unit("");
        let encoded = MetricSeriesEncoder::default()
            .encode(&LogicalMetricBatch::new(vec![series]), Instant::now());
        let datum::Data::MetricSeriesBatch(batch) =
            encoded.datums().last().unwrap().data.as_ref().unwrap()
        else {
            panic!("last datum should be the series batch");
        };
        let columns = MetricDataColumns::decode(batch.metric_data.as_slice()).unwrap();

        assert!(!encoded
            .definitions()
            .iter()
            .any(|datum| matches!(datum.data, Some(datum::Data::MetricUnitDefine(_)))));
        assert!(columns.unit_refs.is_empty());
        assert_eq!(columns.types[0] & FLAG_HAS_UNIT, 0);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn transport_encoder_wraps_and_compresses_metric_sequence() {
        let mut dictionary = MetricDictionary::default();
        let encoded = encode_logical_batch(
            &mut dictionary,
            &LogicalMetricBatch::new(vec![complete_series()]),
            Instant::now(),
        );
        let batch = MetricStatefulBatch {
            stream: "stream".to_string(),
            batch_id: 7,
            datums: encoded.datums,
        };

        let wire = MetricBatchEncoder::new(ZstdBatchCompressor::default())
            .encode(&batch)
            .unwrap();
        let decompressed = zstd::stream::decode_all(wire.data.as_slice()).unwrap();
        let sequence = MetricDatumSequence::decode(decompressed.as_slice()).unwrap();

        assert_eq!(batch.stream, "stream");
        assert_eq!(wire.batch_id, 7);
        assert!(matches!(
            sequence.data.last().unwrap().data,
            Some(datum::Data::MetricSeriesBatch(_))
        ));
    }

    #[test]
    fn transport_encoder_preserves_batch_when_compression_fails() {
        let datum = MetricDatum {
            data: Some(datum::Data::MetricSeriesBatch(MetricSeriesBatch {
                metric_data: vec![1, 2, 3],
            })),
        };
        let batch = MetricStatefulBatch {
            stream: "stream".to_string(),
            batch_id: 7,
            datums: vec![datum.clone()],
        };

        let error = MetricBatchEncoder::new(FailingCompressor)
            .encode(&batch)
            .unwrap_err();

        assert_eq!(
            error,
            MetricBatchEncodeError::Compress("compression failed".to_string())
        );
        assert_eq!(batch.stream, "stream");
        assert_eq!(batch.batch_id, 7);
        assert_eq!(batch.datums, vec![datum]);
    }

    #[test]
    fn large_integer_and_float32_values_use_float64_without_precision_loss() {
        let series = LogicalMetricSeries::new(
            "mixed",
            MetricSeriesType::Gauge,
            vec![
                MetricPoint::new(1, (1_i64 << 30) as f64),
                MetricPoint::new(2, 1.5),
            ],
        );
        let mut dictionary = MetricDictionary::default();
        let encoded = encode_logical_batch(
            &mut dictionary,
            &LogicalMetricBatch::new(vec![series]),
            Instant::now(),
        );
        let datum::Data::MetricSeriesBatch(batch) =
            encoded.datums().last().unwrap().data.as_ref().unwrap()
        else {
            panic!("last datum should be the series batch");
        };
        let columns = MetricDataColumns::decode(batch.metric_data.as_slice()).unwrap();

        assert_eq!(
            columns.types,
            vec![MetricSeriesType::Gauge.as_u64() | VALUE_TYPE_FLOAT64]
        );
        assert_eq!(columns.vals_float64, vec![(1_i64 << 30) as f64, 1.5]);
    }
}
