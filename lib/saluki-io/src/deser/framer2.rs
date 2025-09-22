#![allow(dead_code)]

use std::ops::Range;

use bytes::{buf::UninitSlice, Buf, BufMut};
use tracing::trace;

use crate::deser::framing::FramingError;

/// An I/O buffer.
pub struct BytesBuffer {
    data: Vec<u8>,
    read_idx: usize,
}

impl BytesBuffer {
    /// Creates a new `BytesBuffer` from a slice of bytes.
    pub fn from_slice(data: &[u8]) -> Self {
        BytesBuffer {
            data: data.to_vec(),
            read_idx: 0,
        }
    }

    /// Creates a `BytesBufferView` from the buffer.
    pub fn as_view(&mut self) -> BytesBufferView<'_> {
        BytesBufferView::from_buffer(self)
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

pub trait BufferView: Send {
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn len(&self) -> usize;
    fn capacity(&self) -> usize;
    fn as_bytes(&self) -> &[u8];
    fn advance_idx(&mut self, cnt: usize);

    fn slice_inner(&mut self, len: usize) -> BytesBufferView<'_>;

    /// Advances the view by the given number of bytes, effectively skipping over them.
    fn skip(&mut self, len: usize) {
        assert!(
            len <= self.len(),
            "buffer too small to skip {} bytes, only {} bytes remaining",
            self.len(),
            len,
        );

        self.advance_idx(len);
    }

    /// Creates a new view by slicing this view from the current position to the given index, relative to the view.
    ///
    /// Advances this view by the length of the new view.
    fn slice_to(&mut self, idx: usize) -> BytesBufferView<'_> {
        self.slice_inner(idx)
    }

    /// Creates a new view by slicing this view from the given index, relative to the view, to the end of this view.
    ///
    /// Advances this view by the length of the new view, plus whatever bytes were skipped prior to the new view.
    fn slice_from(&mut self, idx: usize) -> BytesBufferView<'_> {
        self.advance_idx(idx);
        self.slice_inner(self.len())
    }

    /// Creates a new view by slicing this view from the given range, relative to the view.
    ///
    /// Advances this view by the length of the new view, plus whatever bytes were skipped prior to the new view.
    fn slice_range(&mut self, range: Range<usize>) -> BytesBufferView<'_> {
        self.advance_idx(range.start);
        self.slice_inner(range.end - range.start)
    }

    /// Creates a new view by slicing this view from the current position to the end of this view.
    ///
    /// This is equivalent to calling `view.slice_from(0)`, `view.slice_to(view.len())`, or
    /// `view.slice_range(0..view.len())`.
    ///
    /// Advances this view by the length of the new view, plus whatever bytes were skipped prior to the new view.
    fn slice(&mut self) -> BytesBufferView<'_> {
        self.slice_from(0)
    }
}

impl BufferView for BytesBuffer {
    fn len(&self) -> usize {
        self.remaining()
    }

    fn capacity(&self) -> usize {
        self.data.capacity()
    }

    fn as_bytes(&self) -> &[u8] {
        self.chunk()
    }

    fn advance_idx(&mut self, cnt: usize) {
        Buf::advance(self, cnt);
    }

    fn slice_inner(&mut self, len: usize) -> BytesBufferView<'_> {
        assert!(
            len <= self.len(),
            "index out of bounds: the len is {} but the index is {}",
            self.len(),
            len,
        );

        BytesBufferView {
            parent: self,
            len,
            idx_advance: 0,
            ridx_advance: 0,
        }
    }
}

/// An I/O buffer view.
///
/// This struct provides a "view" over an I/O buffer, allowing for efficiently extracting a narrowly scoped "view"
/// (range) from an I/O buffer without copying the data. Views can be nested, allowing for iteratively slicing and
/// trimming a view, such as when handling deserializing network messages that contain headers or trailers.
///
/// # Reading vs consuming
///
/// A buffer view can be used in two ways: reading and consuming.
///
/// Reading allows you to inspect the data without modifying the buffer, such as when searching for a specific character
/// that marks the boundary of a message. Consuming allows you to temporarily take that slice of the buffer for further
/// processing, and then when done, mark that region of the parent buffer as consumed so that it cannot be read again.
/// Essentially, we examine the buffer view to see if we have a full message (reading), and only then do we take a
/// _view_ over that data to process it and remove it (consuming).
///
/// Practically speaking, all methods that create a new view are consuming, and will advance the parent buffer/view by the
/// length of the new view when they are dropped. This means that if you start with a buffer view with a length of 20 bytes,
/// and create a new view of the first 10 bytes, the parent buffer will be advanced by 10 bytes when the new view is dropped.
/// This behavior occurs regardless of how deeply a view is nested, and so the caller can slice up each nested view as desired,
/// ensuring that their respective parent buffer/views are updated accordingly.
pub struct BytesBufferView<'a> {
    parent: &'a mut dyn BufferView,
    len: usize,
    idx_advance: usize,
    ridx_advance: usize,
}

impl<'a> BytesBufferView<'a> {
    /// Creates a new `BytesBufferView` from a `BytesBuffer`.
    pub fn from_buffer(parent: &'a mut BytesBuffer) -> Self {
        let len = parent.chunk().len();
        Self {
            parent,
            len,
            idx_advance: 0,
            ridx_advance: 0,
        }
    }

    fn slice_inner(&mut self, len: usize) -> BytesBufferView<'_> {
        assert!(
            len <= self.len(),
            "index out of bounds: the len is {} but the index is {}",
            self.len(),
            len,
        );

        BytesBufferView {
            parent: self,
            len,
            idx_advance: 0,
            ridx_advance: 0,
        }
    }

    /// Advances the view by the given number of bytes, effectively skipping over them.
    pub fn skip(&mut self, len: usize) {
        assert!(
            len <= self.len(),
            "buffer too small to skip {} bytes, only {} bytes remaining",
            self.len(),
            len,
        );

        self.advance_idx(len);
    }

    /// Advances the view by the given number of bytes, starting at the end of the view, effectively skipping over them.
    pub fn rskip(&mut self, len: usize) {
        assert!(
            len <= self.len(),
            "buffer too small to rskip {} bytes, only {} bytes remaining",
            self.len(),
            len,
        );

        self.ridx_advance += len;
    }
}

impl Drop for BytesBufferView<'_> {
    fn drop(&mut self) {
        self.parent.advance_idx(self.len);
    }
}

impl std::fmt::Debug for BytesBufferView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BytesBufferView")
            .field("len", &self.len)
            .field("idx_advance", &self.idx_advance)
            .finish_non_exhaustive()
    }
}

impl PartialEq for BytesBufferView<'_> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.parent, other.parent) && self.len == other.len && self.idx_advance == other.idx_advance
    }
}

impl BufferView for BytesBufferView<'_> {
    fn len(&self) -> usize {
        self.len - self.idx_advance - self.ridx_advance
    }

    fn capacity(&self) -> usize {
        self.parent.capacity()
    }

    fn as_bytes(&self) -> &[u8] {
        let buf = self.parent.as_bytes();
        let start = self.idx_advance;
        let end = self.len - self.ridx_advance;
        &buf[start..end]
    }

    fn advance_idx(&mut self, cnt: usize) {
        self.idx_advance += cnt;
    }

    fn slice_inner(&mut self, len: usize) -> BytesBufferView<'_> {
        assert!(
            len <= self.len(),
            "index out of bounds: the len is {} but the index is {}",
            self.len(),
            len,
        );

        BytesBufferView {
            parent: self,
            len,
            idx_advance: 0,
            ridx_advance: 0,
        }
    }
}

pub trait Framer2 {
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
    fn next_frame<'buf, B: BufferView>(
        &mut self, buf: &'buf mut B, is_eof: bool,
    ) -> Result<Option<BytesBufferView<'buf>>, FramingError>;
}

impl<F> Framer2 for &mut F
where
    F: Framer2,
{
    fn next_frame<'buf, B: BufferView>(
        &mut self, buf: &'buf mut B, is_eof: bool,
    ) -> Result<Option<BytesBufferView<'buf>>, FramingError> {
        (**self).next_frame(buf, is_eof)
    }
}

#[derive(Default)]
pub struct LengthDelimitedFramer2;

impl Framer2 for LengthDelimitedFramer2 {
    fn next_frame<'buf, B: BufferView>(
        &mut self, buf: &'buf mut B, is_eof: bool,
    ) -> Result<Option<BytesBufferView<'buf>>, FramingError> {
        let chunk = buf.as_bytes();
        if chunk.is_empty() {
            trace!("Buffer empty.");
            return Ok(None);
        }

        let chunk_len = chunk.len();
        trace!(chunk_len, "Processing buffer chunk.");

        // See if there's enough data to read the frame length.
        if chunk_len < 4 {
            return if is_eof {
                Err(FramingError::PartialFrame {
                    needed: 4,
                    remaining: chunk_len,
                })
            } else {
                Ok(None)
            };
        }

        // See if we have enough data to read the full frame.
        let frame_len = u32::from_le_bytes(chunk[0..4].try_into().unwrap()) as usize;
        let frame_len_with_length = frame_len.saturating_add(4);
        if frame_len_with_length > buf.capacity() {
            return Err(oversized_frame_err(frame_len));
        }

        if chunk.len() < frame_len_with_length {
            return if is_eof {
                // If we've hit EOF and we have a partial frame here, well, then... it's invalid.
                Err(FramingError::PartialFrame {
                    needed: frame_len_with_length,
                    remaining: chunk.len(),
                })
            } else {
                Ok(None)
            };
        }

        // Split out the entire frame -- length delimiter included -- and then carve out the length delimiter from the
        // frame that we return.
        let frame = buf.slice_range(4..frame_len_with_length);

        Ok(Some(frame))
    }
}

#[derive(Default)]
pub struct NewlineFramer2 {
    required_on_eof: bool,
}

impl Framer2 for NewlineFramer2 {
    fn next_frame<'buf, B: BufferView>(
        &mut self, buf: &'buf mut B, is_eof: bool,
    ) -> Result<Option<BytesBufferView<'buf>>, FramingError> {
        let chunk = buf.as_bytes();
        if chunk.is_empty() {
            trace!("Buffer empty.");
            return Ok(None);
        }

        let chunk_len = chunk.len();
        trace!(chunk_len, "Processing buffer chunk.");

        // Search through the buffer for our delimiter.
        match find_newline(chunk) {
            Some(idx) => {
                // If we found the delimiter, then slice out the frame from the buffer,
                // and chop the delimiter off the end of the frame.
                let mut frame = buf.slice_to(idx);
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
                    return Err(missing_delimiter_err(chunk.len()));
                }

                Ok(Some(buf.slice()))
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
    type Item<'next>: BufferView
    where
        Self: 'next;

    fn next(&mut self) -> Option<Result<Self::Item<'_>, FramingError>>;
}

struct DirectFrameIterator<F, Buffer> {
    framer: F,
    buf: Buffer,
    is_eof: bool,
}

impl<F, Buffer> DirectFrameIterator<F, Buffer> {
    pub fn new(framer: F, buf: Buffer, is_eof: bool) -> Self {
        Self { framer, buf, is_eof }
    }
}

#[nougat::gat]
impl<'buf, F> FrameIterator for DirectFrameIterator<F, &'buf mut BytesBuffer>
where
    F: Framer2,
{
    type Item<'next>
    where
        Self: 'next,
    = BytesBufferView<'next>;

    fn next<'next>(
        self: &'next mut DirectFrameIterator<F, &'buf mut BytesBuffer>,
    ) -> Option<Result<BytesBufferView<'next>, FramingError>> {
        match self.framer.next_frame(self.buf, self.is_eof) {
            Ok(Some(frame)) => Some(Ok(frame)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

struct NestedFrameIterator<F, F2, Buffer> {
    outer_iter: DirectFrameIterator<F, Buffer>,
    inner: F2,
    current_outer_frame: Option<BytesBufferView<'something>>,
    is_eof: bool,
}

impl<F, F2, Buffer> NestedFrameIterator<F, F2, Buffer> {
    pub fn new(outer: F, inner: F2, buf: Buffer, is_eof: bool) -> Self {
        Self {
            outer_iter: DirectFrameIterator::new(outer, buf, is_eof),
            inner,
            current_outer_frame: None,
            is_eof,
        }
    }
}

#[nougat::gat]
impl<'buf, F, F2> FrameIterator for NestedFrameIterator<F, F2, &'buf mut BytesBuffer>
where
    F: Framer2,
    F2: Framer2,
{
    type Item<'next>
    where
        Self: 'next,
    = BytesBufferView<'next>;

    fn next<'next>(
        self: &'next mut NestedFrameIterator<F, F2, &'buf mut BytesBuffer>,
    ) -> Option<Result<BytesBufferView<'next>, FramingError>> {
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
