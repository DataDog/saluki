use std::io::Write;

use saluki_error::{ErrorContext as _, GenericError};

use super::callsite_table::{CallsiteEntry, CallsiteTable, FILE_INDEX_NONE};
use super::codec::{
    encode_level, encode_varint, encode_varint_u128, rle_encode, write_length_prefixed, TIMESTAMP_GRANULARITY_NS,
};
use super::event::{CondensedEvent, FieldIter};
use super::pattern_cluster::ClusterManager;
use super::segment::CompressedSegment;
use super::string_table::StringTable;

/// Columnar event buffer that accumulates log events into per-field column buffers.
///
/// On flush, the columns are serialized into two payloads (metadata + content) and compressed as separate zstd
/// frames. See [`CompressedRingBuffer`] for the full architecture description.
///
/// ## Metadata payload layout
///
/// ```text
/// [string_table]                  -- varint(count) [varint(len) bytes ...]
/// [varint(event_count)]
/// [col: timestamps]               -- length-prefixed blob of varint-delta-encoded millisecond deltas
/// [callsite_table]                -- varint(count) [varint(target_idx) varint(file+1|0) varint(line+1|0) varint(level) ...]
/// [col: callsite_indices]         -- RLE of callsite table indices
/// [col: field_counts]             -- RLE of per-event field counts
/// [col: field_key_indices]        -- length-prefixed blob of varint string table indices
/// [col: msg_template_indices]     -- RLE of string table indices for message skeletons
/// ```
///
/// ## Content payload layout
///
/// ```text
/// [col: msg_variables]            -- length-prefixed blob: per-event varint-length-prefixed variable tokens
/// [col: field_values]             -- concatenated varint-length-prefixed field value byte strings
/// ```
pub struct EventBuffer {
    // Column accumulators.
    col_timestamps: Vec<u8>,
    // Target, file, line, and level are all fixed properties of the callsite, so they are interned
    // once per segment into `callsite_table` and events store only a compact index here (RLE).
    col_callsite_indices: Vec<usize>,
    col_msg_template_indices: Vec<usize>,
    // Message variable tokens, length-prefixed inline in the content frame. Per-event token counts
    // are not stored -- they are recovered from the `\0` placeholder count in each event's template
    // skeleton. Length and bytes are kept co-located (not split into separate columns): repeated
    // verbatim variable values let zstd match `[len][bytes]` as one unit, and splitting them was a
    // measured regression for the analogous field-value column.
    col_msg_variables: Vec<u8>,
    cluster_manager: ClusterManager,
    col_field_counts: Vec<usize>,
    col_field_key_indices: Vec<u8>,
    // Field values keep their length prefix inline (unlike message variables): field values have
    // strong length<->value correlation (fixed-length repeated strings), so co-locating length and
    // bytes lets zstd match them as one unit. Splitting them was measured as a net regression.
    col_field_values: Vec<u8>,
    callsite_table: CallsiteTable,

    event_count: usize,
    compression_level: i32,
    oldest_timestamp_nanos: u128,
    /// Previous event's timestamp, in granularity units (see [`TIMESTAMP_GRANULARITY_NS`]).
    last_timestamp_units: u128,
    string_table: StringTable,
}

impl EventBuffer {
    pub fn from_compression_level(compression_level: i32) -> Self {
        EventBuffer {
            col_timestamps: Vec::new(),
            col_callsite_indices: Vec::new(),
            col_msg_template_indices: Vec::new(),
            col_msg_variables: Vec::new(),
            cluster_manager: ClusterManager::default(),
            col_field_counts: Vec::new(),
            col_field_key_indices: Vec::new(),
            col_field_values: Vec::new(),
            callsite_table: CallsiteTable::default(),

            event_count: 0,
            compression_level,
            oldest_timestamp_nanos: 0,
            last_timestamp_units: 0,
            string_table: StringTable::default(),
        }
    }

    pub fn size_bytes(&self) -> usize {
        // Estimate the serialized payload size (pre-compression). The byte-based columns
        // (timestamps, messages, fields) are already in their serialized form. The RLE-encoded
        // callsite index column compresses dramatically -- the serialized RLE is typically a small
        // fraction of the element count. We estimate ~1 byte per element as a rough upper bound,
        // since most runs are longer than 1.
        self.col_timestamps.len()
            + self.col_msg_variables.len()
            + self.col_field_key_indices.len()
            + self.col_field_values.len()
            + self.col_callsite_indices.len()
    }

    pub fn event_count(&self) -> usize {
        self.event_count
    }

    pub fn oldest_timestamp_nanos(&self) -> u128 {
        self.oldest_timestamp_nanos
    }

    fn clear(&mut self) {
        self.col_timestamps.clear();
        self.col_callsite_indices.clear();
        self.col_msg_template_indices.clear();
        self.col_msg_variables.clear();
        self.col_field_counts.clear();
        self.col_field_key_indices.clear();
        self.col_field_values.clear();
        self.callsite_table.clear();
        self.event_count = 0;
        self.oldest_timestamp_nanos = 0;
        self.last_timestamp_units = 0;
        self.string_table.clear();
    }

    pub fn encode_event(&mut self, event: &CondensedEvent) -> Result<(), GenericError> {
        let meta = event
            .metadata
            .expect("metadata must be set on events passed to encode_event");

        if self.oldest_timestamp_nanos == 0 {
            self.oldest_timestamp_nanos = event.timestamp_nanos;
        }

        // Timestamps: delta-encoded varints at reduced (millisecond) granularity. Operating on
        // integer granularity units on both encode and decode keeps reconstruction drift-free.
        let ts_units = event.timestamp_nanos / TIMESTAMP_GRANULARITY_NS;
        let delta_units = ts_units.saturating_sub(self.last_timestamp_units);
        self.last_timestamp_units = ts_units;
        encode_varint_u128(delta_units, &mut self.col_timestamps);

        // Callsite: target, file, line, and level are fixed per callsite, so intern the callsite
        // (keyed by its `&'static Metadata` pointer identity) once per segment and store only its
        // index. The index column is RLE-encoded on flush.
        let callsite_key = meta as *const _ as usize;
        let callsite_idx = match self.callsite_table.get(callsite_key) {
            Some(idx) => idx,
            None => {
                let target_idx = self.string_table.intern(meta.target());
                let file_idx = meta
                    .file()
                    .map_or(FILE_INDEX_NONE, |file| self.string_table.intern(file));
                let entry = CallsiteEntry {
                    target_idx,
                    file_idx,
                    line: meta.line(),
                    level: encode_level(meta.level().as_str()),
                };
                self.callsite_table.insert(callsite_key, entry)
            }
        };
        self.col_callsite_indices.push(callsite_idx);

        // Message: extract template skeleton + variable tokens via Drain-inspired clustering. The
        // per-event variable count is NOT stored: it equals the number of `\0` placeholders in the
        // skeleton, which the decoder recovers from the interned template. We only write the
        // length-prefixed token bytes.
        let (skeleton, _var_count) = self.cluster_manager.extract(&event.message, meta);
        let tpl_idx = self.string_table.intern(skeleton);
        self.col_msg_template_indices.push(tpl_idx);
        for token in self.cluster_manager.variable_tokens(&event.message) {
            write_length_prefixed(&mut self.col_msg_variables, token.as_bytes());
        }

        // Fields: split into count (RLE-able), key indices (low-entropy), and values (high-entropy).
        let field_count = FieldIter {
            buf: &event.fields,
            idx: 0,
        }
        .count();
        self.col_field_counts.push(field_count);

        let field_iter = FieldIter::from_buf(&event.fields);
        for (key, val) in field_iter {
            let key_idx = self.string_table.intern(key);
            encode_varint(key_idx, &mut self.col_field_key_indices);
            write_length_prefixed(&mut self.col_field_values, val.as_bytes());
        }

        self.event_count += 1;

        Ok(())
    }

    pub fn flush(&mut self) -> Result<CompressedSegment, GenericError> {
        // Build two separate payloads for split compression:
        //   1. Metadata: string table, event count, timestamps, callsite table + indices, fields metadata
        //   2. Content: messages, fields (high-entropy text data)
        // This lets zstd build optimal coding tables for each data distribution.

        // --- Metadata payload ---
        let mut meta = Vec::with_capacity(4096);
        self.string_table.encode(&mut meta);
        encode_varint(self.event_count, &mut meta);
        write_length_prefixed(&mut meta, &self.col_timestamps);
        // Callsite table (target/file/line/level, interned once) then per-event indices (RLE).
        self.callsite_table.encode(&mut meta);
        rle_encode(&self.col_callsite_indices, &mut meta);
        // Field counts (RLE) and key indices go in meta (low-entropy).
        rle_encode(&self.col_field_counts, &mut meta);
        write_length_prefixed(&mut meta, &self.col_field_key_indices);
        // Message template indices (RLE, low-entropy).
        rle_encode(&self.col_msg_template_indices, &mut meta);

        // --- Content payload ---
        let mut content = Vec::with_capacity(self.col_msg_variables.len() + self.col_field_values.len() + 16);
        // Message variable tokens (length-prefixed inline).
        write_length_prefixed(&mut content, &self.col_msg_variables);
        // Field values (length-prefixed inline).
        content.extend_from_slice(&self.col_field_values);

        let uncompressed_size = meta.len() + content.len();

        // Compress each payload independently.
        let compress = |data: &[u8], level: i32| -> Result<Vec<u8>, GenericError> {
            let mut encoder = zstd::Encoder::new(Vec::new(), level).error_context("Failed to create ZSTD encoder.")?;
            encoder.write_all(data).error_context("Failed to compress.")?;
            encoder.finish().error_context("Failed to finalize ZSTD encoder.")
        };

        let meta_compressed = compress(&meta, self.compression_level)?;
        let content_compressed = compress(&content, self.compression_level)?;

        // Pack both frames into compressed_data: varint(meta_len) meta_bytes content_bytes.
        let mut compressed_data = Vec::with_capacity(8 + meta_compressed.len() + content_compressed.len());
        encode_varint(meta_compressed.len(), &mut compressed_data);
        compressed_data.extend_from_slice(&meta_compressed);
        compressed_data.extend_from_slice(&content_compressed);

        let compressed_segment = CompressedSegment::new(
            self.event_count,
            self.oldest_timestamp_nanos,
            uncompressed_size,
            compressed_data,
        );

        self.clear();

        Ok(compressed_segment)
    }

    /// Diagnostic: reports the per-column uncompressed and independently-compressed byte sizes for the
    /// current (un-flushed) contents. Used only by benchmarks to see where the bytes actually go.
    #[cfg(test)]
    pub fn column_breakdown(&self) -> ColumnBreakdown {
        let level = self.compression_level;
        let zc = |data: &[u8]| -> usize {
            let mut enc = zstd::Encoder::new(Vec::new(), level).unwrap();
            enc.write_all(data).unwrap();
            enc.finish().unwrap().len()
        };

        // Serialize each column exactly as flush() does, in isolation.
        let mut st = Vec::new();
        self.string_table.encode(&mut st);
        let mut ts = Vec::new();
        write_length_prefixed(&mut ts, &self.col_timestamps);
        let mut ctable = Vec::new();
        self.callsite_table.encode(&mut ctable);
        let mut cidx = Vec::new();
        rle_encode(&self.col_callsite_indices, &mut cidx);
        let mut fcounts = Vec::new();
        rle_encode(&self.col_field_counts, &mut fcounts);
        let mut fkeys = Vec::new();
        write_length_prefixed(&mut fkeys, &self.col_field_key_indices);
        let mut mtpl = Vec::new();
        rle_encode(&self.col_msg_template_indices, &mut mtpl);
        let mut mvars = Vec::new();
        write_length_prefixed(&mut mvars, &self.col_msg_variables);
        let fvals = self.col_field_values.clone();

        let cols: Vec<(&'static str, Vec<u8>)> = vec![
            ("string_table", st),
            ("timestamps", ts),
            ("callsite_table", ctable),
            ("callsite_indices", cidx),
            ("field_counts", fcounts),
            ("field_key_indices", fkeys),
            ("msg_template_indices", mtpl),
            ("msg_variables", mvars),
            ("field_values", fvals),
        ];

        let entries = cols
            .iter()
            .map(|(name, data)| ColumnStat {
                name,
                uncompressed: data.len(),
                compressed_alone: zc(data),
            })
            .collect();

        // Also measure the two frames as actually compressed together.
        let mut meta = Vec::new();
        self.string_table.encode(&mut meta);
        encode_varint(self.event_count, &mut meta);
        write_length_prefixed(&mut meta, &self.col_timestamps);
        self.callsite_table.encode(&mut meta);
        rle_encode(&self.col_callsite_indices, &mut meta);
        rle_encode(&self.col_field_counts, &mut meta);
        write_length_prefixed(&mut meta, &self.col_field_key_indices);
        rle_encode(&self.col_msg_template_indices, &mut meta);
        let mut content = Vec::new();
        write_length_prefixed(&mut content, &self.col_msg_variables);
        content.extend_from_slice(&self.col_field_values);

        ColumnBreakdown {
            entries,
            event_count: self.event_count,
            string_table_entries: self.string_table.len(),
            meta_uncompressed: meta.len(),
            meta_compressed: zc(&meta),
            content_uncompressed: content.len(),
            content_compressed: zc(&content),
        }
    }
}

/// Diagnostic per-column statistics (test-only).
#[cfg(test)]
pub struct ColumnStat {
    pub name: &'static str,
    pub uncompressed: usize,
    pub compressed_alone: usize,
}

/// Diagnostic breakdown of a segment's columns (test-only).
#[cfg(test)]
pub struct ColumnBreakdown {
    pub entries: Vec<ColumnStat>,
    pub event_count: usize,
    pub string_table_entries: usize,
    pub meta_uncompressed: usize,
    pub meta_compressed: usize,
    pub content_uncompressed: usize,
    pub content_compressed: usize,
}
