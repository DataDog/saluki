use std::mem::ManuallyDrop;

use ouroboros::self_referencing;

use super::{Framer, FramingError, RawBuffer};
use crate::buf::ReadIoBuffer;

struct DirectIter<'framer, 'buf> {
    framer: &'framer dyn Framer,
    buf: &'buf [u8],
    is_eof: bool,
}

impl<'framer, 'buf> DirectIter<'framer, 'buf> {
    fn new(framer: &'framer dyn Framer, buf: &'buf [u8], is_eof: bool) -> Self {
        Self { framer, buf, is_eof }
    }

    fn buf_len(&self) -> usize {
        self.buf.len()
    }

    fn next_frame(&mut self) -> Result<Option<&[u8]>, FramingError> {
        // Our buffer is empty, so we're all done.
        if self.buf.is_empty() {
            return Ok(None);
        }

        match self.framer.next_frame(RawBuffer::new(&self.buf), self.is_eof)? {
            Some(frame) => {
                // Advance our buffer past the frame we just extracted.
                self.buf = &self.buf[frame.buf_len()..];
                return Ok(Some(frame.into_bytes()));
            }
            None => return Ok(None),
        }
    }
}

struct NestedIter<'framer, 'buf> {
    outer_framer: &'framer dyn Framer,
    inner_framer: &'framer dyn Framer,
    buf: &'buf [u8],
    outer_frame_buf: &'buf [u8],
    is_eof: bool,
}

impl<'framer, 'buf> NestedIter<'framer, 'buf> {
    fn new(
        outer_framer: &'framer dyn Framer, inner_framer: &'framer dyn Framer, buf: &'buf [u8], is_eof: bool,
    ) -> Self {
        Self {
            outer_framer,
            inner_framer,
            buf,
            outer_frame_buf: &[],
            is_eof,
        }
    }

    fn buf_len(&self) -> usize {
        self.buf.len()
    }

    fn next_frame(&mut self) -> Result<Option<&[u8]>, FramingError> {
        loop {
            // Check if our outer frame is empty, and try to extract the next outer frame if so.
            if self.outer_frame_buf.is_empty() {
                // If our root buffer is also empty, then we're done.
                if self.buf.is_empty() {
                    return Ok(None);
                }

                match self.outer_framer.next_frame(RawBuffer::new(&self.buf), self.is_eof)? {
                    Some(frame) => {
                        // Advance our root buffer past the outer frame we just extracted.
                        self.buf = &self.buf[frame.buf_len()..];

                        self.outer_frame_buf = frame.into_bytes();
                    }
                    None => return Ok(None),
                }
            }

            // NOTE: This should never happen, based on what we're doing above.. but it gives us an invariant that we depend on
            // below, where if we get no inner frame, then we know the outer frame is corrupted somehow (or we have a framer bug).
            assert!(!self.outer_frame_buf.is_empty(), "outer frame buf should not be empty");

            // Try to extract an inner frame from the outer frame.
            match self
                .inner_framer
                .next_frame(RawBuffer::new(&self.outer_frame_buf), true)?
            {
                Some(frame) => {
                    // Advance our outer frame past the inner frame we just extracted.
                    self.outer_frame_buf = &self.outer_frame_buf[frame.buf_len()..];
                    return Ok(Some(frame.into_bytes()));
                }
                None => {
                    return Err(FramingError::InvalidFrame {
                        frame_len: self.outer_frame_buf.len(),
                        reason: "outer frame non-empty but no remaining inner frames",
                    })
                }
            }
        }
    }
}

enum Iter<'framer, 'buf> {
    Direct(DirectIter<'framer, 'buf>),
    Nested(NestedIter<'framer, 'buf>),
}

impl<'framer, 'buf> Iter<'framer, 'buf> {
    fn buf_len(&self) -> usize {
        match self {
            Iter::Direct(iter) => iter.buf_len(),
            Iter::Nested(iter) => iter.buf_len(),
        }
    }
}

#[self_referencing]
struct FramedInnerRef<'buf, 'framer, B>
where
    B: ReadIoBuffer,
{
    buf: &'buf mut B,
    #[borrows(buf)]
    #[covariant]
    iter: Iter<'framer, 'this>,
}

struct FramedInner<'buf, 'framer, B>(ManuallyDrop<FramedInnerRef<'buf, 'framer, B>>)
where
    B: ReadIoBuffer;

impl<'buf, 'framer, B> FramedInner<'buf, 'framer, B>
where
    B: ReadIoBuffer,
{
    fn direct(framer: &'framer dyn Framer, buf: &'buf mut B, is_eof: bool) -> Self {
        Self(ManuallyDrop::new(FramedInnerRef::new(buf, |buf| {
            Iter::Direct(DirectIter::new(framer, buf.chunk(), is_eof))
        })))
    }

    fn nested(
        outer_framer: &'framer dyn Framer, inner_framer: &'framer dyn Framer, buf: &'buf mut B, is_eof: bool,
    ) -> Self {
        Self(ManuallyDrop::new(FramedInnerRef::new(buf, |buf| {
            Iter::Nested(NestedIter::new(outer_framer, inner_framer, buf.chunk(), is_eof))
        })))
    }
}

impl<'buf, 'framer, B> Drop for FramedInner<'buf, 'framer, B>
where
    B: ReadIoBuffer,
{
    fn drop(&mut self) {
        // Figure out how far the buffer view has been advanced and advance the underlying buffer
        // by that many bytes, which keeps things consistent.
        let advance_by = self.0.borrow_buf().remaining() - self.0.borrow_iter().buf_len();

        // SAFETY: The drop implementation will only be called once, so we know that we can safely
        // consume the inner struct from `ManuallyDrop`.
        let inner = unsafe { ManuallyDrop::take(&mut self.0) };
        let heads = inner.into_heads();
        heads.buf.advance(advance_by);
    }
}

/// An iterator of framed messages over a generic buffer.
pub struct Framed<'buf, 'framer, B>
where
    B: ReadIoBuffer,
{
    inner: FramedInner<'buf, 'framer, B>,
}

impl<'buf, 'framer, B> Framed<'buf, 'framer, B>
where
    B: ReadIoBuffer,
{
    pub fn direct(framer: &'framer dyn Framer, buf: &'buf mut B, is_eof: bool) -> Self {
        Self {
            inner: FramedInner::direct(framer, buf, is_eof),
        }
    }

    pub fn nested(outer: &'framer dyn Framer, inner: &'framer dyn Framer, buf: &'buf mut B, is_eof: bool) -> Self {
        Self {
            inner: FramedInner::nested(outer, inner, buf, is_eof),
        }
    }

    pub fn next_frame(&mut self) -> Option<Result<&[u8], FramingError>> {
        self.inner.0.with_iter_mut(|iter| match iter {
            Iter::Direct(direct) => direct.next_frame().transpose(),
            Iter::Nested(nested) => nested.next_frame().transpose(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::deser::framing::{LengthDelimitedFramer, NewlineFramer};

    fn nested_payload(inner_frames: &[&[u8]], outer_frame_count: usize) -> VecDeque<u8> {
        let mut outer_frames = VecDeque::new();

        let inner_frames_chunk_size = inner_frames.len() / outer_frame_count;
        for inner_frames in inner_frames.chunks(inner_frames_chunk_size) {
            let mut inner_frames_chunk = Vec::new();
            for inner_frame in inner_frames {
                inner_frames_chunk.extend_from_slice(&inner_frame[..]);
                inner_frames_chunk.push(b'\n');
            }

            outer_frames.extend(&(inner_frames_chunk.len() as u32).to_le_bytes());
            outer_frames.extend(inner_frames_chunk);
        }

        outer_frames
    }

    #[test]
    fn framed_nested_single_outer_multiple_inner() {
        // We create a buffer that has a single outer (length delimited) frame with multiple inner (newline delimited) frames.
        let input_frames = [&b"frame1"[..], &b"frame2"[..], &b"frame3"[..]];
        let mut buf = nested_payload(input_frames.as_slice(), 1);

        // Create our framer: length-delimited frames on the outside, and newline-delimited frames on the inside.
        let outer = LengthDelimitedFramer::default();
        let inner = NewlineFramer::default();
        let mut framed = Framed::nested(&outer, &inner, &mut buf, false);

        // Now we should be able to extract our original three frames from the buffer.
        for input_frame in input_frames {
            let frame = framed
                .next_frame()
                .expect("should not fail to read from payload")
                .expect("should not fail to extract frame from payload");
            assert_eq!(&frame[..], &input_frame[..]);
        }

        let maybe_frame = framed.next_frame();
        assert!(maybe_frame.is_none());

        drop(framed);

        // We should have consumed the entire buffer.
        assert!(buf.is_empty());
    }

    #[test]
    fn framed_nested_multiple_outer_single_inner() {
        // We create a buffer that has multiple outer (length delimited) frames each with a single inner (newline delimited) frame.
        let input_frames = &[&b"frame1"[..], &b"frame2"[..], &b"frame3"[..]];
        let mut buf = nested_payload(input_frames.as_slice(), 3);

        // Create our framer: length-delimited frames on the outside, and newline-delimited frames on the inside.
        let outer = LengthDelimitedFramer::default();
        let inner = NewlineFramer::default();
        let mut framed = Framed::nested(&outer, &inner, &mut buf, false);

        // Now we should be able to extract our original three frames from the buffer.
        for input_frame in input_frames {
            let frame = framed
                .next_frame()
                .expect("should not fail to read from payload")
                .expect("should not fail to extract frame from payload");
            assert_eq!(&frame[..], &input_frame[..]);
        }

        let maybe_frame = framed.next_frame();
        assert!(maybe_frame.is_none());

        drop(framed);

        // We should have consumed the entire buffer.
        assert!(buf.is_empty());
    }

    #[test]
    fn framed_nested_multiple_outer_multiple_inner() {
        // We create a buffer that has multiple outer (length delimited) frames each with multiple inner (newline delimited) frames.
        let input_frames = &[
            &b"frame1"[..],
            &b"frame2"[..],
            &b"frame3"[..],
            &b"frame4"[..],
            &b"frame5"[..],
            &b"frame6"[..],
        ];
        let mut buf = nested_payload(input_frames.as_slice(), 3);

        // Create our framer: length-delimited frames on the outside, and newline-delimited frames on the inside.
        let outer = LengthDelimitedFramer::default();
        let inner = NewlineFramer::default();
        let mut framed = Framed::nested(&outer, &inner, &mut buf, false);

        // Now we should be able to extract our original six frames from the buffer.
        for input_frame in input_frames {
            let frame = framed
                .next_frame()
                .expect("should not fail to read from payload")
                .expect("should not fail to extract frame from payload");
            assert_eq!(&frame[..], &input_frame[..]);
        }

        let maybe_frame = framed.next_frame();
        assert!(maybe_frame.is_none());

        drop(framed);

        // We should have consumed the entire buffer.
        assert!(buf.is_empty());
    }
}
