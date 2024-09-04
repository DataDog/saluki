use bytes::Bytes;
use snafu::Snafu;
use tracing::trace;

use crate::buf::ReadIoBuffer;

mod length_delimited;
pub use self::length_delimited::LengthDelimitedFramer;

mod newline;
pub use self::newline::NewlineFramer;

/// Framing error.
#[derive(Debug, Snafu, Eq, PartialEq)]
#[snafu(context(suffix(false)))]
pub enum FramingError {
    /// An invalid frame was received.
    ///
    /// This generally occurs when a partial frame is present and EOF has already been reached, as additional bytes
    /// would be necessary to read the entire frame.
    ///
    /// In some cases, the framing itself may be obviously invalid or corrupted.
    #[snafu(display(
        "received invalid frame (hit EOF and couldn't not parse frame, {} bytes remaining)",
        buffer_len
    ))]
    InvalidFrame { buffer_len: usize },
}

/// A trait for reading framed messages from a buffer.
pub trait Framer {
    /// Attempt to extract the next frame from the buffer.
    ///
    /// On success, returns a byte slice of the frame data and the number of bytes to advance the buffer.
    fn next_frame<B: ReadIoBuffer>(&mut self, buf: &mut B, is_eof: bool) -> Result<Option<Bytes>, FramingError>;
}

/// A nested framer that extracts inner frames from outer frames.
///
/// This framer takes two input framers -- the "outer" and "inner" framers -- and extracts outer frames, and once an
/// outer frame has been extract, extracts as many inner frames from the outer frame as possible. Callers deal
/// exclusively with the extracted inner frames.
pub struct NestedFramer<Inner, Outer> {
    inner: Inner,
    outer: Outer,
    current_outer_frame: Option<Bytes>,
}

impl<Inner, Outer> NestedFramer<Inner, Outer> {
    /// Creates a new `NestedFramer` from the given inner and outer framers.
    pub fn new(inner: Inner, outer: Outer) -> Self {
        Self {
            inner,
            outer,
            current_outer_frame: None,
        }
    }
}

impl<Inner, Outer> Framer for NestedFramer<Inner, Outer>
where
    Inner: Framer,
    Outer: Framer,
{
    fn next_frame<B: ReadIoBuffer>(&mut self, buf: &mut B, is_eof: bool) -> Result<Option<Bytes>, FramingError> {
        loop {
            // Take our current outer frame, or if we have none, try to get the next one.
            let outer_frame = match self.current_outer_frame.as_mut() {
                Some(frame) => {
                    trace!(
                        buf_len = buf.remaining(),
                        frame_len = frame.len(),
                        "Using existing outer frame."
                    );

                    frame
                }
                None => {
                    trace!(buf_len = buf.remaining(), "No existing outer frame.");

                    match self.outer.next_frame(buf, is_eof)? {
                        Some(frame) => {
                            trace!(
                                buf_len = buf.remaining(),
                                frame_len = frame.len(),
                                ?frame,
                                "Extracted outer frame."
                            );

                            self.current_outer_frame.get_or_insert(frame)
                        }

                        // If we can't get another outer frame, then we're done for now.
                        None => return Ok(None),
                    }
                }
            };

            // Try to get the next inner frame.
            match self.inner.next_frame(outer_frame, is_eof)? {
                Some(frame) => {
                    trace!(
                        buf_len = buf.remaining(),
                        outer_frame_len = outer_frame.len(),
                        inner_frame_len = frame.len(),
                        "Extracted inner frame."
                    );

                    return Ok(Some(frame));
                }
                None => {
                    // We can't get anything else from our inner frame. If our outer frame is empty, and our input buffer
                    // isn't empty, clear the current outer frame so that we can try to grab the next one.
                    trace!(
                        buf_len = buf.remaining(),
                        outer_frame_len = outer_frame.len(),
                        "Couldn't extract inner frame from existing outer frame."
                    );

                    if outer_frame.is_empty() && buf.remaining() != 0 {
                        self.current_outer_frame = None;
                        continue;
                    } else {
                        return Ok(None);
                    }
                }
            }
        }
    }
}

/// An iterator of framed messages over a generic buffer.
pub struct Framed<'a, F, B> {
    framer: &'a mut F,
    buffer: &'a mut B,
    is_eof: bool,
}

impl<'a, F, B> Iterator for Framed<'a, F, B>
where
    F: Framer,
    B: ReadIoBuffer,
{
    type Item = Result<Bytes, FramingError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.framer.next_frame(self.buffer, self.is_eof).transpose()
    }
}

/// Extension trait for ergonomically working with framers and buffers.
pub trait FramerExt {
    /// Creates a new `Framed` iterator over the buffer, using the given framer.
    ///
    /// Returns an iterator that extracts frames from the given buffer, consuming the bytes from the buffer as frames
    /// are yielded.
    fn framed<'a, F>(&'a mut self, framer: &'a mut F, is_eof: bool) -> Framed<'a, F, Self>
    where
        Self: ReadIoBuffer + Sized,
        F: Framer;
}

impl<B> FramerExt for B
where
    B: ReadIoBuffer,
{
    fn framed<'a, F>(&'a mut self, framer: &'a mut F, is_eof: bool) -> Framed<'a, F, Self> {
        Framed {
            framer,
            buffer: self,
            is_eof,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::{Framer as _, LengthDelimitedFramer, NestedFramer, NewlineFramer};

    #[test]
    fn nested_framer_single_outer_multiple_inner() {
        let input_frames = &[b"frame1", b"frame2", b"frame3"];

        // We create a framer that does length-delimited payloads as the outer layer, and newline-delimited payloads as
        // the inner layer.
        let mut framer = NestedFramer::new(NewlineFramer::default(), LengthDelimitedFramer);

        // Create a buffer that has a single length-delimited frame with three newline-delimited frames inside of that.
        let mut inner_frames = Vec::new();

        for inner_frame_data in input_frames {
            inner_frames.extend_from_slice(&inner_frame_data[..]);
            inner_frames.push(b'\n');
        }

        let mut buf = VecDeque::new();
        buf.extend(&(inner_frames.len() as u32).to_le_bytes());
        buf.extend(inner_frames);

        // Now we should be able to extract our original three frames from the buffer.
        for input_frame in input_frames {
            let frame = framer
                .next_frame(&mut buf, false)
                .expect("should not fail to read from payload")
                .expect("should not fail to extract frame from payload");
            assert_eq!(&frame[..], &input_frame[..]);
        }

        let maybe_frame = framer
            .next_frame(&mut buf, false)
            .expect("should not fail to read from payload");
        assert!(maybe_frame.is_none());

        // We should have consumed the entire buffer.
        assert!(buf.is_empty());
    }

    #[test]
    fn nested_framer_multiple_outer_single_inner() {
        let input_frames = &[b"frame1", b"frame2", b"frame3"];

        // We create a framer that does length-delimited payloads as the outer layer, and newline-delimited payloads as
        // the inner layer.
        let mut framer = NestedFramer::new(NewlineFramer::default(), LengthDelimitedFramer);

        // Create a buffer that has a three length-delimited frames with a single newline-delimited frame inside.
        let mut buf = VecDeque::new();

        for inner_frame_data in input_frames {
            let mut inner_frame = Vec::new();
            inner_frame.extend_from_slice(&inner_frame_data[..]);
            inner_frame.push(b'\n');

            buf.extend(&(inner_frame.len() as u32).to_le_bytes());
            buf.extend(inner_frame);
        }

        // Now we should be able to extract our original three frames from the buffer.
        for input_frame in input_frames {
            let frame = framer
                .next_frame(&mut buf, false)
                .expect("should not fail to read from payload")
                .expect("should not fail to extract frame from payload");
            assert_eq!(&frame[..], &input_frame[..]);
        }

        let maybe_frame = framer
            .next_frame(&mut buf, false)
            .expect("should not fail to read from payload");
        assert!(maybe_frame.is_none());

        // We should have consumed the entire buffer.
        assert!(buf.is_empty());
    }

    #[test]
    fn nested_framer_multiple_outer_multiple_inner() {
        let input_frames = &[b"frame1", b"frame2", b"frame3", b"frame4", b"frame5", b"frame6"];

        // We create a framer that does length-delimited payloads as the outer layer, and newline-delimited payloads as
        // the inner layer.
        let mut framer = NestedFramer::new(NewlineFramer::default(), LengthDelimitedFramer);

        // Create a buffer that has a three length-delimited frames with two newline-delimited frames inside.
        let mut buf = VecDeque::new();

        for inner_frame_data in input_frames.chunks(2) {
            let mut inner_frames = Vec::new();
            inner_frames.extend_from_slice(&inner_frame_data[0][..]);
            inner_frames.push(b'\n');
            inner_frames.extend_from_slice(&inner_frame_data[1][..]);
            inner_frames.push(b'\n');

            buf.extend(&(inner_frames.len() as u32).to_le_bytes());
            buf.extend(inner_frames);
        }

        // Now we should be able to extract our original six frames from the buffer.
        for input_frame in input_frames {
            let frame = framer
                .next_frame(&mut buf, false)
                .expect("should not fail to read from payload")
                .expect("should not fail to extract frame from payload");
            assert_eq!(&frame[..], &input_frame[..]);
        }

        let maybe_frame = framer
            .next_frame(&mut buf, false)
            .expect("should not fail to read from payload");
        assert!(maybe_frame.is_none());

        // We should have consumed the entire buffer.
        assert!(buf.is_empty());
    }
}
