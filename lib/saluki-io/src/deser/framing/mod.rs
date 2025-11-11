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
    fn next_frame<'buf>(&self, buf: &mut &'buf [u8], is_eof: bool) -> Result<Option<&'buf [u8]>, FramingError>;
}

impl<F> Framer for &F
where
    F: Framer,
{
    fn next_frame<'buf>(&self, buf: &mut &'buf [u8], is_eof: bool) -> Result<Option<&'buf [u8]>, FramingError> {
        (**self).next_frame(buf, is_eof)
    }
}
