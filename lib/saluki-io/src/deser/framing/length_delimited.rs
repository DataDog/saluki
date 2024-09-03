use tracing::trace;

use super::{Framer, FramingError};
use crate::buf::ReadIoBuffer;

/// Frames incoming data by splitting data based on a fixed-size length delimiter.
///
/// All frames are prepended with a 4-byte integer, in little endian order, which indicates how much additional data is
/// included in the frame. This framer only supports frame lengths that fit within the given buffer, which is to say
/// that if the length described in the delimiter would exceed the current buffer, it is considered an invalid frame.
pub struct LengthDelimitedFramer {
    strip_trailing_newline: bool,
}

impl LengthDelimitedFramer {
    /// Whether or not to strip trailing newline characters.
    ///
    /// This controls whether or not frames have trailing newlines stripped before being returned. In some case, while
    /// the frame is length-delimited, it may be wrapping a newline-delimited payload, while the codec being used may
    /// expect the newline to be stripped.
    ///
    /// This doesn't change how frames are otherwise interpreted: frames are still initially split based on the length
    /// delimiter itself, and newlines would only be included in the frame if they were part of the payload.
    ///
    /// Defaults to `true`.
    pub fn strip_trailing_newline(mut self, strip: bool) -> Self {
        self.strip_trailing_newline = strip;
        self
    }
}

impl Default for LengthDelimitedFramer {
    fn default() -> Self {
        Self {
            strip_trailing_newline: true,
        }
    }
}

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
            return if is_eof {
                Err(FramingError::InvalidFrame {
                    buffer_len: buf.remaining(),
                })
            } else {
                Ok(None)
            };
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

        // If we're stripping trailing newlines, then check if the last byte is a newline and adjust the frame if so.
        let frame = if self.strip_trailing_newline && frame[frame.len() - 1] == b'\n' {
            &frame[..frame.len() - 1]
        } else {
            frame
        };

        Ok(Some((frame, frame_len_with_length)))
    }
}

#[cfg(test)]
mod tests {
    use super::LengthDelimitedFramer;
    use crate::deser::framing::{Framer as _, FramingError};

    fn get_delimited_payload(inner: &[u8], with_newline: bool) -> Vec<u8> {
        let payload_len = if with_newline { inner.len() + 1 } else { inner.len() };

        let mut payload = Vec::new();
        payload.extend_from_slice(&(payload_len as u32).to_le_bytes());
        payload.extend_from_slice(inner);
        if with_newline {
            payload.push(b'\n');
        }

        payload
    }

    #[test]
    fn basic() {
        let payload = b"hello, world!";
        let delimited_payload = get_delimited_payload(payload, false);

        let buf = delimited_payload.as_slice();

        let mut framer = LengthDelimitedFramer::default();
        let (frame, advance_len) = framer
            .next_frame(&buf, false)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(frame, payload);
        assert_eq!(advance_len, delimited_payload.len());
    }

    #[test]
    fn partial_read() {
        let payload = b"hello, world!";
        let delimited_payload = get_delimited_payload(payload, false);

        let buf = delimited_payload.as_slice();

        let no_delimiter_buf = &buf[0..3];
        let delimiter_but_partial_buf = &buf[0..7];
        let full_buf = buf;

        let mut framer = LengthDelimitedFramer::default();

        let result = framer
            .next_frame(&no_delimiter_buf, false)
            .expect("should not fail to read from payload");
        assert!(result.is_none());

        let result = framer
            .next_frame(&delimiter_but_partial_buf, false)
            .expect("should not fail to read from payload");
        assert!(result.is_none());

        let (frame, advance_len) = framer
            .next_frame(&full_buf, false)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(frame, payload);
        assert_eq!(advance_len, delimited_payload.len());
    }

    #[test]
    fn partial_read_eof() {
        let payload = b"hello, world!";
        let delimited_payload = get_delimited_payload(payload, false);

        let buf = delimited_payload.as_slice();

        let no_delimiter_buf = &buf[0..3];
        let delimiter_but_partial_buf = &buf[0..7];
        let full_buf = buf;

        let mut framer = LengthDelimitedFramer::default();

        let buffer_len = no_delimiter_buf.len();
        let result = framer.next_frame(&no_delimiter_buf, true);
        assert_eq!(result, Err(FramingError::InvalidFrame { buffer_len }));

        let buffer_len = delimiter_but_partial_buf.len();
        let result = framer.next_frame(&delimiter_but_partial_buf, true);
        assert_eq!(result, Err(FramingError::InvalidFrame { buffer_len }));

        let (frame, advance_len) = framer
            .next_frame(&full_buf, true)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");
        assert_eq!(frame, payload);
        assert_eq!(advance_len, delimited_payload.len());
    }

    #[test]
    fn strips_trailing_newline() {
        // Construct our payload with a trailing newline.
        let payload = b"hello, world!";
        let delimited_payload = get_delimited_payload(payload, true);

        let buf = delimited_payload.as_slice();

        // Create our framer, ensuring that newline stripping is enabled, and then extract the frame.
        let mut framer = LengthDelimitedFramer::default().strip_trailing_newline(true);
        let (frame, advance_len) = framer
            .next_frame(&buf, false)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(frame, payload);
        assert_eq!(advance_len, delimited_payload.len());

        // Now we'll construct the framer with newline stripping disabled and extract the frame, which should give us a
        // frame that still includes the newline.
        let buf = delimited_payload.as_slice();

        let mut framer = LengthDelimitedFramer::default().strip_trailing_newline(false);

        let (frame, advance_len) = framer
            .next_frame(&buf, false)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(frame.len(), payload.len() + 1);
        assert!(frame.starts_with(payload));
        assert_eq!(frame[frame.len() - 1], b'\n');
        assert_eq!(advance_len, delimited_payload.len());
    }

    #[test]
    fn trailing_newline_single_stripped_when_multiple() {
        // Construct our payload with multiple trailing newlines.
        let payload = b"hello, world!\n\n";
        let delimited_payload = get_delimited_payload(payload, true);

        let buf = delimited_payload.as_slice();

        // Create our framer, ensuring that newline stripping is enabled, and then extract the frame.
        let mut framer = LengthDelimitedFramer::default().strip_trailing_newline(true);
        let (frame, advance_len) = framer
            .next_frame(&buf, false)
            .expect("should not fail to read from payload")
            .expect("should not fail to extract frame from payload");

        assert_eq!(frame, payload);
        assert_eq!(advance_len, delimited_payload.len());
    }
}
