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
use tracing::info;

// "Red zone" threshold factor.
//
// See `CompressionEstimator::would_write_exceed_threshold` for details.
const THRESHOLD_RED_ZONE: f64 = 0.99;

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

/// Statistics for a writer.
pub trait WriteStatistics {
    /// Returns the total number of bytes written.
    fn total_written(&self) -> u64;
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

impl<W: AsyncWrite> WriteStatistics for Compressor<W> {
    fn total_written(&self) -> u64 {
        match self {
            Compressor::Noop(encoder) => encoder.total_written(),
            Compressor::Zlib(encoder) => encoder.total_out(),
            Compressor::Zstd(encoder) => encoder.get_ref().total_written(),
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
    in_flight_uncompressed_len: usize,
    total_uncompressed_len: usize,
    total_compressed_len: u64,
    current_compression_ratio: f64,
}

impl CompressionEstimator {
    /// Tracks a write to the compressor.
    pub fn track_write<W>(&mut self, compressor: &W, uncompressed_len: usize)
    where
        W: WriteStatistics,
    {
        self.in_flight_uncompressed_len += uncompressed_len;
        self.total_uncompressed_len += uncompressed_len;

        let compressed_len = compressor.total_written();
        let compressed_len_delta = (compressed_len - self.total_compressed_len) as usize;
        if compressed_len_delta > 0 {
            // We just observed the compressor flushing data, so we need to recalculate our compression ratio.
            self.current_compression_ratio = compressed_len as f64 / self.total_uncompressed_len as f64;
            self.total_compressed_len = compressed_len;
            self.in_flight_uncompressed_len = 0;

            info!(
                block_size = compressed_len_delta,
                uncompressed_len = self.total_uncompressed_len,
                compressed_len = self.total_compressed_len,
                compression_ratio = self.current_compression_ratio,
                "Compressor wrote block to output stream."
            );
        }
    }

    /// Resets the estimator.
    pub fn reset(&mut self) {
        self.in_flight_uncompressed_len = 0;
        self.total_uncompressed_len = 0;
        self.total_compressed_len = 0;
        self.current_compression_ratio = 0.0;
    }

    /// Returns the estimated length of the compressor.
    ///
    /// This figure is the sum of the total bytes written by the compressor to the output stream and the number of
    /// uncompressed bytes written to the compressor since the last time the compressor wrote to the output stream
    /// when factoring in the estimated compression ratio over the overall output stream.
    pub fn estimated_len(&self) -> usize {
        let estimated_in_flight_compressed_len =
            (self.in_flight_uncompressed_len as f64 * self.current_compression_ratio) as usize;

        self.total_compressed_len as usize + estimated_in_flight_compressed_len
    }

    /// Estimates if writing `len` bytes to the compressor would cause the final compressed size to exceed `threshold`
    /// bytes.
    pub fn would_write_exceed_threshold(&self, len: usize, threshold: usize) -> bool {
        // If the length of the data to be written exceeds the threshold, then it obviously would exceed the threshold.
        if len > threshold {
            return true;
        }

        // If we have yet to see any compressed data, we can't make a meaningful estimate, and this likely means that
        // the compressor is still actively able to compress more data into the first block, which when eventually
        // written, should never exceed the compressed size limit... so we choose to not block writes in this case.
        if self.total_compressed_len == 0 {
            return false;
        }

        // We adjust the given threshold down by a small amount to account for the fact that the final block written by
        // the compressor has more variability in size than the rest, due to being more likely to be flushed before
        // internal buffers are full and having the chance to most efficiently compress the data. Essentially, if we
        // estimate that writing `len` more bytes would put our compressed length into the "red zone", then it's too
        // risky to write those bytes.
        //
        // This is a bit of a fudge factor, but we arrived at the value through empirical testing with the regression
        // detector benchmarks. Small enough to not have a major impact on payload size efficiency, but large enough to
        // entirely get rid of compressed payload size limit violations.
        let adjusted_threshold = (threshold as f64 * THRESHOLD_RED_ZONE) as usize;
        self.estimated_len() + len > adjusted_threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockCompressor {
        current_uncompressed_len: u64,
        total_uncompressed_len: usize,
        compressed_len: u64,
    }

    impl MockCompressor {
        fn new() -> Self {
            MockCompressor {
                current_uncompressed_len: 0,
                total_uncompressed_len: 0,
                compressed_len: 0,
            }
        }

        fn write(&mut self, n: usize) {
            self.current_uncompressed_len += n as u64;
            self.total_uncompressed_len += n;
        }

        fn flush(&mut self, compression_ratio: f64) {
            self.compressed_len += (self.current_uncompressed_len as f64 * compression_ratio) as u64;
            self.current_uncompressed_len = 0;
        }

        fn total_uncompressed_len(&self) -> usize {
            self.total_uncompressed_len
        }
    }

    impl WriteStatistics for MockCompressor {
        fn total_written(&self) -> u64 {
            self.compressed_len
        }
    }

    #[test]
    fn compression_estimator_no_output() {
        let estimator = CompressionEstimator::default();

        assert!(!estimator.would_write_exceed_threshold(10, 100));
        assert!(estimator.would_write_exceed_threshold(100, 90));
    }

    #[test]
    fn compression_estimator_single_flush() {
        const MAX_COMPRESSED_LEN: usize = 100;
        const COMPRESSION_RATIO: f64 = 0.7;
        const WRITE_LEN: usize = 50;

        let mut estimator = CompressionEstimator::default();

        // Create our mock compressor and do a basic write, and then flush, so that our estimator can get some data.
        let mut compressor = MockCompressor::new();
        assert!(!estimator.would_write_exceed_threshold(WRITE_LEN, MAX_COMPRESSED_LEN));

        // Write 50 bytes with a compression ratio of 0.7, giving us 35 bytes compressed.
        compressor.write(WRITE_LEN);
        compressor.flush(COMPRESSION_RATIO);
        assert_eq!(compressor.total_written(), 35);

        estimator.track_write(&compressor, WRITE_LEN);

        // We should be able to write 65 more bytes compressed, so 100 bytes uncompressed, given the compression ratio we have (0.7),
        // would give us 70 bytes estimated.. which is over the threshold.
        assert!(estimator.would_write_exceed_threshold(100, MAX_COMPRESSED_LEN));

        // However, another 50 byte write would theoretically just be another 35 bytes compressed, so 85 bytes compressed total,
        // which is under our threshold and should be allowed.
        assert!(!estimator.would_write_exceed_threshold(WRITE_LEN, MAX_COMPRESSED_LEN));
    }

    #[test]
    fn compression_estimator_multiple_flush_partial() {
        const MAX_COMPRESSED_LEN: usize = 5000;
        const FIRST_COMPRESSION_RATIO: f64 = 0.7;
        const FIRST_WRITE_LEN: usize = 5000;
        const SECOND_COMPRESSION_RATIO: f64 = 2.1;
        const SECOND_WRITE_LEN: usize = 300;
        const THIRD_WRITE_LEN: usize = 820;

        let mut estimator = CompressionEstimator::default();

        // Create our mock compressor and assert we can do our first write.
        let mut compressor = MockCompressor::new();
        assert!(!estimator.would_write_exceed_threshold(FIRST_WRITE_LEN, MAX_COMPRESSED_LEN));

        // Write 5,000 bytes with a compression ratio of 0.7, giving us 3,500 bytes compressed.
        compressor.write(FIRST_WRITE_LEN);
        compressor.flush(FIRST_COMPRESSION_RATIO);
        assert_eq!(compressor.total_uncompressed_len(), FIRST_WRITE_LEN);
        assert_eq!(compressor.total_written(), 3500);

        estimator.track_write(&compressor, FIRST_WRITE_LEN);

        // We now do second write that simulates a "short" flush on the compressor: this might just be the compressor writing a partial block.
        //
        // What we want to test here is the estimator's ability to focus on the overall compression ratio rather than getting "lost" due to
        // a single block being flushed which, when viewed naively, appears to be vastly bigger than the actual in-flight uncompressed data
        // that it represents.
        //
        // We end up writing 300 bytes uncompressed with a compression ratio of 2.1, giving us 630 bytes compressed. We now have a total of
        // 5,300 bytes uncompressed and 4,130 bytes compressed. Our overall compression ratio is now 0.77.
        compressor.write(SECOND_WRITE_LEN);
        compressor.flush(SECOND_COMPRESSION_RATIO);
        assert_eq!(compressor.total_uncompressed_len(), FIRST_WRITE_LEN + SECOND_WRITE_LEN);
        assert_eq!(compressor.total_written(), 4130);

        estimator.track_write(&compressor, SECOND_WRITE_LEN);

        // At this point, with our compressed limit of 5,000 bytes, we should be able to fit in another 870 bytes compressed. We do have to
        // compensate for the "red zone" threshold, though, which should put us at a threshold of 4,950 bytes so 820 bytes compressed.
        //
        // We use the compressed length when calling `would_write_exceed_threshold` because it uses the uncompressed length as the worst-case
        // scenario, which is that the write would not be compressed at all.
        assert!(!estimator.would_write_exceed_threshold(THIRD_WRITE_LEN, MAX_COMPRESSED_LEN));
    }
}
