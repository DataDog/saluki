use tracing::trace;

use super::{Framer, FramingError};
use crate::buf::{BufferView as _, BytesBufferView};

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
    fn next_frame<'buf>(&mut self, buf: &'buf mut BytesBufferView<'_>, is_eof: bool) -> Result<Option<BytesBufferView<'buf>>, FramingError> {
        trace!(buf_len = buf.len(), "Processing buffer.");

        let data = buf.as_bytes();
        if data.is_empty() {
            return Ok(None);
        }

        // Search through the buffer for our delimiter.
        match find_newline(data) {
            Some(idx) => {
                // We found the delimiter, so carve out our frame.
                //
                // We include the delimiter to remove it from the input buffer, but we then immediately skip it so that
                // the view we give back to the caller doesn't include it.
                let mut frame = buf.slice_to(idx + 1);
                frame.rskip(1);

                Ok(Some(frame))
            }
            None => {
                // If we're not at EOF, then we can't do anything else right now.
                if !is_eof {
                    return Ok(None);
                }

                // If we're at EOF and we require the delimiter, then this is an invalid frame.
                if self.required_on_eof {
                    return Err(missing_delimiter_err(data.len()));
                }

                // We're at EOF, and we don't require the delimiter... so just consume the entire frame.
                Ok(Some(buf.slice_from(0)))
            }
        }
    }
}

const fn missing_delimiter_err(len: usize) -> FramingError {
    FramingError::InvalidFrame {
        frame_len: len,
        reason: "reached EOF without finding newline delimiter",
    }
}

fn find_newline(haystack: &[u8]) -> Option<usize> {
    memchr::memchr(b'\n', haystack)
}

#[cfg(test)]
mod tests {
    use bytes::BufMut as _;
    use saluki_core::pooling::helpers::get_pooled_object_via_builder;

    use crate::buf::{BytesBuffer, FixedSizeVec};

    use super::*;

    fn get_bytes_buffer(cap: usize) -> BytesBuffer {
        get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(cap))
    }

    fn get_delimited_payload(inner: &[u8], with_newline: bool) -> BytesBuffer {
        let mut io_buf = get_bytes_buffer(inner.len() + 1);
        io_buf.put_slice(inner);
        if with_newline {
            io_buf.put_u8(b'\n');
        }

        io_buf
    }

    #[test]
    fn newline_no_eof() {
        let payload = b"hello, world!";
        let mut io_buf = get_delimited_payload(payload, true);
        let mut io_buf_view = io_buf.as_view();

        let mut framer = NewlineFramer::default();

        let frame = framer
            .next_frame(&mut io_buf_view, false)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(frame.as_bytes(), payload);
        drop(frame);

        assert!(io_buf_view.is_empty());
    }

    #[test]
    fn no_newline_no_eof() {
        let payload = b"hello, world!";
        let mut io_buf = get_delimited_payload(payload, false);
        let mut io_buf_view = io_buf.as_view();
        let io_buf_view_len = io_buf_view.len();

        let mut framer = NewlineFramer::default();

        let maybe_frame = framer
            .next_frame(&mut io_buf_view, false)
            .expect("should not fail to read from payload");

        assert_eq!(maybe_frame, None);
        drop(maybe_frame);

        assert_eq!(io_buf_view.len(), io_buf_view_len);
    }

    #[test]
    fn newline_eof() {
        let payload = b"hello, world!";
        let mut io_buf = get_delimited_payload(payload, true);
        let mut io_buf_view = io_buf.as_view();

        let mut framer = NewlineFramer::default();

        let frame = framer
            .next_frame(&mut io_buf_view, true)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(frame.as_bytes(), payload);
        drop(frame);

        assert!(io_buf_view.is_empty());
    }

    #[test]
    fn no_newline_eof_not_required_on_eof() {
        let payload = b"hello, world!";
        let mut io_buf = get_delimited_payload(payload, false);
        let mut io_buf_view = io_buf.as_view();

        let mut framer = NewlineFramer::default();

        let frame = framer
            .next_frame(&mut io_buf_view, true)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(frame.as_bytes(), payload);
        drop(frame);

        assert!(io_buf_view.is_empty());
    }

    #[test]
    fn no_newline_eof_required_on_eof() {
        let payload = b"hello, world!";
        let mut io_buf = get_delimited_payload(payload, false);
        let mut io_buf_view = io_buf.as_view();
        let io_buf_view_len = io_buf_view.len();

        let mut framer = NewlineFramer::default().required_on_eof(true);

        let maybe_frame = framer.next_frame(&mut io_buf_view, true);

        assert_eq!(maybe_frame, Err(missing_delimiter_err(io_buf_view_len)));
        drop(maybe_frame);

        assert_eq!(io_buf_view.len(), io_buf_view_len);
    }
}
