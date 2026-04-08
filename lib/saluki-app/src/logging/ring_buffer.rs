use std::{
    collections::VecDeque,
    fmt::Write as _,
    io::{Read as _, Write},
    sync::atomic::{AtomicBool, Ordering::AcqRel},
    time::{Duration, Instant},
};

use metrics::gauge;
use saluki_common::time::get_unix_timestamp_nanos;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use thingbuf::mpsc;
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};

const DEFAULT_MIN_UNCOMPRESSED_SEGMENT_SIZE_BYTES: usize = 128 * 1024;
const DEFAULT_COMPRESSION_LEVEL: i32 = 19;
const DEFAULT_MAX_RING_BUFFER_SIZE_BYTES: usize = 1024 * 1024;

/// Ring buffer configuration.
#[derive(Clone, Debug)]
pub struct RingBufferConfig {
    /// Minimum size of an uncompressed segment, in bytes, before compression is allowed.
    ///
    /// Compression is only triggered when the total ring buffer size exceeds the maximum ring buffer size _and_ the
    /// event buffer has reached at least this size.
    min_uncompressed_segment_size_bytes: usize,

    /// Compression level.
    ///
    /// Higher values provide better compression but slower performance. Valid range is 1-22.
    compression_level: i32,

    /// Maximum size of the ring buffer, in bytes.
    max_ring_buffer_size_bytes: usize,
}

impl RingBufferConfig {
    /// Sets the minimum size of an uncompressed segment, in bytes, before compression is allowed.
    ///
    /// Compression is only triggered when the total ring buffer size exceeds the configured maximum _and_ the event
    /// buffer has reached at least this size. This allows segments to grow larger for better compression ratios.
    ///
    /// Defaults to 256KiB.
    pub fn with_min_uncompressed_segment_size_bytes(mut self, min_uncompressed_segment_size_bytes: usize) -> Self {
        self.min_uncompressed_segment_size_bytes = min_uncompressed_segment_size_bytes;
        self
    }

    /// Sets the compression level.
    ///
    /// Higher levels provide better compression at the cost of higher CPU usage.
    ///
    /// Defaults to 3.
    pub fn with_compression_level(mut self, compression_level: i32) -> Self {
        self.compression_level = compression_level;
        self
    }

    /// Sets the maximum size of the ring buffer, in bytes.
    ///
    /// This value controls how much memory the ring buffer can use, and when new log events being added would cause
    /// the buffer to exceed this limit, compressed segments will be dropped (oldest first) to ensure the limit is
    /// not exceeded.
    ///
    /// Defaults to 1MiB.
    pub fn with_max_ring_buffer_size_bytes(mut self, max_ring_buffer_size_bytes: usize) -> Self {
        self.max_ring_buffer_size_bytes = max_ring_buffer_size_bytes;
        self
    }
}

impl Default for RingBufferConfig {
    fn default() -> Self {
        Self {
            min_uncompressed_segment_size_bytes: DEFAULT_MIN_UNCOMPRESSED_SEGMENT_SIZE_BYTES,
            compression_level: DEFAULT_COMPRESSION_LEVEL,
            max_ring_buffer_size_bytes: DEFAULT_MAX_RING_BUFFER_SIZE_BYTES,
        }
    }
}

/// Encodes a `usize` as a variable-length integer (LEB128) into `buf`.
fn encode_varint(mut value: usize, buf: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Encodes a `u128` as a variable-length integer (LEB128) into `buf`.
fn encode_varint_u128(mut value: u128, buf: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Decodes a variable-length integer (LEB128) from `buf` starting at `idx`.
///
/// Returns `(value, bytes_consumed)`, or `None` if the buffer is too short or the varint is
/// malformed.
fn decode_varint(buf: &[u8], mut idx: usize) -> Option<(usize, usize)> {
    let start = idx;
    let mut value: usize = 0;
    let mut shift = 0;
    loop {
        if idx >= buf.len() {
            return None;
        }
        let byte = buf[idx];
        idx += 1;
        value |= ((byte & 0x7F) as usize) << shift;
        if byte & 0x80 == 0 {
            return Some((value, idx - start));
        }
        shift += 7;
        if shift >= usize::BITS {
            return None;
        }
    }
}

/// Decodes a `u128` variable-length integer (LEB128) from `buf` starting at `idx`.
fn decode_varint_u128(buf: &[u8], mut idx: usize) -> Option<(u128, usize)> {
    let start = idx;
    let mut value: u128 = 0;
    let mut shift = 0;
    loop {
        if idx >= buf.len() {
            return None;
        }
        let byte = buf[idx];
        idx += 1;
        value |= ((byte & 0x7F) as u128) << shift;
        if byte & 0x80 == 0 {
            return Some((value, idx - start));
        }
        shift += 7;
        if shift >= 128 {
            return None;
        }
    }
}

/// Encodes a log level as a single byte.
fn encode_level(level: &str) -> u8 {
    match level {
        "TRACE" => 0,
        "DEBUG" => 1,
        "INFO" => 2,
        "WARN" => 3,
        "ERROR" => 4,
        _ => 5,
    }
}

/// Decodes a log level byte back to a static string.
fn decode_level(byte: u8) -> &'static str {
    match byte {
        0 => "TRACE",
        1 => "DEBUG",
        2 => "INFO",
        3 => "WARN",
        4 => "ERROR",
        _ => "UNKNOWN",
    }
}

/// Writes a varint-length-prefixed byte slice into `buf`.
fn write_length_prefixed(buf: &mut Vec<u8>, data: &[u8]) {
    encode_varint(data.len(), buf);
    buf.extend_from_slice(data);
}

/// A log event captured from the tracing layer, ready for encoding into the ring buffer.
///
/// This is the write-path representation that travels through the thingbuf channel from the writer
/// thread to the processor thread. It stores a reference to the event's static callsite
/// [`Metadata`] rather than copying individual metadata fields, making hydration cheaper (one
/// pointer store vs four field copies) and the struct smaller.
#[derive(Clone, Default)]
struct CondensedEvent {
    /// Unix timestamp when the event was logged, in nanoseconds.
    timestamp_nanos: u128,

    /// Static callsite metadata (level, target, file, line). Set by the hydrator; `None` only in
    /// the default-initialized thingbuf arena slots before first use.
    metadata: Option<&'static Metadata<'static>>,

    /// The main log message.
    message: String,

    /// Additional structured fields, stored as sequential varint-length-prefixed key-value pairs.
    ///
    /// Each field is encoded as: `varint(key_len) key_bytes varint(value_len) value_bytes`.
    fields: Vec<u8>,
}

/// A decoded log event reconstructed from a compressed segment.
///
/// This is the read-path representation returned by [`CompressedSegmentReader::next`]. Since
/// decoded events do not originate from a tracing callsite, they carry individual metadata fields
/// rather than a [`Metadata`] pointer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DecodedEvent<'a> {
    /// Unix timestamp when the event was logged, in nanoseconds.
    pub timestamp_nanos: u128,

    /// Log level (e.g., "INFO", "WARN", "ERROR").
    pub level: &'a str,

    /// Target module or component that generated the event.
    pub target: &'a str,

    /// The main log message.
    pub message: String,

    /// Additional structured fields, stored as sequential varint-length-prefixed key-value pairs.
    ///
    /// Each field is encoded as: `varint(key_len) key_bytes varint(value_len) value_bytes`.
    pub fields: Vec<u8>,

    /// Source file where the event was logged.
    pub file: Option<&'a str>,

    /// Line number where the event was logged.
    pub line: Option<u32>,
}

impl<'a> DecodedEvent<'a> {
    /// Returns the number of bytes that this event occupies in memory.
    #[allow(dead_code)] // Useful for diagnostics; exercised in tests.
    pub fn size_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.message.capacity() + self.fields.capacity()
    }

    /// Returns an iterator over the decoded field key-value pairs.
    #[allow(dead_code)] // Will be used by the read API; exercised in tests.
    pub fn iter_fields(&self) -> FieldIter<'_> {
        FieldIter {
            buf: &self.fields,
            idx: 0,
        }
    }
}

/// Iterator over varint-length-prefixed field key-value pairs.
#[allow(dead_code)] // Will be used by the read API; exercised in tests.
pub struct FieldIter<'a> {
    buf: &'a [u8],
    idx: usize,
}

impl<'a> FieldIter<'a> {
    pub const fn from_buf(buf: &'a [u8]) -> Self {
        Self { buf, idx: 0 }
    }
}

impl<'a> Iterator for FieldIter<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        // Read key.
        let (key_len, consumed) = decode_varint(self.buf, self.idx)?;
        self.idx += consumed;
        if self.idx + key_len > self.buf.len() {
            return None;
        }
        let key = std::str::from_utf8(&self.buf[self.idx..self.idx + key_len]).ok()?;
        self.idx += key_len;

        // Read value.
        let (val_len, consumed) = decode_varint(self.buf, self.idx)?;
        self.idx += consumed;
        if self.idx + val_len > self.buf.len() {
            return None;
        }
        let val = std::str::from_utf8(&self.buf[self.idx..self.idx + val_len]).ok()?;
        self.idx += val_len;

        Some((key, val))
    }
}

/// Wrapper that pairs a [`CondensedEvent`] with a reusable scratch buffer for `Visit` field
/// formatting.
struct EventHydrator<'a> {
    event: &'a mut CondensedEvent,
    value_buf: &'a mut String,
}

impl<'a> EventHydrator<'a> {
    fn hydrate(event: &'a mut CondensedEvent, value_buf: &'a mut String, tracing_event: &Event<'_>) {
        event.message.clear();
        event.fields.clear();

        event.timestamp_nanos = get_unix_timestamp_nanos();
        event.metadata = Some(tracing_event.metadata());

        let mut hydrator = EventHydrator { event, value_buf };
        tracing_event.record(&mut hydrator);
    }
}

impl tracing::field::Visit for EventHydrator<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.event.message, "{:?}", value);
        } else {
            write_length_prefixed(&mut self.event.fields, field.name().as_bytes());

            self.value_buf.clear();
            let _ = write!(self.value_buf, "{:?}", value);
            write_length_prefixed(&mut self.event.fields, self.value_buf.as_bytes());
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.event.message.push_str(value);
        } else {
            write_length_prefixed(&mut self.event.fields, field.name().as_bytes());
            write_length_prefixed(&mut self.event.fields, value.as_bytes());
        }
    }
}

/// A string intern table that maps strings to compact varint indices.
///
/// Used during encoding to replace repeated target/file strings with small integer indices,
/// and during decoding to look up the original strings.
struct StringTable {
    strings: Vec<String>,
    index: std::collections::HashMap<String, usize>,
}

impl StringTable {
    fn new() -> Self {
        Self {
            strings: Vec::new(),
            index: std::collections::HashMap::new(),
        }
    }

    /// Interns a string and returns its index.
    fn intern(&mut self, s: &str) -> usize {
        if let Some(&idx) = self.index.get(s) {
            return idx;
        }
        let idx = self.strings.len();
        self.strings.push(s.to_string());
        self.index.insert(s.to_string(), idx);
        idx
    }

    /// Serializes the string table as: varint(count) then for each entry varint(len) bytes.
    fn encode(&self, buf: &mut Vec<u8>) {
        encode_varint(self.strings.len(), buf);
        for s in &self.strings {
            write_length_prefixed(buf, s.as_bytes());
        }
    }

    fn clear(&mut self) {
        self.strings.clear();
        self.index.clear();
    }
}

/// RLE-encodes a column of small integers: varint(num_runs) [varint(run_len) varint(value) ...].
fn rle_encode(values: &[usize], buf: &mut Vec<u8>) {
    if values.is_empty() {
        encode_varint(0, buf);
        return;
    }

    // Count runs first.
    let mut runs: Vec<(usize, usize)> = Vec::new(); // (count, value)
    let mut cur_val = values[0];
    let mut cur_count = 1;
    for &v in &values[1..] {
        if v == cur_val {
            cur_count += 1;
        } else {
            runs.push((cur_count, cur_val));
            cur_val = v;
            cur_count = 1;
        }
    }
    runs.push((cur_count, cur_val));

    encode_varint(runs.len(), buf);
    for (count, value) in runs {
        encode_varint(count, buf);
        encode_varint(value, buf);
    }
}

/// Decodes an RLE-encoded column into a Vec of values.
fn rle_decode(buf: &[u8], idx: &mut usize) -> Option<Vec<usize>> {
    let (num_runs, consumed) = decode_varint(buf, *idx)?;
    *idx += consumed;

    let mut values = Vec::new();
    for _ in 0..num_runs {
        let (count, consumed) = decode_varint(buf, *idx)?;
        *idx += consumed;
        let (value, consumed) = decode_varint(buf, *idx)?;
        *idx += consumed;
        values.extend(std::iter::repeat_n(value, count));
    }
    Some(values)
}

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
struct EventBuffer {
    // Column accumulators.
    col_timestamps: Vec<u8>,
    col_levels: Vec<usize>,
    col_target_indices: Vec<usize>,
    col_msg_template_indices: Vec<usize>,
    col_msg_variables: Vec<u8>,
    msg_skeleton_buf: String,
    col_field_counts: Vec<usize>,
    col_field_key_indices: Vec<u8>,
    col_field_values: Vec<u8>,
    col_file_indices: Vec<usize>,
    col_lines: Vec<u8>,

    event_count: usize,
    compression_level: i32,
    last_timestamp: u128,
    string_table: StringTable,
}

const FILE_INDEX_NONE: usize = usize::MAX;

impl EventBuffer {
    fn from_compression_level(compression_level: i32) -> Self {
        EventBuffer {
            col_timestamps: Vec::new(),
            col_levels: Vec::new(),
            col_target_indices: Vec::new(),
            col_msg_template_indices: Vec::new(),
            col_msg_variables: Vec::new(),
            msg_skeleton_buf: String::new(),
            col_field_counts: Vec::new(),
            col_field_key_indices: Vec::new(),
            col_field_values: Vec::new(),
            col_file_indices: Vec::new(),
            col_lines: Vec::new(),
            event_count: 0,
            compression_level,
            last_timestamp: 0,
            string_table: StringTable::new(),
        }
    }

    fn size_bytes(&self) -> usize {
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

    fn event_count(&self) -> usize {
        self.event_count
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
        self.last_timestamp = 0;
        self.string_table.clear();
    }

    fn encode_event(&mut self, event: &CondensedEvent) -> Result<(), GenericError> {
        let meta = event
            .metadata
            .expect("metadata must be set on events passed to encode_event");

        // Timestamps: delta-encoded varints.
        let ts_delta = event.timestamp_nanos.saturating_sub(self.last_timestamp);
        self.last_timestamp = event.timestamp_nanos;
        encode_varint_u128(ts_delta, &mut self.col_timestamps);

        // Level: stored as usize for RLE, encoded on flush.
        self.col_levels.push(encode_level(meta.level().as_str()) as usize);

        // Target: interned index, stored for RLE.
        let target_idx = self.string_table.intern(meta.target());
        self.col_target_indices.push(target_idx);

        // Message: extract template skeleton + variable tokens.
        // Tokens containing digits are "variables"; everything else is "static".
        self.msg_skeleton_buf.clear();
        let mut var_count: usize = 0;
        // We need to collect variables first, then write them, since we need the count upfront.
        // Use a simple two-pass: first build skeleton and count, then write variables.
        let mut first = true;
        for token in event.message.split_ascii_whitespace() {
            if !first {
                self.msg_skeleton_buf.push(' ');
            }
            first = false;
            if token.bytes().any(|b| b.is_ascii_digit()) {
                self.msg_skeleton_buf.push('\x00'); // placeholder for variable
                var_count += 1;
            } else {
                self.msg_skeleton_buf.push_str(token);
            }
        }
        let tpl_idx = self.string_table.intern(&self.msg_skeleton_buf);
        self.col_msg_template_indices.push(tpl_idx);
        // Write variable tokens: varint(count) [varint-len-prefixed token ...].
        encode_varint(var_count, &mut self.col_msg_variables);
        if var_count > 0 {
            for token in event.message.split_ascii_whitespace() {
                if token.bytes().any(|b| b.is_ascii_digit()) {
                    write_length_prefixed(&mut self.col_msg_variables, token.as_bytes());
                }
            }
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

    fn flush(&mut self) -> Result<CompressedSegment, GenericError> {
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

        let compressed_segment = CompressedSegment {
            event_count: self.event_count,
            uncompressed_size,
            compressed_data,
        };

        self.clear();

        Ok(compressed_segment)
    }
}

/// A compressed segment containing a batch of log events.
#[derive(Clone, Debug)]
struct CompressedSegment {
    event_count: usize,
    #[allow(dead_code)] // Used by events() for decompression pre-allocation.
    uncompressed_size: usize,
    compressed_data: Vec<u8>,
}

impl CompressedSegment {
    fn size_bytes(&self) -> usize {
        self.compressed_data.len()
    }

    fn event_count(&self) -> usize {
        self.event_count
    }

    /// Returns a reader that can read every event in the segment.
    #[allow(dead_code)] // Will be used by the read API; exercised in tests.
    fn events(&self) -> Result<CompressedSegmentReader, GenericError> {
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
/// Decodes columns from two separate buffers (metadata + content), then yields events one at a
/// time by reading across the decoded columns.
#[allow(dead_code)] // Will be used by the read API; exercised in tests.
struct CompressedSegmentReader {
    // Metadata buffer: string table, timestamps, levels, targets, files, lines.
    meta: Vec<u8>,
    string_table_ranges: Vec<(usize, usize)>,

    // Content buffer: messages, fields.
    content: Vec<u8>,

    event_count: usize,
    event_idx: usize,

    // Cursors into `meta`.
    ts_cursor: usize,
    last_timestamp: u128,
    levels: Vec<usize>,
    target_indices: Vec<usize>,
    file_indices: Vec<usize>,
    line_cursor: usize,

    // Field sub-columns (from meta).
    field_counts: Vec<usize>,
    field_keys_cursor: usize,
    // Message template sub-columns (from meta).
    msg_template_indices: Vec<usize>,

    // Cursors into `content`.
    msg_vars_cursor: usize,
    field_values_cursor: usize,
}

#[allow(dead_code)] // Will be used by the read API; exercised in tests.
impl CompressedSegmentReader {
    fn new(meta: Vec<u8>, content: Vec<u8>) -> Result<Self, GenericError> {
        let mut reader = Self {
            meta,
            string_table_ranges: Vec::new(),
            content,
            event_count: 0,
            event_idx: 0,
            ts_cursor: 0,
            last_timestamp: 0,
            levels: Vec::new(),
            target_indices: Vec::new(),
            file_indices: Vec::new(),
            line_cursor: 0,
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

        // Levels column (RLE).
        self.levels =
            rle_decode(&self.meta, &mut idx).ok_or_else(|| generic_error!("Failed to decode levels column"))?;

        // Target indices column (RLE).
        self.target_indices =
            rle_decode(&self.meta, &mut idx).ok_or_else(|| generic_error!("Failed to decode target indices column"))?;

        // File indices column (RLE).
        self.file_indices =
            rle_decode(&self.meta, &mut idx).ok_or_else(|| generic_error!("Failed to decode file indices column"))?;

        // Lines column.
        let (lines_len, consumed) =
            decode_varint(&self.meta, idx).ok_or_else(|| generic_error!("Failed to decode lines column length"))?;
        idx += consumed;
        self.line_cursor = idx;
        let _ = lines_len; // cursor advances during iteration
        idx += lines_len;

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

    fn next(&mut self) -> Result<Option<DecodedEvent<'_>>, GenericError> {
        if self.event_idx >= self.event_count {
            return Ok(None);
        }
        let i = self.event_idx;
        self.event_idx += 1;

        // Timestamp (from meta).
        let (ts_delta, consumed) = decode_varint_u128(&self.meta, self.ts_cursor)
            .ok_or_else(|| generic_error!("Failed to decode timestamp delta for event {}", i))?;
        self.ts_cursor += consumed;
        let timestamp_nanos = self.last_timestamp + ts_delta;
        self.last_timestamp = timestamp_nanos;

        // Level (pre-decoded RLE).
        let level = decode_level(self.levels[i] as u8);

        // Target (pre-decoded RLE → string table in meta).
        let target_idx = self.target_indices[i];
        let &(t_start, t_end) = self
            .string_table_ranges
            .get(target_idx)
            .ok_or_else(|| generic_error!("Target string table index {} out of range", target_idx))?;
        let target = std::str::from_utf8(&self.meta[t_start..t_end])
            .map_err(|e| generic_error!("Invalid UTF-8 in target: {}", e))?;

        // Message: reconstruct from template (meta) + variables (content).
        let tpl_idx = self.msg_template_indices[i];
        let &(tpl_start, tpl_end) = self
            .string_table_ranges
            .get(tpl_idx)
            .ok_or_else(|| generic_error!("Message template index {} out of range", tpl_idx))?;
        let template = std::str::from_utf8(&self.meta[tpl_start..tpl_end])
            .map_err(|e| generic_error!("Invalid UTF-8 in message template: {}", e))?;

        // Read variable count and tokens.
        let (var_count, consumed) = decode_varint(&self.content, self.msg_vars_cursor)
            .ok_or_else(|| generic_error!("Failed to decode message variable count for event {}", i))?;
        self.msg_vars_cursor += consumed;

        let message = if var_count == 0 {
            template.to_string()
        } else {
            // Collect variable tokens.
            let mut vars = Vec::with_capacity(var_count);
            for _ in 0..var_count {
                let (vlen, consumed) = decode_varint(&self.content, self.msg_vars_cursor)
                    .ok_or_else(|| generic_error!("Failed to decode message variable"))?;
                self.msg_vars_cursor += consumed;
                let v = std::str::from_utf8(&self.content[self.msg_vars_cursor..self.msg_vars_cursor + vlen])
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

            // Value from content.
            let (val_len, consumed) = decode_varint(&self.content, self.field_values_cursor)
                .ok_or_else(|| generic_error!("Failed to decode field value length"))?;
            self.field_values_cursor += consumed;
            write_length_prefixed(
                &mut fields,
                &self.content[self.field_values_cursor..self.field_values_cursor + val_len],
            );
            self.field_values_cursor += val_len;
        }

        // File (pre-decoded RLE → string table in meta).
        let file_idx = self.file_indices[i];
        let file = if file_idx == FILE_INDEX_NONE {
            None
        } else {
            let &(f_start, f_end) = self
                .string_table_ranges
                .get(file_idx)
                .ok_or_else(|| generic_error!("File string table index {} out of range", file_idx))?;
            Some(
                std::str::from_utf8(&self.meta[f_start..f_end])
                    .map_err(|e| generic_error!("Invalid UTF-8 in file: {}", e))?,
            )
        };

        // Line (from meta).
        let (line_enc, consumed) = decode_varint(&self.meta, self.line_cursor)
            .ok_or_else(|| generic_error!("Failed to decode line for event {}", i))?;
        self.line_cursor += consumed;
        let line = if line_enc == 0 {
            None
        } else {
            Some((line_enc - 1) as u32)
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

struct CompressedSegments {
    segments: VecDeque<CompressedSegment>,
    total_compressed_size_bytes: usize,
    event_count: usize,
    segments_dropped_total: u64,
}

impl CompressedSegments {
    fn new() -> Self {
        CompressedSegments {
            segments: VecDeque::new(),
            total_compressed_size_bytes: 0,
            event_count: 0,
            segments_dropped_total: 0,
        }
    }

    fn size_bytes(&self) -> usize {
        self.total_compressed_size_bytes
    }

    fn event_count(&self) -> usize {
        self.event_count
    }

    fn add_segment(&mut self, segment: CompressedSegment) {
        self.total_compressed_size_bytes += segment.size_bytes();
        self.event_count += segment.event_count();
        self.segments.push_back(segment);
    }

    fn drop_oldest_segment(&mut self) {
        if let Some(segment) = self.segments.pop_front() {
            self.total_compressed_size_bytes -= segment.size_bytes();
            self.event_count -= segment.event_count();
            self.segments_dropped_total += 1;
        }
    }
}

const METRICS_PUBLISH_INTERVAL: Duration = Duration::from_secs(1);

struct Metrics {
    // Locally tracked values.
    events_total: u64,
    events_live: u64,
    segments_live: u64,
    segments_dropped_total: u64,
    compressed_bytes_live: u64,
    bytes_live: u64,

    last_publish: Instant,
}

impl Metrics {
    fn new() -> Self {
        Self {
            events_total: 0,
            events_live: 0,
            segments_live: 0,
            segments_dropped_total: 0,
            compressed_bytes_live: 0,
            bytes_live: 0,
            last_publish: Instant::now(),
        }
    }

    fn publish(&mut self) {
        gauge!("crb_events_total").set(self.events_total as f64);
        gauge!("crb_events_live").set(self.events_live as f64);
        gauge!("crb_segments_live").set(self.segments_live as f64);
        gauge!("crb_segments_dropped_total").set(self.segments_dropped_total as f64);
        gauge!("crb_compressed_bytes_live").set(self.compressed_bytes_live as f64);
        gauge!("crb_bytes_live").set(self.bytes_live as f64);
        self.last_publish = Instant::now();
    }

    fn publish_if_elapsed(&mut self) {
        if self.last_publish.elapsed() >= METRICS_PUBLISH_INTERVAL {
            self.publish();
        }
    }
}

/// Internal state for the processor.
struct ProcessorState {
    config: RingBufferConfig,
    compressed_segments: CompressedSegments,
    event_buffer: EventBuffer,
    metrics: Metrics,
}

impl ProcessorState {
    fn new(config: RingBufferConfig) -> Self {
        let event_buffer = EventBuffer::from_compression_level(config.compression_level);
        Self {
            config,
            compressed_segments: CompressedSegments::new(),
            event_buffer,
            metrics: Metrics::new(),
        }
    }

    fn total_size_bytes(&self) -> usize {
        self.compressed_segments.size_bytes() + self.event_buffer.size_bytes()
    }

    fn add_event(&mut self, event: &CondensedEvent) -> Result<(), GenericError> {
        // Encode the event into our event buffer.
        self.event_buffer.encode_event(event)?;
        self.metrics.events_total += 1;

        // If the total ring buffer size exceeds the maximum and the event buffer has reached the minimum segment
        // size, flush and compress it. This lets segments grow as large as possible for better compression ratios,
        // only compressing when we're actually under memory pressure.
        if self.total_size_bytes() > self.config.max_ring_buffer_size_bytes
            && self.event_buffer.size_bytes() >= self.config.min_uncompressed_segment_size_bytes
        {
            let compressed_segment = self.event_buffer.flush()?;
            self.compressed_segments.add_segment(compressed_segment);
        }

        // Ensure that we're under our configured size limits by potentially dropping old segments.
        self.ensure_size_limits()?;

        self.metrics.events_live = (self.compressed_segments.event_count() + self.event_buffer.event_count()) as u64;
        self.metrics.segments_live = self.compressed_segments.segments.len() as u64;
        self.metrics.segments_dropped_total = self.compressed_segments.segments_dropped_total;
        self.metrics.compressed_bytes_live = self.compressed_segments.size_bytes() as u64;
        self.metrics.bytes_live = self.total_size_bytes() as u64;

        Ok(())
    }

    fn ensure_size_limits(&mut self) -> Result<(), GenericError> {
        // While our total size exceeds the maximum allowed size, remove the oldest segment.
        while self.total_size_bytes() > self.config.max_ring_buffer_size_bytes
            && self.compressed_segments.size_bytes() > 0
        {
            self.compressed_segments.drop_oldest_segment();
        }

        Ok(())
    }
}

/// Shared state for the write side of the ring buffer.
struct WriterState {
    events_tx: mpsc::blocking::Sender<CondensedEvent>,
    events_tx_closed: AtomicBool,
}

impl WriterState {
    fn add_event(&self, event: &Event<'_>) {
        thread_local! {
            static VALUE_BUF: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
        }

        match self.events_tx.send_ref() {
            Ok(mut slot) => {
                VALUE_BUF.with(|buf| {
                    EventHydrator::hydrate(&mut slot, &mut buf.borrow_mut(), event);
                });
            }
            Err(_) => {
                // We only want to log about this once, since otherwise we'd just end up spamming stdout.
                if !self.events_tx_closed.swap(true, AcqRel) {
                    eprintln!("Failed to send event to background thread for compressed ring buffer. Debug logging will not be present.");
                }
            }
        }
    }
}

/// A `Layer` that stores a sliding window of compressed log events.
///
/// This layer captures log events and stores them in a memory-bounded ring buffer using columnar encoding and zstd
/// compression. It is designed for capturing verbose (e.g. DEBUG-level) events for potential post-mortem collection
/// without emitting them as part of the normal log output, while ensuring the collection does not consume excessive
/// memory.
///
/// # Architecture
///
/// The ring buffer is split across two threads connected by a bounded MPSC channel:
///
/// - **Writer thread** (the application's logging thread): Hydrates incoming [`tracing::Event`]s into condensed
///   event structs and sends them through the channel. This path is non-blocking -- if the channel is full,
///   events are dropped.
///
/// - **Processor thread** (a dedicated background thread): Receives events and manages the two-tier storage:
///
///   1. **Event buffer**: Accumulates incoming events into columnar storage. Each event field (timestamps,
///      levels, targets, messages, structured fields, source locations) is appended to a separate column buffer,
///      enabling specialized per-column encoding. The buffer also maintains a per-segment string intern table for
///      deduplicating repeated strings (targets, file paths, and field keys).
///
///   2. **Compressed segments**: A FIFO queue of zstd-compressed segment payloads. When the total ring buffer
///      size exceeds the configured maximum _and_ the event buffer has accumulated at least
///      `min_uncompressed_segment_size_bytes`, the event buffer is flushed: its columns are serialized and
///      compressed into a segment, which is pushed onto the queue. If the total size still exceeds the limit,
///      the oldest segments are dropped.
///
/// # Columnar Encoding
///
/// Rather than serializing events row-by-row, the event buffer stores each field as a separate column. This
/// groups similar data together, enabling specialized lightweight encoding before general-purpose compression:
///
/// - **Timestamps**: Delta-encoded as variable-length integers (LEB128). Each event stores only the nanosecond
///   delta from the previous event rather than the full 128-bit absolute timestamp.
///
/// - **Levels, target indices, file indices, field counts, message template indices**: Run-length encoded (RLE).
///   These columns have few distinct values and tend to appear in runs (e.g. a burst of DEBUG events from the
///   same module), so RLE compresses them dramatically.
///
/// - **Messages**: Decomposed into a _template skeleton_ and _variable tokens_. Tokens containing digits are
///   extracted as variables (e.g. `"Processed 1234 events in 56ms"` becomes skeleton
///   `"Processed \0 events in \0"` plus variables `["1234", "56ms"]`). Skeletons are interned in the string
///   table and their indices are RLE-encoded; variable tokens are stored separately as length-prefixed bytes.
///
/// - **Structured fields**: Split into three sub-columns: field counts (RLE), field key indices (varint, with
///   keys interned in the string table), and field values (length-prefixed bytes).
///
/// # Split Compression
///
/// On flush, the columnar data is divided into two payloads that are compressed as separate zstd frames:
///
/// - **Metadata frame**: String table, event count, timestamps, levels, targets, files, lines, field counts,
///   field key indices, and message template indices. This data is low-entropy and highly structured.
///
/// - **Content frame**: Message variable tokens and field values. This data is high-entropy and mostly unique.
///
/// Compressing these separately allows zstd to build optimal Huffman tables and match-finding strategies for
/// each distribution, yielding better overall compression ratios than a single combined frame.
///
/// # Memory Management
///
/// The total memory footprint is bounded by `max_ring_buffer_size_bytes` (default 1 MiB, typically configured
/// to 2 MiB in production). This budget covers both the uncompressed event buffer and all compressed segments.
/// When the budget is exceeded, the oldest compressed segments are evicted in FIFO order.
pub struct CompressedRingBuffer {
    writer_state: WriterState,
}

impl CompressedRingBuffer {
    /// Creates a new `CompressedRingBuffer` with the given configuration.
    pub fn with_config(config: RingBufferConfig) -> Self {
        let writer_state = create_and_spawn_processor(config);

        Self { writer_state }
    }

    /// Creates a new `CompressedRingBuffer` with the default configuration.
    pub fn new() -> Self {
        Self::with_config(RingBufferConfig::default())
    }
}

impl Default for CompressedRingBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Layer<S> for CompressedRingBuffer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        self.writer_state.add_event(event);
    }
}

fn create_and_spawn_processor(config: RingBufferConfig) -> WriterState {
    let (events_tx, events_rx) = mpsc::blocking::channel(1024);
    let writer_state = WriterState {
        events_tx,
        events_tx_closed: AtomicBool::new(false),
    };

    let processor_state = ProcessorState::new(config);
    std::thread::spawn(move || run_processor(events_rx, processor_state));

    writer_state
}

fn run_processor(events_rx: mpsc::blocking::Receiver<CondensedEvent>, mut processor_state: ProcessorState) {
    use thingbuf::mpsc::errors::RecvTimeoutError;

    loop {
        match events_rx.recv_ref_timeout(METRICS_PUBLISH_INTERVAL) {
            Ok(event) => {
                if let Err(e) = processor_state.add_event(&event) {
                    eprintln!("Failed to process event in compressed ring buffer: {}", e);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Closed) | Err(_) => break,
        }

        processor_state.metrics.publish_if_elapsed();
    }

    // Final publish to capture any remaining state.
    processor_state.metrics.publish();
}

#[cfg(test)]
mod tests {
    use rand::{rngs::SmallRng, Rng, SeedableRng};

    use super::*;

    /// Declares a `static` [`Metadata`] and returns `&'static Metadata<'static>`.
    ///
    /// Usage: `test_metadata!(target: "my::target", level: tracing::Level::INFO, file: "foo.rs", line: 42)`
    macro_rules! test_metadata {
        (target: $target:expr, level: $level:expr, file: $file:expr, line: $line:expr) => {{
            use tracing::callsite;
            struct TestCallsite;
            static META: tracing::Metadata<'static> = tracing::Metadata::new(
                "test",
                $target,
                $level,
                Some($file),
                Some($line),
                Some($target),
                tracing::field::FieldSet::new(&[], tracing::callsite::Identifier(&TestCallsite)),
                tracing::metadata::Kind::EVENT,
            );
            impl callsite::Callsite for TestCallsite {
                fn set_interest(&self, _: tracing::subscriber::Interest) {}
                fn metadata(&self) -> &tracing::Metadata<'_> {
                    &META
                }
            }
            &META
        }};
    }

    const BENCH_TARGETS: &[&str] = &[
        "saluki_components::sources::dogstatsd",
        "saluki_core::topology::runner",
        "saluki_io::net::udp",
        "saluki_app::logging::ring_buffer",
        "saluki_components::transforms::aggregate",
        "saluki_metadata::origin::resolver",
        "saluki_io::compression",
        "saluki_components::destinations::datadog",
        "saluki_core::buffers::memory",
        "agent_data_plane::cli::run",
        "saluki_components::sources::otlp",
        "saluki_tls::provider",
        "saluki_core::topology::interconnect",
        "saluki_app::internal::remote_agent",
        "saluki_metrics::recorder",
    ];

    const BENCH_SHORT_MESSAGES: &[&str] = &[
        "Listener started.",
        "Connection closed.",
        "Shutting down.",
        "Health check passed.",
        "Config reloaded.",
        "Worker started.",
        "Flushing buffers.",
        "Checkpoint saved.",
        "Reconnecting.",
        "Pipeline initialized.",
    ];

    const BENCH_FIELD_KEYS: &[&str] = &[
        "error",
        "listen_addr",
        "count",
        "path",
        "duration_ms",
        "component",
        "peer_addr",
        "bytes",
    ];

    const BENCH_FIELD_VALUES: &[&str] = &[
        "127.0.0.1:8125",
        "/var/run/datadog/apm.socket",
        "connection reset by peer",
        "dogstatsd",
        "topology_runner",
        "Permission denied (os error 13)",
        "https://intake.datadoghq.com/api/v2/series",
        "sess_a1b2c3d4e5f6",
    ];

    const BENCH_FILES: &[&str] = &[
        "src/sources/dogstatsd/mod.rs",
        "src/topology/runner.rs",
        "src/net/udp.rs",
        "src/logging/ring_buffer.rs",
        "src/transforms/aggregate.rs",
        "src/destinations/datadog.rs",
        "src/cli/run.rs",
        "src/internal/remote_agent.rs",
    ];

    const BENCH_LEVELS: &[tracing::Level] = &[
        tracing::Level::DEBUG,
        tracing::Level::INFO,
        tracing::Level::WARN,
        tracing::Level::ERROR,
    ];

    /// Builds a leaked `&'static Metadata<'static>` for benchmark use with the given target and
    /// level. File/line are set to a representative value; in production these come from the
    /// callsite and are not varied per-event.
    fn bench_metadata(target: &'static str, level: tracing::Level, file: &'static str) -> &'static Metadata<'static> {
        // Intentionally leak: benchmark metadata lives for the process lifetime.
        &*Box::leak(Box::new(tracing::Metadata::new(
            "bench",
            target,
            level,
            Some(file),
            Some(1),
            Some(target),
            tracing::field::FieldSet::new(
                &[],
                tracing::callsite::Identifier(
                    // Each leaked Metadata gets its own unique leaked callsite so that
                    // Identifier uniqueness is satisfied.
                    &*Box::leak(Box::new(BenchCallsite)),
                ),
            ),
            tracing::metadata::Kind::EVENT,
        )))
    }

    struct BenchCallsite;
    impl tracing::callsite::Callsite for BenchCallsite {
        fn set_interest(&self, _: tracing::subscriber::Interest) {}
        fn metadata(&self) -> &Metadata<'_> {
            unimplemented!("bench callsite metadata should never be called")
        }
    }

    struct EventGenerator {
        rng: SmallRng,
        timestamp_nanos: u128,
        metadata_table: Vec<&'static Metadata<'static>>,
    }

    impl EventGenerator {
        fn new(seed: u64) -> Self {
            // Pre-build a metadata entry for each (target, level) combination.
            let mut metadata_table = Vec::new();
            for &target in BENCH_TARGETS {
                let file = BENCH_FILES[metadata_table.len() % BENCH_FILES.len()];
                for &level in BENCH_LEVELS {
                    metadata_table.push(bench_metadata(target, level, file));
                }
            }
            Self {
                rng: SmallRng::seed_from_u64(seed),
                timestamp_nanos: 1_700_000_000_000_000_000, // ~Nov 2023
                metadata_table,
            }
        }

        fn next_event(&mut self) -> CondensedEvent {
            // Advance timestamp by 1-50ms.
            self.timestamp_nanos += self.rng.random_range(1_000_000u128..50_000_000u128);

            // Weighted level distribution: 40% DEBUG, 30% INFO, 20% WARN, 10% ERROR.
            let level_roll: u32 = self.rng.random_range(0..100);
            let level_idx = if level_roll < 40 {
                0 // DEBUG
            } else if level_roll < 70 {
                1 // INFO
            } else if level_roll < 90 {
                2 // WARN
            } else {
                3 // ERROR
            };

            let target_idx = self.rng.random_range(0..BENCH_TARGETS.len());
            let metadata = self.metadata_table[target_idx * BENCH_LEVELS.len() + level_idx];

            // 60% short static messages, 40% formatted with numbers.
            let message = if self.rng.random_range(0..100u32) < 60 {
                BENCH_SHORT_MESSAGES[self.rng.random_range(0..BENCH_SHORT_MESSAGES.len())].to_string()
            } else {
                let variant: u32 = self.rng.random_range(0..4);
                match variant {
                    0 => format!(
                        "Processed {} events in {}ms",
                        self.rng.random_range(1u64..100_000),
                        self.rng.random_range(1u32..5000)
                    ),
                    1 => format!(
                        "Buffer capacity at {}%, {} bytes used of {}",
                        self.rng.random_range(10u32..100),
                        self.rng.random_range(1024u64..1_048_576),
                        self.rng.random_range(1_048_576u64..10_485_760)
                    ),
                    2 => format!(
                        "Forwarding {} metric(s) to {}",
                        self.rng.random_range(1u32..10_000),
                        BENCH_FIELD_VALUES[self.rng.random_range(0..BENCH_FIELD_VALUES.len())]
                    ),
                    _ => format!(
                        "Retry attempt {} of {} for endpoint {}",
                        self.rng.random_range(1u32..5),
                        self.rng.random_range(3u32..10),
                        BENCH_FIELD_VALUES[self.rng.random_range(0..BENCH_FIELD_VALUES.len())]
                    ),
                }
            };

            // 0-5 fields, weighted toward fewer: 30% 0, 30% 1, 20% 2, 10% 3, 5% 4, 5% 5.
            let field_roll: u32 = self.rng.random_range(0..100);
            let num_fields = if field_roll < 30 {
                0
            } else if field_roll < 60 {
                1
            } else if field_roll < 80 {
                2
            } else if field_roll < 90 {
                3
            } else if field_roll < 95 {
                4
            } else {
                5
            };

            let mut fields = Vec::new();
            for _ in 0..num_fields {
                let key = BENCH_FIELD_KEYS[self.rng.random_range(0..BENCH_FIELD_KEYS.len())];
                // 50% use a static value, 50% use a formatted number.
                let value: String = if self.rng.random_range(0..2u32) == 0 {
                    BENCH_FIELD_VALUES[self.rng.random_range(0..BENCH_FIELD_VALUES.len())].to_string()
                } else {
                    format!("{}", self.rng.random_range(0u64..1_000_000))
                };
                write_length_prefixed(&mut fields, key.as_bytes());
                write_length_prefixed(&mut fields, value.as_bytes());
            }

            CondensedEvent {
                timestamp_nanos: self.timestamp_nanos,
                metadata: Some(metadata),
                message,
                fields,
            }
        }
    }

    struct SegmentStats {
        event_count: usize,
        compressed_bytes: usize,
        uncompressed_bytes: usize,
    }

    struct BenchmarkResult {
        config_desc: String,
        seed: u64,
        events_fed: usize,
        events_retained: usize,
        segments_live: usize,
        segments_dropped: u64,
        total_compressed_bytes: usize,
        event_buffer_bytes: usize,
        total_size_bytes: usize,
        per_segment: Vec<SegmentStats>,
    }

    fn run_benchmark(config: RingBufferConfig, config_desc: &str, seed: u64, num_events: usize) -> BenchmarkResult {
        let mut state = ProcessorState::new(config);
        let mut gen = EventGenerator::new(seed);

        for _ in 0..num_events {
            let event = gen.next_event();
            state.add_event(&event).unwrap();
        }

        let per_segment: Vec<SegmentStats> = state
            .compressed_segments
            .segments
            .iter()
            .map(|seg| SegmentStats {
                event_count: seg.event_count,
                compressed_bytes: seg.compressed_data.len(),
                uncompressed_bytes: seg.uncompressed_size,
            })
            .collect();

        let events_retained = state.compressed_segments.event_count() + state.event_buffer.event_count();

        BenchmarkResult {
            config_desc: config_desc.to_string(),
            seed,
            events_fed: num_events,
            events_retained,
            segments_live: state.compressed_segments.segments.len(),
            segments_dropped: state.compressed_segments.segments_dropped_total,
            total_compressed_bytes: state.compressed_segments.size_bytes(),
            event_buffer_bytes: state.event_buffer.size_bytes(),
            total_size_bytes: state.total_size_bytes(),
            per_segment,
        }
    }

    fn format_bytes(bytes: usize) -> String {
        if bytes >= 1_048_576 {
            format!("{:.2} MiB", bytes as f64 / 1_048_576.0)
        } else if bytes >= 1024 {
            format!("{:.1} KiB", bytes as f64 / 1024.0)
        } else {
            format!("{} B", bytes)
        }
    }

    fn print_report(result: &BenchmarkResult) {
        let total_uncompressed: usize = result.per_segment.iter().map(|s| s.uncompressed_bytes).sum();
        let total_compressed: usize = result.per_segment.iter().map(|s| s.compressed_bytes).sum();
        let overall_ratio = if total_compressed > 0 {
            total_uncompressed as f64 / total_compressed as f64
        } else {
            0.0
        };
        let avg_bytes_per_event = if result.events_retained > 0 {
            result.total_compressed_bytes as f64 / result.events_retained as f64
        } else {
            0.0
        };
        let retention_pct = if result.events_fed > 0 {
            100.0 * result.events_retained as f64 / result.events_fed as f64
        } else {
            0.0
        };

        println!();
        println!("=== Ring Buffer Capacity Benchmark ===");
        println!("Config: {}", result.config_desc);
        println!("Seed: 0x{:X} | Events fed: {}", result.seed, result.events_fed);
        println!();
        println!("--- Summary ---");
        println!("Events retained:          {}", result.events_retained);
        println!(
            "Events evicted:           {}",
            result.events_fed - result.events_retained
        );
        println!("Retention rate:           {:.1}%", retention_pct);
        println!("Segments live:            {}", result.segments_live);
        println!("Segments dropped:         {}", result.segments_dropped);
        println!(
            "Compressed bytes:         {} ({})",
            result.total_compressed_bytes,
            format_bytes(result.total_compressed_bytes)
        );
        println!(
            "Event buffer bytes:       {} ({})",
            result.event_buffer_bytes,
            format_bytes(result.event_buffer_bytes)
        );
        println!(
            "Total size:               {} ({})",
            result.total_size_bytes,
            format_bytes(result.total_size_bytes)
        );
        println!("Avg compressed bytes/evt: {:.1}", avg_bytes_per_event);
        println!("Overall compression ratio:{:.2}x", overall_ratio);

        if !result.per_segment.is_empty() {
            println!();
            println!("--- Per-Segment Stats ---");
            println!(
                "  {:>3} | {:>6} | {:>10} | {:>10} | {:>5}",
                "#", "Events", "Uncompr.", "Compr.", "Ratio"
            );
            for (i, seg) in result.per_segment.iter().enumerate() {
                let ratio = if seg.compressed_bytes > 0 {
                    seg.uncompressed_bytes as f64 / seg.compressed_bytes as f64
                } else {
                    0.0
                };
                println!(
                    "  {:>3} | {:>6} | {:>10} | {:>10} | {:>4.2}x",
                    i + 1,
                    seg.event_count,
                    seg.uncompressed_bytes,
                    seg.compressed_bytes,
                    ratio
                );
            }
        }
        println!();
    }

    #[test]
    #[ignore]
    fn ring_buffer_capacity_bench() {
        let seed = 0xDEAD_BEEF_CAFE;
        let num_events = 500_000;

        let config = RingBufferConfig::default().with_max_ring_buffer_size_bytes(2 * 1024 * 1024);

        let result = run_benchmark(config, "max=2MiB, min_segment=128KiB, zstd_level=19", seed, num_events);
        print_report(&result);
    }

    #[test]
    #[ignore]
    fn ring_buffer_capacity_sweep() {
        let seed = 0xDEAD_BEEF_CAFE;
        let num_events = 500_000;

        let configs: Vec<(&str, RingBufferConfig)> = vec![
            (
                "default: max=2MiB, min_segment=128KiB, zstd=19",
                RingBufferConfig::default().with_max_ring_buffer_size_bytes(2 * 1024 * 1024),
            ),
            (
                "zstd=3: max=2MiB, min_segment=128KiB, zstd=3",
                RingBufferConfig::default()
                    .with_max_ring_buffer_size_bytes(2 * 1024 * 1024)
                    .with_compression_level(3),
            ),
            (
                "zstd=11: max=2MiB, min_segment=128KiB, zstd=11",
                RingBufferConfig::default()
                    .with_max_ring_buffer_size_bytes(2 * 1024 * 1024)
                    .with_compression_level(11),
            ),
            (
                "256k seg: max=2MiB, min_segment=256KiB, zstd=19",
                RingBufferConfig::default()
                    .with_max_ring_buffer_size_bytes(2 * 1024 * 1024)
                    .with_min_uncompressed_segment_size_bytes(256 * 1024),
            ),
            (
                "64k seg: max=2MiB, min_segment=64KiB, zstd=19",
                RingBufferConfig::default()
                    .with_max_ring_buffer_size_bytes(2 * 1024 * 1024)
                    .with_min_uncompressed_segment_size_bytes(64 * 1024),
            ),
        ];

        let mut results = Vec::new();
        for (name, config) in configs {
            let result = run_benchmark(config, name, seed, num_events);
            print_report(&result);
            results.push(result);
        }

        // Print compact comparison table.
        println!("=== Comparison ===");
        println!(
            "  {:<50} | {:>8} | {:>6} | {:>7} | {:>6}",
            "Config", "Retained", "Rate", "Ratio", "B/evt"
        );
        for r in &results {
            let total_uncompr: usize = r.per_segment.iter().map(|s| s.uncompressed_bytes).sum();
            let total_compr: usize = r.per_segment.iter().map(|s| s.compressed_bytes).sum();
            let ratio = if total_compr > 0 {
                total_uncompr as f64 / total_compr as f64
            } else {
                0.0
            };
            let bytes_per_evt = if r.events_retained > 0 {
                r.total_compressed_bytes as f64 / r.events_retained as f64
            } else {
                0.0
            };
            let rate = 100.0 * r.events_retained as f64 / r.events_fed as f64;
            println!(
                "  {:<50} | {:>8} | {:>5.1}% | {:>5.2}x | {:>6.1}",
                r.config_desc, r.events_retained, rate, ratio, bytes_per_evt
            );
        }
        println!();
    }

    fn encode_test_fields(pairs: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (key, value) in pairs {
            write_length_prefixed(&mut buf, key.as_bytes());
            write_length_prefixed(&mut buf, value.as_bytes());
        }
        buf
    }

    fn make_test_event(message: &str) -> CondensedEvent {
        CondensedEvent {
            timestamp_nanos: 1_000_000_000,
            metadata: Some(test_metadata!(
                target: "test::target",
                level: tracing::Level::INFO,
                file: "test.rs",
                line: 42
            )),
            message: message.to_string(),
            fields: encode_test_fields(&[("key1", "value1"), ("key2", "value2")]),
        }
    }

    #[test]
    fn round_trip_encode_decode() {
        let event = make_test_event("hello world");

        let mut buffer = EventBuffer::from_compression_level(zstd::DEFAULT_COMPRESSION_LEVEL);
        buffer.encode_event(&event).unwrap();
        assert_eq!(buffer.event_count(), 1);

        let segment = buffer.flush().unwrap();
        assert_eq!(segment.event_count(), 1);
        assert!(segment.size_bytes() > 0);

        let meta = event.metadata.unwrap();
        let mut reader = segment.events().unwrap();
        let decoded = reader.next().unwrap().expect("should have one event");

        assert_eq!(decoded.timestamp_nanos, event.timestamp_nanos);
        assert_eq!(decoded.level, meta.level().as_str());
        assert_eq!(decoded.target, meta.target());
        assert_eq!(decoded.message, event.message);
        assert_eq!(decoded.fields, event.fields);
        assert_eq!(decoded.file, meta.file());
        assert_eq!(decoded.line, meta.line());

        // No more events.
        assert!(reader.next().unwrap().is_none());
    }

    #[test]
    fn round_trip_multiple_events() {
        let mut buffer = EventBuffer::from_compression_level(zstd::DEFAULT_COMPRESSION_LEVEL);

        for i in 0..10 {
            let event = make_test_event(&format!("message {}", i));
            buffer.encode_event(&event).unwrap();
        }
        assert_eq!(buffer.event_count(), 10);

        let segment = buffer.flush().unwrap();
        assert_eq!(segment.event_count(), 10);

        let mut reader = segment.events().unwrap();
        for i in 0..10 {
            let decoded = reader.next().unwrap().expect("should have event");
            assert_eq!(decoded.message, format!("message {}", i));
        }
        assert!(reader.next().unwrap().is_none());
    }

    #[test]
    fn segment_flush_on_size_threshold() {
        let config = RingBufferConfig::default()
            .with_min_uncompressed_segment_size_bytes(128) // Very small minimum segment size.
            .with_max_ring_buffer_size_bytes(256); // Small ring buffer to trigger pressure quickly.

        let mut state = ProcessorState::new(config);
        assert_eq!(state.compressed_segments.segments.len(), 0);

        // Add events until the ring buffer is under pressure and a segment is flushed.
        for i in 0..50 {
            let event = make_test_event(&format!("event number {}", i));
            state.add_event(&event).unwrap();
        }

        // With a 128-byte min segment size and 256-byte ring buffer, we should have flushed at least one segment.
        assert!(
            !state.compressed_segments.segments.is_empty(),
            "expected at least one compressed segment"
        );
    }

    #[test]
    fn ring_buffer_eviction() {
        let config = RingBufferConfig::default()
            .with_min_uncompressed_segment_size_bytes(64) // Tiny minimum segment size.
            .with_max_ring_buffer_size_bytes(256); // Very small ring buffer.

        let mut state = ProcessorState::new(config);

        // Add enough events to fill and overflow the ring buffer.
        for i in 0..200 {
            let event = make_test_event(&format!("event {}", i));
            state.add_event(&event).unwrap();
        }

        // The total size should respect the limit (compressed segments + event buffer).
        // The compressed segments alone should be within the limit.
        assert!(
            state.compressed_segments.size_bytes() <= 256,
            "compressed segments size {} exceeds limit 256",
            state.compressed_segments.size_bytes()
        );
    }

    #[test]
    fn ensure_size_limits_does_not_hang_when_event_buffer_exceeds_limit() {
        // Regression test: ensure_size_limits must not infinite-loop when the event buffer
        // alone exceeds max_ring_buffer_size_bytes and there are no compressed segments to drop.
        let config = RingBufferConfig::default()
            .with_min_uncompressed_segment_size_bytes(1024 * 1024) // Large, so no flush happens.
            .with_max_ring_buffer_size_bytes(1); // Tiny limit -- event buffer will exceed this.

        let mut state = ProcessorState::new(config);
        let event = make_test_event("this event will exceed the ring buffer limit");

        // This must return without hanging.
        state.add_event(&event).unwrap();

        // The event buffer should have the event even though it exceeds the limit,
        // since there are no compressed segments to evict.
        assert_eq!(state.event_buffer.event_count(), 1);
        assert_eq!(state.compressed_segments.segments.len(), 0);
    }

    #[test]
    fn reader_empty_buffer() {
        // Build a valid columnar segment with 0 events (split into meta + content).
        let mut meta = Vec::new();
        encode_varint(0, &mut meta); // string table: 0 entries
        encode_varint(0, &mut meta); // event count: 0
        encode_varint(0, &mut meta); // timestamps column: 0 bytes
        rle_encode(&[], &mut meta); // levels: empty RLE
        rle_encode(&[], &mut meta); // target_indices: empty RLE
        rle_encode(&[], &mut meta); // file_indices: empty RLE
        encode_varint(0, &mut meta); // lines column: 0 bytes
        rle_encode(&[], &mut meta); // field_counts: empty RLE
        encode_varint(0, &mut meta); // field_key_indices: 0 bytes
        rle_encode(&[], &mut meta); // msg_template_indices: empty RLE

        let mut content = Vec::new();
        encode_varint(0, &mut content); // message variables: 0 bytes
                                        // field values: empty (rest of buffer)

        let mut reader = CompressedSegmentReader::new(meta, content).unwrap();
        assert!(reader.next().unwrap().is_none());
    }

    #[test]
    fn reader_truncated_framing_returns_error() {
        // A header that claims 100 events but has no column data.
        let mut meta = Vec::new();
        encode_varint(0, &mut meta); // string table: 0 entries
        encode_varint(100, &mut meta); // event count: 100
                                       // Missing all columns -- should fail during header decode.
        assert!(
            CompressedSegmentReader::new(meta, Vec::new()).is_err(),
            "expected error on truncated data"
        );
    }

    #[test]
    fn varint_round_trip() {
        for &value in &[0, 1, 127, 128, 255, 256, 16383, 16384, usize::MAX >> 1] {
            let mut buf = Vec::new();
            encode_varint(value, &mut buf);
            let (decoded, consumed) = decode_varint(&buf, 0).expect("should decode");
            assert_eq!(decoded, value, "varint round-trip failed for {}", value);
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn field_encoding_round_trip() {
        let fields = encode_test_fields(&[("key1", "value1"), ("key2", "value2")]);
        let event = DecodedEvent {
            fields,
            ..Default::default()
        };

        let pairs: Vec<_> = event.iter_fields().collect();
        assert_eq!(pairs, vec![("key1", "value1"), ("key2", "value2")]);
    }

    #[test]
    fn field_values_with_newlines_and_special_chars() {
        let fields = encode_test_fields(&[
            ("msg", "line1\nline2\nline3"),
            ("json", "{\"key\": \"val\"}"),
            ("empty", ""),
        ]);
        let event = DecodedEvent {
            fields,
            ..Default::default()
        };

        let pairs: Vec<_> = event.iter_fields().collect();
        assert_eq!(pairs[0], ("msg", "line1\nline2\nline3"));
        assert_eq!(pairs[1], ("json", "{\"key\": \"val\"}"));
        assert_eq!(pairs[2], ("empty", ""));
    }

    #[test]
    fn field_encoding_survives_compression_round_trip() {
        let mut event = make_test_event("hello");
        // Add a field with newlines to verify it survives the full pipeline.
        write_length_prefixed(&mut event.fields, b"multiline");
        write_length_prefixed(&mut event.fields, b"line1\nline2\nline3");

        let mut buffer = EventBuffer::from_compression_level(zstd::DEFAULT_COMPRESSION_LEVEL);
        buffer.encode_event(&event).unwrap();

        let segment = buffer.flush().unwrap();
        let mut reader = segment.events().unwrap();
        let decoded = reader.next().unwrap().expect("should have event");

        let pairs: Vec<_> = decoded.iter_fields().collect();
        assert_eq!(pairs[0], ("key1", "value1"));
        assert_eq!(pairs[1], ("key2", "value2"));
        assert_eq!(pairs[2], ("multiline", "line1\nline2\nline3"));
    }
}
