use bytes::Buf;
use snafu::ResultExt as _;
use tracing::trace;

use saluki_core::topology::interconnect::EventBuffer;

use crate::{buf::ReadIoBuffer, deser::Decoder};

use super::{FailedToDecode, Framer, FramingError};

/// Frames incoming data by splitting on newlines.
///
/// Only the newline character (0x0A, also known as "line feed") is used to split the payload into frames. If there are
/// carriage return characters (0x0D) in the payload, they will be included in the resulting frames.
#[derive(Default)]
pub struct NewlineFramer {
    required_on_eof: bool,
}

impl NewlineFramer {
    /// Whether or not the delimiter is required when EOF has been reached.
    ///
    /// This controls whether or not the frames must always be suffixed by the delimiter character.  In some cases, a
    /// delimiter is purely for separating multiple frames within a single payload, and when the final payload is sent,
    /// it may not include the delimiter at the end.
    ///
    /// If the framing configuration requires the delimiter to always be present, then set this to `true`.
    ///
    /// Defaults to `false`.
    pub fn required_on_eof(mut self, require: bool) -> Self {
        self.required_on_eof = require;
        self
    }
}

impl<D: Decoder + 'static> Framer<D> for NewlineFramer {
    type Output = NewlineFraming<D>;

    fn with_decoder(self, decoder: D) -> Self::Output {
        NewlineFraming {
            inner: decoder,
            required_on_eof: self.required_on_eof,
        }
    }
}

#[derive(Debug)]
pub struct NewlineFraming<D> {
    inner: D,
    required_on_eof: bool,
}

impl<D: Decoder> NewlineFraming<D> {
    fn decode_inner<B: ReadIoBuffer>(
        &mut self, buf: &mut B, events: &mut EventBuffer, is_eof: bool,
    ) -> Result<usize, FramingError<D>> {
        trace!(buf_len = buf.remaining(), "Received buffer.");

        let mut events_decoded = 0;

        loop {
            let chunk = buf.chunk();
            if chunk.is_empty() {
                break;
            }

            trace!(events_decoded, chunk_len = chunk.len(), "Received chunk.");

            // Search through the buffer for our delimiter.
            let (mut frame, advance_len) = match find_newline(chunk) {
                Some(idx) => (&chunk[..idx], idx + 1),
                None => {
                    // If we're not at EOF, then we can't do anything else right now.
                    if !is_eof {
                        break;
                    }

                    // If we're at EOF and we require the delimiter, then this is an invalid frame.
                    if self.required_on_eof {
                        return Err(FramingError::InvalidFrame {
                            buffer_len: buf.remaining(),
                        });
                    }

                    (chunk, chunk.len())
                }
            };

            // Pass the frame to the inner decoder.
            //
            // We specifically advance the buffer before checking the result, just to make sure we don't forget to
            // advance it.
            let frame_len = frame.len();
            let decode_result = self.inner.decode(&mut frame, events).context(FailedToDecode);
            buf.advance(advance_len);

            let event_count = decode_result?;
            trace!(frame_len, event_count, "Decoded frame.");

            // TODO: Emit a metric if `event_count` is zero, since that means we've decoded zero events _without_ an
            // error. In some cases, this is entirely fine (e.g. DogStatsD couldn't resolve the context due to string
            // interner being full) and so we don't want to emit an error -- it's intentional! -- but we should still
            // emit a metric to track that it's happening.

            events_decoded += event_count;
        }

        Ok(events_decoded)
    }
}

impl<D> Decoder for NewlineFraming<D>
where
    D: Decoder + 'static,
{
    type Error = FramingError<D>;

    fn decode<B: ReadIoBuffer>(&mut self, buf: &mut B, events: &mut EventBuffer) -> Result<usize, Self::Error> {
        self.decode_inner(buf, events, false)
    }

    fn decode_eof<B: ReadIoBuffer>(&mut self, buf: &mut B, events: &mut EventBuffer) -> Result<usize, Self::Error> {
        self.decode_inner(buf, events, true)
    }
}

fn find_newline(haystack: &[u8]) -> Option<usize> {
    memchr::memchr(b'\n', haystack)
}
