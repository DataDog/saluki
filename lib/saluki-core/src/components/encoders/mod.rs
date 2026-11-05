//! Encoder component basics.

use async_trait::async_trait;
use saluki_error::GenericError;
use snafu::Snafu;

use crate::{data_model::event::Event, topology::PayloadsDispatcher};

mod builder;
pub use self::builder::{EncoderBuilder, IncrementalEncoderBuilder};

mod context;
pub use self::context::EncoderContext;

/// Encoder process result.
#[allow(clippy::large_enum_variant)]
pub enum ProcessResult {
    /// The encoder processed the event successfully and is ready to process more events.
    Continue,

    /// The encoder can't process the event without flushing first.
    ///
    /// The caller should flush the encoder and try again to process the event.
    FlushRequired(Event),
}

/// An error that interrupted flushing an incremental encoder.
///
/// A flush can dispatch several payloads before one of them fails, so the error reports how many events had already
/// been dispatched, letting the caller account for them even though the flush failed.
#[derive(Debug, Snafu)]
#[snafu(display("Failed to flush encoder after dispatching {dispatched_events} events."))]
pub struct FlushError {
    dispatched_events: usize,
    source: GenericError,
}

impl FlushError {
    /// Creates a new `FlushError` for an error that occurred after `dispatched_events` events had been dispatched.
    pub fn new(dispatched_events: usize, source: GenericError) -> Self {
        Self {
            dispatched_events,
            source,
        }
    }

    /// Returns the number of events that were dispatched before the error occurred.
    pub fn dispatched_events(&self) -> usize {
        self.dispatched_events
    }

    /// Consumes the error, returning the underlying error.
    pub fn into_source(self) -> GenericError {
        self.source
    }
}

/// An encoder.
///
/// Encoders are the bridge between forwarders and the rest of the topology. They're responsible for encoding
/// telemetry events into output payloads that can then be forwarded. Most encoders are specific to a particular system
/// and not simply equivalent to a certain encoding or serialization format: while two encoders may both produce JSON,
/// they may produce different JSON formats such that one format only works for product A and the other only works for
/// product B. In essence, the process of taking telemetry events and sending them to product A ends up becoming the sum
/// of a specific encoder and forwarder combination.
///
/// Examples of typical encoders include Datadog Metrics, Events, and Service Checks.
#[async_trait]
pub trait Encoder {
    /// Runs the encoder.
    ///
    /// The encoder context provides access primarily to the event stream, used to receive events sent to the encoder,
    /// and the dispatcher, used to send payloads to the downstream forwarder in the topology, as well as other
    /// information such as the component context.
    ///
    /// Encoders are expected to run indefinitely until their event stream is terminated, or an error occurs.
    ///
    /// # Errors
    ///
    /// If an unrecoverable error occurs while running, an error is returned.
    async fn run(self: Box<Self>, context: EncoderContext) -> Result<(), GenericError>;
}

/// An incremental encoder.
///
/// Incremental encoders represent the essential operations of a encoder: adding events and flushing the resulting
/// payloads. Generally, encoders shouldn't need to concern themselves with the high-level details of being a
/// component within a topology, such as responding to health probes, handling graceful shutdown, and so on. Through
/// separating out the core encoder functionality from the component functionality, we can make it easier to implement
/// various encoder implementations while only maintaining a few common encoder components that actually _drive_ the
/// underlying encoder logic.
#[async_trait]
pub trait IncrementalEncoder {
    /// Process a single event.
    ///
    /// The encoder will process the event, attempting to add it to the current payload. If the encoder is unable to
    /// process the event without flushing first, `Ok(ProcessResult::FlushRequired(event))` is returned, containing the
    /// event that couldn't be processed. Otherwise, `Ok(ProcessResult::Continue)` is returned.
    ///
    /// # Errors
    ///
    /// If the encoder can't process the event due to an unrecoverable error, an error is returned.
    async fn process_event(&mut self, event: Event) -> Result<ProcessResult, GenericError>;

    /// Flush the encoder, finalizing all current payloads and sending them to the dispatcher.
    ///
    /// Returns the number of events contained in the payloads that were dispatched. An encoder can discard events it
    /// has accepted but can't send, such as when a payload exceeds its size limit and can't be split, and those aren't
    /// included in the count.
    ///
    /// # Errors
    ///
    /// If the encoder can't dispatch a payload, an error is returned. It reports the number of events that were
    /// dispatched earlier in the same flush, so they can still be accounted for.
    async fn flush(&mut self, dispatcher: &PayloadsDispatcher) -> Result<usize, FlushError>;
}
