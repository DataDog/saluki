#![allow(dead_code)]
use tracing::{Event, Subscriber};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};

mod codec;
mod event;
mod event_buffer;
mod pattern_cluster;
mod processor;
mod segment;
mod skeleton;
mod string_table;

use self::processor::{create_and_spawn_processor, WriterState};

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

#[cfg(test)]
mod benchmarks;

#[cfg(test)]
mod tests {
    use super::codec::*;
    use super::event::*;
    use super::event_buffer::*;
    use super::processor::ProcessorState;
    use super::segment::*;
    use super::RingBufferConfig;

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
        assert_eq!(state.compressed_segments.segment_count(), 0);

        // Add events until the ring buffer is under pressure and a segment is flushed.
        for i in 0..50 {
            let event = make_test_event(&format!("event number {}", i));
            state.add_event(&event).unwrap();
        }

        // With a 128-byte min segment size and 256-byte ring buffer, we should have flushed at least one segment.
        assert!(
            state.compressed_segments.segment_count() > 0,
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
        assert_eq!(state.compressed_segments.segment_count(), 0);
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
