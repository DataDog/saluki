use tracing::trace;

use super::{Framer, FramingError};
use crate::buf::{BufferView, BytesBufferView};

/// Frames incoming data by splitting data based on a fixed-size length delimiter.
///
/// All frames are prepended with a 4-byte integer, in little endian order, which indicates how much additional data is
/// included in the frame. This framer only supports frame lengths that fit within the given buffer, which is to say
/// that if the length described in the delimiter would exceed the current buffer, it is considered an invalid frame.
#[derive(Default)]
pub struct LengthDelimitedFramer;

impl Framer for LengthDelimitedFramer {
    type Frame<'a>
        = BytesBufferView<'a>
    where
        Self: 'a;

    fn next_frame<'a, 'buf, B>(
        &'a mut self, buf: &'a mut B, is_eof: bool,
    ) -> Result<Option<Self::Frame<'a>>, FramingError>
    where
        B: BufferView,
        'buf: 'a,
    {
        trace!(buf_len = buf.len(), "Processing buffer.");

        let data = buf.as_bytes();
        if data.is_empty() {
            return Ok(None);
        }

        // See if there's enough data to read the frame length.
        if data.len() < 4 {
            return if is_eof {
                Err(FramingError::PartialFrame {
                    needed: 4,
                    remaining: data.len(),
                })
            } else {
                Ok(None)
            };
        }

        // See if we have enough data to read the full frame.
        let frame_len = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
        let frame_len_with_length = frame_len.saturating_add(4);
        if frame_len_with_length > buf.capacity() {
            return Err(oversized_frame_err(frame_len));
        }

        if data.len() < frame_len_with_length {
            return if is_eof {
                // If we've hit EOF and we have a partial frame here, well, then... it's invalid.
                Err(FramingError::PartialFrame {
                    needed: frame_len_with_length,
                    remaining: data.len(),
                })
            } else {
                Ok(None)
            };
        }

        // Skip over the length delimiter and then slice off the frame.
        buf.skip(4);

        Ok(Some(buf.slice_to(frame_len)))
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
    use bytes::BufMut as _;
    use saluki_core::pooling::helpers::get_pooled_object_via_builder;

    use super::*;
    use crate::buf::{BytesBuffer, FixedSizeVec};

    fn get_bytes_buffer(cap: usize) -> BytesBuffer {
        get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(cap))
    }

    fn get_delimited_payload(inner: &[u8], with_newline: bool) -> BytesBuffer {
        let payload_len = if with_newline { inner.len() + 1 } else { inner.len() };

        get_delimited_payload_with_fixed_length(inner, payload_len as u32, with_newline)
    }

    fn get_delimited_payload_with_fixed_length(inner: &[u8], frame_len: u32, with_newline: bool) -> BytesBuffer {
        let mut io_buf = get_bytes_buffer(inner.len() + 5);

        io_buf.put_slice(&frame_len.to_le_bytes());
        io_buf.put_slice(inner);
        if with_newline {
            io_buf.put_u8(b'\n');
        }

        io_buf
    }

    #[test]
    fn basic() {
        let payload = b"hello, world!";
        let mut io_buf = get_delimited_payload(payload, false);
        let mut io_buf_view = io_buf.as_view();

        let mut framer = LengthDelimitedFramer;

        let frame = framer
            .next_frame(&mut io_buf_view, false)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(frame.as_bytes(), payload);
        drop(frame);

        assert!(io_buf_view.is_empty());
    }

    #[test]
    fn partial_read() {
        // We create a full, valid frame and then take incrementally larger slices of it, ensuring that we can't
        // actually read the frame until we give the framer the entire buffer.
        let payload = b"hello, world!";
        let mut io_buf = get_delimited_payload(payload, false);

        let mut framer = LengthDelimitedFramer;

        // Try reading a frame from a buffer that doesn't have enough bytes for the length delimiter itself.
        {
            let mut io_buf_view = io_buf.as_view();
            let mut no_delimiter_buf_view = io_buf_view.slice_to(3);

            let maybe_frame = framer
                .next_frame(&mut no_delimiter_buf_view, false)
                .expect("should not fail to read from payload");
            assert!(maybe_frame.is_none());
            drop(maybe_frame);

            assert_eq!(no_delimiter_buf_view.len(), 3);
        }

        // Try reading a frame from a buffer that has enough bytes for the length delimiter, but not as many bytes as
        // the length delimiter indicates.
        {
            let mut io_buf_view = io_buf.as_view();
            let mut delimiter_but_partial_buf_view = io_buf_view.slice_to(7);

            let maybe_frame = framer
                .next_frame(&mut delimiter_but_partial_buf_view, false)
                .expect("should not fail to read from payload");
            assert!(maybe_frame.is_none());
            drop(maybe_frame);

            assert_eq!(delimiter_but_partial_buf_view.len(), 7);
        }

        // Now try reading a frame from the original buffer, which should succeed.
        let mut io_buf_view = io_buf.as_view();
        let frame = framer
            .next_frame(&mut io_buf_view, false)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(frame.as_bytes(), payload);
        drop(frame);

        assert!(io_buf_view.is_empty());
    }

    #[test]
    fn partial_read_eof() {
        // We create a full, valid frame and then take incrementally larger slices of it, ensuring that we can't
        // actually read the frame until we give the framer the entire buffer.
        let payload = b"hello, world!";
        let mut io_buf = get_delimited_payload(payload, false);
        let frame_len = io_buf.len();

        let mut framer = LengthDelimitedFramer;

        // Try reading a frame from a buffer that doesn't have enough bytes for the length delimiter itself.
        {
            let mut io_buf_view = io_buf.as_view();
            let mut no_delimiter_buf_view = io_buf_view.slice_to(3);

            let maybe_frame = framer.next_frame(&mut no_delimiter_buf_view, true);
            assert_eq!(
                maybe_frame,
                Err(FramingError::PartialFrame {
                    needed: 4,
                    remaining: 3
                })
            );
            drop(maybe_frame);

            assert_eq!(no_delimiter_buf_view.len(), 3);
        }

        // Try reading a frame from a buffer that has enough bytes for the length delimiter, but not as many bytes as
        // the length delimiter indicates.
        {
            let mut io_buf_view = io_buf.as_view();
            let mut delimiter_but_partial_buf_view = io_buf_view.slice_to(7);

            let maybe_frame = framer.next_frame(&mut delimiter_but_partial_buf_view, true);
            assert_eq!(
                maybe_frame,
                Err(FramingError::PartialFrame {
                    needed: frame_len,
                    remaining: 7
                })
            );
            drop(maybe_frame);

            assert_eq!(delimiter_but_partial_buf_view.len(), 7);
        }

        // Now try reading a frame from the original buffer, which should succeed.
        let mut io_buf_view = io_buf.as_view();
        let frame = framer
            .next_frame(&mut io_buf_view, true)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(frame.as_bytes(), payload);
        drop(frame);

        assert!(io_buf_view.is_empty());
    }

    #[test]
    fn oversized_frame() {
        // We create an invalid frame with a length that exceeds the overall length of the resulting buffer.
        let payload = b"hello, world!";
        let mut io_buf = get_delimited_payload_with_fixed_length(payload, (payload.len() * 10) as u32, false);
        let io_buf_len = io_buf.len();

        let mut framer = LengthDelimitedFramer;

        // We should get back an error that the frame is invalid, and the original buffer should not be altered at all.
        let mut io_buf_view = io_buf.as_view();
        let maybe_frame = framer.next_frame(&mut io_buf_view, false);
        assert_eq!(maybe_frame, Err(oversized_frame_err(payload.len() * 10)));
        drop(maybe_frame);

        assert_eq!(io_buf_view.len(), io_buf_len);
    }
}
