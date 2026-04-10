use std::{
    sync::atomic::{AtomicBool, Ordering::AcqRel},
    time::{Duration, Instant},
};

use metrics::gauge;
use saluki_common::time::get_unix_timestamp_nanos;
use saluki_error::GenericError;
use thingbuf::mpsc;
use tracing::Event;

use super::event::{CondensedEvent, EventHydrator};
use super::event_buffer::EventBuffer;
use super::segment::CompressedSegments;
use super::RingBufferConfig;

const METRICS_PUBLISH_INTERVAL: Duration = Duration::from_secs(1);

struct Metrics {
    // Locally tracked values.
    events_total: u64,
    events_live: u64,
    segments_live: u64,
    segments_dropped_total: u64,
    compressed_bytes_live: u64,
    bytes_live: u64,
    oldest_timestamp_nanos: u128,

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
            oldest_timestamp_nanos: 0,
            last_publish: Instant::now(),
        }
    }

    fn publish(&mut self) {
        let event_coverage_dur = if self.oldest_timestamp_nanos > 0 {
            let now_ns = get_unix_timestamp_nanos();
            Duration::from_nanos((now_ns - self.oldest_timestamp_nanos) as u64)
        } else {
            Duration::ZERO
        };
        gauge!("crb_events_total").set(self.events_total as f64);
        gauge!("crb_events_live").set(self.events_live as f64);
        gauge!("crb_segments_live").set(self.segments_live as f64);
        gauge!("crb_segments_dropped_total").set(self.segments_dropped_total as f64);
        gauge!("crb_compressed_bytes_live").set(self.compressed_bytes_live as f64);
        gauge!("crb_bytes_live").set(self.bytes_live as f64);
        gauge!("crb_coverage_secs").set(event_coverage_dur.as_secs() as f64);
        self.last_publish = Instant::now();
    }

    fn publish_if_elapsed(&mut self) {
        if self.last_publish.elapsed() >= METRICS_PUBLISH_INTERVAL {
            self.publish();
        }
    }
}

/// Internal state for the processor.
pub struct ProcessorState {
    config: RingBufferConfig,
    pub compressed_segments: CompressedSegments,
    pub event_buffer: EventBuffer,
    metrics: Metrics,
}

impl ProcessorState {
    pub fn new(config: RingBufferConfig) -> Self {
        let event_buffer = EventBuffer::from_compression_level(config.compression_level);
        Self {
            config,
            compressed_segments: CompressedSegments::default(),
            event_buffer,
            metrics: Metrics::new(),
        }
    }

    pub fn total_size_bytes(&self) -> usize {
        self.compressed_segments.size_bytes() + self.event_buffer.size_bytes()
    }

    pub fn add_event(&mut self, event: &CondensedEvent) -> Result<(), GenericError> {
        // Encode the event into our event buffer.
        self.event_buffer.encode_event(event)?;
        self.metrics.events_total += 1;

        // If we haven't started tracking the oldest event timestamp, do so now.
        if self.metrics.oldest_timestamp_nanos == 0 {
            self.metrics.oldest_timestamp_nanos = self.event_buffer.oldest_timestamp_nanos();
        }

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
        self.metrics.segments_live = self.compressed_segments.segment_count() as u64;
        self.metrics.segments_dropped_total = self.compressed_segments.segments_dropped_total();
        self.metrics.compressed_bytes_live = self.compressed_segments.size_bytes() as u64;
        self.metrics.bytes_live = self.total_size_bytes() as u64;

        Ok(())
    }

    fn ensure_size_limits(&mut self) -> Result<(), GenericError> {
        let mut dropped_segments = false;

        // While our total size exceeds the maximum allowed size, remove the oldest segment.
        while self.total_size_bytes() > self.config.max_ring_buffer_size_bytes
            && self.compressed_segments.size_bytes() > 0
        {
            self.compressed_segments.drop_oldest_segment();
            dropped_segments = true;
        }

        if dropped_segments {
            self.metrics.oldest_timestamp_nanos = self.compressed_segments.oldest_timestamp_nanos();
        }

        Ok(())
    }
}

/// Shared state for the write side of the ring buffer.
pub struct WriterState {
    events_tx: mpsc::blocking::Sender<CondensedEvent>,
    events_tx_closed: AtomicBool,
}

impl WriterState {
    pub fn add_event(&self, event: &Event<'_>) {
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

pub fn create_and_spawn_processor(config: RingBufferConfig) -> WriterState {
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
