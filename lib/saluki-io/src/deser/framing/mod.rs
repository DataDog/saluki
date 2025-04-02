use snafu::Snafu;

use crate::buf::{BufferView, BytesBufferView};

mod length_delimited;
pub use self::length_delimited::LengthDelimitedFramer;

mod nested;
//pub use self::nested::NestedFramer;

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

pub trait FramerLifetime<'this, ImplicitBounds: Sealed = Bounds<&'this Self>> {
	type Frame;
}

mod sealed {
	pub trait Sealed: Sized {}
	pub struct Bounds<T>(T);
	impl<T> Sealed for Bounds<T> {}
}
use self::sealed::{Bounds, Sealed};

/// A trait for reading framed messages from a buffer.
pub trait Framer: for<'this> FramerLifetime<'this> {
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
    fn next_frame<'a, 'buf, B>(
        &'a mut self, buf: &'a mut B, is_eof: bool,
    ) -> Result<Option<<Self as FramerLifetime<'_>>::Frame, FramingError>
    where
        B: BufferView,
        'buf: 'a;
}

/// An iterator of framed messages over a generic buffer.
pub struct Framed<'a, 'buf, F> {
    framer: &'a mut F,
    buffer: &'a mut BytesBufferView<'buf>,
    is_eof: bool,
}

impl<'a, 'buf, F> Framed<'a, 'buf, F> {
    /// Creates a new `Framed` over the given view, using the given framer.
    pub fn from_view(framer: &'a mut F, buffer: &'a mut BytesBufferView<'buf>, is_eof: bool) -> Self {
        Self { framer, buffer, is_eof }
    }
}

impl<'a, 'buf, F> Framed<'a, 'buf, F>
where
    F: Framer,
    'buf: 'a,
{
    /// Extracts the next frame from the buffer if one is available.
    ///
    /// If a frame is available, `Ok(Some(frame))` is returned. If no frame is available, and EOF has not yet been
    /// reached, `Ok(None)` is returned.
    ///
    /// # Errors
    ///
    /// If an error is encountered while reading the next frame, which can potentially include encountering an invalid
    /// frame prior to EOF, or failing to read a valid frame when EOF has been reached, an error is returned.
    pub fn try_next<'b>(&'b mut self) -> Result<Option<F::Frame<'b>>, FramingError> {
        self.framer.next_frame(self.buffer, self.is_eof)
    }
}
