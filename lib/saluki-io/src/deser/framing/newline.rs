use bytes::Bytes;
use tracing::trace;

use super::{Framer, FramingError};
use crate::buf::ReadIoBuffer;

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

impl Framer for NewlineFramer {
    fn next_frame<'a, B: ReadIoBuffer>(&mut self, buf: &mut B, is_eof: bool) -> Result<Option<Bytes>, FramingError> {
        trace!(buf_len = buf.remaining(), "Processing buffer.");

        let chunk = buf.chunk();
        if chunk.is_empty() {
            return Ok(None);
        }

        trace!(chunk_len = chunk.len(), "Processing chunk.");

        // Search through the buffer for our delimiter.
        match find_newline(chunk) {
            Some(idx) => {
                // If we found the delimiter, then we can return the frame.
                let frame = buf.copy_to_bytes(idx);

                // Advance the buffer past the delimiter.
                buf.advance(1);

                Ok(Some(frame))
            }
            None => {
                // If we're not at EOF, then we can't do anything else right now.
                if !is_eof {
                    return Ok(None);
                }

                // If we're at EOF and we require the delimiter, then this is an invalid frame.
                if self.required_on_eof {
                    return Err(FramingError::InvalidFrame {
                        buffer_len: buf.remaining(),
                    });
                }

                Ok(Some(buf.copy_to_bytes(chunk.len())))
            }
        }
    }
}

fn find_newline(haystack: &[u8]) -> Option<usize> {
    memchr::memchr(b'\n', haystack)
}
