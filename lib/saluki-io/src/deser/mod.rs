use std::{fmt::Debug, io};

use bytes::{Buf, BufMut};
use metrics::{Counter, Histogram};
use snafu::{ResultExt as _, Snafu};
use tracing::{debug, trace};

use saluki_core::{buffers::BufferPool, components::metrics::MetricsBuilder, topology::interconnect::EventBuffer};

use crate::buf::ReadWriteIoBuffer;

use self::framing::Framer;

use super::{
    buf::ReadIoBuffer,
    net::{addr::ConnectionAddress, stream::Stream},
};

pub mod codec;
pub mod framing;

mod macros;
pub use self::macros::multi_framing;

pub trait Decoder: Debug {
    type Error: std::error::Error + 'static;

    fn decode<B: ReadIoBuffer>(&mut self, buf: &mut B, events: &mut EventBuffer) -> Result<usize, Self::Error>;

    fn decode_eof<B: ReadIoBuffer>(&mut self, buf: &mut B, events: &mut EventBuffer) -> Result<usize, Self::Error> {
        self.decode(buf, events)
    }
}

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum DeserializerError<D: Decoder> {
    #[snafu(display("I/O error: {}", source))]
    Io { source: io::Error },
    #[snafu(display("failed to decode events: {}", source))]
    FailedToDecode { source: D::Error },
    #[snafu(display("buffer full; cannot receive additional data ({} unread bytes in buffer)", remaining))]
    BufferFull { remaining: usize },
}

pub struct Deserializer<D, B> {
    stream: Stream,
    decoder: D,
    buffer_pool: B,
    eof: bool,
    eof_addr: Option<ConnectionAddress>,
    metrics: DeserializerMetrics,
}

impl<D, B> Deserializer<D, B>
where
    D: Decoder,
    B: BufferPool,
    B::Buffer: ReadWriteIoBuffer,
{
    fn new(stream: Stream, decoder: D, buffer_pool: B, metrics: DeserializerMetrics) -> Self {
        Self {
            stream,
            decoder,
            buffer_pool,
            eof: false,
            eof_addr: None,
            metrics,
        }
    }

    async fn decode_oneshot(
        &mut self, buffer: &mut B::Buffer, events: &mut EventBuffer,
    ) -> Result<(usize, usize, ConnectionAddress), DeserializerError<D>> {
        // If our buffer is full, we can't do any reads.
        if !buffer.has_remaining_mut() {
            return Err(DeserializerError::BufferFull {
                remaining: buffer.remaining(),
            });
        }

        // Try filling our buffer from the underlying reader first.
        debug!("About to receive data from the stream.");
        let (bytes_read, connection_addr) = self.stream.receive(buffer).await.context(Io)?;
        if bytes_read == 0 {
            self.eof = true;
            self.eof_addr = Some(connection_addr.clone());
        }

        self.metrics.bytes_received().increment(bytes_read as u64);
        self.metrics.bytes_received_size().record(bytes_read as f64);

        // When we're actually at EOF, or we're dealing with a connectionless stream, we try to decode in EOF mode.
        //
        // For connectionless streams, we always try to decode the buffer as if it's EOF, since it effectively _is_
        // always the end of file after a receive. For connection-oriented streams, we only want to do this once we've
        // actually hit true EOF.
        let reached_eof = self.eof || self.stream.is_connectionless();

        debug!(
            chunk_len = buffer.chunk().len(),
            chunk_cap = buffer.chunk_mut().len(),
            buffer_len = buffer.remaining(),
            buffer_cap = buffer.remaining_mut(),
            eof = reached_eof,
            "Received {} bytes from stream.",
            bytes_read
        );

        let mut total_events_decoded = 0;
        loop {
            if !buffer.has_remaining() {
                break;
            }

            let buf_start_len = buffer.remaining();

            let events_decoded = if reached_eof {
                self.decoder.decode_eof(buffer, events).context(FailedToDecode)?
            } else {
                self.decoder.decode(buffer, events).context(FailedToDecode)?
            };

            trace!(
                events_decoded,
                buf_start_len,
                buf_end_len = buffer.remaining(),
                "Decoded events."
            );

            if events_decoded == 0 {
                if buffer.has_remaining() {
                    // We decoded no events from the buffer, but there _is_ data in it. This means we hit an unexpected
                    // EOF condition, as more data is needed to decode a valid event, but it can never come as we're in
                    // oneshot mode.
                    return Err(DeserializerError::Io {
                        source: io::ErrorKind::UnexpectedEof.into(),
                    });
                }

                break;
            }

            total_events_decoded += events_decoded;
        }

        self.metrics.events_received().increment(total_events_decoded as u64);

        Ok((bytes_read, total_events_decoded, connection_addr))
    }

    pub async fn decode(
        &mut self, events: &mut EventBuffer,
    ) -> Result<(usize, ConnectionAddress), DeserializerError<D>> {
        if self.eof {
            return Ok((0, self.eof_addr.take().expect("EOF flag set but no EOF address")));
        }

        let mut buffer = self.buffer_pool.acquire().await;
        debug!(capacity = buffer.remaining_mut(), "Acquired buffer for decoding.");

        loop {
            // Do a oneshot decode, which does a single receive and tries to decode any events that exist in the buffer
            // afterwards.
            match self.decode_oneshot(&mut buffer, events).await {
                Ok((bytes_read, events_decoded, connection_addr)) => {
                    if events_decoded == 0 {
                        // When we're dealing with a connectionless stream, we _may_ not decode any events, since it's
                        // sometimes common for clients to probe UDP sockets by sending zero-byte payloads.
                        //
                        // Ignore this and continue trying to receive/decode.
                        if self.stream.is_connectionless() {
                            continue;
                        }

                        // If we decoded no events from the buffer, but we managed to read some data, then we might just
                        // need to receive some more data to get the complete payload.
                        //
                        // Continue trying to receive/decode.
                        //
                        // (A note here is that we might be here because the payload is too big for our buffer, and we
                        // only just received what we had left for remaining buffer capacity... but the next iteration
                        // will let us know that the buffer is full before it tries to receive again, so we'll fall
                        // through to the error logic further down.)
                        if bytes_read != 0 {
                            continue;
                        }
                    }

                    // We decoded some events successfully.
                    return Ok((events_decoded, connection_addr));
                }
                Err(e) => match e {
                    // If we're not dealing with a connectionless stream, then that means there's still a chance of more
                    // data coming that could allow properly decoding events from the buffer.
                    //
                    // Continue trying to receive/decode.
                    DeserializerError::Io { source }
                        if source.kind() == io::ErrorKind::UnexpectedEof && !self.stream.is_connectionless() =>
                    {
                        continue
                    }
                    // If we specifically failed to decode, then track that... but still forward the error.
                    e @ DeserializerError::FailedToDecode { .. } => {
                        self.metrics.decoder_errors().increment(1);
                        return Err(e);
                    }
                    e => return Err(e),
                },
            }
        }
    }
}

pub struct DeserializerBuilder<D = (), B = ()> {
    decoder: D,
    buffer_pool: B,
    metrics_builder: Option<MetricsBuilder>,
}

impl DeserializerBuilder {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            decoder: (),
            buffer_pool: (),
            metrics_builder: None,
        }
    }
}

impl<D, B> DeserializerBuilder<D, B> {
    pub fn with_framer_and_decoder<F2, D2>(self, framer: F2, decoder: D2) -> DeserializerBuilder<F2::Output, B>
    where
        F2: Framer<D2>,
        D2: Decoder,
    {
        DeserializerBuilder {
            decoder: framer.with_decoder(decoder),
            buffer_pool: self.buffer_pool,
            metrics_builder: self.metrics_builder,
        }
    }

    pub fn with_buffer_pool<B2>(self, buffer_pool: B2) -> DeserializerBuilder<D, B2>
    where
        B2: BufferPool,
        B2::Buffer: ReadWriteIoBuffer,
    {
        DeserializerBuilder {
            decoder: self.decoder,
            buffer_pool,
            metrics_builder: self.metrics_builder,
        }
    }

    pub fn with_metrics_builder(self, metrics_builder: MetricsBuilder) -> Self {
        Self {
            decoder: self.decoder,
            buffer_pool: self.buffer_pool,
            metrics_builder: Some(metrics_builder),
        }
    }
}

impl<D, B> DeserializerBuilder<D, B>
where
    D: Decoder,
    B: BufferPool,
    B::Buffer: ReadWriteIoBuffer,
{
    pub fn into_deserializer(self, stream: Stream) -> Deserializer<D, B> {
        Deserializer::new(
            stream,
            self.decoder,
            self.buffer_pool,
            self.metrics_builder.map(build_deserializer_metrics).unwrap_or_default(),
        )
    }
}

// TODO: We ought to be able to do this with `static_metrics!` but it doesn't support setting the metric name to one
// thing and the name of the field/accessor to another.... which would makes things less ergonomic. We also have a
// specific label for just our error metric, which is another thing the macro doesn't support.
struct DeserializerMetrics {
    events_received: Counter,
    bytes_received: Counter,
    bytes_received_size: Histogram,
    decoder_errors: Counter,
}

impl DeserializerMetrics {
    fn events_received(&self) -> &Counter {
        &self.events_received
    }

    fn bytes_received(&self) -> &Counter {
        &self.bytes_received
    }

    fn bytes_received_size(&self) -> &Histogram {
        &self.bytes_received_size
    }

    fn decoder_errors(&self) -> &Counter {
        &self.decoder_errors
    }
}

impl Default for DeserializerMetrics {
    fn default() -> Self {
        DeserializerMetrics {
            events_received: Counter::noop(),
            bytes_received: Counter::noop(),
            bytes_received_size: Histogram::noop(),
            decoder_errors: Counter::noop(),
        }
    }
}

fn build_deserializer_metrics(builder: MetricsBuilder) -> DeserializerMetrics {
    DeserializerMetrics {
        events_received: builder.register_counter("component_events_received_total"),
        bytes_received: builder.register_counter("component_bytes_received_total"),
        bytes_received_size: builder.register_histogram("component_bytes_received_size"),
        decoder_errors: builder.register_counter_with_labels("component_errors_total", &[("error_type", "decode")]),
    }
}
