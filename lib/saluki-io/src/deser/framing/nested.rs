use tracing::trace;

use crate::buf::{BufferView as _, BytesBufferView};

use super::{Framer, FramingError};

/// A nested framer that extracts inner frames from outer frames.
///
/// This framer takes two input framers -- the "outer" and "inner" framers -- and extracts outer frames, and once an
/// outer frame has been extract, extracts as many inner frames from the outer frame as possible. Callers deal
/// exclusively with the extracted inner frames.
pub struct NestedFramer<'outer, Inner, Outer> {
    inner: Inner,
    outer: Outer,
    current_outer_frame: Option<BytesBufferView<'outer>>,
}

impl<'outer, Inner, Outer> NestedFramer<'outer, Inner, Outer> {
    /// Creates a new `NestedFramer` from the given inner and outer framers.
    pub fn new(inner: Inner, outer: Outer) -> Self {
        Self {
            inner,
            outer,
            current_outer_frame: None,
        }
    }
}

impl<'outer, Inner, Outer> Framer for NestedFramer<'outer, Inner, Outer>
where
    Inner: Framer,
    Outer: Framer,
{
    fn next_frame<'buf>(&mut self, buf: &'buf mut BytesBufferView<'_>, is_eof: bool) -> Result<Option<BytesBufferView<'buf>>, FramingError> {
        loop {
            // Take our current outer frame, or if we have none, try to get the next one.
            let outer_frame = match self.current_outer_frame.as_mut() {
                Some(frame) => {
                    trace!(frame_len = frame.len(), "Using existing outer frame.");

                    frame
                }
                None => {
                    trace!(buf_len = buf.len(), "No existing outer frame.");

                    match self.outer.next_frame(buf, is_eof)? {
                        Some(frame) => {
                            trace!(
                                buf_len = buf.len(),
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
            match self.inner.next_frame(outer_frame, true)? {
                Some(frame) => {
                    trace!(
                        buf_len = buf.len(),
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
                        buf_len = buf.len(),
                        outer_frame_len = outer_frame.len(),
                        "Couldn't extract inner frame from existing outer frame."
                    );

                    if outer_frame.is_empty() && buf.len() != 0 {
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

#[cfg(test)]
mod tests {
	use super::*;

    use saluki_core::pooling::helpers::get_pooled_object_via_builder;

    use crate::{buf::{BytesBuffer, FixedSizeVec}, deser::framing::{LengthDelimitedFramer, NewlineFramer}};

    fn get_bytes_buffer(cap: usize) -> BytesBuffer {
        get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(cap))
    }

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
