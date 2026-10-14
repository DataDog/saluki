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
    /// This generally occurs if the frame is corrupted in some way, due to a bug in how the frame was encoded or
    /// sent/received. For example, if a length-delimited frame indicates that the frame is larger than the buffer can
    /// handle, it generally indicates that frame was created incorrectly by not respecting the maximum frame length
    /// limitations, or the buffer is corrupt and spurious bytes are contributing to a decoded frame length that's
    /// nonsensical.
    #[snafu(display("invalid frame (frame length: {}, {})", frame_len, reason))]
    InvalidFrame { frame_len: usize, reason: &'static str },

    /// Failed to read frame due to a partial frame after reaching EOF.
    ///
    /// This generally only occurs if the peer closes their connection before sending the entire frame, or if a partial
    /// write occurs on a connectionless stream, such as UDP, perhaps due to fragmentation.
    #[snafu(display(
        "partial frame after EOF (needed {} bytes, but only {} bytes remaining)",
        needed,
        remaining
    ))]
    PartialFrame { needed: usize, remaining: usize },
}

/// A trait for reading framed messages from a buffer.
pub trait Framer {
    /// Attempt to extract the next frame from the buffer.
    ///
    /// If enough data was present to extract a frame, `Ok(Some(frame))` is returned. If not enough data was present, and
    /// EOF hasn't been reached, `Ok(None)` is returned.
    ///
    /// Behavior when EOF is reached is framer-specific and in some cases may allow for decoding a frame even when the
    /// inherent delimiting data isn't present.
    ///
    /// # Errors
    ///
    /// If an error is detected when reading the next frame, an error is returned.
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
    completed_outer_frames: usize,
    retain_outer_frames: bool,
    retained_outer_frames: Vec<Bytes>,
}

impl<Inner, Outer> NestedFramer<Inner, Outer> {
    /// Creates a new `NestedFramer` from the given inner and outer framers.
    pub fn new(inner: Inner, outer: Outer) -> Self {
        Self {
            inner,
            outer,
            current_outer_frame: None,
            completed_outer_frames: 0,
            retain_outer_frames: false,
            retained_outer_frames: Vec::new(),
        }
    }

    /// Returns the number of outer frames that have been fully consumed since the last call.
    pub fn take_completed_outer_frames(&mut self) -> usize {
        let completed = self.completed_outer_frames;
        self.completed_outer_frames = 0;
        completed
    }

    /// Sets whether newly extracted outer frames are retained for later retrieval.
    ///
    /// When enabled, every outer frame extracted from the input buffer is retained, in order, until it is drained with
    /// [`drain_outer_frames`][Self::drain_outer_frames]. Retained frames share their underlying storage with the frame
    /// being split into inner frames, so retaining them does not copy the frame data.
    ///
    /// Callers that enable retention **MUST** drain retained frames after each call to `next_frame`, as they are
    /// otherwise held indefinitely. Disabling retention discards any frames that have not yet been drained.
    ///
    /// Defaults to disabled.
    pub fn set_retain_outer_frames(&mut self, retain: bool) {
        self.retain_outer_frames = retain;
        if !retain {
            self.retained_outer_frames.clear();
        }
    }

    /// Drains the outer frames retained since the last call, in the order they were extracted.
    pub fn drain_outer_frames(&mut self) -> std::vec::Drain<'_, Bytes> {
        self.retained_outer_frames.drain(..)
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

                            if self.retain_outer_frames {
                                self.retained_outer_frames.push(frame.clone());
                            }

                            self.current_outer_frame.get_or_insert(frame)
                        }

                        // If we can't get another outer frame, then we're done for now.
                        None => return Ok(None),
                    }
                }
            };

            // Try to get the next inner frame.
            match self.inner.next_frame(outer_frame, true)? {
                Some(frame) => {
                    let completed_outer_frame = outer_frame.is_empty();

                    trace!(
                        buf_len = buf.remaining(),
                        outer_frame_len = outer_frame.len(),
                        inner_frame_len = frame.len(),
                        "Extracted inner frame."
                    );

                    if completed_outer_frame {
                        self.current_outer_frame = None;
                        self.completed_outer_frames += 1;
                    }

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

                    if outer_frame.is_empty() {
                        self.current_outer_frame = None;
                        self.completed_outer_frames += 1;
                        if buf.remaining() != 0 {
                            continue;
                        }
                    }

                    return Ok(None);
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

impl<F, B> Iterator for Framed<'_, F, B>
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
    fn nested_framer_tracks_completed_outer_frames() {
        let input_frames = &[b"frame1", b"frame2", b"frame3"];
        let mut framer = NestedFramer::new(NewlineFramer::default(), LengthDelimitedFramer);
        let mut buf = VecDeque::new();

        for inner_frame_data in input_frames {
            let mut inner_frame = Vec::new();
            inner_frame.extend_from_slice(&inner_frame_data[..]);
            inner_frame.push(b'\n');

            buf.extend(&(inner_frame.len() as u32).to_le_bytes());
            buf.extend(inner_frame);
        }

        for input_frame in input_frames {
            let frame = framer
                .next_frame(&mut buf, false)
                .expect("should not fail to read from payload")
                .expect("should extract frame from payload");
            assert_eq!(&frame[..], &input_frame[..]);
            assert_eq!(framer.take_completed_outer_frames(), 1);
        }

        assert!(buf.is_empty());
        assert_eq!(framer.take_completed_outer_frames(), 0);
    }

    #[test]
    fn nested_framer_retains_outer_frames_only_when_enabled() {
        let outer_payloads: &[&[u8]] = &[b"frame1\nframe2\n", b"frame3\n"];
        let mut buf = VecDeque::new();
        for payload in outer_payloads {
            buf.extend(&(payload.len() as u32).to_le_bytes());
            buf.extend(payload.iter());
        }
        let mut framer = NestedFramer::new(NewlineFramer::default(), LengthDelimitedFramer);

        let first = framer.next_frame(&mut buf, false).unwrap().unwrap();
        assert_eq!(&first[..], b"frame1");
        assert_eq!(framer.drain_outer_frames().count(), 0);

        framer.set_retain_outer_frames(true);
        let second = framer.next_frame(&mut buf, false).unwrap().unwrap();
        assert_eq!(&second[..], b"frame2");
        assert_eq!(framer.drain_outer_frames().count(), 0);

        let third = framer.next_frame(&mut buf, false).unwrap().unwrap();
        assert_eq!(&third[..], b"frame3");
        let retained = framer.drain_outer_frames().collect::<Vec<_>>();
        assert_eq!(retained.len(), 1);
        assert_eq!(&retained[0][..], outer_payloads[1]);
        assert_eq!(framer.drain_outer_frames().count(), 0);

        buf.extend(&(outer_payloads[0].len() as u32).to_le_bytes());
        buf.extend(outer_payloads[0].iter());
        let _ = framer.next_frame(&mut buf, false).unwrap().unwrap();
        framer.set_retain_outer_frames(false);
        assert_eq!(framer.drain_outer_frames().count(), 0);
    }

    #[test]
    fn nested_framer_waits_to_complete_outer_frame_until_last_inner_frame() {
        let mut framer = NestedFramer::new(NewlineFramer::default(), LengthDelimitedFramer);
        let mut inner_frames = Vec::new();
        inner_frames.extend_from_slice(b"frame1\nframe2\n");

        let mut buf = VecDeque::new();
        buf.extend(&(inner_frames.len() as u32).to_le_bytes());
        buf.extend(inner_frames);

        let first_frame = framer
            .next_frame(&mut buf, false)
            .expect("should not fail to read from payload")
            .expect("should extract first frame from payload");
        assert_eq!(&first_frame[..], b"frame1");
        assert_eq!(framer.take_completed_outer_frames(), 0);

        let second_frame = framer
            .next_frame(&mut buf, false)
            .expect("should not fail to read from payload")
            .expect("should extract second frame from payload");
        assert_eq!(&second_frame[..], b"frame2");
        assert_eq!(framer.take_completed_outer_frames(), 1);
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
