use std::ops::Deref;

use snafu::Snafu;

mod iter;
pub use self::iter::Framed;

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
    /// limitations, or the buffer is corrupt and spurious bytes are contributing to a decoded frame length that is
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

/// A virtual "view" over a buffer.
///
/// `BufferView` is a lightweight wrapper over a buffer that allows for constructing a virtual "view" -- a subset of the
/// buffer -- that is controlled by arbitrarily offsetting the start and end indices in a user-controlled way.
/// `BufferView` is used, in lieu of manually reslicing the buffer, in order to allow the original byte slice to be
/// captured alongside the virtual offsets. This supports some of the more ergonomic framing helpers which need to know
/// the full size of a frame after extraction, while the view allows delimiters to be removed so that the caller
/// receives a clean frame.
#[derive(Clone)]
pub struct BufferView<'a> {
    buf: &'a [u8],
    idx: usize,
    ridx: usize,
}

impl<'a> BufferView<'a> {
    const fn from_slice(buf: &'a [u8]) -> Self {
        Self { buf, idx: 0, ridx: 0 }
    }

    fn as_bytes(&self) -> &'a [u8] {
        let start = self.idx;
        let end = self.buf.len() - self.ridx;
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
    const fn buf_len(&self) -> usize {
        self.buf.len()
    }

    /// Skips the specified number of bytes from the beginning of the view.
    ///
    /// # Panics
    ///
    /// Panics if the view is too small to skip the specified number of bytes.
    pub fn skip(&mut self, cnt: usize) {
        assert!(
            cnt <= self.len(),
            "buffer too small to skip {} bytes, only {} bytes remaining",
            self.len(),
            cnt,
        );

        self.idx += cnt;
    }

    /// Skips the specified number of bytes from the end of the view.
    ///
    /// # Panics
    ///
    /// Panics if the view is too small to skip the specified number of bytes.
    pub fn rskip(&mut self, cnt: usize) {
        assert!(
            cnt <= self.len(),
            "buffer too small to rskip {} bytes, only {} bytes remaining",
            self.len(),
            cnt,
        );

        self.ridx += cnt;
    }

    /// Consumes the view, returning the virtual byte slice.
    fn into_bytes(self) -> &'a [u8] {
        self.as_bytes()
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

/// A buffer abstraction for carving out frames from a byte slice.
///
/// Framers take an arbitrary byte slice and attempt to extract "frames". Frames are delimited chunks of data, such as
/// newline delimited lines, or data that is prefixed by a length header. Extracting frames thus requires handling two
/// concerns: identifying how large the overall frame is, and removing the framing data so that only the original data
/// is left.
///
/// For example, a newline delimited framer will search for newline characters, and extract all of the data up to that
/// newline character. However, we don't want to return the newline character itself, but we still must effectively
/// "consume" it so that it's removed from the input buffer before we try to extract any subsequent frames. This means
/// that framers cannot simply operate on a raw byte slices: we cannot include the necessary information by returning a
/// byte slice along.
///
/// `RawBuffer` provides a minimal wrapper over a raw byte slice which allows framers to interact with it as if it was a
/// raw byte slice for the purpose for determining if a valid frame is present. Once that is determined, different
/// methods on `RawBuffer` can be used to extract the frame in the form of `BufferView`. `BufferView` is used to hold
/// both the full frame (delimiters included), as well as a "view" over the frame which excludes any frame delimiters.
///
/// By enforcing that `BufferView`s can only be created from `RawBuffer`s, we can provide a more ergonomic way of
/// extracting frames that carry the necessary information for properly advancing the underlying input buffer while
/// ultimately providing the trimmed data back to the caller.
///
/// # Usage
///
/// `RawBuffer` implements `Deref<Target = [u8]>`, and so it can generally be interacted with as if it were a byte
/// slice. Once the frame length has been determined, either `RawBuffer::partial` or `RawBuffer::full` can be used to
/// extract a view over the frame. `RawBuffer::partial` is for cases when a frame delimiter has been found, and there
/// may or may not be additional data in the buffer. `RawBuffer::full` is for cases when we know that we simply want to
/// use all data in the buffer, such as in cases where EOF has been reached and we may opt to return all data in the
/// buffer without requiring a delimiter.
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

/// A trait for reading framed messages from a buffer.
pub trait Framer: Sync {
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
