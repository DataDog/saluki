use std::future::Future;

use remit::Remit;
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

pub trait Framer {
    /// Extracts frames from the buffer and sends them to the given generator.
    fn extract_frames<'a, 'buf, B>(
        &'a mut self,
        buf: &'buf mut B,
        is_eof: bool,
        frames: &Remit<'_, Result<Option<BytesBufferView<'a>>, FramingError>>,
    ) -> impl Future<Output = ()>
    where
        B: BufferView,
        'buf: 'a;
}
