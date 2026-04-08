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
use tracing::{Event, Subscriber};
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

/// A condensed, serializable representation of a log event.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CondensedEvent<'a> {
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

impl<'a> CondensedEvent<'a> {
    /// Returns the number of bytes that this event occupies in memory.
    #[allow(dead_code)] // Useful for diagnostics; exercised in tests.
    pub fn size_bytes(&self) -> usize {
        // We use the _capacity_ of the backing buffers to account for the actual memory allocated
        // rather than just the amount of it that we're _using_.
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

/// Wrapper that pairs a `CondensedEvent` with a reusable scratch buffer for `Visit` field
/// formatting. The scratch buffer lives here (not in `CondensedEvent`) so the event struct stays
/// serialization-clean.
struct EventHydrator<'a> {
    event: &'a mut CondensedEvent<'static>,
    value_buf: &'a mut String,
}

impl<'a> EventHydrator<'a> {
    fn hydrate(event: &'a mut CondensedEvent<'static>, value_buf: &'a mut String, tracing_event: &Event<'_>) {
        event.message.clear();
        event.fields.clear();

        event.timestamp_nanos = get_unix_timestamp_nanos();

        let metadata = tracing_event.metadata();
        event.level = metadata.level().as_str();
        event.target = metadata.target();
        event.file = metadata.file();
        event.line = metadata.line();

        let mut hydrator = EventHydrator { event, value_buf };
        tracing_event.record(&mut hydrator);
    }
}

impl tracing::field::Visit for EventHydrator<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.event.message, "{:?}", value);
        } else {
            // Write key.
            write_length_prefixed(&mut self.event.fields, field.name().as_bytes());

            // Format value into the scratch buffer, then write it length-prefixed.
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

/// Columnar event buffer that stores each field in a separate column for better compression.
///
/// Segment layout on flush:
///   [string_table]
///   [varint(event_count)]
///   [col: timestamps]      -- varint-delta-encoded
///   [col: levels]          -- RLE-encoded
///   [col: target_indices]  -- RLE-encoded
///   [col: messages]        -- varint-length-prefixed strings concatenated
///   [col: fields]          -- per-event: varint(num_fields) [varint(key_idx) varint(val_len) val ...]
///   [col: file_indices]    -- RLE-encoded (usize::MAX sentinel for None)
///   [col: lines]           -- varint-encoded (0 sentinel for None, actual values stored as line+1)
struct EventBuffer {
    // Column accumulators.
    col_timestamps: Vec<u8>,
    col_levels: Vec<usize>,
    col_target_indices: Vec<usize>,
    col_messages: Vec<u8>,
    col_fields: Vec<u8>,
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
            col_messages: Vec::new(),
            col_fields: Vec::new(),
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
            + self.col_messages.len()
            + self.col_fields.len()
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
        self.col_messages.clear();
        self.col_fields.clear();
        self.col_file_indices.clear();
        self.col_lines.clear();
        self.event_count = 0;
        self.last_timestamp = 0;
        self.string_table.clear();
    }

    fn encode_event(&mut self, event: &CondensedEvent<'_>) -> Result<(), GenericError> {
        // Timestamps: delta-encoded varints.
        let ts_delta = event.timestamp_nanos.saturating_sub(self.last_timestamp);
        self.last_timestamp = event.timestamp_nanos;
        encode_varint_u128(ts_delta, &mut self.col_timestamps);

        // Level: stored as usize for RLE, encoded on flush.
        self.col_levels.push(encode_level(event.level) as usize);

        // Target: interned index, stored for RLE.
        let target_idx = self.string_table.intern(event.target);
        self.col_target_indices.push(target_idx);

        // Message: varint-length-prefixed into message column.
        write_length_prefixed(&mut self.col_messages, event.message.as_bytes());

        // Fields: re-encode with interned keys into fields column.
        let field_count = FieldIter { buf: &event.fields, idx: 0 }.count();
        encode_varint(field_count, &mut self.col_fields);
        let mut field_iter = FieldIter { buf: &event.fields, idx: 0 };
        while let Some((key, val)) = field_iter.next() {
            let key_idx = self.string_table.intern(key);
            encode_varint(key_idx, &mut self.col_fields);
            write_length_prefixed(&mut self.col_fields, val.as_bytes());
        }

        // File: interned index or sentinel, stored for RLE.
        match event.file {
            Some(file) => self.col_file_indices.push(self.string_table.intern(file)),
            None => self.col_file_indices.push(FILE_INDEX_NONE),
        }

        // Line: varint with 0 = None, line+1 = Some(line).
        match event.line {
            Some(line) => encode_varint(line as usize + 1, &mut self.col_lines),
            None => encode_varint(0, &mut self.col_lines),
        }

        self.event_count += 1;

        Ok(())
    }

    fn flush(&mut self) -> Result<CompressedSegment, GenericError> {
        // Build columnar payload.
        let mut payload = Vec::with_capacity(self.size_bytes() + 256);

        // String table header.
        self.string_table.encode(&mut payload);

        // Event count.
        encode_varint(self.event_count, &mut payload);

        // Column: timestamps (already varint-delta-encoded).
        write_length_prefixed(&mut payload, &self.col_timestamps);

        // Column: levels (RLE-encoded).
        rle_encode(&self.col_levels, &mut payload);

        // Column: target indices (RLE-encoded).
        rle_encode(&self.col_target_indices, &mut payload);

        // Column: messages (already varint-length-prefixed).
        write_length_prefixed(&mut payload, &self.col_messages);

        // Column: fields (already encoded per-event).
        write_length_prefixed(&mut payload, &self.col_fields);

        // Column: file indices (RLE-encoded).
        rle_encode(&self.col_file_indices, &mut payload);

        // Column: lines (already varint-encoded).
        write_length_prefixed(&mut payload, &self.col_lines);

        // Compress.
        let mut encoder =
            zstd::Encoder::new(Vec::new(), self.compression_level).error_context("Failed to create ZSTD encoder.")?;
        encoder
            .write_all(&payload)
            .error_context("Failed to compress columnar payload.")?;
        let compressed_data = encoder.finish().error_context("Failed to finalize ZSTD encoder.")?;

        let compressed_segment = CompressedSegment {
            event_count: self.event_count,
            uncompressed_size: payload.len(),
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
        let mut decompressed = Vec::with_capacity(self.uncompressed_size);
        let mut decoder = zstd::Decoder::with_buffer(&self.compressed_data[..])?;
        decoder.read_to_end(&mut decompressed)?;

        CompressedSegmentReader::new(decompressed)
    }
}

/// Columnar compressed segment reader.
///
/// Decodes all columns from the segment payload, then yields events one at a time by indexing
/// across the decoded columns.
#[allow(dead_code)] // Will be used by the read API; exercised in tests.
struct CompressedSegmentReader {
    buf: Vec<u8>,
    string_table_ranges: Vec<(usize, usize)>,

    // Decoded column state.
    event_count: usize,
    event_idx: usize,

    // Column cursors into `buf`.
    ts_cursor: usize,
    ts_end: usize,
    last_timestamp: u128,

    levels: Vec<usize>,
    target_indices: Vec<usize>,

    msg_cursor: usize,
    msg_end: usize,

    fields_cursor: usize,
    fields_end: usize,

    file_indices: Vec<usize>,

    line_cursor: usize,
    line_end: usize,
}

#[allow(dead_code)] // Will be used by the read API; exercised in tests.
impl CompressedSegmentReader {
    fn new(buf: Vec<u8>) -> Result<Self, GenericError> {
        let mut reader = Self {
            buf,
            string_table_ranges: Vec::new(),
            event_count: 0,
            event_idx: 0,
            ts_cursor: 0,
            ts_end: 0,
            last_timestamp: 0,
            levels: Vec::new(),
            target_indices: Vec::new(),
            msg_cursor: 0,
            msg_end: 0,
            fields_cursor: 0,
            fields_end: 0,
            file_indices: Vec::new(),
            line_cursor: 0,
            line_end: 0,
        };
        reader.decode_header()?;
        Ok(reader)
    }

    fn decode_header(&mut self) -> Result<(), GenericError> {
        let mut idx = 0;

        // String table.
        let (count, consumed) = decode_varint(&self.buf, idx)
            .ok_or_else(|| generic_error!("Failed to decode string table count"))?;
        idx += consumed;
        self.string_table_ranges.reserve(count);
        for _ in 0..count {
            let (len, consumed) = decode_varint(&self.buf, idx)
                .ok_or_else(|| generic_error!("Failed to decode string table entry length"))?;
            idx += consumed;
            if idx + len > self.buf.len() {
                return Err(generic_error!("String table entry exceeds buffer"));
            }
            self.string_table_ranges.push((idx, idx + len));
            idx += len;
        }

        // Event count.
        let (event_count, consumed) = decode_varint(&self.buf, idx)
            .ok_or_else(|| generic_error!("Failed to decode event count"))?;
        idx += consumed;
        self.event_count = event_count;

        // Timestamps column (length-prefixed blob).
        let (ts_len, consumed) = decode_varint(&self.buf, idx)
            .ok_or_else(|| generic_error!("Failed to decode timestamps column length"))?;
        idx += consumed;
        self.ts_cursor = idx;
        self.ts_end = idx + ts_len;
        idx += ts_len;

        // Levels column (RLE).
        self.levels = rle_decode(&self.buf, &mut idx)
            .ok_or_else(|| generic_error!("Failed to decode levels column"))?;

        // Target indices column (RLE).
        self.target_indices = rle_decode(&self.buf, &mut idx)
            .ok_or_else(|| generic_error!("Failed to decode target indices column"))?;

        // Messages column (length-prefixed blob).
        let (msg_len, consumed) = decode_varint(&self.buf, idx)
            .ok_or_else(|| generic_error!("Failed to decode messages column length"))?;
        idx += consumed;
        self.msg_cursor = idx;
        self.msg_end = idx + msg_len;
        idx += msg_len;

        // Fields column (length-prefixed blob).
        let (fields_len, consumed) = decode_varint(&self.buf, idx)
            .ok_or_else(|| generic_error!("Failed to decode fields column length"))?;
        idx += consumed;
        self.fields_cursor = idx;
        self.fields_end = idx + fields_len;
        idx += fields_len;

        // File indices column (RLE).
        self.file_indices = rle_decode(&self.buf, &mut idx)
            .ok_or_else(|| generic_error!("Failed to decode file indices column"))?;

        // Lines column (length-prefixed blob).
        let (lines_len, consumed) = decode_varint(&self.buf, idx)
            .ok_or_else(|| generic_error!("Failed to decode lines column length"))?;
        idx += consumed;
        self.line_cursor = idx;
        self.line_end = idx + lines_len;

        Ok(())
    }

    fn next(&mut self) -> Result<Option<CondensedEvent<'_>>, GenericError> {
        if self.event_idx >= self.event_count {
            return Ok(None);
        }
        let i = self.event_idx;
        self.event_idx += 1;

        // Timestamp: read next varint delta from timestamp column.
        let (ts_delta, consumed) = decode_varint_u128(&self.buf, self.ts_cursor)
            .ok_or_else(|| generic_error!("Failed to decode timestamp delta for event {}", i))?;
        self.ts_cursor += consumed;
        let timestamp_nanos = self.last_timestamp + ts_delta;
        self.last_timestamp = timestamp_nanos;

        // Level: index into pre-decoded RLE vector.
        let level = decode_level(self.levels[i] as u8);

        // Target: index into pre-decoded RLE vector, then look up string table.
        let target_idx = self.target_indices[i];
        let &(t_start, t_end) = self.string_table_ranges.get(target_idx).ok_or_else(|| {
            generic_error!("Target string table index {} out of range", target_idx)
        })?;
        let target = std::str::from_utf8(&self.buf[t_start..t_end])
            .map_err(|e| generic_error!("Invalid UTF-8 in target: {}", e))?;

        // Message: read next varint-length-prefixed string from message column.
        let (msg_len, consumed) = decode_varint(&self.buf, self.msg_cursor)
            .ok_or_else(|| generic_error!("Failed to decode message length for event {}", i))?;
        self.msg_cursor += consumed;
        let message = std::str::from_utf8(&self.buf[self.msg_cursor..self.msg_cursor + msg_len])
            .map_err(|e| generic_error!("Invalid UTF-8 in message: {}", e))?
            .to_string();
        self.msg_cursor += msg_len;

        // Fields: read next field group from fields column.
        let (field_count, consumed) = decode_varint(&self.buf, self.fields_cursor)
            .ok_or_else(|| generic_error!("Failed to decode field count for event {}", i))?;
        self.fields_cursor += consumed;
        let mut fields = Vec::new();
        for _ in 0..field_count {
            let (key_idx, consumed) = decode_varint(&self.buf, self.fields_cursor)
                .ok_or_else(|| generic_error!("Failed to decode field key index"))?;
            self.fields_cursor += consumed;
            let &(k_start, k_end) = self.string_table_ranges.get(key_idx).ok_or_else(|| {
                generic_error!("Field key string table index {} out of range", key_idx)
            })?;
            write_length_prefixed(&mut fields, &self.buf[k_start..k_end]);

            let (val_len, consumed) = decode_varint(&self.buf, self.fields_cursor)
                .ok_or_else(|| generic_error!("Failed to decode field value length"))?;
            self.fields_cursor += consumed;
            write_length_prefixed(&mut fields, &self.buf[self.fields_cursor..self.fields_cursor + val_len]);
            self.fields_cursor += val_len;
        }

        // File: index into pre-decoded RLE vector.
        let file_idx = self.file_indices[i];
        let file = if file_idx == FILE_INDEX_NONE {
            None
        } else {
            let &(f_start, f_end) = self.string_table_ranges.get(file_idx).ok_or_else(|| {
                generic_error!("File string table index {} out of range", file_idx)
            })?;
            Some(
                std::str::from_utf8(&self.buf[f_start..f_end])
                    .map_err(|e| generic_error!("Invalid UTF-8 in file: {}", e))?,
            )
        };

        // Line: read next varint from lines column (0 = None, n = Some(n-1)).
        let (line_enc, consumed) = decode_varint(&self.buf, self.line_cursor)
            .ok_or_else(|| generic_error!("Failed to decode line for event {}", i))?;
        self.line_cursor += consumed;
        let line = if line_enc == 0 {
            None
        } else {
            Some((line_enc - 1) as u32)
        };

        Ok(Some(CondensedEvent {
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

    fn add_event(&mut self, event: &CondensedEvent<'static>) -> Result<(), GenericError> {
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
    events_tx: mpsc::blocking::Sender<CondensedEvent<'static>>,
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
/// This layer is designed to allow for storing a sliding window of log events in an efficient manner by chunking them
/// into "segments", which are then compressed and stored in a ring buffer. Given a configurable upper bound on the total
/// in-memory size of all log events, the layer will drop the oldest compressed segment if storing new log events would
/// cause the total size to exceed the limit.
///
/// This effectively allows for storing more verbose log events for potential collection (such as when debugging an issue)
/// without having to emit them as part of the normal log output, while ensuring that the collection does not consume an
/// excessive amount of memory.
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

fn run_processor(events_rx: mpsc::blocking::Receiver<CondensedEvent<'static>>, mut processor_state: ProcessorState) {
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
    use super::*;
    use rand::{rngs::SmallRng, Rng, SeedableRng};

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

    struct EventGenerator {
        rng: SmallRng,
        timestamp_nanos: u128,
    }

    impl EventGenerator {
        fn new(seed: u64) -> Self {
            Self {
                rng: SmallRng::seed_from_u64(seed),
                timestamp_nanos: 1_700_000_000_000_000_000, // ~Nov 2023
            }
        }

        fn next_event(&mut self) -> CondensedEvent<'static> {
            // Advance timestamp by 1-50ms.
            self.timestamp_nanos += self.rng.random_range(1_000_000u128..50_000_000u128);

            // Weighted level distribution: 40% DEBUG, 30% INFO, 20% WARN, 10% ERROR.
            let level_roll: u32 = self.rng.random_range(0..100);
            let level = if level_roll < 40 {
                "DEBUG"
            } else if level_roll < 70 {
                "INFO"
            } else if level_roll < 90 {
                "WARN"
            } else {
                "ERROR"
            };

            let target = BENCH_TARGETS[self.rng.random_range(0..BENCH_TARGETS.len())];

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

            let file = Some(BENCH_FILES[self.rng.random_range(0..BENCH_FILES.len())]);
            let line = Some(self.rng.random_range(1u32..500));

            CondensedEvent {
                timestamp_nanos: self.timestamp_nanos,
                level,
                target,
                message,
                fields,
                file,
                line,
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

        let events_retained =
            state.compressed_segments.event_count() + state.event_buffer.event_count();

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
        println!(
            "Seed: 0x{:X} | Events fed: {}",
            result.seed, result.events_fed
        );
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

        let config = RingBufferConfig::default()
            .with_max_ring_buffer_size_bytes(2 * 1024 * 1024);

        let result = run_benchmark(
            config,
            "max=2MiB, min_segment=128KiB, zstd_level=19",
            seed,
            num_events,
        );
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
                RingBufferConfig::default()
                    .with_max_ring_buffer_size_bytes(2 * 1024 * 1024),
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

    fn make_test_event(message: &str) -> CondensedEvent<'static> {
        CondensedEvent {
            timestamp_nanos: 1_000_000_000,
            level: "INFO",
            target: "test::target",
            message: message.to_string(),
            fields: encode_test_fields(&[("key1", "value1"), ("key2", "value2")]),
            file: Some("test.rs"),
            line: Some(42),
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

        let mut reader = segment.events().unwrap();
        let decoded = reader.next().unwrap().expect("should have one event");

        assert_eq!(decoded.timestamp_nanos, event.timestamp_nanos);
        assert_eq!(decoded.level, event.level);
        assert_eq!(decoded.target, event.target);
        assert_eq!(decoded.message, event.message);
        assert_eq!(decoded.fields, event.fields);
        assert_eq!(decoded.file, event.file);
        assert_eq!(decoded.line, event.line);

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
        // Build a valid columnar segment with 0 events.
        let mut buf = Vec::new();
        encode_varint(0, &mut buf); // string table: 0 entries
        encode_varint(0, &mut buf); // event count: 0
        encode_varint(0, &mut buf); // timestamps column: 0 bytes
        rle_encode(&[], &mut buf);  // levels: empty RLE
        rle_encode(&[], &mut buf);  // target_indices: empty RLE
        encode_varint(0, &mut buf); // messages column: 0 bytes
        encode_varint(0, &mut buf); // fields column: 0 bytes
        rle_encode(&[], &mut buf);  // file_indices: empty RLE
        encode_varint(0, &mut buf); // lines column: 0 bytes
        let mut reader = CompressedSegmentReader::new(buf).unwrap();
        assert!(reader.next().unwrap().is_none());
    }

    #[test]
    fn reader_truncated_framing_returns_error() {
        // A header that claims 100 events but has no column data.
        let mut buf = Vec::new();
        encode_varint(0, &mut buf);   // string table: 0 entries
        encode_varint(100, &mut buf); // event count: 100
        // Missing all columns -- should fail during header decode.
        assert!(
            CompressedSegmentReader::new(buf).is_err(),
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
        let event = CondensedEvent {
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
        let event = CondensedEvent {
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
