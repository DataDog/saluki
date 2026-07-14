use std::io::Write;

use saluki_error::{ErrorContext as _, GenericError};

use super::codec::{encode_level, encode_varint, encode_varint_u128, rle_encode, write_length_prefixed};
use super::event::{CondensedEvent, FieldIter};
use super::pattern_cluster::ClusterManager;
use super::segment::CompressedSegment;
use super::string_table::StringTable;

pub const FILE_INDEX_NONE: usize = usize::MAX;

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
/// [col: timestamps]               -- length-prefixed blob of varint-delta-encoded nanosecond deltas
/// [col: levels]                   -- RLE: varint(num_runs) [varint(run_len) varint(level_u8) ...]
/// [col: target_indices]           -- RLE of string table indices
/// [col: file_indices]             -- RLE of string table indices (usize::MAX = None)
/// [col: lines]                    -- length-prefixed blob of varints (0 = None, line+1 = Some)
/// [col: field_counts]             -- RLE of per-event field counts
/// [col: field_key_indices]        -- length-prefixed blob of varint string table indices
/// [col: msg_template_indices]     -- RLE of string table indices for message skeletons
/// ```
///
/// ## Content payload layout
///
/// ```text
/// [col: msg_variables]            -- length-prefixed blob: per-event varint(var_count) [varint(len) bytes ...]
/// [col: field_values]             -- concatenated varint-length-prefixed value byte strings
/// ```
pub struct EventBuffer {
    // Column accumulators.
    col_timestamps: Vec<u8>,
    col_levels: Vec<usize>,
    col_target_indices: Vec<usize>,
    col_msg_template_indices: Vec<usize>,
    col_msg_variables: Vec<u8>,
    cluster_manager: ClusterManager,
    col_field_counts: Vec<usize>,
    col_field_key_indices: Vec<u8>,
    col_field_values: Vec<u8>,
    col_file_indices: Vec<usize>,
    col_lines: Vec<u8>,

    event_count: usize,
    compression_level: i32,
    oldest_timestamp_nanos: u128,
    last_timestamp: u128,
    string_table: StringTable,
}

impl EventBuffer {
    pub fn from_compression_level(compression_level: i32) -> Self {
        EventBuffer {
            col_timestamps: Vec::new(),
            col_levels: Vec::new(),
            col_target_indices: Vec::new(),
            col_msg_template_indices: Vec::new(),
            col_msg_variables: Vec::new(),
            cluster_manager: ClusterManager::default(),
            col_field_counts: Vec::new(),
            col_field_key_indices: Vec::new(),
            col_field_values: Vec::new(),
            col_file_indices: Vec::new(),
            col_lines: Vec::new(),

            event_count: 0,
            compression_level,
            oldest_timestamp_nanos: 0,
            last_timestamp: 0,
            string_table: StringTable::default(),
        }
    }

    pub fn size_bytes(&self) -> usize {
        // Estimate the serialized payload size (pre-compression). The byte-based columns
        // (timestamps, messages, fields, lines) are already in their serialized form.
        // RLE-encoded columns (levels, targets, files) compress dramatically -- the serialized
        // RLE is typically a small fraction of the element count. We estimate ~1 byte per
        // element as a rough upper bound, since most runs are longer than 1.
        self.col_timestamps.len()
            + self.col_msg_variables.len()
            + self.col_field_key_indices.len()
            + self.col_field_values.len()
            + self.col_lines.len()
            + self.col_levels.len()
            + self.col_target_indices.len()
            + self.col_file_indices.len()
    }

    pub fn event_count(&self) -> usize {
        self.event_count
    }

    pub fn oldest_timestamp_nanos(&self) -> u128 {
        self.oldest_timestamp_nanos
    }

    fn clear(&mut self) {
        self.col_timestamps.clear();
        self.col_levels.clear();
        self.col_target_indices.clear();
        self.col_msg_template_indices.clear();
        self.col_msg_variables.clear();
        self.col_field_counts.clear();
        self.col_field_key_indices.clear();
        self.col_field_values.clear();
        self.col_file_indices.clear();
        self.col_lines.clear();
        self.event_count = 0;
        self.oldest_timestamp_nanos = 0;
        self.last_timestamp = 0;
        self.string_table.clear();
    }

    pub fn encode_event(&mut self, event: &CondensedEvent) -> Result<(), GenericError> {
        let meta = event
            .metadata
            .expect("metadata must be set on events passed to encode_event");

        if self.oldest_timestamp_nanos == 0 {
            self.oldest_timestamp_nanos = event.timestamp_nanos;
        }

        // Timestamps: delta-encoded varints.
        let ts_delta = event.timestamp_nanos.saturating_sub(self.last_timestamp);
        self.last_timestamp = event.timestamp_nanos;
        encode_varint_u128(ts_delta, &mut self.col_timestamps);

        // Level: stored as usize for RLE, encoded on flush.
        self.col_levels.push(encode_level(meta.level().as_str()) as usize);

        // Target: interned index, stored for RLE.
        let target_idx = self.string_table.intern(meta.target());
        self.col_target_indices.push(target_idx);

        // Message: extract template skeleton + variable tokens via Drain-inspired clustering.
        let (skeleton, var_count) = self.cluster_manager.extract(&event.message, meta);
        let tpl_idx = self.string_table.intern(skeleton);
        self.col_msg_template_indices.push(tpl_idx);
        // Write variable tokens: varint(count) [varint-len-prefixed token ...].
        encode_varint(var_count, &mut self.col_msg_variables);
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

        // File: interned index or sentinel, stored for RLE.
        match meta.file() {
            Some(file) => self.col_file_indices.push(self.string_table.intern(file)),
            None => self.col_file_indices.push(FILE_INDEX_NONE),
        }

        // Line: varint with 0 = None, line+1 = Some(line).
        match meta.line() {
            Some(line) => encode_varint(line as usize + 1, &mut self.col_lines),
            None => encode_varint(0, &mut self.col_lines),
        }

        self.event_count += 1;

        Ok(())
    }

    pub fn flush(&mut self) -> Result<CompressedSegment, GenericError> {
        // Build two separate payloads for split compression:
        //   1. Metadata: string table, event count, timestamps, levels, targets, files, lines
        //   2. Content: messages, fields (high-entropy text data)
        // This lets zstd build optimal coding tables for each data distribution.

        // --- Metadata payload ---
        let mut meta = Vec::with_capacity(4096);
        self.string_table.encode(&mut meta);
        encode_varint(self.event_count, &mut meta);
        write_length_prefixed(&mut meta, &self.col_timestamps);
        rle_encode(&self.col_levels, &mut meta);
        rle_encode(&self.col_target_indices, &mut meta);
        rle_encode(&self.col_file_indices, &mut meta);
        write_length_prefixed(&mut meta, &self.col_lines);
        // Field counts (RLE) and key indices go in meta (low-entropy).
        rle_encode(&self.col_field_counts, &mut meta);
        write_length_prefixed(&mut meta, &self.col_field_key_indices);
        // Message template indices (RLE, low-entropy).
        rle_encode(&self.col_msg_template_indices, &mut meta);

        // --- Content payload ---
        let mut content = Vec::with_capacity(self.col_msg_variables.len() + self.col_field_values.len() + 16);
        // Message variables.
        write_length_prefixed(&mut content, &self.col_msg_variables);
        // Field values.
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
}
