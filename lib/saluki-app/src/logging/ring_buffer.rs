use std::{
    borrow::Cow,
    collections::VecDeque,
    io::Read as _,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use bincode::{DefaultOptions, Options};
use metrics::{counter, gauge, histogram};
use saluki_error::GenericError;
use serde::{Deserialize, Serialize};
use tracing::{Event, Subscriber};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};

const DEFAULT_MAX_UNCOMPRESSED_SEGMENT_SIZE_BYTES: usize = 256 * 1024;
const DEFAULT_COMPRESSION_LEVEL: i32 = zstd::DEFAULT_COMPRESSION_LEVEL;
const DEFAULT_MAX_RING_BUFFER_SIZE_BYTES: usize = 1024 * 1024;

/// Ring buffer configuration.
#[derive(Clone, Debug)]
pub struct RingBufferConfig {
    /// Maximum size of an uncompressed segment, in bytes, before compression is triggered.
    max_uncompressed_segment_size_bytes: usize,

    /// Compression level.
    ///
    /// Higher values provide better compression but slower performance. Valid range is 1-22.
    compression_level: i32,

    /// Maximum size of the ring buffer, in bytes.
    max_ring_buffer_size_bytes: usize,
}

impl RingBufferConfig {
    /// Sets the maximum size of an uncompressed segment, in bytes, before compression is triggered.
    ///
    /// Defaults to 256KiB.
    pub fn with_max_uncompressed_segment_size_bytes(mut self, max_uncompressed_segment_size_bytes: usize) -> Self {
        self.max_uncompressed_segment_size_bytes = max_uncompressed_segment_size_bytes;
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
            max_uncompressed_segment_size_bytes: DEFAULT_MAX_UNCOMPRESSED_SEGMENT_SIZE_BYTES,
            compression_level: DEFAULT_COMPRESSION_LEVEL,
            max_ring_buffer_size_bytes: DEFAULT_MAX_RING_BUFFER_SIZE_BYTES,
        }
    }
}

/// A compressed segment containing a batch of log events.
#[derive(Clone, Debug)]
struct CompressedSegment {
    event_count: usize,
    uncompressed_size: usize,
    compressed_data: Vec<u8>,
}

impl CompressedSegment {
    /// Decompress and deserialize the segment data into events.
    fn decompress_events(&self) -> Result<Vec<SerializedEvent>, GenericError> {
        let mut decompressed = Vec::with_capacity(self.uncompressed_size);

        let mut decoder = zstd::Decoder::with_buffer(&self.compressed_data[..])?;
        decoder.read_to_end(&mut decompressed)?;

        let bincode_opts = get_bincode_options();
        let events = bincode_opts.deserialize(&decompressed)?;
        Ok(events)
    }
}

/// A serializable representation of a log event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SerializedEvent {
    /// Unix timestamp when the event was logged, in seconds.
    pub timestamp: u64,

    /// Log level (e.g., "INFO", "WARN", "ERROR").
    pub level: Cow<'static, str>,

    /// Target module or component that generated the event.
    pub target: Cow<'static, str>,

    /// The main log message.
    pub message: String,

    /// Additional structured fields as key-value pairs.
    pub fields: Vec<(Cow<'static, str>, String)>,

    /// Source file where the event was logged.
    pub file: Option<Cow<'static, str>>,

    /// Line number where the event was logged.
    pub line: Option<u32>,
}

impl SerializedEvent {
    /// Creates a new `SerializedEvent` from a `tracing::Event`.
    pub fn from_event(event: &Event<'_>) -> Self {
        let metadata = event.metadata();

        let mut serialized_event = Self {
            timestamp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
            level: metadata.level().as_str().into(),
            target: metadata.target().into(),
            message: String::new(),
            fields: Vec::new(),
            file: metadata.file().map(Into::into),
            line: metadata.line(),
        };

        event.record(&mut serialized_event);

        serialized_event
    }
}

impl tracing::field::Visit for SerializedEvent {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else {
            self.fields.push((field.name().into(), format!("{:?}", value)));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push((field.name().into(), value.to_string()));
        }
    }
}

/// Buffer for collecting events before compression.
#[derive(Debug, Default)]
struct EventBuffer {
    events: Vec<SerializedEvent>,
    total_size_bytes: usize,
}

impl EventBuffer {
    fn len(&self) -> usize {
        self.events.len()
    }

    fn add_event(&mut self, event: SerializedEvent) {
        let event_size_bytes = event.message.len()
            + event.target.len()
            + event.level.len()
            + event.fields.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>()
            + event.file.as_ref().map(|f| f.len()).unwrap_or(0)
            + std::mem::size_of::<SerializedEvent>();

        histogram!("compressed_log_buffer_event_size_bytes").record(event_size_bytes as f64);

        self.total_size_bytes += event_size_bytes;
        self.events.push(event);
    }

    fn compress(&mut self, compression_level: i32) -> Result<Option<CompressedSegment>, GenericError> {
        if self.events.is_empty() {
            return Ok(None);
        }

        counter!("compressed_log_buffer_compressed_segments_total").increment(1);

        let bincode_opts = get_bincode_options();
        let serialized = bincode_opts.serialize(&self.events)?;
        let uncompressed_size = serialized.len();

        let compressed = zstd::encode_all(&serialized[..], compression_level)?;
        let compressed_size = compressed.len();

        let compression_ratio = if uncompressed_size > 0 {
            compressed_size as f64 / uncompressed_size as f64
        } else {
            1.0
        };
        histogram!("compressed_log_buffer_compression_ratio").record(compression_ratio);

        let segment = CompressedSegment {
            uncompressed_size,
            compressed_data: compressed,
            event_count: self.events.len(),
        };

        self.events.clear();
        self.total_size_bytes = 0;

        Ok(Some(segment))
    }
}

fn get_bincode_options() -> impl Options {
    DefaultOptions::new().with_varint_encoding().with_little_endian()
}

/// Ring buffer internal state.
#[derive(Debug)]
struct RingBufferState {
    config: RingBufferConfig,
    segments: VecDeque<CompressedSegment>,
    current_buffer: EventBuffer,
    total_compressed_bytes: usize,
    total_compressed_events: usize,
    dropped_segments: usize,
}

impl RingBufferState {
    fn new(config: RingBufferConfig) -> Self {
        Self {
            config,
            segments: VecDeque::new(),
            current_buffer: EventBuffer::default(),
            total_compressed_bytes: 0,
            total_compressed_events: 0,
            dropped_segments: 0,
        }
    }

    fn total_size_bytes(&self) -> usize {
        self.current_buffer.total_size_bytes + self.total_compressed_bytes
    }

    fn total_event_count(&self) -> usize {
        self.current_buffer.len() + self.total_compressed_events
    }

    fn should_compress_current_buffer(&self) -> bool {
        self.current_buffer.total_size_bytes > self.config.max_uncompressed_segment_size_bytes
    }

    fn add_event(&mut self, event: SerializedEvent) {
        counter!("compressed_log_buffer_events_total").increment(1);
        self.current_buffer.add_event(event);

        // Before checking if we've exceeded our maximum size limit, see if we can/should compress the current buffer.
        if self.should_compress_current_buffer() {
            match self.current_buffer.compress(self.config.compression_level) {
                Ok(Some(segment)) => self.add_compressed_segment(segment),
                Ok(None) => {}
                // We explicitly log via `eprintln!` to avoid reentrancy with logging.
                Err(e) => eprintln!("Failed to compress buffer: {}", e),
            }
        }

        gauge!("compressed_log_buffer_size_bytes_live").set(self.total_size_bytes() as f64);
        gauge!("compressed_log_buffer_events_live").set(self.total_event_count() as f64);
    }

    fn add_compressed_segment(&mut self, segment: CompressedSegment) {
        self.total_compressed_bytes += segment.compressed_data.len();
        self.total_compressed_events += segment.event_count;
        self.segments.push_back(segment);

        counter!("compressed_log_buffer_compressed_segments_added_total").increment(1);

        loop {
            if self.total_size_bytes() > self.config.max_ring_buffer_size_bytes {
                match self.segments.pop_front() {
                    Some(old_segment) => {
                        self.total_compressed_bytes -= old_segment.compressed_data.len();
                        self.total_compressed_events -= old_segment.event_count;
                        self.dropped_segments += 1;
                        counter!("compressed_log_buffer_compressed_segments_dropped_total").increment(1);
                    }
                    None => unreachable!("Cannot exceed maximum ring buffer size with no compressed segments."),
                }
            } else {
                break;
            }
        }

        gauge!("compressed_log_buffer_compressed_segments_live").set(self.segments.len() as f64);
    }
}

/// A read-only view into the ring buffer.
#[derive(Clone)]
pub struct RingBufferView {
    state: Arc<Mutex<RingBufferState>>,
}

impl RingBufferView {
    /// Get all events currently in the ring buffer, both compressed and uncompressed.
    ///
    /// Events are returned in chronological order (oldest first).
    pub fn get_events(&self) -> Vec<SerializedEvent> {
        let state = self.state.lock().unwrap();
        let mut all_events = Vec::new();

        // First, get events from compressed segments (oldest)
        for segment in &state.segments {
            match segment.decompress_events() {
                Ok(events) => all_events.extend(events),
                Err(e) => {
                    eprintln!("Failed to decompress compressed segment: {}", e);
                }
            }
        }

        // Then, add events from current uncompressed buffer (newest)
        all_events.extend(state.current_buffer.events.clone());

        all_events
    }

    #[cfg(test)]
    fn state(&self) -> &Mutex<RingBufferState> {
        &self.state
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
#[derive(Clone)]
pub struct CompressedRingBuffer {
    state: Arc<Mutex<RingBufferState>>,
}

impl CompressedRingBuffer {
    /// Creates a new `CompressedRingBuffer` with the given configuration.
    pub fn with_config(config: RingBufferConfig) -> Self {
        Self {
            state: Arc::new(Mutex::new(RingBufferState::new(config))),
        }
    }

    /// Creates a new `CompressedRingBuffer` with the default configuration.
    pub fn new() -> Self {
        Self::with_config(RingBufferConfig::default())
    }

    /// Create a read-only view into this ring buffer.
    pub fn view(&self) -> RingBufferView {
        RingBufferView {
            state: Arc::clone(&self.state),
        }
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
        let serialized_event = SerializedEvent::from_event(event);

        if let Ok(mut state) = self.state.lock() {
            state.add_event(serialized_event);
        }
    }
}

#[cfg(test)]
mod tests {
    use tracing::info;
    use tracing::{dispatcher, Dispatch};
    use tracing_subscriber::{layer::SubscriberExt, Registry};

    use super::*;

    fn create_ring_buffer() -> (Dispatch, CompressedRingBuffer) {
        let config = RingBufferConfig::default()
            .with_max_uncompressed_segment_size_bytes(512)
            .with_compression_level(1)
            .with_max_ring_buffer_size_bytes(2048);

        create_ring_buffer_with_config(config)
    }

    fn create_ring_buffer_with_config(config: RingBufferConfig) -> (Dispatch, CompressedRingBuffer) {
        let ring_buffer = CompressedRingBuffer::with_config(config);
        let subscriber = Registry::default().with(ring_buffer.clone());

        (Dispatch::new(subscriber), ring_buffer)
    }

    #[test]
    fn test_ring_buffer_config_default() {
        let config = RingBufferConfig::default();
        assert_eq!(
            config.max_uncompressed_segment_size_bytes,
            DEFAULT_MAX_UNCOMPRESSED_SEGMENT_SIZE_BYTES
        );
        assert_eq!(config.compression_level, DEFAULT_COMPRESSION_LEVEL);
        assert_eq!(config.max_ring_buffer_size_bytes, DEFAULT_MAX_RING_BUFFER_SIZE_BYTES);
    }

    #[test]
    fn test_compressed_segment_roundtrip() {
        // Manually write a number of serialized events to an event buffer, compress it, and then ensure the compressed segment
        // returns the same serialized events after decompression.
        let mut input_events = Vec::new();
        for i in 0..10 {
            input_events.push(SerializedEvent {
                timestamp: i,
                level: "INFO".into(),
                target: "test".into(),
                message: "hello world!".into(),
                fields: Vec::new(),
                file: Some("test.rs".into()),
                line: Some(10),
            });
        }

        let mut event_buffer = EventBuffer::default();
        for input_event in &input_events {
            event_buffer.add_event(input_event.clone());
        }

        let compressed_segment = event_buffer
            .compress(DEFAULT_COMPRESSION_LEVEL)
            .expect("should not fail to compress event buffer")
            .expect("event buffer should have contained events");

        let decompressed_events = compressed_segment
            .decompress_events()
            .expect("should not fail to decompress events");

        assert_eq!(decompressed_events, input_events);
    }

    #[test]
    fn test_ring_buffer_basic_roundtrip() {
        let (dispatcher, ring_buffer) = create_ring_buffer();
        let view = ring_buffer.view();

        // Ring buffer should start out empty.
        let initial_events = view.get_events();
        assert!(initial_events.is_empty());

        // Add a single log event.
        dispatcher::with_default(&dispatcher, || {
            info!(key = "value", "test message");
        });

        // Ensure we get the same event back.
        let events = view.get_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "test message");
        assert_eq!(events[0].level, "INFO");
        assert_eq!(events[0].fields[0].0, "key");
        assert_eq!(events[0].fields[0].1, "value");
    }

    #[test]
    fn test_ring_buffer_compressed_roundtrip() {
        let (dispatcher, ring_buffer) = create_ring_buffer();
        let view = ring_buffer.view();

        // Ring buffer should start out empty.
        let initial_events = view.get_events();
        assert!(initial_events.is_empty());

        // Add a bunch of log events to ensure we have at least one compressed segment.
        let large_message = "x".repeat(200);
        dispatcher::with_default(&dispatcher, || {
            for i in 0..5 {
                info!(key = i, "{} {}", large_message, i);
            }
        });

        {
            let buffer_state = view.state().lock().unwrap();
            assert!(!buffer_state.segments.is_empty());
        }

        // Ensure we get the same events back.
        let events = view.get_events();
        assert_eq!(events.len(), 5);
        for (i, event) in events.iter().enumerate().take(5) {
            assert_eq!(event.message, format!("{} {}", large_message, i));
            assert_eq!(event.level, "INFO");
            assert_eq!(event.fields[0].0, "key");
            assert_eq!(event.fields[0].1, i.to_string());
        }
    }

    #[test]
    fn test_ring_buffer_size_limit() {
        const TOTAL_SIZE_BYTES: usize = 1000;

        // Create a ring buffer with a small uncompressed segment size _and_ small maximum ring buffer size
        // to force compression of small segments and also force the ring buffer to grow beyond its maximum size
        // and be forced to delete old compressed segments.
        let config = RingBufferConfig::default()
            .with_max_uncompressed_segment_size_bytes(100)
            .with_compression_level(1)
            .with_max_ring_buffer_size_bytes(TOTAL_SIZE_BYTES);
        let (dispatcher, ring_buffer) = create_ring_buffer_with_config(config);
        let view = ring_buffer.view();

        // Continuously add events to the buffer.
        //
        // After every log event, we check to ensure that the total ring buffer size is at or below the maximum size. We'll
        // repeat this until we've dropped at least 10 segments, mostly just to ensure that we've exercised the size limit
        // logic sufficiently.
        let large_message = "foo".repeat(20);

        let mut i = 0;
        let mut dropped_segments = 0;
        while dropped_segments < 10 {
            i += 1;

            dispatcher::with_default(&dispatcher, || {
                info!(key = i, "message {}", large_message);
            });

            // Check to make sure we haven't exceeded the maximum ring buffer size, and track the number of dropped segments.
            let buffer_state = view.state().lock().unwrap();
            assert!(buffer_state.total_size_bytes() <= TOTAL_SIZE_BYTES);
            dropped_segments = buffer_state.dropped_segments;
        }
    }

    #[test]
    fn test_ring_buffer_view_read_only() {
        let (dispatcher, ring_buffer) = create_ring_buffer();
        let view = ring_buffer.view();

        // Add a single log event.
        dispatcher::with_default(&dispatcher, || {
            info!("test message");
        });

        // Ensure that multiple calls to `get_events` don't change the state, consume events, etc.
        let events_first = view.get_events();
        let events_second = view.get_events();
        let events_third = view.get_events();

        // All calls should have emitted the exact same thing.
        assert_eq!(events_first, events_second);
        assert_eq!(events_second, events_third);
        assert_eq!(events_first, events_third);
    }
}
