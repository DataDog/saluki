use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use async_compression::{
    tokio::write::{ZlibEncoder, ZstdEncoder},
    Level,
};
use average::{Estimate as _, Variance};
use http::HeaderValue;
use pin_project::pin_project;
use tokio::io::AsyncWrite;
use tracing::info;

static CONTENT_ENCODING_DEFLATE: HeaderValue = HeaderValue::from_static("deflate");
static CONTENT_ENCODING_ZSTD: HeaderValue = HeaderValue::from_static("zstd");

/// Compression schemes supported by `Compressor`.
#[derive(Copy, Clone, Debug)]
pub enum CompressionScheme {
    /// No compression.
    Noop,
    /// Zlib.
    Zlib(Level),
    /// Zstd.
    Zstd(Level),
}

impl CompressionScheme {
    /// No compression.
    pub const fn noop() -> Self {
        Self::Noop
    }

    /// Zlib compression, using the default compression level (6).
    pub const fn zlib_default() -> Self {
        Self::Zlib(Level::Default)
    }

    /// Zstd compression, using the default compression level (3).
    pub const fn zstd_default() -> Self {
        Self::Zstd(Level::Default)
    }

    /// Create a new compression scheme from a string and level.
    ///
    /// Level is only used if the scheme is `zstd`.
    ///
    /// Defaults to zstd with level 3.
    pub fn new(scheme: &str, level: i32) -> Self {
        match scheme {
            "zlib" => CompressionScheme::zlib_default(),
            "zstd" => Self::Zstd(Level::Precise(level)),
            _ => Self::Zstd(Level::Default),
        }
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
    /// No-op compressor.
    Noop(#[pin] CountingWriter<W>),
    /// Zlib compressor.
    Zlib(#[pin] ZlibEncoder<W>),
    /// Zstd compressor.
    Zstd(#[pin] ZstdEncoder<CountingWriter<W>>),
}

impl<W: AsyncWrite> Compressor<W> {
    /// Creates a new compressor from a given compression scheme and writer.
    pub fn from_scheme(scheme: CompressionScheme, writer: W) -> Self {
        match scheme {
            CompressionScheme::Noop => Self::Noop(CountingWriter::new(writer)),
            CompressionScheme::Zlib(level) => Self::Zlib(ZlibEncoder::with_quality(writer, level)),
            CompressionScheme::Zstd(level) => Self::Zstd(ZstdEncoder::with_quality(CountingWriter::new(writer), level)),
        }
    }

    /// Returns the total number of bytes written by the compressor.
    ///
    /// This does not account for any buffered data that has not yet been written to the output stream.
    pub fn total_out(&self) -> u64 {
        match self {
            Self::Noop(encoder) => encoder.total_written(),
            Self::Zlib(encoder) => encoder.total_out(),
            Self::Zstd(encoder) => encoder.get_ref().total_written(),
        }
    }

    /// Consumes the compressor, returning the inner writer.
    pub fn into_inner(self) -> W {
        match self {
            Self::Noop(encoder) => encoder.into_inner(),
            Self::Zlib(encoder) => encoder.into_inner(),
            Self::Zstd(encoder) => encoder.into_inner().into_inner(),
        }
    }

    /// Returns the content encoding for this compressor.
    pub fn content_encoding(&self) -> Option<HeaderValue> {
        match self {
            Self::Noop(_) => None,
            Self::Zlib(_) => Some(CONTENT_ENCODING_DEFLATE.clone()),
            Self::Zstd(_) => Some(CONTENT_ENCODING_ZSTD.clone()),
        }
    }
}

impl<W: AsyncWrite> AsyncWrite for Compressor<W> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        match self.project() {
            CompressorProjected::Noop(encoder) => encoder.poll_write(cx, buf),
            CompressorProjected::Zlib(encoder) => encoder.poll_write(cx, buf),
            CompressorProjected::Zstd(encoder) => encoder.poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.project() {
            CompressorProjected::Noop(encoder) => encoder.poll_flush(cx),
            CompressorProjected::Zlib(encoder) => encoder.poll_flush(cx),
            CompressorProjected::Zstd(encoder) => encoder.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.project() {
            CompressorProjected::Noop(encoder) => encoder.poll_shutdown(cx),
            CompressorProjected::Zlib(encoder) => encoder.poll_shutdown(cx),
            CompressorProjected::Zstd(encoder) => encoder.poll_shutdown(cx),
        }
    }
}

/// A streaming estimator for the size of compressed data.
///
/// For many compression algorithms, there is a large amount of buffering and state during compression. This allows
/// compression algorithms to better compress data by finding patterns across the current and previous inputs, as well
/// as amortize how often they write compressed data to the output stream, increasing the potential efficiency of the
/// related function or system calls to do so.
///
/// However, this presents a problem when there is a need to ensure that the size of the compressed data does not exceed
/// a certain threshold. As many inputs can be written to the compressor before the next chunk of compressed data is
/// output, it is possible to write enough data that the compressed output exceeds the threshold. Further, many
/// compression algorithms/implementations do not provide a way to query the size of the compressed data without
/// expensive operations that either require doing multiple compression passes on different slices of the data, or early
/// flushing of compressed data, potentially leading to abnormally low compression ratios.
///
/// This estimator provides a way to estimate the size of the compressed data by combining both the known size of data
/// written to the compressor's output stream, as well as the inputs written to the compressor. We track the state
/// changes of the compressor, observing when it writes compressed data to the output stream. We additionally track
/// every write in terms of its uncompressed size. In combining the two, we estimate the worst-case size of the
/// compressed data based on what we know has been compressed so far and what we've written since the last time the
/// compressed flush to the output stream.
///
/// TODO: We should probably move this into `Compressor` itself, because it will also make it easier to do
/// per-compression-algorithm tweaks to the estimation logic if that's a path we want to take, and it also would be
/// cleaner and let us avoid any footguns around forgetting to update the necessary estimator state, etc.
#[derive(Debug, Default)]
pub struct CompressionEstimator {
    known_compressed_len: u64,
    in_flight_uncompressed_len: usize,
    block_compression_ratio_variance: Variance,
}

impl CompressionEstimator {
    /// Tracks a write to the compressor.
    pub fn track_write<W>(&mut self, compressor: &Compressor<W>, uncompressed_len: usize)
    where
        W: AsyncWrite,
    {
        self.in_flight_uncompressed_len += uncompressed_len;

        let compressed_len = compressor.total_out();
        let compressed_len_delta = (compressed_len - self.known_compressed_len) as usize;
        if compressed_len_delta > 0 {
            // Calculate the compression ratio for the block we just observed being flushed based on how many in-flight
            // uncompressed bytes we were tracking. We don't try and compensate for the fact that only a few bytes of
            // the last write may have actually been compressed, which could erroneously drive up the estimated
            // compression ratio for that block.
            //
            // This does mean that some blocks may be under or over their actual compression ratio, but it should
            // generally even out over the course of a full payload.
            let block_compression_ratio = compressed_len_delta as f64 / self.in_flight_uncompressed_len as f64;
            self.block_compression_ratio_variance.add(block_compression_ratio);

            self.known_compressed_len = compressed_len;
            self.in_flight_uncompressed_len = 0;

            info!(
                block_size = compressed_len_delta,
                block_compression_ratio,
                compressed_len = self.known_compressed_len,
                "Compressor wrote block to output stream."
            );
        }
    }

    /// Resets the estimator.
    pub fn reset(&mut self) {
        self.known_compressed_len = 0;
        self.in_flight_uncompressed_len = 0;
        self.block_compression_ratio_variance = Variance::default();
    }

    /// Returns the estimated length of the compressor.
    ///
    /// This figure is the sum of the total bytes written by the compressor to the output stream and the number of
    /// uncompressed bytes written to the compressor since the last time the compressor wrote to the output stream.
    /// Effectively, we emit the upper bound -- input was incompressible -- in size for the input, while integrating
    /// what we know the compressor _has_ compressed.
    pub fn estimated_len(&self) -> usize {
        // Get our estimated block compression ratio, which is taken across all observed compressed blocks.
        //
        // We adjust that compression ratio upwards by the standard error of the block compression ratios we've
        // observed, simply as a safety net against the having blocks with wildly differing compression ratios.
        let mut estimated_compression_ratio = self.block_compression_ratio_variance.mean();
        if estimated_compression_ratio.is_nan() {
            // Not enough data yet to estimate compression ratio, so we just assume no compression.
            estimated_compression_ratio = 1.0;
        } else {
            estimated_compression_ratio *= 1.0 + self.block_compression_ratio_variance.error();
        }

        let estimated_in_flight_compressed_len =
            (self.in_flight_uncompressed_len as f64 * estimated_compression_ratio) as usize;

        self.known_compressed_len as usize + estimated_in_flight_compressed_len
    }

    /// Estimates if writing `len` bytes to the compressor would cause the final compressed size to exceed `threshold`
    /// bytes.
    pub fn would_write_exceed_threshold(&self, len: usize, threshold: usize) -> bool {
        // If we have yet to see any compressed data, we can't make a meaningful estimate, and this likely means that
        // the compressor is still actively able to compress more data into the first block, which when eventually
        // written, should never exceed the compressed size limit... so we choose to not block writes in this case.
        if self.known_compressed_len == 0 {
            return false;
        }

        // We adjust the given threshold down by a small amount to account for the fact that the final block written by
        // the compressor has more variability in size than the rest, due to being more likely to be flushed before
        // internal buffers are full and having the chance to most efficiently compress the data. Essentially, if we
        // estimate that writing `len` more bytes would put our compressed length into the "red zone", then it's too
        // risky to write those bytes.
        //
        // This is a bit of a fudge factor, but we arrived at 1% through empirical testing with the regression
        // detector benchmarks. Small enough to not have a major impact on payload size efficiency, but large enough to
        // entirely get rid of compressed payload size limit violations.
        const THRESHOLD_RED_ZONE: f64 = 0.99;

        let adjusted_threshold = (threshold as f64 * THRESHOLD_RED_ZONE) as usize;
        self.estimated_len() + len > adjusted_threshold
    }
}
