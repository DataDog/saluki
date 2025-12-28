use tracing::trace;

use super::{Framer, FramingError};

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
    fn next_frame<'buf>(&self, buf: &mut &'buf [u8], is_eof: bool) -> Result<Option<&'buf [u8]>, FramingError> {
        trace!(buf_len = buf.len(), "Processing buffer.");

        if buf.is_empty() {
            return Ok(None);
        }

        // Search through the buffer for our delimiter.
        match find_newline(buf) {
            Some(idx) => {
                // If we found the delimiter, then we can return the frame.
                let mut frame = buf.split_off(..idx + 1).unwrap();
                frame = &frame[..frame.len() - 1];

                Ok(Some(frame))
            }
            None => {
                // If we're not at EOF, then we can't do anything else right now.
                if !is_eof {
                    return Ok(None);
                }

                // If we're at EOF and we require the delimiter, then this is an invalid frame.
                if self.required_on_eof {
                    return Err(missing_delimiter_err(buf.len()));
                }

                Ok(Some(buf.split_off(0..).unwrap()))
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
    use super::*;

    fn get_delimited_payload(inner: &[u8], with_newline: bool) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend(inner);
        if with_newline {
            payload.push(b'\n');
        }

        payload
    }

    #[test]
    fn newline_no_eof() {
        let payload = b"hello, world!";
        let buf = get_delimited_payload(payload, true);
        let mut src = &buf[..];

        let framer = NewlineFramer::default();
        let frame = framer
            .next_frame(&mut src, false)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(frame, payload);
        assert!(src.is_empty(), "frame should consume entire buffer");
    }

    #[test]
    fn no_newline_no_eof() {
        let payload = b"hello, world!";
        let buf = get_delimited_payload(payload, false);
        let mut src = &buf[..];

        let framer = NewlineFramer::default();
        let maybe_frame = framer
            .next_frame(&mut src, false)
            .expect("should not fail to read from payload");

        assert_eq!(maybe_frame, None);
    }

    #[test]
    fn newline_eof() {
        let payload = b"hello, world!";
        let buf = get_delimited_payload(payload, true);
        let mut src = &buf[..];

        let framer = NewlineFramer::default();
        let frame = framer
            .next_frame(&mut src, true)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(frame, payload);
        assert!(src.is_empty(), "frame should consume entire buffer");
    }

    #[test]
    fn no_newline_eof_not_required_on_eof() {
        let payload = b"hello, world!";
        let buf = get_delimited_payload(payload, false);
        let mut src = &buf[..];

        let framer = NewlineFramer::default();
        let frame = framer
            .next_frame(&mut src, true)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(frame, payload);
        assert!(src.is_empty(), "frame should consume entire buffer");
    }

    #[test]
    fn no_newline_eof_required_on_eof() {
        let payload = b"hello, world!";
        let buf = get_delimited_payload(payload, false);
        let mut src = &buf[..];

        let framer = NewlineFramer::default().required_on_eof(true);
        let maybe_frame = framer.next_frame(&mut src, true);

        assert_eq!(maybe_frame, Err(missing_delimiter_err(buf.len())));
    }
}
