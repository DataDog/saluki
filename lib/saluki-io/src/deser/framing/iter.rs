use std::mem::ManuallyDrop;

use ouroboros::self_referencing;

use super::{Framer, FramingError, RawBuffer};
use crate::buf::ReadIoBuffer;

struct DirectIter<'framer, 'buf> {
    framer: &'framer dyn Framer,
    buf: &'buf [u8],
    consumed: usize,
    is_eof: bool,
}

impl<'framer, 'buf> DirectIter<'framer, 'buf> {
    fn new(framer: &'framer dyn Framer, buf: &'buf [u8], is_eof: bool) -> Self {
        Self {
            framer,
            buf,
            consumed: 0,
            is_eof,
        }
    }

    fn consumed(&self) -> usize {
        self.consumed
    }

    fn next_frame(&mut self) -> Result<Option<&'buf [u8]>, FramingError> {
        // Our buffer is empty, so we're all done.
        if self.buf.is_empty() {
            return Ok(None);
        }

        match self.framer.next_frame(RawBuffer::new(self.buf), self.is_eof)? {
            Some(frame) => {
                // Advance our buffer past the frame we just extracted.
                self.buf = &self.buf[frame.buf_len()..];
                self.consumed += frame.buf_len();
                Ok(Some(frame.into_bytes()))
            }
            None => Ok(None),
        }
    }
}

struct NestedIter<'framer, 'buf> {
    outer_framer: &'framer dyn Framer,
    inner_framer: &'framer dyn Framer,
    buf: &'buf [u8],
    consumed: usize,
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
            consumed: 0,
            outer_frame_buf: &[],
            is_eof,
        }
    }

    fn consumed(&self) -> usize {
        self.consumed
    }

    fn next_frame(&mut self) -> Result<Option<&'buf [u8]>, FramingError> {
        loop {
            // Check if our outer frame is empty, and try to extract the next outer frame if so.
            if self.outer_frame_buf.is_empty() {
                // If our root buffer is also empty, then we're done.
                if self.buf.is_empty() {
                    return Ok(None);
                }

                match self.outer_framer.next_frame(RawBuffer::new(self.buf), self.is_eof)? {
                    Some(frame) => {
                        // Advance our root buffer past the outer frame we just extracted.
                        self.buf = &self.buf[frame.buf_len()..];
                        self.consumed += frame.buf_len();

                        self.outer_frame_buf = frame.into_bytes();

                        // If the outer frame is empty, there are no inner frames to extract, so continue.
                        if self.outer_frame_buf.is_empty() {
                            continue;
                        }
                    }
                    None => return Ok(None),
                }
            }

            // Try to extract an inner frame from the outer frame.
            match self
                .inner_framer
                .next_frame(RawBuffer::new(self.outer_frame_buf), true)?
            {
                Some(frame) => {
                    // Advance our outer frame past the inner frame we just extracted.
                    self.outer_frame_buf = &self.outer_frame_buf[frame.buf_len()..];
                    return Ok(Some(frame.into_bytes()));
                }
                None => {
                    if self.outer_frame_buf.is_empty() {
                        // We've exhausted the current outer frame, so loop back around and attempt to get the next one.
                        continue;
                    }

                    return Err(FramingError::InvalidFrame {
                        frame_len: self.outer_frame_buf.len(),
                        reason: "outer frame non-empty but no remaining inner frames",
                    });
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
    fn consumed(&self) -> usize {
        match self {
            Iter::Direct(iter) => iter.consumed(),
            Iter::Nested(iter) => iter.consumed(),
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
        let advance_by = self.0.borrow_iter().consumed();

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
    'framer: 'buf,
{
    /// Creates a new `Framed` iterator in direct mode.
    pub fn direct(framer: &'framer dyn Framer, buf: &'buf mut B, is_eof: bool) -> Self {
        Self {
            inner: FramedInner::direct(framer, buf, is_eof),
        }
    }

    /// Creates a new `Framed` iterator in nested mode.
    pub fn nested(outer: &'framer dyn Framer, inner: &'framer dyn Framer, buf: &'buf mut B, is_eof: bool) -> Self {
        Self {
            inner: FramedInner::nested(outer, inner, buf, is_eof),
        }
    }

    /// Attempt to extract the next frame from the buffer.
    ///
    /// If enough data was present to extract a frame, `Ok(Some(frame))` is returned. If not enough data was present, and
    /// EOF has not been reached, `Ok(None)` is returned.
    ///
    /// Behavior when EOF is reached is framer-specific and in some cases may allow for decoding a frame even when the
    /// inherent delimiting data is not present.
    ///
    /// # Errors
    ///
    /// If an error is detected when reading the next frame, an error is returned.
    pub fn next_frame(&mut self) -> Option<Result<&'buf [u8], FramingError>> {
        let raw: Option<Result<&[u8], FramingError>> = self.inner.0.with_iter_mut(move |iter| match iter {
            Iter::Direct(direct) => direct.next_frame().transpose(),
            Iter::Nested(nested) => nested.next_frame().transpose(),
        });

        raw.map(|res| {
            res.map(|frame| {
                // SAFETY: The slices produced by the inner iterators are derived from the underlying buffer referenced
                // by `Framed`. That buffer is borrowed for the lifetime `'buf` and remains valid for the duration of
                // `Framed`, so extending the lifetime of the slice to `'buf` is sound.
                unsafe { std::mem::transmute::<&[u8], &'buf [u8]>(frame) }
            })
        })
    }
}

impl<'buf, 'framer, B> Iterator for Framed<'buf, 'framer, B>
where
    B: ReadIoBuffer,
    'framer: 'buf,
{
    type Item = Result<&'buf [u8], FramingError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_frame()
    }
}

#[cfg(test)]
mod tests {
    use bytes::Buf;
    use std::collections::VecDeque;

    use super::*;
    use crate::deser::framing::{LengthDelimitedFramer, NewlineFramer};

    struct SplitBuf<'a> {
        first: &'a [u8],
        second: &'a [u8],
        first_idx: usize,
        second_idx: usize,
    }

    impl<'a> SplitBuf<'a> {
        fn new(first: &'a [u8], second: &'a [u8]) -> Self {
            Self {
                first,
                second,
                first_idx: 0,
                second_idx: 0,
            }
        }

        fn first_consumed(&self) -> usize {
            self.first_idx
        }

        fn second_consumed(&self) -> usize {
            self.second_idx
        }
    }

    impl Buf for SplitBuf<'_> {
        fn remaining(&self) -> usize {
            (self.first.len() - self.first_idx) + (self.second.len() - self.second_idx)
        }

        fn chunk(&self) -> &[u8] {
            if self.first_idx < self.first.len() {
                &self.first[self.first_idx..]
            } else {
                &self.second[self.second_idx..]
            }
        }

        fn advance(&mut self, cnt: usize) {
            let first_remaining = self.first.len() - self.first_idx;
            if cnt <= first_remaining {
                self.first_idx += cnt;
            } else {
                self.first_idx = self.first.len();
                let rest = cnt - first_remaining;
                let remaining_second = self.second.len() - self.second_idx;
                assert!(rest <= remaining_second, "attempted to advance past end of buffer");
                self.second_idx += rest;
            }
        }
    }

    impl ReadIoBuffer for SplitBuf<'_> {
        fn capacity(&self) -> usize {
            self.first.len() + self.second.len()
        }
    }

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
            assert_eq!(frame, input_frame);
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
            assert_eq!(frame, &input_frame[..]);
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
            assert_eq!(frame, &input_frame[..]);
        }

        let maybe_frame = framed.next_frame();
        assert!(maybe_frame.is_none());

        drop(framed);

        // We should have consumed the entire buffer.
        assert!(buf.is_empty());
    }

    #[test]
    fn framed_direct_does_not_consume_tail() {
        let first = b"frame\n";
        let second = b"tail";

        let mut buf = SplitBuf::new(first.as_slice(), second.as_slice());
        let framer = NewlineFramer::default();

        let mut framed = Framed::direct(&framer, &mut buf, false);
        let frame = framed
            .next_frame()
            .expect("should not fail to read from buffer")
            .expect("should not fail to extract frame");
        assert_eq!(frame, b"frame");

        drop(framed);

        assert_eq!(buf.first_consumed(), first.len());
        assert_eq!(buf.second_consumed(), 0);
        assert_eq!(buf.remaining(), second.len());
    }

    #[test]
    fn framed_nested_zero_length_outer_frame() {
        let mut buf = VecDeque::new();
        buf.extend(&0u32.to_le_bytes());
        buf.extend(&6u32.to_le_bytes());
        buf.extend(b"frame\n");

        let outer = LengthDelimitedFramer::default();
        let inner = NewlineFramer::default();
        let mut framed = Framed::nested(&outer, &inner, &mut buf, false);

        let frame = framed
            .next_frame()
            .expect("should not fail to read from buffer")
            .expect("should not fail to extract frame");
        assert_eq!(frame, b"frame");

        assert!(framed.next_frame().is_none());

        drop(framed);
        assert!(buf.is_empty());
    }
}
