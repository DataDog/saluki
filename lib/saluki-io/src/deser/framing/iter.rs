use std::{marker::PhantomData, mem::ManuallyDrop, ptr::NonNull};

use super::{Framer, FramingError};
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

    fn next_frame(&mut self) -> Result<Option<&[u8]>, FramingError> {
        // Our buffer is empty, so we're all done.
        if self.buf.is_empty() {
            return Ok(None);
        }

        match self.framer.next_frame(&mut self.buf, self.is_eof)? {
            Some(frame) => Ok(Some(frame)),
            None => Ok(None),
        }
    }

    fn finish(self) -> usize {
        self.buf.len()
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

    fn next_frame(&mut self) -> Result<Option<&[u8]>, FramingError> {
        loop {
            // Check if our outer frame is empty, and try to extract the next outer frame if so.
            if self.outer_frame_buf.is_empty() {
                // If our root buffer is also empty, then we're done.
                if self.buf.is_empty() {
                    return Ok(None);
                }

                match self.outer_framer.next_frame(&mut self.buf, self.is_eof)? {
                    Some(frame) => {
                        self.outer_frame_buf = frame;
                    }
                    None => return Ok(None),
                }
            }

            // Try to extract an inner frame from the outer frame.
            match self.inner_framer.next_frame(&mut self.outer_frame_buf, true)? {
                Some(frame) => return Ok(Some(frame)),
                None => continue,
            }
        }
    }

    fn finish(self) -> usize {
        self.buf.len()
    }
}

enum Iter<'framer, 'buf> {
    Direct(DirectIter<'framer, 'buf>),
    Nested(NestedIter<'framer, 'buf>),
}

impl<'framer, 'buf> Iter<'framer, 'buf> {
    fn next_frame(&mut self) -> Result<Option<&[u8]>, FramingError> {
        match self {
            Self::Direct(iter) => iter.next_frame(),
            Self::Nested(iter) => iter.next_frame(),
        }
    }

    fn finish(self) -> usize {
        match self {
            Self::Direct(iter) => iter.finish(),
            Self::Nested(iter) => iter.finish(),
        }
    }
}

/// Core primitive for advancing the source buffer passed to `Framed` on drop.
///
/// Overall, our goal is to hold a mutable reference to our source buffer -- so that we can advance it when we're all
/// done extracting any complete frames -- while also returning _immutable_ references to subslices of that same buffer,
/// representing the complete frames, to avoid the need to allocate or copy any data. This would already be trivially
/// doable with the usage of the `Buf` trait which `ReadIoBuffer` is a superset of, except for our other constraint:
/// nested framing.
///
/// Nested framing requires holding a subslice of the buffer and then extracting frames from _that_ until exhausted. This
/// is difficult to do with just `Buf` because if we're tying the lifetimes of the frames we emit to the caller to the frame
/// iterator, then we can't prove that we have exclusive access to the buffer in order to advance it after an outer frame
/// has been exhausted. This is sort of a chicken-and-egg problem, and there are some potential solutions in the vein of
/// vector indexing -- track regions of the source buffer instead of holding slices directly -- but they end up being
/// pretty ugly, IMO.
///
/// We've chosen to solve this by splitting things up into two phases: extraction and advancement. Extraction is everything
/// from the time we create `Framed` up until the time it's dropped: we utilize immutable borrows of the source buffer internally
/// to make writing the framers themselves, and the direct/nested wrappers, as easy as possible. Advancement occurs during drop,
/// where we switch back to a mutable borrow of the source buffer in order to advance it. This is where the unsafety lies.
///
/// In order to hold on to the original mutable reference despite needing _immutable_ references for the framers, we
/// convert the mutable reference into a mutable pointer. Creating this pointer initially, and accessing it during drop,
/// is the entirety of the unsafety. This creates a risk of aliasing violations because of how easy it is to create a
/// mutable reference to the buffer while there are active immutable references. We take care to ensure there are no
/// outstanding immutable references to the buffer before recreating the mutable reference from the pointer during drop,
/// which would otherwise represent immediate UB.
struct FramedInner<'buf, 'framer, B>
where
    B: ReadIoBuffer,
{
    buf_ptr: NonNull<B>,
    iter: ManuallyDrop<Iter<'framer, 'buf>>,
    _buf: PhantomData<&'buf mut B>,
}

impl<'buf, 'framer, B> FramedInner<'buf, 'framer, B>
where
    B: ReadIoBuffer,
{
    fn direct(framer: &'framer dyn Framer, buf: &'buf mut B, is_eof: bool) -> Self {
        Self {
            buf_ptr: NonNull::new(buf as *mut B).expect("buffer reference cannot be invalid"),
            iter: ManuallyDrop::new(Iter::Direct(DirectIter::new(framer, buf.chunk(), is_eof))),
            _buf: PhantomData,
        }
    }

    fn nested(
        outer_framer: &'framer dyn Framer, inner_framer: &'framer dyn Framer, buf: &'buf mut B, is_eof: bool,
    ) -> Self {
        Self {
            buf_ptr: NonNull::new(buf as *mut B).expect("buffer reference cannot be invalid"),
            iter: ManuallyDrop::new(Iter::Nested(NestedIter::new(
                outer_framer,
                inner_framer,
                buf.chunk(),
                is_eof,
            ))),
            _buf: PhantomData,
        }
    }
}

impl<'buf, 'framer, B> Drop for FramedInner<'buf, 'framer, B>
where
    B: ReadIoBuffer,
{
    fn drop(&mut self) {
        // Consume the inner iterator to ensure we have no outstanding references to the underlying buffer, and in doing
        // so, get the final buffer length so we know how much to advance by.
        //
        // SAFETY: We only consume the iterator value here in the drop logic, so we know that this logic will only be
        // triggered once, and the iterator can't be taken again.
        let iter = unsafe { ManuallyDrop::take(&mut self.iter) };
        let iter_buf_len = iter.finish();

        // Reconstitute our mutable reference to the underlying buffer.
        //
        // SAFETY: Our iterator implementations only give out immutable references to slices of the buffer tied to the
        // lifetime of the iterator itself, and we've consumed and dropped the iterator right before this point, so we
        // know that there are no other outstanding references to the buffer other than the one we were given when
        // constructing `FramedInner` itself.
        let buf = unsafe { self.buf_ptr.as_mut() };

        // Figure out how far the buffer view from the iterator has been advanced and advance the underlying buffer by
        // that many bytes, which keeps things consistent.
        let advance_by = buf.remaining() - iter_buf_len;
        buf.advance(advance_by);
    }
}

// SAFETY: `ReadIoBuffer` is `Send`, so we can safely hold a mutable pointer of `B` and still be `Send` ourselves.
unsafe impl<'buf, 'framer, B> Send for FramedInner<'buf, 'framer, B> where B: ReadIoBuffer {}

/// An iterator of framed messages over a generic buffer.
///
/// When reading messages from a network socket, it is typical to use a single buffer that is read into and then checked
/// for complete messages: read some data from the socket, extract as many complete messages as possible, over and over
/// again until the connection is closed. Framers help abstract the process of finding these messages in a buffer, but
/// it can still be cumbersome to ensure that the buffer is advanced once a message has been extracted. Advancing the buffer
/// is the final step that ensures we track the progress made, free up the space in the buffer, and allow the buffer to be
/// reused for the next read.
///
/// `Framed` provides a safe abstraction over the process of using an arbitrary `Framer` implementation (or nested
/// implementation, see "Direct vs Nested" section) over an arbitrary buffer implementation. It does this while avoiding
/// unnecessary allocations and copies.
///
/// # Direct vs Nested
///
/// `Framed` is constructed in either "direct" or "nested" mode. In direct mode, a single `Framer` is provided and used
/// for all messages. This is the most common mode: messages are framed individually.
///
/// In nested mode, two `Framer`s are provided: an "outer" framer and an "inner" framer. The outer framer is used to
/// extract top-level frames from the buffer, and when a top-level frame is extracted, the inner framer is used to
/// extract messages from the top-level frame itself, which are ultimately returned to the caller. This allows for more
/// complex framing scenarios where we only care about the "inner" frames, but still want the convenience of using a
/// `Framer` implementation for extracting the outer frames.
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
    /// If there was insufficient data to extract a frame, and it may be possible in the future to do so, `None` is
    /// returned. Otherwise, `Some` is returned with either an extracted frame or an error indicating why a frame could
    /// not be extracted.
    ///
    /// Behavior when EOF is reached is framer-specific and in some cases may allow for decoding a frame even when the
    /// inherent delimiting data is not present.
    ///
    /// # Errors
    ///
    /// If an error is detected when reading the next frame, an error is returned.
    pub fn next_frame(&mut self) -> Option<Result<&[u8], FramingError>> {
        self.inner.iter.next_frame().transpose()
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

    fn nested_payload_direct(raw_outer_frames: &[&[u8]]) -> VecDeque<u8> {
        let mut outer_frames = VecDeque::new();

        for raw_outer_frame in raw_outer_frames {
            outer_frames.extend(&(raw_outer_frame.len() as u32).to_le_bytes());
            outer_frames.extend(raw_outer_frame.iter());
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
    fn framed_nested_multiple_outer_with_zero_length() {
        // We create a buffer that has multiple outer (length delimited) frames each with multiple inner (newline
        // delimited) frames, but one of the outer frames has an empty inner frame, such that the outer frame is just
        // the length delimiter, with a value of 0.
        let input_frames = &[&b"frame1\n"[..], &b""[..], &b"frame3\n"[..]];
        let mut buf = nested_payload_direct(input_frames.as_slice());

        // Create our framer: length-delimited frames on the outside, and newline-delimited frames on the inside.
        let outer = LengthDelimitedFramer::default();
        let inner = NewlineFramer::default();
        let mut framed = Framed::nested(&outer, &inner, &mut buf, false);

        // Now we should be able to extract our original two frames from the buffer.
        //
        // Specifically, this means we correctly handle skipping over the empty outer frame. We filter out the empty
        // frame from our comparison, and we also adjust our comparison since our raw input frames include the newline
        // delimiter.
        for input_frame in input_frames.iter().filter(|frame| !frame.is_empty()) {
            let frame = framed
                .next_frame()
                .expect("should not fail to read from payload")
                .expect("should not fail to extract frame from payload");
            assert_eq!(frame, &input_frame[..input_frame.len() - 1]);
        }

        let maybe_frame = framed.next_frame();
        assert!(maybe_frame.is_none());

        drop(framed);

        // We should have consumed the entire buffer.
        assert!(buf.is_empty());
    }
}
