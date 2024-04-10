use bytes::Buf;
use snafu::ResultExt as _;
use tracing::trace;

use saluki_core::topology::interconnect::EventBuffer;

use crate::deser::Decoder;

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
            last_idx: 0,
            required_on_eof: self.required_on_eof,
        }
    }
}

#[derive(Debug)]
pub struct NewlineFraming<D> {
    inner: D,
    last_idx: usize,
    required_on_eof: bool,
}

impl<D: Decoder> NewlineFraming<D> {
    fn decode_inner<B: Buf>(
        &mut self, buf: &mut B, events: &mut EventBuffer, is_eof: bool,
    ) -> Result<usize, FramingError<D>> {
        trace!(buf_len = buf.remaining(), "framed buffer received");

        let mut events_decoded = 0;

        loop {
            let chunk = buf.chunk();
            if chunk.is_empty() {
                break;
            }

            trace!(chunk_len = chunk.len(), "chunk acquired from input");

            // Do a sanity check that our internal index doesn't extend past the end of the buffer. This _shouldn't_
            // happen unless the inner decoder, or something above the framer, is messing with the buffer... but better
            // to be safe and avoid a panic if we can help it.
            if self.last_idx > chunk.len() {
                self.last_idx = 0;
            }

            // Search through the buffer for our delimiter. We slice the buffer chunk by starting at where we last left
            // off, avoiding searching over the same bytes again.
            let mut frame = match find_newline(&chunk[self.last_idx..]) {
                Some(idx) => {
                    // We found our delimiter. Do a small amount of math to figure out how many bytes to actually carve
                    // out, based on the fact the index we just got is relative to `self.last_idx`.
                    let frame_len = self.last_idx + idx;

                    // Extract our frame by itself, which advances the buffer by the length of the frame, and then do an
                    // additional advance to move past the delimiter.
                    //
                    // Finally, reset our internal index.
                    let frame = buf.copy_to_bytes(frame_len);
                    buf.advance(1);

                    self.last_idx = 0;

                    frame
                }
                None => {
                    // We didn't find our delimiter.
                    //
                    // If we're at EOF, then we just try to decode what we have left in the buffer and see what happens.
                    // Otherwise, we track how many bytes we just searched over, update our internal index, and break
                    // out of the loop so the caller can wait for more data.
                    if is_eof {
                        if self.required_on_eof {
                            return Err(FramingError::InvalidFrame {
                                buffer_len: buf.remaining(),
                            });
                        }

                        buf.copy_to_bytes(chunk.len())
                    } else {
                        self.last_idx = chunk.len();
                        break;
                    }
                }
            };

            // Pass the frame to the inner decoder.
            let frame_len = frame.len();
            let event_count = self.inner.decode(&mut frame, events).context(FailedToDecode)?;
            if event_count == 0 {
                // It's not normal to encounter a full frame and be unable to decode even a single event from it.

                // TODO: do we return `Ok(n)` if we've decoded some events successfully, or do we return an error about
                // an undecodable frame? or maybe we log the error if n > 0 and return Ok(n)?
                return Err(FramingError::UndecodableFrame { frame_len });
            }

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

    fn decode<B: Buf>(&mut self, buf: &mut B, events: &mut EventBuffer) -> Result<usize, Self::Error> {
        self.decode_inner(buf, events, false)
    }

    fn decode_eof<B: Buf>(&mut self, buf: &mut B, events: &mut EventBuffer) -> Result<usize, Self::Error> {
        self.decode_inner(buf, events, true)
    }
}

fn find_newline(haystack: &[u8]) -> Option<usize> {
    memchr::memchr(b'\n', haystack)
}
