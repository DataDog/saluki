use std::{
    collections::VecDeque,
    fmt::Write as _,
    io::{Read as _, Write},
    sync::atomic::{AtomicBool, Ordering::AcqRel},
    time::{Duration, Instant},
};

use metrics::gauge;
use musli::{Decode, Encode};
use saluki_common::time::get_unix_timestamp_nanos;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use thingbuf::mpsc;
use tracing::{Event, Subscriber};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};

const DEFAULT_MIN_UNCOMPRESSED_SEGMENT_SIZE_BYTES: usize = 256 * 1024;
const DEFAULT_COMPRESSION_LEVEL: i32 = 11; //zstd::DEFAULT_COMPRESSION_LEVEL;
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

/// Writes a varint-length-prefixed byte slice into `buf`.
fn write_length_prefixed(buf: &mut Vec<u8>, data: &[u8]) {
    encode_varint(data.len(), buf);
    buf.extend_from_slice(data);
}

/// A condensed, serializable representation of a log event.
#[derive(Clone, Debug, Default, Eq, PartialEq, Encode, Decode)]
#[musli(packed)]
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

struct EventBuffer {
    scratch_buf: Vec<u8>,
    encoded_buf: Vec<u8>,
    event_count: usize,
    compression_level: i32,
}

impl EventBuffer {
    fn from_compression_level(compression_level: i32) -> Self {
        EventBuffer {
            scratch_buf: Vec::new(),
            encoded_buf: Vec::new(),
            event_count: 0,
            compression_level,
        }
    }

    fn size_bytes(&self) -> usize {
        self.encoded_buf.len()
    }

    fn event_count(&self) -> usize {
        self.event_count
    }

    fn clear(&mut self) {
        self.encoded_buf.clear();
        self.event_count = 0;
    }

    fn encode_event(&mut self, event: &CondensedEvent<'_>) -> Result<(), GenericError> {
        // Encode our event.
        self.scratch_buf.clear();
        musli::packed::to_writer(&mut self.scratch_buf, event).error_context("Failed to encode condensed event.")?;

        // Write the event to our encoded buffer, with a 32-bit length prefix.
        let event_length = (self.scratch_buf.len() as u32).to_be_bytes();
        self.encoded_buf.extend_from_slice(&event_length[..]);
        self.encoded_buf.extend_from_slice(&self.scratch_buf[..]);

        self.event_count += 1;

        Ok(())
    }

    fn flush(&mut self) -> Result<CompressedSegment, GenericError> {
        // Compress the encoded buffer.
        let mut encoder =
            zstd::Encoder::new(Vec::new(), self.compression_level).error_context("Failed to create ZSTD encoder.")?;

        encoder
            .write_all(&self.encoded_buf)
            .error_context("Failed to compress encoded buffer.")?;

        let compressed_data = encoder.finish().error_context("Failed to finalize ZSTD encoder.")?;

        // Create a new compressed segment.
        let compressed_segment = CompressedSegment {
            event_count: self.event_count,
            uncompressed_size: self.encoded_buf.len(),
            compressed_data,
        };

        // Clear the event buffer.
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

        Ok(CompressedSegmentReader::new(decompressed))
    }
}

/// A compressed segment reader.
#[allow(dead_code)] // Will be used by the read API; exercised in tests.
struct CompressedSegmentReader {
    buf: Vec<u8>,
    idx: usize,
}

#[allow(dead_code)] // Will be used by the read API; exercised in tests.
impl CompressedSegmentReader {
    fn new(buf: Vec<u8>) -> Self {
        Self { buf, idx: 0 }
    }

    fn next(&mut self) -> Result<Option<CondensedEvent<'_>>, GenericError> {
        // If there's not enough data to read the length delimiter, we're done.
        if self.idx + 4 > self.buf.len() {
            return Ok(None);
        }

        let event_len = u32::from_be_bytes(self.buf[self.idx..self.idx + 4].try_into().unwrap()) as usize;
        self.idx += 4;

        // If the length prefix points past the end of the buffer, the data is truncated/corrupt.
        if self.idx + event_len > self.buf.len() {
            return Err(generic_error!(
                "Truncated event: expected {} bytes at offset {}, but only {} remain",
                event_len,
                self.idx,
                self.buf.len() - self.idx
            ));
        }

        // Read out the actual event.
        let event = musli::packed::from_slice(&self.buf[self.idx..self.idx + event_len])?;
        self.idx += event_len;
        Ok(Some(event))
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
        let mut reader = CompressedSegmentReader::new(Vec::new());
        assert!(reader.next().unwrap().is_none());
    }

    #[test]
    fn reader_truncated_framing_returns_error() {
        // A length prefix claiming 1000 bytes, but only 2 bytes of data follow.
        let mut buf = Vec::new();
        buf.extend_from_slice(&1000u32.to_be_bytes());
        buf.extend_from_slice(&[0u8, 0u8]);

        let mut reader = CompressedSegmentReader::new(buf);
        let result = reader.next();
        assert!(result.is_err(), "expected error on truncated data, got {:?}", result);
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
