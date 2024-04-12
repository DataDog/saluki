use snafu::Snafu;

use super::Decoder;

mod length_delimited;
pub use self::length_delimited::LengthDelimitedFramer;

mod newline;
pub use self::newline::NewlineFramer;

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum FramingError<D: Decoder> {
    #[snafu(display("decoder error: {}", source))]
    FailedToDecode { source: D::Error },
    #[snafu(display(
        "received invalid frame (hit EOF and couldn't not parse frame, {} bytes remaining)",
        buffer_len
    ))]
    InvalidFrame { buffer_len: usize },
    #[snafu(display("received frame that contained no decodable events ({} bytes remaining)", frame_len))]
    UndecodableFrame { frame_len: usize },
}

pub trait Framer<D: Decoder> {
    type Output: Decoder;

    fn with_decoder(self, decoder: D) -> Self::Output;
}
