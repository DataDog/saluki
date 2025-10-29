#![allow(dead_code)]
#![allow(warnings)]

use std::{
    mem::ManuallyDrop,
    ops::{Bound, Deref, Range, RangeBounds, RangeTo},
};

use bytes::{buf::UninitSlice, Buf, BufMut};
use tokio_util::codec::FramedParts;
use tracing::trace;

use crate::deser::framing::FramingError;

trait IoBuffer: Buf {
    fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
}

impl<T> IoBuffer for T where T: Buf {}

pub struct BytesBuffer {
    data: Vec<u8>,
    read_idx: usize,
}

impl BytesBuffer {
    /// Creates a new, empty `BytesBuffer`.
    pub fn empty() -> Self {
        BytesBuffer {
            data: Vec::new(),
            read_idx: 0,
        }
    }

    /// Creates a new `BytesBuffer` with the given `capacity`.
    pub fn with_capacity(capacity: usize) -> Self {
        BytesBuffer {
            data: Vec::with_capacity(capacity),
            read_idx: 0,
        }
    }

    /// Creates a new `BytesBuffer` from a slice of bytes.
    pub fn from_slice(data: &[u8]) -> Self {
        BytesBuffer {
            data: data.to_vec(),
            read_idx: 0,
        }
    }
}

impl Buf for BytesBuffer {
    fn remaining(&self) -> usize {
        self.data.len() - self.read_idx
    }

    fn chunk(&self) -> &[u8] {
        &self.data[self.read_idx..self.data.len()]
    }

    fn advance(&mut self, cnt: usize) {
        assert!(self.read_idx + cnt <= self.data.len());
        self.read_idx += cnt;
    }
}

unsafe impl BufMut for BytesBuffer {
    fn remaining_mut(&self) -> usize {
        self.data.capacity() - self.data.len()
    }

    fn chunk_mut(&mut self) -> &mut UninitSlice {
        self.data.spare_capacity_mut().into()
    }

    unsafe fn advance_mut(&mut self, cnt: usize) {
        let new_len = self.data.len() + cnt;
        self.data.set_len(new_len);
    }
}

impl Deref for BytesBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.chunk()
    }
}

#[derive(Default)]
struct BufferViewRange {
    idx: usize,
    ridx: usize,
}

#[derive(Clone)]
pub struct BufferView<'a> {
    buf: &'a [u8],
    idx: usize,
    ridx: usize,
}

impl<'a> BufferView<'a> {
    pub const fn from_slice(buf: &'a [u8]) -> Self {
        Self { buf, idx: 0, ridx: 0 }
    }

    fn as_bytes(&self) -> &'a [u8] {
        let start = self.idx;
        let end = self.len() - self.ridx;
        &self.buf[start..end]
    }

    /// Returns `true` if the view is empty.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the length of the view, in bytes.
    pub const fn len(&self) -> usize {
        self.buf.len() - self.idx - self.ridx
    }

    /// Returns the length of the underlying buffer, in bytes.
    pub const fn buf_len(&self) -> usize {
        self.buf.len()
    }

    pub fn skip(&mut self, len: usize) {
        assert!(
            len <= self.len(),
            "buffer too small to skip {} bytes, only {} bytes remaining",
            self.len(),
            len,
        );

        self.idx += len;
    }

    pub fn rskip(&mut self, len: usize) {
        assert!(
            len <= self.len(),
            "buffer too small to rskip {} bytes, only {} bytes remaining",
            self.len(),
            len,
        );

        self.ridx += len;
    }

    /// Returns the bytes of the view.
    ///
    /// This represents a constrained view of the underlying I/O buffer based on any advancing from the front or back.
    pub fn into_bytes(self) -> &'a [u8] {
        self.as_bytes()
    }
}

impl Deref for BufferView<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_bytes()
    }
}

impl Buf for BufferView<'_> {
    fn remaining(&self) -> usize {
        self.len()
    }

    fn chunk(&self) -> &[u8] {
        self.as_bytes()
    }

    fn advance(&mut self, cnt: usize) {
        self.skip(cnt);
    }
}

impl std::fmt::Debug for BufferView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferView")
            .field("buf", &self.buf.as_ptr())
            .field("idx", &self.idx)
            .field("ridx", &self.ridx)
            .finish()
    }
}

impl PartialEq for BufferView<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

/// A raw input buffer used by framers.
///
/// `RawBuffer` is a wrapper around a byte slice that allows for creating a view over the byte slice when
/// extracting a frame. This ensures that the resulting view represents the entire frame so that the input buffer
/// can be properly advanced after the frame has been processed. Views can be further manipulated in order to
/// constrain the view.
///
/// # Usage
///
/// Framers will receive a `RawBuffer`, which represents all available data in the buffer. Framers are then expected to
/// check if a valid frame can be extracted from the buffer. If so, the framer should either extract a "partial" view or
/// "full" view. For example, if the buffer contains two frames, then it would be expected to create two "partial"
/// views.
///
/// After constructing these views, the views can be further manipulated in order to constrain the view. For example,
/// the view might initially include the frame delimiter, such as a newline character. This can be skipped over such
/// that the view only shows the data before the newline character. However, the view still points to the overall frame
/// (delimiter and all), which allows the framer machinery to properly advance the underlying buffer.
pub struct RawBuffer<'buf> {
    buf: &'buf [u8],
}

impl<'buf> RawBuffer<'buf> {
    /// Creates a new `RawBuffer` from the given buffer.
    pub const fn new(buf: &'buf [u8]) -> RawBuffer<'buf> {
        Self { buf }
    }

    /// Creates a "partial" view from the buffer.
    ///
    /// The view will point to the first `cnt` bytes of the underlying buffer.
    ///
    /// # Panics
    ///
    /// Panics if `cnt` is greater than the buffer length.
    pub fn partial(self, cnt: usize) -> BufferView<'buf> {
        assert!(
            cnt <= self.buf.len(),
            "`cnt` must be less than or equal to the buffer length ({} > {})",
            cnt,
            self.buf.len()
        );

        BufferView::from_slice(&self.buf[..cnt])
    }

    /// Creates a "full" view from the buffer.
    ///
    /// The view will point to the entire buffer.
    pub const fn full(self) -> BufferView<'buf> {
        BufferView::from_slice(self.buf)
    }
}

impl Deref for RawBuffer<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.buf
    }
}

pub trait Framer {
    fn next_frame<'buf>(&self, buf: RawBuffer<'buf>, is_eof: bool) -> Result<Option<BufferView<'buf>>, FramingError>;
}

impl<F> Framer for &F
where
    F: Framer,
{
    fn next_frame<'buf>(&self, buf: RawBuffer<'buf>, is_eof: bool) -> Result<Option<BufferView<'buf>>, FramingError> {
        (**self).next_frame(buf, is_eof)
    }
}

#[derive(Default)]
pub struct LengthDelimitedFramer {
    max_frame_size: usize,
}

impl Framer for LengthDelimitedFramer {
    fn next_frame<'buf>(&self, buf: RawBuffer<'buf>, is_eof: bool) -> Result<Option<BufferView<'buf>>, FramingError> {
        if buf.is_empty() {
            trace!("Buffer empty.");
            return Ok(None);
        }

        trace!(buf_len = buf.len(), "Processing buffer chunk.");

        // See if there's enough data to read the frame length.
        if buf.len() < 4 {
            return if is_eof {
                Err(FramingError::PartialFrame {
                    needed: 4,
                    remaining: buf.len(),
                })
            } else {
                Ok(None)
            };
        }

        // See if we have enough data to read the full frame.
        let frame_len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        let frame_len_with_length = frame_len.saturating_add(4);
        if frame_len_with_length > self.max_frame_size {
            return Err(oversized_frame_err(frame_len));
        }

        if buf.len() < frame_len_with_length {
            return if is_eof {
                // If we've hit EOF and we have a partial frame here, well, then... it's invalid.
                Err(FramingError::PartialFrame {
                    needed: frame_len_with_length,
                    remaining: buf.len(),
                })
            } else {
                Ok(None)
            };
        }

        // Carve out the entire frame, and then adjust our view to start after the delimiter.
        let mut frame = buf.partial(frame_len_with_length);
        frame.skip(4);

        Ok(Some(frame))
    }
}

#[derive(Default)]
pub struct NewlineFramer {
    required_on_eof: bool,
}

impl Framer for NewlineFramer {
    fn next_frame<'buf>(&self, buf: RawBuffer<'buf>, is_eof: bool) -> Result<Option<BufferView<'buf>>, FramingError> {
        if buf.is_empty() {
            trace!("Buffer empty.");
            return Ok(None);
        }

        trace!(buf_len = buf.len(), "Processing buffer chunk.");

        // Search through the buffer for our delimiter.
        match find_newline(&buf[..]) {
            Some(idx) => {
                // If we found the delimiter, then slice out the frame from the buffer,
                // and chop the delimiter off the end of the frame.
                let mut frame = buf.partial(idx);
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
                    return Err(missing_delimiter_err(buf.len()));
                }

                Ok(Some(buf.full()))
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

const fn oversized_frame_err(frame_len: usize) -> FramingError {
    FramingError::InvalidFrame {
        frame_len,
        reason: "frame length exceeds maximum frame length",
    }
}

struct DirectFrameIterator<'framer, 'buf> {
    framer: &'framer dyn Framer,
    buf: &'buf mut BytesBuffer,
    pending_commit: Option<usize>,
    is_eof: bool,
}

impl<'framer, 'buf> DirectFrameIterator<'framer, 'buf> {
    fn next_frame(&mut self) -> Result<Option<&[u8]>, FramingError> {
        if self.buf.is_empty() {
            return Ok(None);
        }

        if let Some(advance_by) = self.pending_commit.take() {
            self.buf.advance(advance_by);
        }

        match self.framer.next_frame(RawBuffer::new(&self.buf), self.is_eof)? {
            Some(frame) => {
                self.pending_commit = Some(frame.buf_len());
                return Ok(Some(frame.into_bytes()));
            }
            None => return Ok(None),
        }
    }
}

struct BufferIndexView {
    len: usize,
    idx: usize,
    ridx: usize,
}

impl BufferIndexView {
    fn from_view(view: &BufferView<'_>) -> Self {
        Self {
            len: view.buf_len(),
            idx: view.idx,
            ridx: view.ridx,
        }
    }

    fn is_empty(&self) -> bool {
        debug_assert!(self.idx <= self.ridx, "idx should never exceed ridx");
        self.idx == self.ridx
    }

    fn buf_len(&self) -> usize {
        self.len
    }

    fn materialize<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        &buf[self.idx..self.ridx]
    }

    fn advance(&mut self, advance_by: usize) {
        self.idx += advance_by;
    }
}

struct NestedFrameIterator<'framer, 'buf> {
    outer_framer: &'framer dyn Framer,
    inner_framer: &'framer dyn Framer,
    root_buf: &'buf mut BytesBuffer,
    outer_frame_index_view: Option<BufferIndexView>,
    pending_root_commit: Option<usize>,
    is_eof: bool,
}

impl<'framer, 'buf> NestedFrameIterator<'framer, 'buf> {
    fn next_frame(&mut self) -> Result<Option<&[u8]>, FramingError> {
        loop {
            // Check if we have an active outer frame, and if so, if it's empty or not.
            //
            // If it's empty, we need to advance our root buffer and clear the outer frame so that we try to grab a new one.
            //
            // TODO: This isn't necessarily as efficient as it could be, since we end up doing a sort of equivalent check below
            if self.outer_frame_index_view.as_ref().map_or(false, |iv| iv.is_empty()) {
                // Our outer frame has been exhausted, so set `outer_frame_index_view` to `None` to ensure we
                // try grabbing a new one.
                let outer_frame_index_view = self.outer_frame_index_view.take().unwrap();
                self.root_buf.advance(outer_frame_index_view.buf_len());
            }

            // Grab the current outer frame, or try grabbing a new one.
            let outer_frame_index_view = match self.outer_frame_index_view.as_mut() {
                // We still have a non-empty outer frame, so rematerialize the outer frame buffer.
                Some(index_view) => index_view,
                None => match self
                    .outer_framer
                    .next_frame(RawBuffer::new(&self.root_buf), self.is_eof)?
                {
                    Some(frame) => {
                        self.outer_frame_index_view = Some(BufferIndexView::from_view(&frame));
                        self.outer_frame_index_view.as_mut().unwrap()
                    }
                    None => return Ok(None),
                },
            };

            let outer_frame = outer_frame_index_view.materialize(&self.root_buf);
            assert!(
                !outer_frame.is_empty(),
                "outer frame should not be empty when index view is not empty"
            );

            // Now try to extract the next inner frame.
            match self.inner_framer.next_frame(RawBuffer::new(outer_frame), self.is_eof)? {
                Some(frame) => {
                    outer_frame_index_view.advance(frame.buf_len());
                    return Ok(Some(frame.into_bytes()));
                }
                None => return Ok(None),
            }
        }
    }
}

/*
#[ouroboros::self_referencing]
struct RootBufferInner<'buf> {
    buf: &'buf mut BytesBuffer,
    #[borrows(buf)]
    #[covariant]
    view: BufferView<'this>,
}

struct RootBuffer<'buf>(ManuallyDrop<RootBufferInner<'buf>>);

impl<'buf> RootBuffer<'buf> {
    fn new(buf: &'buf mut BytesBuffer) -> Self {
        Self(ManuallyDrop::new(RootBufferInner::new(buf, |buf| {
            BufferView::from_slice(buf)
        })))
    }

    fn is_empty(&self) -> bool {
        self.0.borrow_view().is_empty()
    }

    fn advance(&mut self, cnt: usize) {
        self.0.with_view_mut(|view| view.skip(cnt));
    }
}

impl<'buf> Deref for RootBuffer<'buf> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.0.borrow_view()
    }
}

impl<'buf> Drop for RootBuffer<'buf> {
    fn drop(&mut self) {
        // Figure out how far to advance the underlying buffer by seeing how far we advanced the view.
        let advance_by = self.0.borrow_buf().len() - self.0.borrow_view().len();

        // Consume the inner wrapper so we can recover the underlying buffer and advance it.
        //
        // SAFETY: We're currently dropping, which will only be called once.
        let inner = unsafe { ManuallyDrop::take(&mut self.0) };
        let heads = inner.into_heads();

        unsafe { heads.buf.advance(advance_by) }
    }
}

#[nougat::gat]
trait FrameIterator {
    type Item<'next>
    where
        Self: 'next;

    fn next(&mut self) -> Option<Result<Self::Item<'_>, FramingError>>;
}

struct DirectFrameIterator<'buf, F, B: IoBuffer> {
    framer: F,
    buf: &'buf mut B,
    last_frame_len: usize,
    is_eof: bool,
}

impl<'buf, F, B> DirectFrameIterator<'buf, F, B>
where
    B: IoBuffer,
{
    pub fn new(framer: F, buf: &'buf mut B, is_eof: bool) -> Self {
        Self {
            framer,
            buf,
            last_frame_len: 0,
            is_eof,
        }
    }

    fn handle_pending_advance(&mut self) {
        if self.last_frame_len != 0 {
            self.buf.advance(self.last_frame_len);
            self.last_frame_len = 0;
        }
    }
}

impl<'buf, F, B> Drop for DirectFrameIterator<'buf, F, B>
where
    B: IoBuffer,
{
    fn drop(&mut self) {
        self.handle_pending_advance();
    }
}

#[nougat::gat]
impl<'buf, F, B> FrameIterator for DirectFrameIterator<'buf, F, B>
where
    F: Framer3,
    B: IoBuffer,
{
    type Item<'next>
    where
        Self: 'next,
    = BufferView<'next>;

    fn next<'next>(
        self: &'next mut DirectFrameIterator<'buf, F, B>,
    ) -> Option<Result<BufferView<'next>, FramingError>> {
        self.handle_pending_advance();

        if self.buf.is_empty() {
            return None;
        }

        match self.framer.next_frame(&self.buf, self.is_eof) {
            Ok(Some(frame)) => {
                // Track how large the view is, which represents the number of bytes consumed from the buffer
                // overall -- which we need to advance past in `self.buf` -- but not necessarily how large the
                // frame view is (e.g., we might be consuming delimiter bytes, but excluding them from the view).
                self.last_frame_len = frame.buf_len();

                Some(Ok(frame))
            }
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}
*/

#[cfg(test)]
mod tests {
    use super::FrameIterator as _;
    use super::*;

    enum TestFramerMode {
        Empty,
        ConsumeAll,
    }

    struct TestFramer(TestFramerMode);

    impl TestFramer {
        pub const fn empty() -> Self {
            Self(TestFramerMode::Empty)
        }

        pub const fn consume_all() -> Self {
            Self(TestFramerMode::ConsumeAll)
        }
    }

    impl Framer for TestFramer {
        fn next_frame<'buf, B: IoBuffer>(
            &self, buf: &'buf B, _: bool,
        ) -> Result<Option<BufferView<'buf>>, FramingError> {
            match self.0 {
                TestFramerMode::Empty => Ok(None),
                TestFramerMode::ConsumeAll => Ok(Some(BufferView::from_io_buffer(buf))),
            }
        }
    }

    fn vec_from_bufs(bufs: &[&[u8]]) -> Vec<u8> {
        let mut data = Vec::new();
        for buf in bufs {
            data.extend_from_slice(buf);
        }
        data
    }

    #[test]
    fn newline_framer_basic() {
        let input_no_nl = &b"hello, world!"[..];
        let input_with_nl = &b"hello, world!\n"[..];

        let cases = [
            // input, expected, required_on_eof, eof
            (input_with_nl, Ok(Some(input_no_nl)), false, false),
            (input_with_nl, Ok(Some(input_no_nl)), false, true),
            (input_no_nl, Ok(Some(input_no_nl)), false, true),
            (input_no_nl, Ok(None), false, false),
            (input_no_nl, Err(missing_delimiter_err(input_no_nl.len())), true, true),
        ];

        for (input, expected, required_on_eof, eof) in cases {
            let mut buf = BytesBuffer::from_slice(input);
            let expected = expected.map(|mf| mf.map(BufferView::from_slice));

            let mut framer = NewlineFramer::default();
            framer.required_on_eof = required_on_eof;

            let maybe_frame = framer.next_frame(&mut buf, eof);
            assert_eq!(maybe_frame, expected);
        }
    }

    #[test]
    fn newline_framer_buf_empty() {
        let mut buf = BytesBuffer::empty();
        let mut framer = NewlineFramer::default();

        let maybe_frame = framer.next_frame(&mut buf, false);
        assert_eq!(maybe_frame, Ok(None));

        let maybe_frame = framer.next_frame(&mut buf, true);
        assert_eq!(maybe_frame, Ok(None));
    }

    #[test]
    fn newline_framer_direct_iter() {
        let empty = &b""[..];
        let input1_no_nl = &b"hello, world!"[..];
        let input1_with_nl = &b"hello, world!\n"[..];
        let input2_no_nl = &b"it's me!"[..];
        let input2_with_nl = &b"it's me!\n"[..];

        let cases = [
            // input, expected frames, required_on_eof, eof
            (&[input1_with_nl][..], vec![Some(Ok(input1_no_nl)), None], false, false),
            (
                &[input1_with_nl, input2_with_nl][..],
                vec![Some(Ok(input1_no_nl)), Some(Ok(input2_no_nl)), None],
                false,
                false,
            ),
            (
                &[input1_with_nl, input2_no_nl][..],
                vec![Some(Ok(input1_no_nl)), None],
                false,
                false,
            ),
            (
                &[input1_with_nl, input2_no_nl][..],
                vec![Some(Ok(input1_no_nl)), Some(Ok(input2_no_nl))],
                false,
                true,
            ),
            (
                &[input1_with_nl, input2_no_nl][..],
                vec![
                    Some(Ok(input1_no_nl)),
                    Some(Err(missing_delimiter_err(input2_no_nl.len()))),
                ],
                true,
                true,
            ),
        ];

        for (input, expected_frames, required_on_eof, eof) in cases {
            let raw_buf = vec_from_bufs(input);
            let mut buf = BytesBuffer::from_slice(&raw_buf);

            let mut framer = NewlineFramer::default();
            framer.required_on_eof = required_on_eof;

            let mut frame_iter = DirectFrameIterator::new(framer, &mut buf, eof);

            for expected in expected_frames {
                let expected = expected.map(|mf| mf.map(BufferView::from_slice));

                let maybe_frame = frame_iter.next();
                assert_eq!(maybe_frame, expected);
            }
        }
    }

    #[test]
    fn direct_frame_iter_basic() {
        // When the framer returns `Ok(None)`, the frame iterator should return `None`.
        let mut buf1 = BytesBuffer::empty();
        let mut frame_iter1 = DirectFrameIterator::new(TestFramer::empty(), &mut buf1, false);
        assert_eq!(frame_iter1.next(), None);

        // When the framer consumes the entire/remainder of the buffer, and the buffer is now empty,
        // the frame iterator should return `None` after that point.
        let input = "hello, world!";

        let mut buf2 = BytesBuffer::from_slice(input.as_bytes());
        let mut frame_iter2 = DirectFrameIterator::new(TestFramer::consume_all(), &mut buf2, false);
        assert_eq!(frame_iter2.next(), Some(Ok(BufferView::from_slice(input.as_bytes()))));
        assert_eq!(frame_iter2.next(), None);
    }
}
