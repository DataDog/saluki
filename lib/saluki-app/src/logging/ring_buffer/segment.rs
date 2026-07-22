#![allow(dead_code)]
use std::{collections::VecDeque, io::Read as _};

use saluki_error::{generic_error, GenericError};

use super::callsite_table::{decode_callsite_table, CallsiteEntry, FILE_INDEX_NONE};
use super::codec::{
    decode_level, decode_varint, decode_varint_u128, rle_decode, write_length_prefixed, TIMESTAMP_GRANULARITY_NS,
};
use super::event::DecodedEvent;

/// A compressed segment containing a batch of log events.
#[derive(Clone, Debug)]
pub struct CompressedSegment {
    event_count: usize,
    oldest_timestamp_nanos: u128,
    #[allow(dead_code)]
    uncompressed_size: usize,
    compressed_data: Vec<u8>,
}

impl CompressedSegment {
    /// Creates a new compressed segment.
    pub fn new(
        event_count: usize, oldest_timestamp_nanos: u128, uncompressed_size: usize, compressed_data: Vec<u8>,
    ) -> Self {
        Self {
            event_count,
            oldest_timestamp_nanos,
            uncompressed_size,
            compressed_data,
        }
    }

    /// Returns the size of the compressed data, in bytes.
    pub fn size_bytes(&self) -> usize {
        self.compressed_data.len()
    }

    /// Returns the size of the uncompressed data, in bytes.
    pub fn uncompressed_size_bytes(&self) -> usize {
        self.uncompressed_size
    }

    /// Returns the numbers of events in the segment.
    pub fn event_count(&self) -> usize {
        self.event_count
    }

    /// Returns a reader that can read every event in the segment.
    pub fn events(&self) -> Result<CompressedSegmentReader, GenericError> {
        // Split compressed_data: varint(meta_len) meta_compressed content_compressed.
        let (meta_len, consumed) = decode_varint(&self.compressed_data, 0)
            .ok_or_else(|| generic_error!("Failed to decode meta frame length"))?;
        let meta_start = consumed;
        let content_start = meta_start + meta_len;

        // Decompress metadata frame.
        let mut meta_decompressed = Vec::new();
        let mut decoder = zstd::Decoder::with_buffer(&self.compressed_data[meta_start..content_start])?;
        decoder.read_to_end(&mut meta_decompressed)?;

        // Decompress content frame.
        let mut content_decompressed = Vec::new();
        let mut decoder = zstd::Decoder::with_buffer(&self.compressed_data[content_start..])?;
        decoder.read_to_end(&mut content_decompressed)?;

        CompressedSegmentReader::new(meta_decompressed, content_decompressed)
    }
}

/// Columnar compressed segment reader.
///
/// Decodes columns from two separate buffers (metadata + content), then yields events one at a time by reading across
/// the decoded columns.
pub struct CompressedSegmentReader {
    // Metadata buffer: string table, timestamps, levels, targets, files, lines.
    meta: Vec<u8>,
    string_table_ranges: Vec<(usize, usize)>,

    // Content buffer: messages, fields.
    content: Vec<u8>,

    event_count: usize,
    event_idx: usize,

    // Cursors into `meta`.
    ts_cursor: usize,
    last_timestamp_units: u128,
    // Callsite table (target/file/line/level) and per-event indices into it.
    callsites: Vec<CallsiteEntry>,
    callsite_indices: Vec<usize>,

    // Field sub-columns (from meta).
    field_counts: Vec<usize>,
    field_keys_cursor: usize,
    // Message template sub-columns (from meta).
    msg_template_indices: Vec<usize>,

    // Cursors into `content`.
    msg_vars_cursor: usize,
    field_values_cursor: usize,
}

impl CompressedSegmentReader {
    pub fn new(meta: Vec<u8>, content: Vec<u8>) -> Result<Self, GenericError> {
        let mut reader = Self {
            meta,
            string_table_ranges: Vec::new(),
            content,
            event_count: 0,
            event_idx: 0,
            ts_cursor: 0,
            last_timestamp_units: 0,
            callsites: Vec::new(),
            callsite_indices: Vec::new(),
            field_counts: Vec::new(),
            field_keys_cursor: 0,
            msg_template_indices: Vec::new(),
            msg_vars_cursor: 0,
            field_values_cursor: 0,
        };
        reader.decode_header()?;
        Ok(reader)
    }

    fn decode_header(&mut self) -> Result<(), GenericError> {
        let mut idx = 0;

        // String table (in meta).
        let (count, consumed) =
            decode_varint(&self.meta, idx).ok_or_else(|| generic_error!("Failed to decode string table count"))?;
        idx += consumed;
        self.string_table_ranges.reserve(count);
        for _ in 0..count {
            let (len, consumed) = decode_varint(&self.meta, idx)
                .ok_or_else(|| generic_error!("Failed to decode string table entry length"))?;
            idx += consumed;
            if idx + len > self.meta.len() {
                return Err(generic_error!("String table entry exceeds buffer"));
            }
            self.string_table_ranges.push((idx, idx + len));
            idx += len;
        }

        // Event count.
        let (event_count, consumed) =
            decode_varint(&self.meta, idx).ok_or_else(|| generic_error!("Failed to decode event count"))?;
        idx += consumed;
        self.event_count = event_count;

        // Timestamps column.
        let (ts_len, consumed) = decode_varint(&self.meta, idx)
            .ok_or_else(|| generic_error!("Failed to decode timestamps column length"))?;
        idx += consumed;
        self.ts_cursor = idx;
        idx += ts_len;

        // Callsite table (target/file/line/level, interned once per segment).
        self.callsites = decode_callsite_table(&self.meta, &mut idx)?;

        // Callsite indices column (RLE).
        self.callsite_indices = rle_decode(&self.meta, &mut idx)
            .ok_or_else(|| generic_error!("Failed to decode callsite indices column"))?;

        // Field counts (RLE, in meta).
        self.field_counts =
            rle_decode(&self.meta, &mut idx).ok_or_else(|| generic_error!("Failed to decode field counts column"))?;

        // Field key indices (length-prefixed blob, in meta).
        let (fk_len, consumed) = decode_varint(&self.meta, idx)
            .ok_or_else(|| generic_error!("Failed to decode field key indices column length"))?;
        idx += consumed;
        self.field_keys_cursor = idx;
        idx += fk_len;

        // Message template indices (RLE, in meta).
        self.msg_template_indices = rle_decode(&self.meta, &mut idx)
            .ok_or_else(|| generic_error!("Failed to decode message template indices column"))?;

        // Content buffer: message variables (length-prefixed blob) then field values (rest of buffer).
        let mut cidx = 0;
        let (mv_len, consumed) = decode_varint(&self.content, cidx)
            .ok_or_else(|| generic_error!("Failed to decode message variables column length"))?;
        cidx += consumed;
        self.msg_vars_cursor = cidx;
        cidx += mv_len;
        self.field_values_cursor = cidx;

        Ok(())
    }

    pub fn next(&mut self) -> Result<Option<DecodedEvent<'_>>, GenericError> {
        if self.event_idx >= self.event_count {
            return Ok(None);
        }
        let i = self.event_idx;
        self.event_idx += 1;

        // Timestamp (from meta): delta is in granularity units; reconstruct the absolute unit count
        // then scale back to nanoseconds.
        let (delta_units, consumed) = decode_varint_u128(&self.meta, self.ts_cursor)
            .ok_or_else(|| generic_error!("Failed to decode timestamp delta for event {}", i))?;
        self.ts_cursor += consumed;
        let ts_units = self.last_timestamp_units + delta_units;
        self.last_timestamp_units = ts_units;
        let timestamp_nanos = ts_units * TIMESTAMP_GRANULARITY_NS;

        // Callsite: a single index resolves target, file, line, and level (all interned per segment
        // in the callsite table). Copy the small Copy fields out so the table borrow does not outlive
        // the per-event cursor mutations further down.
        let callsite_idx = self.callsite_indices[i];
        let (target_idx, file_idx, line, level_byte) = {
            let cs = self
                .callsites
                .get(callsite_idx)
                .ok_or_else(|| generic_error!("Callsite index {} out of range", callsite_idx))?;
            (cs.target_idx, cs.file_idx, cs.line, cs.level)
        };

        let level = decode_level(level_byte);

        // Target (string table in meta).
        let &(t_start, t_end) = self
            .string_table_ranges
            .get(target_idx)
            .ok_or_else(|| generic_error!("Target string table index {} out of range", target_idx))?;
        let target = simdutf8::basic::from_utf8(&self.meta[t_start..t_end])
            .map_err(|e| generic_error!("Invalid UTF-8 in target: {}", e))?;

        // Message: reconstruct from template (meta) + variables (content).
        let tpl_idx = self.msg_template_indices[i];
        let &(tpl_start, tpl_end) = self
            .string_table_ranges
            .get(tpl_idx)
            .ok_or_else(|| generic_error!("Message template index {} out of range", tpl_idx))?;
        let template = simdutf8::basic::from_utf8(&self.meta[tpl_start..tpl_end])
            .map_err(|e| generic_error!("Invalid UTF-8 in message template: {}", e))?;

        // The number of variables is not stored per-event: it equals the number of `\0` placeholders
        // in the template skeleton.
        let var_count = template.bytes().filter(|&b| b == 0).count();

        let message = if var_count == 0 {
            template.to_string()
        } else {
            // Collect variable tokens (length-prefixed inline) from the content frame.
            let mut vars = Vec::with_capacity(var_count);
            for _ in 0..var_count {
                let (vlen, consumed) = decode_varint(&self.content, self.msg_vars_cursor)
                    .ok_or_else(|| generic_error!("Failed to decode message variable length"))?;
                self.msg_vars_cursor += consumed;
                let v = simdutf8::basic::from_utf8(&self.content[self.msg_vars_cursor..self.msg_vars_cursor + vlen])
                    .map_err(|e| generic_error!("Invalid UTF-8 in message variable: {}", e))?;
                self.msg_vars_cursor += vlen;
                vars.push(v);
            }
            // Reconstruct: replace each \x00 placeholder with the next variable.
            let mut result = String::with_capacity(template.len() + 32);
            let mut var_idx = 0;
            for part in template.split('\x00') {
                result.push_str(part);
                if var_idx < vars.len() {
                    result.push_str(vars[var_idx]);
                    var_idx += 1;
                }
            }
            result
        };

        // Fields: count from pre-decoded RLE, keys from meta, values from content.
        let field_count = self.field_counts[i];
        let mut fields = Vec::new();
        for _ in 0..field_count {
            // Key index from meta.
            let (key_idx, consumed) = decode_varint(&self.meta, self.field_keys_cursor)
                .ok_or_else(|| generic_error!("Failed to decode field key index"))?;
            self.field_keys_cursor += consumed;
            let &(k_start, k_end) = self
                .string_table_ranges
                .get(key_idx)
                .ok_or_else(|| generic_error!("Field key string table index {} out of range", key_idx))?;
            write_length_prefixed(&mut fields, &self.meta[k_start..k_end]);

            // Value from content (length-prefixed inline).
            let (val_len, consumed) = decode_varint(&self.content, self.field_values_cursor)
                .ok_or_else(|| generic_error!("Failed to decode field value length"))?;
            self.field_values_cursor += consumed;
            write_length_prefixed(
                &mut fields,
                &self.content[self.field_values_cursor..self.field_values_cursor + val_len],
            );
            self.field_values_cursor += val_len;
        }

        // File (string table in meta); line comes directly from the callsite entry resolved above.
        let file = if file_idx == FILE_INDEX_NONE {
            None
        } else {
            let &(f_start, f_end) = self
                .string_table_ranges
                .get(file_idx)
                .ok_or_else(|| generic_error!("File string table index {} out of range", file_idx))?;
            Some(
                simdutf8::basic::from_utf8(&self.meta[f_start..f_end])
                    .map_err(|e| generic_error!("Invalid UTF-8 in file: {}", e))?,
            )
        };

        Ok(Some(DecodedEvent {
            timestamp_nanos,
            level,
            target,
            message,
            fields,
            file,
            line,
        }))
    }
}

/// A collection of compressed segments.
#[derive(Default)]
pub struct CompressedSegments {
    segments: VecDeque<CompressedSegment>,
    total_compressed_size_bytes: usize,
    event_count: usize,
    segments_dropped_total: u64,
}

impl CompressedSegments {
    /// Returns the total size of all compressed segments, in bytes.
    pub fn size_bytes(&self) -> usize {
        self.total_compressed_size_bytes
    }

    /// Returns the total number of events across all segments.
    pub fn event_count(&self) -> usize {
        self.event_count
    }

    /// Returns the total number of segments in the collection.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Returns the total number of segments dropped from the collection.
    pub fn segments_dropped_total(&self) -> u64 {
        self.segments_dropped_total
    }

    /// Returns the timestamp of the oldest event across all segments, in nanoseconds.
    pub fn oldest_timestamp_nanos(&self) -> u128 {
        self.segments.front().map_or(0, |s| s.oldest_timestamp_nanos)
    }

    /// Adds a new compressed segment to the collection.
    pub fn add_segment(&mut self, segment: CompressedSegment) {
        self.total_compressed_size_bytes += segment.size_bytes();
        self.event_count += segment.event_count();
        self.segments.push_back(segment);
    }

    pub fn iter_segments(&self) -> impl Iterator<Item = &CompressedSegment> {
        self.segments.iter()
    }

    /// Drops the oldest segment in the collection, if any.
    pub fn drop_oldest_segment(&mut self) {
        if let Some(segment) = self.segments.pop_front() {
            self.total_compressed_size_bytes -= segment.size_bytes();
            self.event_count -= segment.event_count();
            self.segments_dropped_total += 1;
        }
    }
}
