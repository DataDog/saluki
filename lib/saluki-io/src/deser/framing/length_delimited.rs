use tracing::trace;

use super::{Framer, FramingError};
use crate::buf::ReadIoBuffer;

/// Frames incoming data by splitting data based on a fixed-size length delimiter.
///
/// All frames are prepended with a 4-byte integer, in little endian order, which indicates how much additional data is
/// included in the frame. This framer only supports frame lengths that fit within the given buffer, which is to say
/// that if the length described in the delimiter would exceed the current buffer, it is considered an invalid frame.
#[derive(Default)]
pub struct LengthDelimitedFramer;

impl Framer for LengthDelimitedFramer {
    fn next_frame<'a, B: ReadIoBuffer>(
        &mut self, buf: &'a B, is_eof: bool,
    ) -> Result<Option<(&'a [u8], usize)>, FramingError> {
        trace!(buf_len = buf.remaining(), "Received buffer.");

        let chunk = buf.chunk();
        if chunk.is_empty() {
            return Ok(None);
        }

        trace!(chunk_len = chunk.len(), "Received chunk.");

        // Read the length of the frame if we have enough bytes, and then see if we have enough bytes for the
        // complete frame.
        if chunk.len() < 4 {
            return Ok(None);
        }

        let frame_len = u32::from_le_bytes(chunk[0..4].try_into().unwrap()) as usize;
        let frame_len_with_length = frame_len + 4;
        if chunk.len() < frame_len_with_length {
            if is_eof {
                // If we've hit EOF and we have a partial frame here, well, then... it's invalid.
                return Err(FramingError::InvalidFrame {
                    buffer_len: buf.remaining(),
                });
            } else {
                return Ok(None);
            }
        }

        // Frames cannot exceed the underlying buffer's capacity.
        if frame_len > buf.capacity() {
            return Err(FramingError::InvalidFrame {
                buffer_len: buf.remaining(),
            });
        }

        // Return the data frame itself and the full length to advance the buffer.
        let frame = &chunk[4..frame_len_with_length];
        Ok(Some((frame, frame_len_with_length)))
    }
}
