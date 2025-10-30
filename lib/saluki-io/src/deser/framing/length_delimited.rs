use tracing::trace;

use super::{Framer, FramingError};
use crate::deser::framing::{BufferView, RawBuffer};

/// Frames incoming data by splitting data based on a fixed-size length delimiter.
///
/// All frames are prepended with a 4-byte integer, in little endian order, which indicates how much additional data is
/// included in the frame. This framer only supports frame lengths that fit within the given buffer, which is to say
/// that if the length described in the delimiter would exceed the current buffer, it is considered an invalid frame.
pub struct LengthDelimitedFramer {
    max_frame_size: usize,
}

impl LengthDelimitedFramer {
    /// Sets the maximum frame size that this framer will accept.
    ///
    /// This controls whether or not a frame is rejected after decoding the frame length delimiter. This should
    /// generally be used if I/O buffers are fixed in size and cannot be expanded, as this represents the effective
    /// upper bound on the size of frames that could be received with such buffers.
    ///
    /// Defaults to `u32::MAX`.
    pub const fn with_max_frame_size(mut self, max_frame_size: usize) -> Self {
        self.max_frame_size = max_frame_size;
        self
    }
}

impl Framer for LengthDelimitedFramer {
    fn next_frame<'buf>(&self, buf: RawBuffer<'buf>, is_eof: bool) -> Result<Option<BufferView<'buf>>, FramingError> {
        trace!(buf_len = buf.len(), "Processing buffer.");

        if buf.is_empty() {
            return Ok(None);
        }

        // See if there's enough data to read the frame length.
        if buf.len() < 4 {
            return if is_eof {
                Err(FramingError::PartialFrame {
                    needed: 4,
                    remaining: buf.len(),
                })
            } else {
                Ok(None)
            };
        }

        // See if we have enough data to read the full frame.
        let frame_len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        let frame_len_with_length = frame_len.saturating_add(4);
        if frame_len_with_length > self.max_frame_size {
            return Err(oversized_frame_err(frame_len));
        }

        if buf.len() < frame_len_with_length {
            return if is_eof {
                // If we've hit EOF and we have a partial frame here, well, then... it's invalid.
                Err(FramingError::PartialFrame {
                    needed: frame_len_with_length,
                    remaining: buf.len(),
                })
            } else {
                Ok(None)
            };
        }

        // Carve out the entire frame, and then adjust our view to start after the delimiter.
        let mut frame = buf.partial(frame_len_with_length);
        frame.skip(4);

        Ok(Some(frame))
    }
}

const fn oversized_frame_err(frame_len: usize) -> FramingError {
    FramingError::InvalidFrame {
        frame_len,
        reason: "frame length exceeds buffer capacity",
    }
}

impl Default for LengthDelimitedFramer {
    fn default() -> Self {
        Self {
            // Use `u32::MAX` since that's the maximum frame size that can be represented in the length delimiter.
            max_frame_size: usize::try_from(u32::MAX).unwrap_or(usize::MAX),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_delimited_payload(inner: &[u8], with_newline: bool) -> Vec<u8> {
        let payload_len = if with_newline { inner.len() + 1 } else { inner.len() };

        get_delimited_payload_with_fixed_length(inner, payload_len as u32, with_newline)
    }

    fn get_delimited_payload_with_fixed_length(inner: &[u8], frame_len: u32, with_newline: bool) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend(&frame_len.to_le_bytes());
        payload.extend(inner);
        if with_newline {
            payload.push(b'\n');
        }

        payload
    }

    #[test]
    fn basic() {
        let payload = b"hello, world!";
        let buf = get_delimited_payload(payload, false);

        let framer = LengthDelimitedFramer::default();
        let frame = framer
            .next_frame(RawBuffer::new(&buf), false)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(&frame[..], payload);
        assert_eq!(buf.len(), frame.buf_len(), "frame should consume entire buffer");
    }

    #[test]
    fn partial_read() {
        // We create a full, valid frame and then take incrementally larger slices of it, ensuring that we can't
        // actually read the frame until we give the framer the entire buffer.
        let payload = b"hello, world!";
        let buf = get_delimited_payload(payload, false);

        let framer = LengthDelimitedFramer::default();

        // Try reading a frame from a buffer that doesn't have enough bytes for the length delimiter itself.
        let mut no_delimiter_buf = buf.clone();
        no_delimiter_buf.truncate(3);

        let maybe_frame = framer
            .next_frame(RawBuffer::new(&no_delimiter_buf), false)
            .expect("should not fail to read from payload");
        assert!(maybe_frame.is_none());

        // Try reading a frame from a buffer that has enough bytes for the length delimiter, but not as many bytes as
        // the length delimiter indicates.
        let mut delimiter_but_partial_buf = buf.clone();
        delimiter_but_partial_buf.truncate(7);

        let maybe_frame = framer
            .next_frame(RawBuffer::new(&delimiter_but_partial_buf), false)
            .expect("should not fail to read from payload");
        assert!(maybe_frame.is_none());

        // Now try reading a frame from the original buffer, which should succeed.
        let frame = framer
            .next_frame(RawBuffer::new(&buf), false)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(&frame[..], payload);
        assert_eq!(buf.len(), frame.buf_len(), "frame should consume entire buffer");
    }

    #[test]
    fn partial_read_eof() {
        // We create a full, valid frame and then take incrementally larger slices of it, ensuring that we can't
        // actually read the frame until we give the framer the entire buffer.
        let payload = b"hello, world!";
        let buf = get_delimited_payload(payload, false);
        let frame_len = buf.len();

        let framer = LengthDelimitedFramer::default();

        // Try reading a frame from a buffer that doesn't have enough bytes for the length delimiter itself.
        let mut no_delimiter_buf = buf.clone();
        no_delimiter_buf.truncate(3);

        let maybe_frame = framer.next_frame(RawBuffer::new(&no_delimiter_buf), true);
        assert_eq!(
            maybe_frame,
            Err(FramingError::PartialFrame {
                needed: 4,
                remaining: 3
            })
        );

        // Try reading a frame from a buffer that has enough bytes for the length delimiter, but not as many bytes as
        // the length delimiter indicates.
        let mut delimiter_but_partial_buf = buf.clone();
        delimiter_but_partial_buf.truncate(7);

        let maybe_frame = framer.next_frame(RawBuffer::new(&delimiter_but_partial_buf), true);
        assert_eq!(
            maybe_frame,
            Err(FramingError::PartialFrame {
                needed: frame_len,
                remaining: 7
            })
        );

        // Now try reading a frame from the original buffer, which should succeed.
        let frame = framer
            .next_frame(RawBuffer::new(&buf), true)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(&frame[..], payload);
        assert_eq!(buf.len(), frame.buf_len(), "frame should consume entire buffer");
    }

    #[test]
    fn oversized_frame() {
        // We create an invalid frame with a length that exceeds the overall length of the resulting buffer.
        let payload = b"hello, world!";
        let buf = get_delimited_payload_with_fixed_length(payload, 32, false);

        let framer = LengthDelimitedFramer::default().with_max_frame_size(24);

        // We should get back an error that the frame is invalid, and the original buffer should not be altered at all.
        let maybe_frame = framer.next_frame(RawBuffer::new(&buf), false);
        assert_eq!(maybe_frame, Err(oversized_frame_err(32)));
    }
}
