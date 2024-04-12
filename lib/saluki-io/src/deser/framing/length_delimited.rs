use bytes::Buf;
use snafu::ResultExt as _;
use tracing::trace;

use saluki_core::topology::interconnect::EventBuffer;

use crate::{buf::ReadIoBuffer, deser::Decoder};

use super::{FailedToDecode, Framer, FramingError};

/// Frames incoming data by splitting data based on a fixed-size length delimiter.
///
/// All frames are prepended with a 4-byte integer, in little endian order, which indicates how much additional data is
/// included in the frame. This framer only supports frame lengths that fit within the given buffer, which is to say
/// that if the length described in the delimiter would exceed the current buffer, it is considered an invalid frame.
#[derive(Default)]
pub struct LengthDelimitedFramer;

impl<D: Decoder + 'static> Framer<D> for LengthDelimitedFramer {
    type Output = LengthDelimitedFraming<D>;

    fn with_decoder(self, decoder: D) -> Self::Output {
        LengthDelimitedFraming { inner: decoder }
    }
}

#[derive(Debug)]
pub struct LengthDelimitedFraming<D> {
    inner: D,
}

impl<D: Decoder> LengthDelimitedFraming<D> {
    fn decode_inner<B: ReadIoBuffer>(
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

            // Read the length of the frame if we have enough bytes, and then see if we have enough bytes for the
            // complete frame.
            if chunk.len() < 4 {
                break;
            }

            let frame_len = u32::from_le_bytes(chunk[0..4].try_into().unwrap()) as usize;
            if chunk.len() < frame_len + 4 {
                if is_eof {
                    // If we've hit EOF and we have a partial frame here, well, then... it's invalid.
                    return Err(FramingError::InvalidFrame {
                        buffer_len: buf.remaining(),
                    });
                } else {
                    break;
                }
            }

            // Frames cannot exceed the underlying buffer's capacity.
            if frame_len > buf.capacity() {
                return Err(FramingError::InvalidFrame {
                    buffer_len: buf.remaining(),
                });
            }

            // Advance past the length delimiter, and carve out the frame.
            buf.advance(4);
            let mut frame = buf.copy_to_bytes(frame_len);

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

impl<D> Decoder for LengthDelimitedFraming<D>
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
