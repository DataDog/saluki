use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use async_compression::{tokio::write::ZlibEncoder, Level};
use pin_project::pin_project;
use tokio::io::AsyncWrite;

/// Compression schemes supported by `Compressor`.
pub enum CompressionScheme {
    /// Zlib.
    Zlib(Level),
}

impl CompressionScheme {
    /// Zlib compression, using the default compression level (6).
    pub const fn zlib_default() -> Self {
        Self::Zlib(Level::Default)
    }
}

/// Generic compressor.
///
/// Exposes a semi-type-erased compression stream, by allowing the compression to be configured via `CompressionScheme`,
/// and generically wrapping over a given writer.
#[pin_project(project = CompressorProjected)]
pub enum Compressor<W: AsyncWrite> {
    /// Zlib compressor.
    Zlib(#[pin] ZlibEncoder<W>),
}

impl<W: AsyncWrite> Compressor<W> {
    /// Creates a new compressor from a given compression scheme and writer.
    pub fn from_scheme(scheme: CompressionScheme, writer: W) -> Self {
        match scheme {
            CompressionScheme::Zlib(level) => Self::Zlib(ZlibEncoder::with_quality(writer, level)),
        }
    }

    /// Consumes the compressor, returning the inner writer.
    pub fn into_inner(self) -> W {
        match self {
            Self::Zlib(encoder) => encoder.into_inner(),
        }
    }
}

impl<W: AsyncWrite> AsyncWrite for Compressor<W> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        match self.project() {
            CompressorProjected::Zlib(encoder) => encoder.poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.project() {
            CompressorProjected::Zlib(encoder) => encoder.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.project() {
            CompressorProjected::Zlib(encoder) => encoder.poll_shutdown(cx),
        }
    }
}
