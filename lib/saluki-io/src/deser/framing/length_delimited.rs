use bytes::Bytes;
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
    fn next_frame<B: ReadIoBuffer>(&mut self, buf: &mut B, is_eof: bool) -> Result<Option<Bytes>, FramingError> {
        trace!(buf_len = buf.remaining(), "Processing buffer.");

        let chunk = buf.chunk();
        if chunk.is_empty() {
            return Ok(None);
        }

        trace!(chunk_len = chunk.len(), "Processing chunk.");

        // See if there's enough data to read the frame length.
        if chunk.len() < 4 {
            return if is_eof {
                Err(FramingError::PartialFrame {
                    needed: 4,
                    remaining: chunk.len(),
                })
            } else {
                Ok(None)
            };
        }

        // See if we have enough data to read the full frame.
        let frame_len = u32::from_le_bytes(chunk[0..4].try_into().unwrap()) as usize;
        let frame_len_with_length = frame_len.saturating_add(4);
        if frame_len_with_length > buf.capacity() {
            return Err(oversized_frame_err(frame_len));
        }

        if chunk.len() < frame_len_with_length {
            return if is_eof {
                // If we've hit EOF and we have a partial frame here, well, then... it's invalid.
                Err(FramingError::PartialFrame {
                    needed: frame_len_with_length,
                    remaining: chunk.len(),
                })
            } else {
                Ok(None)
            };
        }

        // Split out the entire frame -- length delimiter included -- and then carve out the length delimiter from the
        // frame that we return.
        //
        // TODO: This is a bit inefficient, as we're copying the entire frame here. We could potentially avoid this by
        // adding some specialized trait methods to `ReadIoBuffer` that could let us, potentially, implement equivalent
        // slicing that is object pool aware (i.e., somehow utilizing `FrozenBytesBuffer`, etc).
        let frame = buf.copy_to_bytes(frame_len_with_length).slice(4..);

        Ok(Some(frame))
    }
}

const fn oversized_frame_err(frame_len: usize) -> FramingError {
    FramingError::InvalidFrame {
        frame_len,
        reason: "frame length exceeds buffer capacity",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    fn get_delimited_payload(inner: &[u8], with_newline: bool) -> VecDeque<u8> {
        let payload_len = if with_newline { inner.len() + 1 } else { inner.len() };

        get_delimited_payload_with_fixed_length(inner, payload_len as u32, with_newline)
    }

    fn get_delimited_payload_with_fixed_length(inner: &[u8], frame_len: u32, with_newline: bool) -> VecDeque<u8> {
        let mut payload = VecDeque::new();
        payload.extend(&frame_len.to_le_bytes());
        payload.extend(inner);
        if with_newline {
            payload.push_back(b'\n');
        }

        payload
    }

    #[test]
    fn basic() {
        let payload = b"hello, world!";
        let mut buf = get_delimited_payload(payload, false);

        let mut framer = LengthDelimitedFramer;

        let frame = framer
            .next_frame(&mut buf, false)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(&frame[..], payload);
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_read() {
        // We create a full, valid frame and then take incrementally larger slices of it, ensuring that we can't
        // actually read the frame until we give the framer the entire buffer.
        let payload = b"hello, world!";
        let mut buf = get_delimited_payload(payload, false);

        let mut framer = LengthDelimitedFramer;

        // Try reading a frame from a buffer that doesn't have enough bytes for the length delimiter itself.
        let mut no_delimiter_buf = buf.clone();
        no_delimiter_buf.truncate(3);

        let maybe_frame = framer
            .next_frame(&mut no_delimiter_buf, false)
            .expect("should not fail to read from payload");
        assert!(maybe_frame.is_none());
        assert_eq!(no_delimiter_buf.len(), 3);

        // Try reading a frame from a buffer that has enough bytes for the length delimiter, but not as many bytes as
        // the length delimiter indicates.
        let mut delimiter_but_partial_buf = buf.clone();
        delimiter_but_partial_buf.truncate(7);

        let maybe_frame = framer
            .next_frame(&mut delimiter_but_partial_buf, false)
            .expect("should not fail to read from payload");
        assert!(maybe_frame.is_none());
        assert_eq!(delimiter_but_partial_buf.len(), 7);

        // Now try reading a frame from the original buffer, which should succeed.
        let frame = framer
            .next_frame(&mut buf, false)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(&frame[..], payload);
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_read_eof() {
        // We create a full, valid frame and then take incrementally larger slices of it, ensuring that we can't
        // actually read the frame until we give the framer the entire buffer.
        let payload = b"hello, world!";
        let mut buf = get_delimited_payload(payload, false);
        let frame_len = buf.len();

        let mut framer = LengthDelimitedFramer;

        // Try reading a frame from a buffer that doesn't have enough bytes for the length delimiter itself.
        let mut no_delimiter_buf = buf.clone();
        no_delimiter_buf.truncate(3);

        let maybe_frame = framer.next_frame(&mut no_delimiter_buf, true);
        assert_eq!(
            maybe_frame,
            Err(FramingError::PartialFrame {
                needed: 4,
                remaining: 3
            })
        );
        assert_eq!(no_delimiter_buf.len(), 3);

        // Try reading a frame from a buffer that has enough bytes for the length delimiter, but not as many bytes as
        // the length delimiter indicates.
        let mut delimiter_but_partial_buf = buf.clone();
        delimiter_but_partial_buf.truncate(7);

        let maybe_frame = framer.next_frame(&mut delimiter_but_partial_buf, true);
        assert_eq!(
            maybe_frame,
            Err(FramingError::PartialFrame {
                needed: frame_len,
                remaining: 7
            })
        );
        assert_eq!(delimiter_but_partial_buf.len(), 7);

        // Now try reading a frame from the original buffer, which should succeed.
        let frame = framer
            .next_frame(&mut buf, true)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(&frame[..], payload);
        assert!(buf.is_empty());
    }

    #[test]
    fn oversized_frame() {
        // We create an invalid frame with a length that exceeds the overall length of the resulting buffer.
        let payload = b"hello, world!";
        let mut buf = get_delimited_payload_with_fixed_length(payload, (payload.len() * 10) as u32, false);
        let buf_len = buf.len();

        let mut framer = LengthDelimitedFramer;

        // We should get back an error that the frame is invalid, and the original buffer should not be altered at all.
        let maybe_frame = framer.next_frame(&mut buf, false);
        assert_eq!(maybe_frame, Err(oversized_frame_err(payload.len() * 10)));
        assert_eq!(buf.len(), buf_len);
    }
}
