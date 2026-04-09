use tracing::{Event, Subscriber};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};

mod codec;
mod event;
mod event_buffer;
mod processor;
mod segment;
mod skeleton;
mod string_table;

// Re-exports for the test module. These make `use super::*` in tests.rs pull in the
// types and functions needed across all submodules.
#[cfg(test)]
use self::codec::*;
#[cfg(test)]
use self::event::*;
#[cfg(test)]
use self::event_buffer::*;
#[cfg(test)]
use self::processor::ProcessorState;
use self::processor::{create_and_spawn_processor, WriterState};
#[cfg(test)]
use self::segment::*;

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
mod tests;
