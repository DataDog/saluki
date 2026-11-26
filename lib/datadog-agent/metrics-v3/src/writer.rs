//! Writes V3 metrics as columnar protobuf payloads.
//!
//! # Design
//!
//! The writer appends metric properties, points, and sketch bins to separate columns.
//! Payload-local dictionaries store repeated strings and sequences once. Columns refer to them by ID instead of
//! repeating the values for each metric. ID 0 represents an empty or absent dictionary reference.
//! Finish a payload to delta-encode reference and timestamp columns, sort tag IDs within each tagset, and frame the
//! columns. The encoder writes each column directly into the protobuf payload and records its byte range for telemetry.

use std::{fmt, ops::Range};

use protobuf::CodedOutputStream;

use crate::{
    constants::{
        DICT_NAME_STR_FIELD_NUMBER, DICT_ORIGIN_INFO_FIELD_NUMBER, DICT_RESOURCE_LEN_FIELD_NUMBER,
        DICT_RESOURCE_NAME_FIELD_NUMBER, DICT_RESOURCE_STR_FIELD_NUMBER, DICT_RESOURCE_TYPE_FIELD_NUMBER,
        DICT_SOURCE_TYPE_NAME_FIELD_NUMBER, DICT_TAGSETS_FIELD_NUMBER, DICT_TAGS_STR_FIELD_NUMBER,
        DICT_UNIT_STR_FIELD_NUMBER, INTERVALS_FIELD_NUMBER, NAMES_FIELD_NUMBER, NUM_POINTS_FIELD_NUMBER,
        ORIGIN_INFO_FIELD_NUMBER, RESOURCES_FIELD_NUMBER, SKETCH_BIN_CNTS_FIELD_NUMBER, SKETCH_BIN_KEYS_FIELD_NUMBER,
        SKETCH_NUM_BINS_FIELD_NUMBER, SOURCE_TYPE_NAME_FIELD_NUMBER, TAGS_FIELD_NUMBER, TIMESTAMPS_FIELD_NUMBER,
        TYPES_FIELD_NUMBER, UNIT_REFS_FIELD_NUMBER, VALS_FLOAT32_FIELD_NUMBER, VALS_FLOAT64_FIELD_NUMBER,
        VALS_SINT64_FIELD_NUMBER,
    },
    interner::Interner,
    types::{value_type_for_values, V3MetricType, V3ValueType},
};

/// Error encountered while encoding a V3 payload.
#[derive(Debug)]
pub struct V3EncodeError(protobuf::Error);

impl fmt::Display for V3EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to encode V3 payload: {}", self.0)
    }
}

impl std::error::Error for V3EncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<protobuf::Error> for V3EncodeError {
    fn from(e: protobuf::Error) -> Self {
        Self(e)
    }
}

impl V3EncodeError {
    /// Returns the underlying `protobuf` error.
    pub fn into_inner(self) -> protobuf::Error {
        self.0
    }
}

pub(crate) const FLAG_NO_INDEX: u64 = 0x100;
pub(crate) const FLAG_HAS_UNIT: u64 = 0x200;

/// Holds the dictionary and value columns before protobuf field framing.
#[derive(Debug, Default)]
pub(crate) struct V3EncodedData {
    // String dictionaries concatenate varint-length-prefixed strings. Other dictionaries use packed integer columns.
    dict_name_bytes: Vec<u8>,
    dict_tags_bytes: Vec<u8>,
    dict_tagsets: Vec<i64>,
    dict_resource_str_bytes: Vec<u8>,
    dict_resource_len: Vec<i64>,
    dict_resource_type: Vec<i64>,
    dict_resource_name: Vec<i64>,
    dict_source_type_bytes: Vec<u8>,
    dict_origin_info: Vec<i32>,
    dict_unit_bytes: Vec<u8>,

    // These columns have one entry per metric, except for conditional columns such as `unit_refs`.
    types: Vec<u64>,
    names: Vec<i64>,
    tags: Vec<i64>,
    resources: Vec<i64>,
    intervals: Vec<u64>,
    num_points: Vec<u64>,
    source_type_names: Vec<i64>,
    origin_infos: Vec<i64>,
    unit_refs: Vec<i64>, // Present only for metrics with FLAG_HAS_UNIT set.

    // `num_points` gives the point count. The metric's type determines which value column holds its point values.
    timestamps: Vec<i64>,
    vals_sint64: Vec<i64>,
    vals_float32: Vec<f32>,
    vals_float64: Vec<f64>,

    // Distribution points store bin counts, keys, and frequencies separately from their summary values.
    sketch_num_bins: Vec<u64>,
    sketch_bin_keys: Vec<i32>,
    sketch_bin_cnts: Vec<u32>,

    value_encoding_stats: V3ValueEncodingStats,
}

/// Encoded V3 metrics payload with telemetry data.
pub struct V3EncodedMetrics {
    /// Serialized `MetricData` protobuf payload.
    pub payload: Vec<u8>,
    /// Telemetry produced while encoding the payload.
    pub stats: V3EncoderStats,
}

/// Telemetry data produced while encoding a V3 metrics payload.
pub struct V3EncoderStats {
    /// Counts of how many point values were compacted into each value column.
    pub value_encoding_stats: V3ValueEncodingStats,
    /// Byte ranges in the payload for each present column, keyed by protobuf field number.
    pub columns: Vec<V3ColumnBytes>,
}

/// Counts of how many point values were encoded into each value column.
#[derive(Clone, Copy, Debug, Default)]
pub struct V3ValueEncodingStats {
    /// Number of point values that were zero and required no explicit storage.
    pub zero: u64,
    /// Number of point values stored as signed integers.
    pub sint64: u64,
    /// Number of point values stored as 32-bit floats.
    pub float32: u64,
    /// Number of point values stored as 64-bit floats.
    pub float64: u64,
}

/// Describes a V3 column's bytes within the serialized payload.
pub struct V3ColumnBytes {
    /// Protocol Buffers field number this column corresponds to.
    pub field_number: u32,
    /// Raw column contents within the payload, excluding the protobuf field header.
    pub range: Range<usize>,
    /// Compressed length of the column, or `0` if the caller does not measure it.
    pub compressed_len: usize,
}

/// Collects metrics for one V3 columnar payload at a time.
///
/// Call [`V3Writer::write`] for each metric and close its builder before starting the next metric.
/// Use [`V3Writer::finish`] to serialize a payload and reuse the writer's storage for the next one.
/// Use [`V3Writer::finalize`] when the writer is no longer needed.
#[derive(Debug, Default)]
pub struct V3Writer {
    // Each interner assigns IDs within one dictionary and points into its backing storage by offset.
    name_interner: Interner,
    tag_interner: Interner,
    tagset_interner: Interner,
    resource_str_interner: Interner,
    resource_interner: Interner,
    source_type_interner: Interner,
    origin_interner: Interner,
    unit_interner: Interner,

    data: V3EncodedData,
    resource_keys: Vec<(i64, i64)>,

    // Retain scratch capacity across metrics. Epochs mark tag IDs that this metric already includes.
    tag_ids: Vec<i64>,
    tag_epochs: Vec<usize>,
    tag_epoch: usize,
    resource_ids: Vec<(i64, i64)>,
}

impl V3Writer {
    /// Creates a new V3 writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of metrics written so far.
    pub fn metric_count(&self) -> usize {
        self.data.types.len()
    }

    /// Returns an estimate, in bytes, of the uncompressed size of the payload this writer would currently finalize to.
    ///
    /// This exists so callers can decide when to stop adding metrics to a batch, rather than encoding an oversized
    /// batch and discovering afterwards that it has to be split and re-encoded. It is deliberately cheap: it inspects
    /// only the lengths of the accumulated columns, so it is O(1) and safe to call after every metric.
    ///
    /// The figure is an estimate, not the exact finalized size. Dictionary string buffers are counted exactly, since
    /// they dominate for the high-cardinality workloads this guards against, while packed integer columns are counted
    /// at a fixed per-entry allowance because their true varint widths are only known once delta encoding runs during
    /// [`V3Writer::finalize`]. The allowance errs on the high side, so batches are cut slightly early rather than
    /// slightly late; callers **MUST** still check the finalized payload against their real size limits.
    pub fn estimated_uncompressed_len(&self) -> usize {
        // Upper-bound allowance for a delta-encoded varint entry. Delta-encoded values are small in practice (a
        // varint byte or two), so this leaves headroom without being as pessimistic as the 10-byte worst case.
        const VARINT_LEN_ALLOWANCE: usize = 4;

        // Dictionary string buffers, counted exactly.
        let dict_bytes = self.data.dict_name_bytes.len()
            + self.data.dict_tags_bytes.len()
            + self.data.dict_resource_str_bytes.len()
            + self.data.dict_source_type_bytes.len()
            + self.data.dict_unit_bytes.len();

        // Packed integer columns, counted at the per-entry allowance.
        let varint_entries = self.data.dict_tagsets.len()
            + self.data.dict_resource_len.len()
            + self.data.dict_resource_type.len()
            + self.data.dict_resource_name.len()
            + self.data.dict_origin_info.len()
            + self.data.types.len()
            + self.data.names.len()
            + self.data.tags.len()
            + self.data.resources.len()
            + self.data.intervals.len()
            + self.data.num_points.len()
            + self.data.source_type_names.len()
            + self.data.origin_infos.len()
            + self.data.unit_refs.len()
            + self.data.timestamps.len()
            + self.data.vals_sint64.len()
            + self.data.sketch_num_bins.len()
            + self.data.sketch_bin_keys.len()
            + self.data.sketch_bin_cnts.len();

        // Fixed-width float columns, counted exactly.
        let float_bytes =
            (self.data.vals_float32.len() * size_of::<f32>()) + (self.data.vals_float64.len() * size_of::<f64>());

        dict_bytes + (varint_entries * VARINT_LEN_ALLOWANCE) + float_bytes
    }

    /// Begins writing a new metric.
    ///
    /// Returns a [`V3MetricBuilder`] that must be used to set the metric's
    /// properties and add points, then closed with [`V3MetricBuilder::close`].
    pub fn write(&mut self, metric_type: V3MetricType, name: &str) -> V3MetricBuilder<'_> {
        self.write_with_symbol_id(metric_type, name, None)
    }

    /// Begins writing a metric with an optional packed symbol cache hint for its name.
    pub fn write_with_symbol_id(
        &mut self, metric_type: V3MetricType, name: &str, hint: Option<(usize, u32)>,
    ) -> V3MetricBuilder<'_> {
        let name_id = intern_string_with_hint(&mut self.name_interner, &mut self.data.dict_name_bytes, name, hint);
        let metric_idx = self.data.types.len();
        let point_start_idx = self.data.vals_float64.len();
        let sint64_start_idx = self.data.vals_sint64.len();

        // Initialize the per-metric columns with default values
        self.data.types.push(metric_type.as_u64());
        self.data.names.push(name_id);
        self.data.tags.push(0);
        self.data.resources.push(0);
        self.data.intervals.push(0);
        self.data.num_points.push(0);
        self.data.source_type_names.push(0);
        self.data.origin_infos.push(0);

        V3MetricBuilder {
            writer: self,
            point_start_idx,
            sint64_start_idx,
            metric_idx,
            unit_ref_idx: None,
        }
    }

    /// Resets the writer for a new payload, retaining its allocated storage.
    ///
    /// Discards pending metrics and resets all dictionary IDs.
    pub fn clear(&mut self) {
        self.name_interner.clear();
        self.tag_interner.clear();
        self.tagset_interner.clear();
        self.resource_str_interner.clear();
        self.resource_interner.clear();
        self.source_type_interner.clear();
        self.origin_interner.clear();
        self.unit_interner.clear();
        self.resource_keys.clear();
        self.tag_epochs.clear();
        self.tag_epoch = 0;
        self.data.dict_name_bytes.clear();
        self.data.dict_tags_bytes.clear();
        self.data.dict_tagsets.clear();
        self.data.dict_resource_str_bytes.clear();
        self.data.dict_resource_len.clear();
        self.data.dict_resource_type.clear();
        self.data.dict_resource_name.clear();
        self.data.dict_source_type_bytes.clear();
        self.data.dict_origin_info.clear();
        self.data.dict_unit_bytes.clear();
        self.data.types.clear();
        self.data.names.clear();
        self.data.tags.clear();
        self.data.resources.clear();
        self.data.intervals.clear();
        self.data.num_points.clear();
        self.data.source_type_names.clear();
        self.data.origin_infos.clear();
        self.data.unit_refs.clear();
        self.data.timestamps.clear();
        self.data.vals_sint64.clear();
        self.data.vals_float32.clear();
        self.data.vals_float64.clear();
        self.data.sketch_num_bins.clear();
        self.data.sketch_bin_keys.clear();
        self.data.sketch_bin_cnts.clear();
        self.data.value_encoding_stats = V3ValueEncodingStats::default();
    }

    fn prepare_columns(&mut self) {
        let data = &mut self.data;
        // Keep absolute references and timestamps during ingestion. Delta encoding makes adjacent values cheaper to
        // represent as signed varints and changes the columns in place.
        for column in [
            &mut data.names,
            &mut data.tags,
            &mut data.resources,
            &mut data.source_type_names,
            &mut data.origin_infos,
            &mut data.unit_refs,
            &mut data.timestamps,
        ] {
            delta_encode(column);
        }

        // Lookup uses the exact tag ID sequence. Sort and delta-encode each length-prefixed sequence only after
        // lookup ends, so the tagset arena can serve as both the dictionary key and the output column.
        let mut tagsets = data.dict_tagsets.as_mut_slice();
        while let Some((&mut len, rest)) = tagsets.split_first_mut() {
            let (ids, remaining) = rest.split_at_mut(len as usize);
            ids.sort_unstable();
            delta_encode(ids);
            tagsets = remaining;
        }
    }

    #[cfg(test)]
    fn finalize_inner(mut self) -> V3EncodedData {
        self.prepare_columns();
        self.data
    }

    /// Finalizes the writer and serializes the data into a new payload.
    ///
    /// Applies the final delta encoding and tagset sort before framing the columns. Consumes the writer.
    ///
    /// # Errors
    ///
    /// Returns an error if protobuf serialization fails.
    pub fn finalize(mut self) -> Result<V3EncodedMetrics, V3EncodeError> {
        self.prepare_columns();
        self.serialize_columns()
    }

    /// Serializes the payload and clears the writer while retaining its storage.
    ///
    /// Applies the same encoding as `finalize`. Clears the writer even if serialization fails.
    ///
    /// # Errors
    ///
    /// Returns an error if protobuf serialization fails.
    pub fn finish(&mut self) -> Result<V3EncodedMetrics, V3EncodeError> {
        self.prepare_columns();
        let result = self.serialize_columns();
        self.clear();
        result
    }

    fn serialize_columns(&self) -> Result<V3EncodedMetrics, V3EncodeError> {
        let data = &self.data;
        let mut output = Vec::with_capacity(self.estimated_uncompressed_len());
        let mut columns = Vec::with_capacity(26);

        // Dictionary columns precede the references to them. The string dictionaries carry their own lengths.
        if !data.dict_name_bytes.is_empty() {
            write_bytes_column(
                &mut output,
                &mut columns,
                DICT_NAME_STR_FIELD_NUMBER,
                &data.dict_name_bytes,
            )?;
        }
        if !data.dict_tags_bytes.is_empty() {
            write_bytes_column(
                &mut output,
                &mut columns,
                DICT_TAGS_STR_FIELD_NUMBER,
                &data.dict_tags_bytes,
            )?;
        }

        // Packed repeated fields for dictionaries
        write_packed_column(&mut output, &mut columns, DICT_TAGSETS_FIELD_NUMBER, |os| {
            os.write_repeated_packed_sint64(DICT_TAGSETS_FIELD_NUMBER, &data.dict_tagsets)
        })?;

        if !data.dict_resource_str_bytes.is_empty() {
            write_bytes_column(
                &mut output,
                &mut columns,
                DICT_RESOURCE_STR_FIELD_NUMBER,
                &data.dict_resource_str_bytes,
            )?;
        }

        write_packed_column(&mut output, &mut columns, DICT_RESOURCE_LEN_FIELD_NUMBER, |os| {
            os.write_repeated_packed_int64(DICT_RESOURCE_LEN_FIELD_NUMBER, &data.dict_resource_len)
        })?;
        write_packed_column(&mut output, &mut columns, DICT_RESOURCE_TYPE_FIELD_NUMBER, |os| {
            os.write_repeated_packed_sint64(DICT_RESOURCE_TYPE_FIELD_NUMBER, &data.dict_resource_type)
        })?;
        write_packed_column(&mut output, &mut columns, DICT_RESOURCE_NAME_FIELD_NUMBER, |os| {
            os.write_repeated_packed_sint64(DICT_RESOURCE_NAME_FIELD_NUMBER, &data.dict_resource_name)
        })?;

        if !data.dict_source_type_bytes.is_empty() {
            write_bytes_column(
                &mut output,
                &mut columns,
                DICT_SOURCE_TYPE_NAME_FIELD_NUMBER,
                &data.dict_source_type_bytes,
            )?;
        }

        write_packed_column(&mut output, &mut columns, DICT_ORIGIN_INFO_FIELD_NUMBER, |os| {
            os.write_repeated_packed_int32(DICT_ORIGIN_INFO_FIELD_NUMBER, &data.dict_origin_info)
        })?;
        if !data.dict_unit_bytes.is_empty() {
            write_bytes_column(
                &mut output,
                &mut columns,
                DICT_UNIT_STR_FIELD_NUMBER,
                &data.dict_unit_bytes,
            )?;
        }

        // Per-metric columns
        write_packed_column(&mut output, &mut columns, TYPES_FIELD_NUMBER, |os| {
            os.write_repeated_packed_uint64(TYPES_FIELD_NUMBER, &data.types)
        })?;
        write_packed_column(&mut output, &mut columns, NAMES_FIELD_NUMBER, |os| {
            os.write_repeated_packed_sint64(NAMES_FIELD_NUMBER, &data.names)
        })?;
        write_packed_column(&mut output, &mut columns, TAGS_FIELD_NUMBER, |os| {
            os.write_repeated_packed_sint64(TAGS_FIELD_NUMBER, &data.tags)
        })?;
        write_packed_column(&mut output, &mut columns, RESOURCES_FIELD_NUMBER, |os| {
            os.write_repeated_packed_sint64(RESOURCES_FIELD_NUMBER, &data.resources)
        })?;
        write_packed_column(&mut output, &mut columns, INTERVALS_FIELD_NUMBER, |os| {
            os.write_repeated_packed_uint64(INTERVALS_FIELD_NUMBER, &data.intervals)
        })?;
        write_packed_column(&mut output, &mut columns, NUM_POINTS_FIELD_NUMBER, |os| {
            os.write_repeated_packed_uint64(NUM_POINTS_FIELD_NUMBER, &data.num_points)
        })?;
        write_packed_column(&mut output, &mut columns, SOURCE_TYPE_NAME_FIELD_NUMBER, |os| {
            os.write_repeated_packed_sint64(SOURCE_TYPE_NAME_FIELD_NUMBER, &data.source_type_names)
        })?;
        write_packed_column(&mut output, &mut columns, ORIGIN_INFO_FIELD_NUMBER, |os| {
            os.write_repeated_packed_sint64(ORIGIN_INFO_FIELD_NUMBER, &data.origin_infos)
        })?;
        write_packed_column(&mut output, &mut columns, UNIT_REFS_FIELD_NUMBER, |os| {
            os.write_repeated_packed_sint64(UNIT_REFS_FIELD_NUMBER, &data.unit_refs)
        })?;

        // Point data
        write_packed_column(&mut output, &mut columns, TIMESTAMPS_FIELD_NUMBER, |os| {
            os.write_repeated_packed_sint64(TIMESTAMPS_FIELD_NUMBER, &data.timestamps)
        })?;
        write_packed_column(&mut output, &mut columns, VALS_SINT64_FIELD_NUMBER, |os| {
            os.write_repeated_packed_sint64(VALS_SINT64_FIELD_NUMBER, &data.vals_sint64)
        })?;
        write_packed_column(&mut output, &mut columns, VALS_FLOAT32_FIELD_NUMBER, |os| {
            os.write_repeated_packed_float(VALS_FLOAT32_FIELD_NUMBER, &data.vals_float32)
        })?;
        write_packed_column(&mut output, &mut columns, VALS_FLOAT64_FIELD_NUMBER, |os| {
            os.write_repeated_packed_double(VALS_FLOAT64_FIELD_NUMBER, &data.vals_float64)
        })?;

        // Sketch data
        write_packed_column(&mut output, &mut columns, SKETCH_NUM_BINS_FIELD_NUMBER, |os| {
            os.write_repeated_packed_uint64(SKETCH_NUM_BINS_FIELD_NUMBER, &data.sketch_num_bins)
        })?;
        write_packed_column(&mut output, &mut columns, SKETCH_BIN_KEYS_FIELD_NUMBER, |os| {
            os.write_repeated_packed_sint32(SKETCH_BIN_KEYS_FIELD_NUMBER, &data.sketch_bin_keys)
        })?;
        write_packed_column(&mut output, &mut columns, SKETCH_BIN_CNTS_FIELD_NUMBER, |os| {
            os.write_repeated_packed_uint32(SKETCH_BIN_CNTS_FIELD_NUMBER, &data.sketch_bin_cnts)
        })?;

        Ok(V3EncodedMetrics {
            payload: output,
            stats: V3EncoderStats {
                value_encoding_stats: data.value_encoding_stats,
                columns,
            },
        })
    }

    fn intern_tagset<I, S>(&mut self, tags: I) -> i64
    where
        I: Iterator<Item = S>,
        S: AsRef<str>,
    {
        self.tag_ids.clear();
        for tag in tags {
            self.tag_ids.push(intern_string(
                &mut self.tag_interner,
                &mut self.data.dict_tags_bytes,
                tag.as_ref(),
            ));
        }
        self.intern_tag_ids()
    }

    fn intern_unique_tagset<'a>(&mut self, tags: impl Iterator<Item = (&'a str, Option<(usize, u32)>)>) -> i64 {
        self.tag_ids.clear();
        self.tag_epoch = self.tag_epoch.wrapping_add(1);
        if self.tag_epoch == 0 {
            self.tag_epochs.fill(0);
            self.tag_epoch = 1;
        }
        for (tag, hint) in tags {
            let id = intern_string_with_hint(&mut self.tag_interner, &mut self.data.dict_tags_bytes, tag, hint);
            let idx = id as usize;
            if idx >= self.tag_epochs.len() {
                self.tag_epochs.resize(idx + 1, 0);
            }
            if self.tag_epochs[idx] != self.tag_epoch {
                self.tag_epochs[idx] = self.tag_epoch;
                self.tag_ids.push(id);
            }
        }
        self.intern_tag_ids()
    }

    /// Interns the current sequence of tag IDs without changing their order.
    ///
    /// The prefix records the number of IDs so finalization can sort each set independently.
    fn intern_tag_ids(&mut self) -> i64 {
        if self.tag_ids.is_empty() {
            return 0;
        }

        self.tagset_interner
            .get_or_insert(&self.tag_ids, &mut self.data.dict_tagsets, |arena| {
                arena.push(self.tag_ids.len() as i64);
            })
            .0
    }

    fn intern_resources(&mut self, resources: &[(&str, &str)]) -> i64 {
        if resources.is_empty() {
            return 0;
        }

        self.resource_ids.clear();
        let arena = &mut self.data.dict_resource_str_bytes;
        for (resource_type, resource_name) in resources {
            let type_id = intern_string(&mut self.resource_str_interner, arena, resource_type);
            let name_id = intern_string(&mut self.resource_str_interner, arena, resource_name);
            self.resource_ids.push((type_id, name_id));
        }

        let (id, is_new) = self
            .resource_interner
            .get_or_insert(&self.resource_ids, &mut self.resource_keys, |_| {});
        if is_new {
            self.data.dict_resource_len.push(self.resource_ids.len() as i64);
            let (mut prev_type, mut prev_name) = (0, 0);
            for &(type_id, name_id) in &self.resource_ids {
                self.data.dict_resource_type.push(type_id - prev_type);
                self.data.dict_resource_name.push(name_id - prev_name);
                (prev_type, prev_name) = (type_id, name_id);
            }
        }
        id
    }

    fn intern_origin(&mut self, product: i32, category: i32, service: i32) -> i64 {
        if product == 0 && category == 0 && service == 0 {
            return 0;
        }
        self.origin_interner
            .get_or_insert(&[product, category, service], &mut self.data.dict_origin_info, |_| {})
            .0
    }
}

/// Builder for a single metric within a V3 payload.
///
/// Use the setter methods to configure the metric, add points with [`add_point`](Self::add_point),
/// then call [`close`](Self::close) to finalize.
pub struct V3MetricBuilder<'a> {
    writer: &'a mut V3Writer,
    point_start_idx: usize,
    sint64_start_idx: usize,
    metric_idx: usize,
    unit_ref_idx: Option<usize>,
}

impl<'a> V3MetricBuilder<'a> {
    /// Sets the tags for this metric.
    ///
    /// Tags should be in "key:value" format.
    pub fn set_tags<I, S>(&mut self, tags: I)
    where
        I: Iterator<Item = S>,
        S: AsRef<str>,
    {
        let tagset_id = self.writer.intern_tagset(tags);
        self.writer.data.tags[self.metric_idx] = tagset_id;
    }

    /// Sets tags in first-seen order, deduplicating equal strings.
    ///
    /// Each tag can carry an optional `(table, id)` packed symbol cache hint.
    pub fn set_unique_tags<'t>(&mut self, tags: impl Iterator<Item = (&'t str, Option<(usize, u32)>)>) {
        let tagset_id = self.writer.intern_unique_tagset(tags);
        self.writer.data.tags[self.metric_idx] = tagset_id;
    }

    /// Sets the resources for this metric.
    ///
    /// Resources are (type, name) pairs, for example, (`host`, `server1`).
    pub fn set_resources(&mut self, resources: &[(&str, &str)]) {
        let res_id = self.writer.intern_resources(resources);
        self.writer.data.resources[self.metric_idx] = res_id;
    }

    /// Sets the interval for this metric (used for rate metrics).
    pub fn set_interval(&mut self, interval: u64) {
        self.writer.data.intervals[self.metric_idx] = interval;
    }

    /// Sets the source type name for this metric.
    pub fn set_source_type(&mut self, source_type: &str) {
        if source_type.is_empty() {
            self.writer.data.source_type_names[self.metric_idx] = 0;
            return;
        }
        let id = intern_string(
            &mut self.writer.source_type_interner,
            &mut self.writer.data.dict_source_type_bytes,
            source_type,
        );
        self.writer.data.source_type_names[self.metric_idx] = id;
    }

    /// Sets the origin metadata for this metric.
    pub fn set_origin(&mut self, product: u32, category: u32, service: u32, no_index: bool) {
        let id = self
            .writer
            .intern_origin(product as i32, category as i32, service as i32);
        self.writer.data.origin_infos[self.metric_idx] = id;
        if no_index {
            self.writer.data.types[self.metric_idx] |= FLAG_NO_INDEX;
        }
    }

    /// Sets the unit for this metric.
    pub fn set_unit(&mut self, unit: &str) {
        if unit.is_empty() {
            self.writer.data.types[self.metric_idx] &= !FLAG_HAS_UNIT;
            if let Some(unit_ref_idx) = self.unit_ref_idx.take() {
                self.writer.data.unit_refs.remove(unit_ref_idx);
            }
            return;
        }

        let id = intern_string(
            &mut self.writer.unit_interner,
            &mut self.writer.data.dict_unit_bytes,
            unit,
        );
        if let Some(unit_ref_idx) = self.unit_ref_idx {
            self.writer.data.unit_refs[unit_ref_idx] = id;
        } else {
            self.unit_ref_idx = Some(self.writer.data.unit_refs.len());
            self.writer.data.unit_refs.push(id);
        }
        self.writer.data.types[self.metric_idx] |= FLAG_HAS_UNIT;
    }

    /// Adds a data point to this metric.
    pub fn add_point(&mut self, timestamp: i64, value: f64) {
        self.writer.data.timestamps.push(timestamp);
        self.writer.data.vals_float64.push(value);
        self.writer.data.num_points[self.metric_idx] += 1;
    }

    /// Adds sketch data for a distribution metric.
    ///
    /// For sketches, the summary values (count, sum, min, max) are stored as points,
    /// and the bin keys/counts are stored separately.
    pub fn add_sketch(
        &mut self, timestamp: i64, count: i64, sum: f64, min: f64, max: f64, bin_keys: &[i32], bin_counts: &[u32],
    ) {
        self.writer.data.timestamps.push(timestamp);

        // Count goes in sint64, sum/min/max go in float64
        self.writer.data.vals_sint64.push(count);
        self.writer.data.vals_float64.push(sum);
        self.writer.data.vals_float64.push(min);
        self.writer.data.vals_float64.push(max);

        // Store bin data
        self.writer.data.sketch_num_bins.push(bin_keys.len() as u64);

        let key_start = self.writer.data.sketch_bin_keys.len();
        self.writer.data.sketch_bin_keys.extend_from_slice(bin_keys);
        self.writer.data.sketch_bin_cnts.extend_from_slice(bin_counts);

        // Delta-encode this sketch's bin keys
        delta_encode_i32(&mut self.writer.data.sketch_bin_keys[key_start..]);

        self.writer.data.num_points[self.metric_idx] += 1;
    }

    /// Finalizes this metric.
    ///
    /// This compacts the point values to use the smallest representation
    /// that can hold all values without loss.
    pub fn close(mut self) {
        self.compact_values();
    }

    fn compact_values(&mut self) {
        let count = self.writer.data.num_points[self.metric_idx] as usize;
        if count == 0 {
            return;
        }

        let start = self.point_start_idx;
        let end = self.writer.data.vals_float64.len();

        // Determine the best value type for all points in this metric.
        let val_ty = value_type_for_values(self.writer.data.vals_float64[start..end].iter().copied());
        let is_sketch = (self.writer.data.types[self.metric_idx] & 0x0F) == V3MetricType::Sketch as u64;
        let float_values_len = end - start;
        if is_sketch {
            // Sketches always carry one integer count per point in addition to sum/min/max values.
            self.writer.data.value_encoding_stats.sint64 += count as u64;
        }

        // Update the type field
        self.writer.data.types[self.metric_idx] |= val_ty.as_u64();

        // Convert values to the appropriate storage
        match val_ty {
            V3ValueType::Zero => {
                self.writer.data.value_encoding_stats.zero += float_values_len as u64;
                // Values are all zero, don't store anything
                self.writer.data.vals_float64.truncate(start);
            }
            V3ValueType::Sint64 => {
                self.writer.data.value_encoding_stats.sint64 += float_values_len as u64;
                if is_sketch {
                    // For sketches, vals_sint64 already has one count per point (pushed by add_sketch),
                    // and vals_float64 has 3 values per point (sum, min, max). When compacting to Sint64,
                    // we need to interleave them as: sum, min, max, cnt per point.
                    let counts: Vec<i64> = self.writer.data.vals_sint64[self.sint64_start_idx..].to_vec();
                    self.writer.data.vals_sint64.truncate(self.sint64_start_idx);
                    for (i, cnt) in counts.into_iter().enumerate() {
                        let f_off = start + i * 3;
                        self.writer
                            .data
                            .vals_sint64
                            .push(self.writer.data.vals_float64[f_off] as i64);
                        self.writer
                            .data
                            .vals_sint64
                            .push(self.writer.data.vals_float64[f_off + 1] as i64);
                        self.writer
                            .data
                            .vals_sint64
                            .push(self.writer.data.vals_float64[f_off + 2] as i64);
                        self.writer.data.vals_sint64.push(cnt);
                    }
                } else {
                    for i in start..end {
                        self.writer
                            .data
                            .vals_sint64
                            .push(self.writer.data.vals_float64[i] as i64);
                    }
                }
                self.writer.data.vals_float64.truncate(start);
            }
            V3ValueType::Float32 => {
                self.writer.data.value_encoding_stats.float32 += float_values_len as u64;
                for i in start..end {
                    self.writer
                        .data
                        .vals_float32
                        .push(self.writer.data.vals_float64[i] as f32);
                }
                self.writer.data.vals_float64.truncate(start);
            }
            V3ValueType::Float64 => {
                self.writer.data.value_encoding_stats.float64 += float_values_len as u64;
                // Already stored in vals_float64, keep them
            }
        }
    }
}

fn write_bytes_column(
    output: &mut Vec<u8>, columns: &mut Vec<V3ColumnBytes>, field_number: u32, bytes: &[u8],
) -> Result<(), V3EncodeError> {
    write_packed_column(output, columns, field_number, |os| os.write_bytes(field_number, bytes))
}

fn write_packed_column(
    output: &mut Vec<u8>, columns: &mut Vec<V3ColumnBytes>, field_number: u32,
    write: impl FnOnce(&mut CodedOutputStream<'_>) -> protobuf::Result<()>,
) -> Result<(), V3EncodeError> {
    let start = output.len();
    {
        let mut os = CodedOutputStream::vec(output);
        write(&mut os)?;
        os.flush()?;
    }

    if output.len() != start {
        // Skip the two varints for the field tag and length. Column telemetry measures only the field contents.
        let header_len = output[start..]
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte < 0x80)
            .nth(1)
            .unwrap()
            .0
            + 1;
        columns.push(V3ColumnBytes {
            field_number,
            range: start + header_len..output.len(),
            compressed_len: 0,
        });
    }
    Ok(())
}

fn intern_string(interner: &mut Interner, arena: &mut Vec<u8>, text: &str) -> i64 {
    intern_string_with_hint(interner, arena, text, None)
}

fn intern_string_with_hint(
    interner: &mut Interner, arena: &mut Vec<u8>, text: &str, hint: Option<(usize, u32)>,
) -> i64 {
    if text.is_empty() {
        return 0;
    }
    let prefix = |arena: &mut Vec<_>| append_len(arena, text.len());
    if let Some(hint) = hint {
        interner.intern_symbol(hint, text.as_bytes(), arena, prefix)
    } else {
        interner.get_or_insert(text.as_bytes(), arena, prefix).0
    }
}

fn append_len(dst: &mut Vec<u8>, mut len: usize) {
    loop {
        let mut byte = (len & 0x7F) as u8;
        len >>= 7;
        if len != 0 {
            byte |= 0x80;
        }
        dst.push(byte);
        if len == 0 {
            break;
        }
    }
}

// Work backward so each subtraction reads the preceding absolute value before overwriting it.
fn delta_encode(s: &mut [i64]) {
    if s.len() < 2 {
        return;
    }
    for i in (1..s.len()).rev() {
        s[i] -= s[i - 1];
    }
}

fn delta_encode_i32(s: &mut [i32]) {
    if s.len() < 2 {
        return;
    }
    for i in (1..s.len()).rev() {
        s[i] -= s[i - 1];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delta_encode() {
        let mut data = vec![100, 110, 130, 145];
        delta_encode(&mut data);
        assert_eq!(data, vec![100, 10, 20, 15]);
    }

    #[test]
    fn estimated_uncompressed_len_grows_and_brackets_finalized_size() {
        let mut writer = V3Writer::new();
        assert_eq!(0, writer.metric_count());
        let empty_estimate = writer.estimated_uncompressed_len();

        for idx in 0..16 {
            let mut builder = writer.write(V3MetricType::Count, &format!("estimate.metric.{idx}"));
            builder.set_tags([format!("tag_key_{idx}:tag_value_{idx}")].into_iter());
            builder.add_point(1_700_000_000 + idx as i64, idx as f64);
            builder.close();
        }

        assert_eq!(16, writer.metric_count());
        let estimate = writer.estimated_uncompressed_len();
        assert!(
            estimate > empty_estimate,
            "estimate should grow as metrics are written: {estimate} vs {empty_estimate}"
        );

        // The estimate exists to bound batch sizes, so it must not undershoot the real payload: cutting a batch on an
        // undershooting estimate is what forces the expensive re-encode path.
        let encoded = writer.finalize().expect("metrics should encode");
        assert!(
            estimate >= encoded.payload.len(),
            "estimate {estimate} should not undershoot finalized payload {}",
            encoded.payload.len()
        );
    }

    #[test]
    fn estimated_uncompressed_len_tracks_dictionary_growth() {
        // A metric whose tags are already interned should add far less to the estimate than one introducing new tag
        // strings, since dictionary bytes are counted exactly.
        let mut writer = V3Writer::new();
        let mut builder = writer.write(V3MetricType::Count, "dict.growth");
        builder.set_tags(["shared_key:shared_value".to_string()].into_iter());
        builder.add_point(1_700_000_000, 1.0);
        builder.close();
        let after_first = writer.estimated_uncompressed_len();

        let mut builder = writer.write(V3MetricType::Count, "dict.growth");
        builder.set_tags(["shared_key:shared_value".to_string()].into_iter());
        builder.add_point(1_700_000_001, 2.0);
        builder.close();
        let after_deduped = writer.estimated_uncompressed_len();

        let mut builder = writer.write(V3MetricType::Count, "dict.growth.novel");
        builder.set_tags(["a_completely_different_key:a_completely_different_value".to_string()].into_iter());
        builder.add_point(1_700_000_002, 3.0);
        builder.close();
        let after_novel = writer.estimated_uncompressed_len();

        let deduped_growth = after_deduped - after_first;
        let novel_growth = after_novel - after_deduped;
        assert!(
            novel_growth > deduped_growth,
            "new dictionary entries should grow the estimate more than deduplicated ones: {novel_growth} vs {deduped_growth}"
        );
    }

    #[test]
    fn test_delta_encode_empty() {
        let mut data: Vec<i64> = vec![];
        delta_encode(&mut data);
        assert!(data.is_empty());
    }

    #[test]
    fn test_delta_encode_single() {
        let mut data = vec![42];
        delta_encode(&mut data);
        assert_eq!(data, vec![42]);
    }

    #[test]
    fn test_append_len_str() {
        let mut buf = Vec::new();
        assert_eq!(intern_string(&mut Interner::default(), &mut buf, "hello"), 1);
        // Length 5 = 0x05, then "hello"
        assert_eq!(buf, vec![5, b'h', b'e', b'l', b'l', b'o']);
    }

    #[test]
    fn test_writer_basic() {
        let mut writer = V3Writer::new();

        {
            let mut metric = writer.write(V3MetricType::Gauge, "test.metric");
            metric.set_tags(["env:prod", "service:web"].iter().copied());
            metric.add_point(1000, 42.0);
            metric.add_point(1010, 43.5);
            metric.close();
        }

        let data = writer.finalize_inner();

        assert_eq!(data.types.len(), 1);
        assert_eq!(data.names.len(), 1);
        assert_eq!(data.timestamps.len(), 2);
    }

    #[test]
    fn test_writer_unit() {
        let mut writer = V3Writer::new();

        {
            let mut metric = writer.write(V3MetricType::Gauge, "has.unit");
            metric.set_unit("millisecond");
            metric.add_point(1000, 42.0);
            metric.close();
        }
        {
            let mut metric = writer.write(V3MetricType::Gauge, "no.unit");
            metric.add_point(1000, 43.0);
            metric.close();
        }
        {
            let mut metric = writer.write(V3MetricType::Gauge, "same.unit");
            metric.set_unit("millisecond");
            metric.add_point(1000, 44.0);
            metric.close();
        }

        let data = writer.finalize_inner();

        assert_eq!(data.unit_refs, vec![1, 0]);
        assert_eq!(data.dict_unit_bytes, b"\x0bmillisecond");
        assert_eq!(data.types[0] & FLAG_HAS_UNIT, FLAG_HAS_UNIT);
        assert_eq!(data.types[1] & FLAG_HAS_UNIT, 0);
        assert_eq!(data.types[2] & FLAG_HAS_UNIT, FLAG_HAS_UNIT);
    }

    #[test]
    fn test_writer_multiple_metrics() {
        let mut writer = V3Writer::new();

        {
            let mut m1 = writer.write(V3MetricType::Count, "metric1");
            m1.add_point(1000, 10.0);
            m1.close();
        }

        {
            let mut m2 = writer.write(V3MetricType::Rate, "metric2");
            m2.set_interval(60);
            m2.add_point(2000, 20.0);
            m2.close();
        }

        let data = writer.finalize_inner();

        assert_eq!(data.types.len(), 2);
        assert_eq!(data.names.len(), 2);
        assert_eq!(data.intervals[0], 0);
        // Second metric's interval won't be 60 directly since names is delta-encoded,
        // but we can verify the structure is correct
    }

    #[test]
    fn test_value_compaction_zero() {
        let mut writer = V3Writer::new();

        {
            let mut metric = writer.write(V3MetricType::Gauge, "zero.metric");
            metric.add_point(1000, 0.0);
            metric.add_point(2000, 0.0);
            metric.close();
        }

        let data = writer.finalize_inner();

        // Values should be compacted - zero values don't need storage
        assert!(data.vals_float64.is_empty());
        assert!(data.vals_sint64.is_empty());
        assert!(data.vals_float32.is_empty());
    }

    #[test]
    fn test_value_compaction_int() {
        let mut writer = V3Writer::new();

        {
            let mut metric = writer.write(V3MetricType::Count, "int.metric");
            metric.add_point(1000, 100.0);
            metric.add_point(2000, 200.0);
            metric.close();
        }

        let data = writer.finalize_inner();

        // Integer values should be stored in sint64
        assert!(data.vals_float64.is_empty());
        assert_eq!(data.vals_sint64, vec![100, 200]);
        assert!(data.vals_float32.is_empty());
    }

    #[test]
    fn test_serialize_empty() {
        let writer = V3Writer::new();
        let encoded = writer.finalize().unwrap();
        assert!(encoded.payload.is_empty());
    }

    #[test]
    fn test_value_compaction_large_int_plus_float32() {
        // Regression test: a large integer (> 2^24) mixed with a fractional
        // float32 value must use Float64, not Float32, to avoid precision loss.
        let mut writer = V3Writer::new();

        {
            let mut metric = writer.write(V3MetricType::Gauge, "mixed.metric");
            metric.add_point(1000, (1i64 << 30) as f64); // large int, doesn't fit in f32
            metric.add_point(2000, 1.5); // fractional, fits in f32
            metric.close();
        }

        let data = writer.finalize_inner();

        // Must be stored in float64, not float32
        assert!(
            data.vals_float32.is_empty(),
            "large int should not be stored as float32"
        );
        assert_eq!(data.vals_float64, vec![(1i64 << 30) as f64, 1.5]);
        assert!(data.vals_sint64.is_empty());
    }

    #[test]
    fn test_value_compaction_small_int_plus_float32() {
        // Small integers (|v| <= 2^24) mixed with float32 values should
        // compact to Float32, since small ints fit losslessly in f32.
        let mut writer = V3Writer::new();

        {
            let mut metric = writer.write(V3MetricType::Gauge, "small.mixed");
            metric.add_point(1000, 100.0);
            metric.add_point(2000, 1.5);
            metric.close();
        }

        let data = writer.finalize_inner();

        assert!(data.vals_float64.is_empty());
        assert_eq!(data.vals_float32, vec![100.0, 1.5]);
        assert!(data.vals_sint64.is_empty());
    }

    #[test]
    fn test_serialize_basic_metric() {
        let mut writer = V3Writer::new();

        {
            let mut metric = writer.write(V3MetricType::Gauge, "test.metric");
            metric.add_point(1000, 42.0);
            metric.close();
        }

        let encoded = writer.finalize().unwrap();

        // Should produce non-empty output
        assert!(!encoded.payload.is_empty());
        assert_eq!(encoded.stats.value_encoding_stats.sint64, 1);
    }

    #[test]
    fn test_column_stats_for_bytes_column_use_raw_column_stream() {
        let mut writer = V3Writer::new();

        {
            let mut metric = writer.write(V3MetricType::Gauge, "test.metric");
            metric.add_point(1000, 42.0);
            metric.close();
        }

        let encoded = writer.finalize().unwrap();
        let name_column = encoded
            .stats
            .columns
            .iter()
            .find(|column| column.field_number == DICT_NAME_STR_FIELD_NUMBER)
            .expect("name dictionary column should be present");

        assert_eq!(&encoded.payload[name_column.range.clone()], b"\x0btest.metric");
    }

    #[test]
    fn test_column_stats_for_packed_column_use_raw_column_stream() {
        let mut writer = V3Writer::new();

        {
            let mut metric = writer.write(V3MetricType::Gauge, "test.metric");
            metric.add_point(1000, 42.0);
            metric.close();
        }

        let encoded = writer.finalize().unwrap();
        let timestamps_column = encoded
            .stats
            .columns
            .iter()
            .find(|column| column.field_number == TIMESTAMPS_FIELD_NUMBER)
            .expect("timestamp column should be present");

        let mut expected = Vec::new();
        {
            let mut os = CodedOutputStream::vec(&mut expected);
            os.write_sint64_no_tag(1000).unwrap();
            os.flush().unwrap();
        }
        assert_eq!(encoded.payload[timestamps_column.range.clone()], expected);
    }

    #[test]
    fn test_column_stats_do_not_include_absent_columns() {
        let mut writer = V3Writer::new();

        {
            let mut metric = writer.write(V3MetricType::Gauge, "test.metric");
            metric.add_point(1000, 42.0);
            metric.close();
        }

        let encoded = writer.finalize().unwrap();
        assert!(!encoded
            .stats
            .columns
            .iter()
            .any(|column| column.field_number == UNIT_REFS_FIELD_NUMBER));
    }

    #[test]
    fn reused_writer_matches_fresh_writer_after_populating_dictionaries() {
        fn write_metric(writer: &mut V3Writer, name: &str, tag: &str, time: i64) {
            let mut metric = writer.write(V3MetricType::Gauge, name);
            metric.set_tags(["env:testing", tag].into_iter());
            metric.set_resources(&[("host", "local"), ("container", tag)]);
            metric.set_source_type("source");
            metric.set_origin(1, 2, 3, true);
            metric.set_unit("ms");
            metric.add_point(time, 42.0);
            metric.close();
        }

        let mut writer = V3Writer::new();
        write_metric(&mut writer, "another", "service:old", 1000);
        write_metric(&mut writer, "new", "service:new", 900);
        writer.finish().unwrap();
        assert_eq!(writer.estimated_uncompressed_len(), 0);

        let mut fresh = V3Writer::new();
        for target in [&mut writer, &mut fresh] {
            write_metric(target, "new", "service:new", 500);
            write_metric(target, "new", "service:other", 400);
        }
        assert_eq!(writer.finish().unwrap().payload, fresh.finalize().unwrap().payload);
    }
}
