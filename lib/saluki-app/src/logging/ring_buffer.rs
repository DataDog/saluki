#![allow(dead_code)]

use std::{
    collections::VecDeque,
    fmt::Write as _,
    io::{Read as _, Write},
    sync::atomic::{AtomicBool, Ordering::AcqRel},
};

use metrics::gauge;
use musli::{Decode, Encode};
use saluki_common::time::get_unix_timestamp_nanos;
use saluki_error::{ErrorContext as _, GenericError};
use thingbuf::mpsc;
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

    /// Additional structured fields as key-value pairs.
    pub fields: Vec<(&'a str, String)>,

    /// Source file where the event was logged.
    pub file: Option<&'a str>,

    /// Line number where the event was logged.
    pub line: Option<u32>,
}

impl<'a> CondensedEvent<'a> {
    /// Returns the number of bytes that this event occupies in memory.
    pub fn size_bytes(&self) -> usize {
        // Calculate the in-memory size of this event.
        //
        // We use the _capacity_ of the message and fields vectors to account for the actual memory allocated rather
        // than just the amount of it that we're _using_.
        let field_entry_size = std::mem::size_of::<(&'static str, String)>();
        std::mem::size_of::<Self>()
            + self.message.capacity()
            + self.fields.iter().map(|(_, v)| v.len()).sum::<usize>()
            + self.fields.capacity() * field_entry_size
    }

    /// Updates this serialized event from a `tracing::Event`.
    pub fn hydrate_from_event(&mut self, event: &Event<'_>) {
        self.message.clear();
        self.fields.clear();

        self.timestamp_nanos = get_unix_timestamp_nanos();

        let metadata = event.metadata();
        self.level = metadata.level().as_str();
        self.target = metadata.target();
        self.file = metadata.file();
        self.line = metadata.line();

        event.record(self);
    }
}

impl<'a> tracing::field::Visit for CondensedEvent<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{:?}", value);
        } else {
            self.fields.push((field.name(), format!("{:?}", value)));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.fields.push((field.name(), value.to_string()));
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
    fn events(&self) -> Result<CompressedSegmentReader, GenericError> {
        let mut decompressed = Vec::with_capacity(self.uncompressed_size);
        let mut decoder = zstd::Decoder::with_buffer(&self.compressed_data[..])?;
        decoder.read_to_end(&mut decompressed)?;

        Ok(CompressedSegmentReader::new(decompressed))
    }
}

/// A compressed segment reader.
struct CompressedSegmentReader {
    buf: Vec<u8>,
    idx: usize,
}

impl CompressedSegmentReader {
    fn new(buf: Vec<u8>) -> Self {
        Self { buf, idx: 0 }
    }

    fn next(&mut self) -> Result<Option<CondensedEvent<'_>>, GenericError> {
        // Read the length delimiter.
        //
        // If there's not enough data to read the length delimiter, or not enough data to read the event based
        // on the length delimiter we read, then we're all done.
        if self.idx + 4 > self.buf.len() {
            return Ok(None);
        }

        let event_len = u32::from_be_bytes(self.buf[self.idx..self.idx + 4].try_into().unwrap()) as usize;
        self.idx += 4;

        if self.idx + event_len > self.buf.len() {
            return Ok(None);
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
}

impl CompressedSegments {
    fn new() -> Self {
        CompressedSegments {
            segments: VecDeque::new(),
            total_compressed_size_bytes: 0,
            event_count: 0,
        }
    }

    fn size_bytes(&self) -> usize {
        self.total_compressed_size_bytes
    }

    fn event_count(&self) -> usize {
        self.event_count
    }

    fn add_segment(&mut self, segment: CompressedSegment) {
        println!(
            "Adding new segment: uncompressed={} compressed={} events={}",
            segment.uncompressed_size,
            segment.size_bytes(),
            segment.event_count()
        );
        self.total_compressed_size_bytes += segment.size_bytes();
        self.event_count += segment.event_count();
        self.segments.push_back(segment);
    }

    fn drop_oldest_segment(&mut self) {
        if let Some(segment) = self.segments.pop_front() {
            println!(
                "Dropped oldest segment: uncompressed={} compressed={} events={}",
                segment.uncompressed_size,
                segment.size_bytes(),
                segment.event_count()
            );
            self.total_compressed_size_bytes -= segment.size_bytes();
            self.event_count -= segment.event_count();
        }
    }
}

/// Internal state for the processor.
struct ProcessorState {
    config: RingBufferConfig,
    compressed_segments: CompressedSegments,
    event_buffer: EventBuffer,
}

impl ProcessorState {
    fn new(config: RingBufferConfig) -> Self {
        let event_buffer = EventBuffer::from_compression_level(config.compression_level);
        Self {
            config,
            compressed_segments: CompressedSegments::new(),
            event_buffer,
        }
    }

    fn total_size_bytes(&self) -> usize {
        self.compressed_segments.size_bytes() + self.event_buffer.size_bytes()
    }

    fn add_event(&mut self, event: &CondensedEvent<'static>) -> Result<(), GenericError> {
        // See if this event would fit into our event buffer when encoded.
        //
        // If not, we flush and compressed the event buffer and start a new one.
        if self.event_buffer.size_bytes() + event.size_bytes() > self.config.max_uncompressed_segment_size_bytes {
            let compressed_segment = self.event_buffer.flush()?;
            self.compressed_segments.add_segment(compressed_segment);
        }

        // Encode the event into our event buffer.
        self.event_buffer.encode_event(event)?;

        // Ensure that we're under our configured size limits by potentially dropping old segments.
        self.ensure_size_limits()?;

        let events_live = self.compressed_segments.event_count() + self.event_buffer.event_count();
        gauge!("compressed_ring_buffer_events_live").set(events_live as f64);

        Ok(())
    }

    fn ensure_size_limits(&mut self) -> Result<(), GenericError> {
        // While our total size exceeds the maximum allowed size, remove the oldest segment.
        while self.total_size_bytes() > self.config.max_ring_buffer_size_bytes
            && !self.compressed_segments.size_bytes() > 0
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
        match self.events_tx.send_ref() {
            Ok(mut slot) => {
                // Hydrate the serialized event in this send slot with our current event, and then update
                // our shared statistics with the new event to properly track the size of the event.
                (*slot).hydrate_from_event(event);
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
    println!("Processor running.");

    let mut events = 0;
    while let Some(event) = events_rx.recv_ref() {
        events += 1;

        if events % 100 == 0 {
            println!("Processed {} events.", events);
        }

        println!("Processed event: {:?}", event);

        if let Err(e) = processor_state.add_event(&event) {
            eprintln!("Failed to process event in compressed ring buffer: {}", e);
        }
    }
}
