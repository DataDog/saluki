use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use async_compression::{
    tokio::write::{ZlibEncoder, ZstdEncoder},
    Level,
};
use http::HeaderValue;
use pin_project::pin_project;
use tokio::io::AsyncWrite;

static CONTENT_ENCODING_DEFLATE: HeaderValue = HeaderValue::from_static("deflate");
static CONTENT_ENCODING_ZSTD: HeaderValue = HeaderValue::from_static("zstd");

/// Compression schemes supported by `Compressor`.
pub enum CompressionScheme {
    /// Zlib.
    Zlib(Level),
    /// Zstd.
    Zstd(Level),
}

impl CompressionScheme {
    /// Zlib compression, using the default compression level (6).
    pub const fn zlib_default() -> Self {
        Self::Zlib(Level::Default)
    }

    /// Zstd compression, using the default compression level (3).
    pub const fn zstd_default() -> Self {
        Self::Zstd(Level::Default)
    }
}

#[pin_project]
pub struct CountingWriter<W> {
    #[pin]
    inner: W,
    total_written: u64,
}

impl<W> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            total_written: 0,
        }
    }

    fn total_written(&self) -> u64 {
        self.total_written
    }

    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: AsyncWrite> AsyncWrite for CountingWriter<W> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        let mut this = self.project();
        this.inner.as_mut().poll_write(cx, buf).map(|result| {
            if let Ok(written) = &result {
                *this.total_written += *written as u64;
            }

            result
        })
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.project().inner.poll_shutdown(cx)
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
    /// Zstd compressor.
    Zstd(#[pin] ZstdEncoder<CountingWriter<W>>),
}

impl<W: AsyncWrite> Compressor<W> {
    /// Creates a new compressor from a given compression scheme and writer.
    pub fn from_scheme(scheme: CompressionScheme, writer: W) -> Self {
        match scheme {
            CompressionScheme::Zlib(level) => Self::Zlib(ZlibEncoder::with_quality(writer, level)),
            CompressionScheme::Zstd(level) => Self::Zstd(ZstdEncoder::with_quality(CountingWriter::new(writer), level)),
        }
    }

    /// Returns the total number of bytes written by the compressor.
    ///
    /// This does not account for any buffered data that has not yet been written to the output stream.
    pub fn total_out(&self) -> u64 {
        match self {
            Self::Zlib(encoder) => encoder.total_out(),
            Self::Zstd(encoder) => encoder.get_ref().total_written(),
        }
    }

    /// Consumes the compressor, returning the inner writer.
    pub fn into_inner(self) -> W {
        match self {
            Self::Zlib(encoder) => encoder.into_inner(),
            Self::Zstd(encoder) => encoder.into_inner().into_inner(),
        }
    }

    /// Returns the appropriate HTTP header value for the compression scheme.
    pub fn header_value(&self) -> HeaderValue {
        match self {
            Self::Zlib(_) => CONTENT_ENCODING_DEFLATE.clone(),
            Self::Zstd(_) => CONTENT_ENCODING_ZSTD.clone(),
        }
    }
}

impl<W: AsyncWrite> AsyncWrite for Compressor<W> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        match self.project() {
            CompressorProjected::Zlib(encoder) => encoder.poll_write(cx, buf),
            CompressorProjected::Zstd(encoder) => encoder.poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.project() {
            CompressorProjected::Zlib(encoder) => encoder.poll_flush(cx),
            CompressorProjected::Zstd(encoder) => encoder.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.project() {
            CompressorProjected::Zlib(encoder) => encoder.poll_shutdown(cx),
            CompressorProjected::Zstd(encoder) => encoder.poll_shutdown(cx),
        }
    }
}
