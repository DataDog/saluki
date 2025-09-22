#![allow(dead_code)]
#![allow(warnings)]

use std::ops::{Bound, Deref, Range, RangeBounds, RangeTo};

use bytes::{buf::UninitSlice, Buf, BufMut};
use tokio_util::codec::FramedParts;
use tracing::trace;

use crate::deser::framing::FramingError;

trait IoBuffer: Buf {
    fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn capacity(&self) -> usize;
}

impl<T> IoBuffer for &mut T
where
    T: IoBuffer,
{
    fn is_empty(&self) -> bool {
        (**self).is_empty()
    }

    fn capacity(&self) -> usize {
        (*self).capacity()
    }
}

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

    pub fn as_view(&self) -> BufferView<'_> {
        BufferView::from_io_buffer(self)
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

impl IoBuffer for BytesBuffer {
    fn capacity(&self) -> usize {
        self.data.capacity()
    }
}

#[derive(Clone)]
pub struct BufferView<'a> {
    buf: &'a [u8],
    len: usize,
    idx: usize,
    ridx: usize,
}

impl<'a> BufferView<'a> {
    #[cfg(test)]
    pub fn from_slice(buf: &'a [u8]) -> Self {
        Self {
            buf,
            len: buf.len(),
            idx: 0,
            ridx: 0,
        }
    }

    /// Creates a new `BufferView` from a given `IoBuffer`.
    pub fn from_io_buffer<B: IoBuffer>(buf: &'a B) -> Self {
        Self {
            buf: buf.chunk(),
            len: buf.remaining(),
            idx: 0,
            ridx: 0,
        }
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
        self.len
    }

    /// Returns the bytes of the view.
    ///
    /// This represents a constrained view of the underlying I/O buffer based on any advancing from the front or back.
    pub fn as_bytes(&self) -> &[u8] {
        let start = self.idx;
        let end = self.buf.len() - self.ridx;
        &self.buf[start..end]
    }

    /// Extracts a region of this view as a new view.
    ///
    /// The extracted view will contain `len` bytes from the beginning of the view.
    ///
    /// # Panics
    ///
    /// Panics if `len` is greater than the length of the view.
    pub fn extract(&self, len: usize) -> BufferView<'a> {
        let view_len = self.len();
        assert!(
            view_len >= len,
            "buffer too small to extract {} bytes, only {} bytes remaining",
            len,
            view_len
        );

        let new_ridx = self.ridx + (view_len - len);

        BufferView {
            buf: self.buf,
            len,
            idx: self.idx,
            ridx: new_ridx,
        }
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

    pub fn take(&mut self, len: usize) {
        assert!(
            len <= self.len(),
            "buffer too small to take {} bytes, only {} bytes remaining",
            self.len(),
            len,
        );

        let ridx_offset = self.len() - len;
        self.ridx += ridx_offset;
    }
}

impl Deref for BufferView<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_bytes()
    }
}

impl std::fmt::Debug for BufferView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferView")
            .field("buf", &self.buf.as_ptr())
            .field("len", &self.len)
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

pub trait Framer3 {
    fn next_frame<'buf, B: IoBuffer>(
        &self, buf: &'buf B, is_eof: bool,
    ) -> Result<Option<BufferView<'buf>>, FramingError>;
}

impl<F> Framer3 for &mut F
where
    F: Framer3,
{
    fn next_frame<'buf, B: IoBuffer>(
        &self, buf: &'buf B, is_eof: bool,
    ) -> Result<Option<BufferView<'buf>>, FramingError> {
        (**self).next_frame(buf, is_eof)
    }
}

#[derive(Default)]
pub struct LengthDelimitedFramer3;

impl Framer3 for LengthDelimitedFramer3 {
    fn next_frame<'buf, B: IoBuffer>(
        &self, buf: &'buf B, is_eof: bool,
    ) -> Result<Option<BufferView<'buf>>, FramingError> {
        if buf.is_empty() {
            trace!("Buffer empty.");
            return Ok(None);
        }

        let mut view = BufferView::from_io_buffer(buf);
        let buf_len = view.len();
        trace!(buf_len, "Processing buffer chunk.");

        // See if there's enough data to read the frame length.
        if buf_len < 4 {
            return if is_eof {
                Err(FramingError::PartialFrame {
                    needed: 4,
                    remaining: buf_len,
                })
            } else {
                Ok(None)
            };
        }

        // See if we have enough data to read the full frame.
        let frame_len = u32::from_le_bytes(view[0..4].try_into().unwrap()) as usize;
        let frame_len_with_length = frame_len.saturating_add(4);
        if frame_len_with_length > buf.capacity() {
            return Err(oversized_frame_err(frame_len));
        }

        if view.len() < frame_len_with_length {
            return if is_eof {
                // If we've hit EOF and we have a partial frame here, well, then... it's invalid.
                Err(FramingError::PartialFrame {
                    needed: frame_len_with_length,
                    remaining: view.len(),
                })
            } else {
                Ok(None)
            };
        }

        // Carve out the frame, excluding the length delimiter.
        let mut frame = view.extract(frame_len_with_length);
        frame.skip(4);

        Ok(Some(frame))
    }
}

#[derive(Default)]
pub struct NewlineFramer3 {
    required_on_eof: bool,
}

impl Framer3 for NewlineFramer3 {
    fn next_frame<'buf, B: IoBuffer>(
        &self, buf: &'buf B, is_eof: bool,
    ) -> Result<Option<BufferView<'buf>>, FramingError> {
        if buf.is_empty() {
            trace!("Buffer empty.");
            return Ok(None);
        }

        let mut view = BufferView::from_io_buffer(buf);
        let buf_len = view.len();
        trace!(buf_len, "Processing buffer chunk.");

        // Search through the buffer for our delimiter.
        match find_newline(&view) {
            Some(idx) => {
                // If we found the delimiter, then slice out the frame from the buffer,
                // and chop the delimiter off the end of the frame.
                let mut frame = view.extract(idx + 1);
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
                    return Err(missing_delimiter_err(view.len()));
                }

                Ok(Some(view))
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
        reason: "frame length exceeds buffer capacity",
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
    last_frame: Option<BufferView<'buf>>,
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
            last_frame: None,
            is_eof,
        }
    }

    fn handle_pending_advance(&mut self) {
        if let Some(last_frame) = self.last_frame.take() {
            self.buf.advance(last_frame.buf_len());
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
                self.last_frame = Some(frame.clone());

                Some(Ok(frame))
            }
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

struct NestedFrameIterator<'outer, F, F2, Buffer> {
    outer: F,
    inner: F2,
    buf: Buffer,
    current_outer_frame: Option<BufferView<'outer>>,
    is_eof: bool,
}

impl<'outer, F, F2, Buffer> NestedFrameIterator<'outer, F, F2, Buffer> {
    pub fn new(outer: F, inner: F2, buf: Buffer, is_eof: bool) -> Self {
        Self {
            outer,
            inner,
            buf,
            current_outer_frame: None,
            is_eof,
        }
    }
}

#[nougat::gat]
impl<'buf, F, F2> FrameIterator for NestedFrameIterator<'buf, F, F2, &'buf mut BytesBuffer>
where
    F: Framer3,
    F2: Framer3,
{
    type Item<'next>
    where
        Self: 'next,
    = BufferView<'next>;

    fn next<'next>(
        self: &'next mut NestedFrameIterator<F, F2, &'buf mut BytesBuffer>,
    ) -> Option<Result<BufferView<'next>, FramingError>> {
        loop {
            // Take our current outer frame, or if we have none, try to get the next one.
            let outer_frame = match self.current_outer_frame.as_mut() {
                Some(frame) => {
                    trace!(frame_len = frame.len(), "Using existing outer frame.");

                    frame
                }
                None => {
                    trace!("No existing outer frame.");

                    match self.outer_iter.next()? {
                        Ok(frame) => {
                            trace!(frame_len = frame.len(), ?frame, "Extracted outer frame.");

                            self.current_outer_frame.get_or_insert(frame)
                        }

                        // If we can't get another outer frame, then we're done for now.
                        Err(e) => return Some(Err(e)),
                    }
                }
            };

            // Try to get the next inner frame.
            match self.inner.next_frame(outer_frame, true) {
                Ok(Some(frame)) => {
                    trace!(
                        outer_frame_len = outer_frame.len(),
                        inner_frame_len = frame.len(),
                        "Extracted inner frame."
                    );

                    return Some(Ok(frame));
                }
                Ok(None) => {
                    // We can't get anything else from our inner frame. If our outer frame is empty, and our input buffer
                    // isn't empty, clear the current outer frame so that we can try to grab the next one.
                    trace!(
                        outer_frame_len = outer_frame.len(),
                        "Couldn't extract inner frame from existing outer frame."
                    );

                    if outer_frame.is_empty() {
                        self.current_outer_frame = None;
                    } else {
                        return None;
                    }
                }
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

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

    impl Framer3 for TestFramer {
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

            let mut framer = NewlineFramer3::default();
            framer.required_on_eof = required_on_eof;

            let maybe_frame = framer.next_frame(&mut buf, eof);
            assert_eq!(maybe_frame, expected);
        }
    }

    #[test]
    fn newline_framer_buf_empty() {
        let mut buf = BytesBuffer::empty();
        let mut framer = NewlineFramer3::default();

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

            let mut framer = NewlineFramer3::default();
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
