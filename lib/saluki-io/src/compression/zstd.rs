use std::io;

use saluki_core::pooling::ObjectPool;
use zstd_safe::{
    zstd_sys::ZSTD_EndDirective, CCtx, CParameter, ErrorCode, InBuffer, OutBuffer, ResetDirective, WriteBuf,
    CLEVEL_DEFAULT,
};

use crate::buf::BytesBuffer;

// Zstandard frame overhead is 4 bytes for the magic number, 14 bytes (maximum; minimum of 2 bytes) for the frame
// header, and 4 bytes (maximum; minimum of 0 bytes) for the frame, and 3 bytes for a block header... and we always need
// at one block, so we throw that in here too.
//
// Really, we would want to calculate the data block header overhead separately but there's no way to actually get the
// running block count, so this is ultimately a guess.
const ZSTD_FRAME_OVERHEAD: usize = 4 + 14 + 4 + 3;

static DEFAULT_CPARAMS: &[(&str, CParameter)] = &[
    // Use the "default" compression level.
    ("set compression level", CParameter::CompressionLevel(CLEVEL_DEFAULT)),
    // Enables checksumming, which adds a checksum at the end of each frame.
    ("enable checksums", CParameter::ChecksumFlag(true)),
];

/// A Zstandard compressor that can be used to generate complete compressed payloads that fit within a configured limit.
///
/// # Overview
///
/// In many systems, limits are placed on the size of payloads that they will accept. Normally, this is done to prevent
/// resource exhaustion issues -- whether unintentional or malicious -- where payloads are not bounded and can lead to
/// the system crashing during processing. For uncompressed payloads, this can usually be handled ahead of time, by
/// checking the payload size is or isn't within the limits, or by altering the payload to fit within the limits.
/// However, when dealing with compressed payloads, this is not as easy, since the size of the compressed payload is
/// determined by how efficiently it can be compressed, which generally cannot be known ahead of time.
///
/// This compressor is designed to inform the caller when it believes a write will cause the size of compressed output
/// to exceed the configured limit, and instead "finalize" the current compressed output and start a new one. It
/// achieves this by using information exposed from the internal `zstd` compression state, along with conservative
/// estimates of the resulting compressed output size based on in-flight uncompressed bytes and the compressed bytes
/// that have been flushed so far.
///
/// # Design
///
/// The Zstandard API exposes "frame progression" data, which tracks three main things: how much input data has been
/// written into the compression context overall (ingested), how much of the input data has been compressed (consumed),
/// and how much compressed data has been flushed out to the underlying output buffer (produced).
///
/// When trying to track what our compressed output size will be, we have two scenarios:
///
/// - when the compressor has yet to produce anything yet, the worst case output size is the number of ingested bytes,
///   as Zstandard would simply write incompressible bytes as-is to the output buffer
/// - when the compressor has produced some output, the worst case output size is the number of produced bytes plus the
///   difference between the number of ingested bytes and the number of consumed bytes, since it follows from the first
///   scenario that if the compressor has outstanding input data to compress, the worst case size of _that_ data is the
///   original size
///
/// All of this said, our goal is ensure we don't accept a write that puts us over our configured maximum compressed
/// size. To do so, we take the calculate "worst case" output size (based on the heuristic described above) and check if
/// adding the length of the item (again, the full length, which covers the worst case compressibility) would cause the
/// compressed output to exceed our configured maximum size. If it would exceed the configured maximum size, we attempt
/// to flush the current internal buffers, and re-evaluate the worst case output size. If we still can't fit the item in
/// the worst case after flushing, we inform the caller that they need to finalize the current compressed payload --
/// cleaning ending the compression stream and flushing remaining data -- before resetting the compressor to allow for
/// writes to continue.
///
/// Taking this approach, we can ensure that the compressor will never exceed the configured maximum size, but we also
/// are able to hedge our otherwise conservative estimates -- most data is at least somewhat compressible -- and allow
/// for the possibility of continuing to write data without bailing out at the first sign of _maybe_ exceeding the
/// configured maximum size.
///
/// # Performance
///
/// As the heuristics used for estimating the potential for exceeding the configured maximum size are inherently
/// conservative, the configured maximum size should _generally_ be set to a value large enough to allow for at least
/// one full "data block" to be written, which is 128KiB. This provides additional room for the compressor to work with,
/// allowing it to hopefully produce intermediate compressed data before reaching the configured maximum size, which in
/// turn gives a chance for the heuristics to provide better estimations.
///
/// That said, the compressor is still designed to properly handle any configured maximum size and will work as expected
/// whether the limit is 10KiB or 10GiB.
///
/// # Missing
///
/// - support for configuring the compression level
pub struct ZstdLimitedCompressor<P: ObjectPool> {
    cctx: SimpleCCtx,
    max_compressed_size: usize,
    buffer_pool: P,
    completed_buffers: Vec<BytesBuffer>,
    output_buffer: Option<BytesBuffer>,
}

impl<P> ZstdLimitedCompressor<P>
where
    P: ObjectPool<Item = BytesBuffer>,
{
    /// Creates a new `ZstdLimitedCompressor` with the given buffer pool and the maximum compressed size.
    ///
    /// The maximum compressed size will be obeyed when `write` is called, and the caller will be informed when a write
    /// cannot be determined to fit without the compressed output size exceeding the configured limit.
    ///
    /// # Errors
    ///
    /// If an error is encountered while creating the compressor, an error is returned.
    pub fn new(buffer_pool: P, max_compressed_size: usize) -> io::Result<Self> {
        // Create our raw compression context, and then wrap it up.
        let cctx = create_simple_cctx()?;

        Ok(Self {
            cctx,
            max_compressed_size,
            buffer_pool,
            completed_buffers: Vec::new(),
            output_buffer: None,
        })
    }

    /// Returns the current estimated compressed size of all data written into the compressor so far, in bytes.
    ///
    /// This method makes a fairly conservative estimate of the compressed size, which is primarily meant to be used as
    /// a heuristic for deciding how to proceed with further writes. See the [`Design`][ZstdLimitedCompressor#Design]
    /// section in the type-level documentation for more details.
    fn estimated_compressed_size(&self) -> usize {
        let frame_progress = self.cctx.get_frame_progression();
        let data_size = if frame_progress.produced == 0 {
            frame_progress.ingested as usize
        } else {
            frame_progress.produced as usize + (frame_progress.ingested as usize - frame_progress.consumed as usize)
        };

        data_size + ZSTD_FRAME_OVERHEAD
    }

    async fn ensure_output_buffer_ready(&mut self) {
        // Ensure we have available capacity in the current output buffer, or that we have an output buffer at all, and
        // acquire a new output buffer if not.
        loop {
            // When acquiring a new output buffer, or replacing a full one, we take advantage of the looping to handle
            // eventually getting to the point of returning `OutBuffer`.
            let is_full_or_empty = self.output_buffer.as_ref().is_none_or(|b| b.is_full());
            if !is_full_or_empty {
                break;
            }

            let new_output_buffer = self.buffer_pool.acquire().await;
            let old_output_buffer = self.output_buffer.replace(new_output_buffer);
            if let Some(output_buffer) = old_output_buffer {
                self.completed_buffers.push(output_buffer);
            }
        }
    }

    /// Writes the input data into the compressor.
    ///
    /// If the input data was able to be entirely written without exceeding the configured maximum size, `Ok(true)` is
    /// returned. Otherwise, `Ok(false)` is returned, indicating that the caller should finalize the compressor before
    /// trying again to write the same data.
    ///
    /// # Errors
    ///
    /// If an error is encountered while writing to the compressor, an error is returned.
    pub async fn write(&mut self, data: &[u8]) -> io::Result<bool> {
        // Make a conservative estimate if we can write this item into the compressor without exceeding the maximum
        // compressed size limit.
        //
        // If our estimate indicates that we would exceed the limit, we'll try to force a flush of the compressor to see
        // if we can free up some space. We'll again calculate the worst case output size and if we still can't fit the
        // item in the worst case, we'll give up and tell the caller that they need to finalize the compressor before
        // proceeding.
        let estimated_compressed_size = self.estimated_compressed_size();
        if estimated_compressed_size + data.len() > self.max_compressed_size {
            loop {
                self.ensure_output_buffer_ready().await;
                let mut output = as_output_buffer(self.output_buffer.as_mut());

                match self.cctx.flush(&mut output)? {
                    WriteStatus::Ok => break,
                    WriteStatus::OutputBufferFull => continue,
                }
            }

            // If we still can't fit the item in the worst case after flushing, the compressor needs to be finalized
            // before proceeding.
            let post_worst_case_output_size = self.estimated_compressed_size();
            if post_worst_case_output_size + data.len() > self.max_compressed_size {
                return Ok(false);
            }
        }

        // We're able to write this item into the compressor without exceeding the maximum size, even if it's
        // incompressible... so let's proceed!
        let mut input = InBuffer::around(data);

        loop {
            self.ensure_output_buffer_ready().await;
            let mut output = as_output_buffer(self.output_buffer.as_mut());

            match self.cctx.write_all(&mut input, &mut output)? {
                WriteStatus::Ok => break,
                WriteStatus::OutputBufferFull => continue,
            }
        }

        Ok(true)
    }

    /// Finalizes the compressor, flushing any remaining data and closing the current compression stream, and returns
    /// the compressed output.
    ///
    /// Returns a vector of all buffers which represent the full compressed output. These buffers must be read
    /// sequentially, in order, to obtain the full compressed payload.
    ///
    /// # Errors
    ///
    /// If an error is encountered while finalizing the compressor, an error is returned.
    pub async fn finalize(&mut self) -> io::Result<Vec<BytesBuffer>> {
        // Finalize the compression context to ensure all data is flushed out and the frame is closed.
        loop {
            self.ensure_output_buffer_ready().await;
            let mut output = as_output_buffer(self.output_buffer.as_mut());

            match self.cctx.finalize(&mut output)? {
                WriteStatus::Ok => break,
                WriteStatus::OutputBufferFull => continue,
            }
        }

        // At this point, the compressor has been finalized and the output buffers are ready to be swapped and returned.
        let old_output_buffer = self.output_buffer.take();
        if let Some(output_buffer) = old_output_buffer {
            self.completed_buffers.push(output_buffer);
        }
        let output_buffers = std::mem::take(&mut self.completed_buffers);

        Ok(output_buffers)
    }
}

enum WriteStatus {
    /// The write completed successfully.
    ///
    /// Any necessary flushing was handled automatically and written to the output buffer.
    Ok,

    /// The given output buffer is full.
    ///
    /// The current write operation cannot be completed until a new output buffer with available capacity is provided.
    OutputBufferFull,
}

/// An ergonomic wrapper around `safe_zstd::CCtx`.
struct SimpleCCtx {
    cctx: CCtx<'static>,
}

impl SimpleCCtx {
    /// Returns the frame progression status of the compression context.
    fn get_frame_progression(&self) -> zstd_safe::FrameProgression {
        self.cctx.get_frame_progression()
    }

    /// Writes the input buffer into the compression context, retrying until all bytes have been written or the output
    /// buffer is full.
    ///
    /// When the input buffer has been fully written, `Ok(WriteStatus::Ok)` is returned. If the output buffer is full,
    /// `Ok(WriteStatus::OutputBufferFull)` is returned, which indicates that the caller should retry the call with the
    /// same input buffer but a new output buffer that has available capacity.
    ///
    /// # Errors
    ///
    /// If an error is encountered while writing to the compression context, an error is returned.
    fn write_all<B: WriteBuf>(
        &mut self, input: &mut InBuffer<'_>, output: &mut OutBuffer<'_, B>,
    ) -> io::Result<WriteStatus> {
        loop {
            match self
                .cctx
                .compress_stream2(output, input, ZSTD_EndDirective::ZSTD_e_continue)
            {
                // All bytes were written into the compressor, and any necessary flushing was handled.
                Ok(0) => return Ok(WriteStatus::Ok),
                // We have remaining bytes to write out, so see if our output buffer is full or if we can continue.
                Ok(_) => {
                    if is_output_buffer_full(output) {
                        return Ok(WriteStatus::OutputBufferFull);
                    }
                }
                Err(e) => return Err(to_io_error(e, "compress_stream2")),
            }
        }
    }

    /// Flushes any internal buffers in the compression context to the output buffer, writing a clean data block that is
    /// guaranteed to contain all the data that has been written so far.
    ///
    /// When the internal buffers have been fully flushed, `Ok(WriteStatus::Ok)` is returned. If the output buffer is
    /// full, `Ok(WriteStatus::OutputBufferFull)` is returned, which indicates that the caller should retry the call
    /// with a new output buffer that has available capacity.
    ///
    /// # Errors
    ///
    /// If an error is encountered while flushing the compression context, an error is returned.
    fn flush<B: WriteBuf>(&mut self, output: &mut OutBuffer<'_, B>) -> io::Result<WriteStatus> {
        // We use a dummy input buffer to satisfy the API, but we're just trying to drive the compressor to flush its
        // internal buffers to the output buffer.
        let mut input = InBuffer::around(&[]);

        loop {
            match self
                .cctx
                .compress_stream2(output, &mut input, ZSTD_EndDirective::ZSTD_e_flush)
            {
                // All bytes were written into the compressor, and any necessary flushing was handled.
                Ok(0) => return Ok(WriteStatus::Ok),
                // We have remaining bytes to write out, so see if our output buffer is full or if we can continue.
                Ok(_) => {
                    if is_output_buffer_full(output) {
                        return Ok(WriteStatus::OutputBufferFull);
                    }
                }
                Err(e) => return Err(to_io_error(e, "compress_stream2")),
            }
        }
    }

    /// Completes the current frame, flushing any internal buffers prior to writing the frame epilogue, and resets the
    /// compression context.
    ///
    /// This method is used to finalize the compression context, at which point the given output buffer contains any
    /// remaining output for the current compression session. This must be called to end the stream prior to consuming
    /// the buffer and sending it off for decompression.
    ///
    /// When the internal buffers have been fully flushed, and the compression context has been reset,
    /// `Ok(WriteStatus::Ok)` is returned. If the output buffer is full, `Ok(WriteStatus::OutputBufferFull)` is
    /// returned, which indicates that the caller should retry the call with a new output buffer that has available
    /// capacity.
    ///
    /// # Errors
    ///
    /// If an error is encountered while flushing or resetting the compression context, an error is returned.
    fn finalize<B: WriteBuf>(&mut self, output: &mut OutBuffer<'_, B>) -> io::Result<WriteStatus> {
        // Finalize the compression context to ensure all data is flushed out and the frame is closed.
        loop {
            match self.cctx.end_stream(output) {
                // All internal buffers were flushed and the frame was closed.
                Ok(0) => break,
                // We have remaining bytes to write out, so see if our output buffer is full or if we can continue.
                Ok(_) => {
                    if is_output_buffer_full(output) {
                        return Ok(WriteStatus::OutputBufferFull);
                    }
                }
                Err(e) => return Err(to_io_error(e, "compress_stream2")),
            }
        }

        // Reset the compression context.
        //
        // This involves both resetting the "session" state of the compression context as well as doing a dummy compress
        // operation, which ensures that the frame progression state is also cleared, as the `reset` call itself does
        // not accomplish that.
        self.cctx
            .reset(ResetDirective::SessionOnly)
            .map_err(|e| to_io_error(e, "reset"))?;

        let mut input = InBuffer::around(&[]);
        match self.write_all(&mut input, output) {
            Ok(WriteStatus::Ok) => Ok(WriteStatus::Ok),
            Ok(WriteStatus::OutputBufferFull) => {
                Err(custom_io_error("output buffer unexpectedly full after context reset"))
            }
            Err(e) => Err(e),
        }
    }
}

fn create_simple_cctx() -> io::Result<SimpleCCtx> {
    let mut cctx = CCtx::create();
    for (param_desc, param) in DEFAULT_CPARAMS {
        cctx.set_parameter(*param)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("failed to {}: {}", param_desc, e)))
            .unwrap();
    }

    Ok(SimpleCCtx { cctx })
}

fn is_output_buffer_full<B: WriteBuf>(output: &OutBuffer<'_, B>) -> bool {
    output.as_slice().len() == output.capacity()
}

fn as_output_buffer(output_buffer: Option<&mut BytesBuffer>) -> OutBuffer<'_, BytesBuffer> {
    // We have this method purely as a convenience to avoid having to copypaste this code in 4-5 different places.
    //
    // It's used because if we tried to have an all-in-method that ensured we had an output buffer, _and_ returned an
    // `OutBuffer` wrapped over it, borrowck gets confused and thinks we have a mutable borrow on `self` even after
    // leaving a loop iteration, since it doesn't understand that we're simply borrowing a specific _piece_ of `self`,
    // one which doesn't conflict with other mutable borrows like accessing the compression context.
    //
    // Until Polonius is a thing, we'll have to do this extra stuff, and so I've decided to try and deduplicate a bit.
    let output_buffer = output_buffer.expect("output buffer is None");
    OutBuffer::around_pos(output_buffer, output_buffer.len())
}

fn to_io_error(error_code: ErrorCode, operation_name: &'static str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Other,
        format!("{} failed: {}", operation_name, error_code),
    )
}

fn custom_io_error(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, reason)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, ops::RangeInclusive};

    use proptest::{collection::vec_deque as arb_vecdeque, prelude::*};
    use rand::{distributions::Alphanumeric, rngs::StdRng, SeedableRng as _};
    use saluki_core::pooling::FixedSizeObjectPool;

    use super::*;
    use crate::buf::FixedSizeVec;

    fn zstd_decompress(uncompressed_size: usize, compressed: &[u8]) -> Vec<u8> {
        let mut decompressed = vec![0; uncompressed_size];
        zstd_safe::decompress(&mut decompressed, compressed).unwrap();

        decompressed
    }

    fn arb_payload() -> impl Strategy<Value = String> {
        // Very crude approximation of a random binary payload.
        "[a-zA-Z0-9]{32,96}"
    }

    fn get_random_string(rng: &mut StdRng, len_range: RangeInclusive<usize>) -> String {
        let len = rng.gen_range(len_range);
        let mut s = String::with_capacity(len);
        for _ in 0..len {
            s.push(rng.sample(Alphanumeric) as char);
        }
        s
    }

    #[tokio::test]
    async fn basic_roundtrip() {
        let input_data = b"hello world!";

        let buffer_pool = FixedSizeObjectPool::with_builder("test", 1, || FixedSizeVec::with_capacity(1024));

        let mut compressor = ZstdLimitedCompressor::new(buffer_pool, 1024).unwrap();
        compressor.write(input_data).await.unwrap();

        let compressed_buffers = compressor.finalize().await.unwrap();
        assert_eq!(compressed_buffers.len(), 1);

        let compressed = compressed_buffers[0].as_slice();
        assert_ne!(compressed, input_data);

        let decompressed = zstd_decompress(input_data.len(), compressed);
        assert_eq!(&decompressed[..], input_data);
    }

    #[test]
    fn incremental_buffers_round_trip() {
        const BUFFER_CAP: usize = 128;

        // Create a simple compression context and a small initial output buffer.
        //
        // Over the course of writing and flushing and finalizing, we'll swap out full output buffers for new ones, and
        // keep a collection of all output buffers. At the end, we should be able to concatenate all of the output
        // buffers together and have a single valid compressed payload.
        let mut cctx = create_simple_cctx().unwrap();
        let mut rng = StdRng::seed_from_u64(0xDEADBEEFCAFEBABE);

        let mut completed_buffers = Vec::new();
        let mut output_buffer = Vec::with_capacity(BUFFER_CAP);
        let mut uncompressed_data = Vec::new();

        for i in 0..1024 {
            // We'll write data if this iteration is not a multiple of 100, otherwise we'll do a manual flush.
            if i % 100 == 0 {
                loop {
                    let output_buffer_pos = output_buffer.len();
                    let mut output = OutBuffer::around_pos(&mut output_buffer, output_buffer_pos);

                    match cctx.flush(&mut output) {
                        Ok(WriteStatus::Ok) => break,
                        Ok(WriteStatus::OutputBufferFull) => {
                            // Swap out the full output buffer for a new one and continue flushing.
                            let new_output_buffer = Vec::with_capacity(BUFFER_CAP);
                            let old_output_buffer = std::mem::replace(&mut output_buffer, new_output_buffer);
                            completed_buffers.push(old_output_buffer);
                        }
                        Err(e) => panic!("flush failed: {}", e),
                    }
                }
            } else {
                // Generate a random string and write it.
                let data = get_random_string(&mut rng, 32..=64);
                let mut input = InBuffer::around(data.as_bytes());

                loop {
                    let output_buffer_pos = output_buffer.len();
                    let mut output = OutBuffer::around_pos(&mut output_buffer, output_buffer_pos);

                    match cctx.write_all(&mut input, &mut output) {
                        Ok(WriteStatus::Ok) => {
                            // Now that we've fully written the item, track it in our "uncompressed data" buffer.
                            uncompressed_data.extend_from_slice(data.as_bytes());
                            break;
                        }
                        Ok(WriteStatus::OutputBufferFull) => {
                            // Swap out the full output buffer for a new one and continue writing.
                            let new_output_buffer = Vec::with_capacity(BUFFER_CAP);
                            let old_output_buffer = std::mem::replace(&mut output_buffer, new_output_buffer);
                            completed_buffers.push(old_output_buffer);
                        }
                        Err(e) => panic!("write_all failed: {}", e),
                    }
                }
            }
        }

        // Finalize our compression context.
        loop {
            let output_buffer_pos = output_buffer.len();
            let mut output = OutBuffer::around_pos(&mut output_buffer, output_buffer_pos);

            match cctx.finalize(&mut output) {
                Ok(WriteStatus::Ok) => break,
                Ok(WriteStatus::OutputBufferFull) => {
                    // Swap out the full output buffer for a new one and continue flushing.
                    let new_output_buffer = Vec::with_capacity(BUFFER_CAP);
                    let old_output_buffer = std::mem::replace(&mut output_buffer, new_output_buffer);
                    completed_buffers.push(old_output_buffer);
                }
                Err(e) => panic!("finalize failed: {}", e),
            }
        }

        // Add the final output buffer to our collection of completed buffers.
        completed_buffers.push(output_buffer);

        // Concatenate all of the output buffers together into a single compressed payload, and then round trip it
        // through decompression and make sure it matches the original uncompressed data.
        let mut compressed_data = Vec::new();
        for buffer in completed_buffers {
            compressed_data.extend_from_slice(&buffer);
        }

        let decompressed_data = zstd_decompress(uncompressed_data.len(), &compressed_data);
        assert_eq!(decompressed_data, uncompressed_data);
    }

    #[test]
    fn simple_cctx_output_buffer_full_write_all() {
        // Create a simple compression context and an output buffer with a very low capacity.
        //
        // Our goal is to write data into the compression context until the point where the compression context decides
        // to flush the data to the output buffer, and then assert that the call returns `WriteStatus::OutputBufferFull`
        // once it fills it all up.
        let mut cctx = create_simple_cctx().unwrap();

        let mut output_buffer = vec![0; 8];
        let mut output = OutBuffer::around_pos(&mut output_buffer, 0);

        loop {
            // The length of the bytes written to the output buffer should always be less than or equal to the
            // "produced" bytes by the compression context. This is more of a sanity check than anything else.
            let frame_progression = cctx.get_frame_progression();
            assert!(output.as_slice().len() <= frame_progression.produced as usize);

            let mut input = InBuffer::around(b"hello world!");
            match cctx.write_all(&mut input, &mut output) {
                Ok(WriteStatus::Ok) => continue,
                Ok(WriteStatus::OutputBufferFull) => {
                    // We have a full output buffer, so let's check the status.
                    assert_eq!(output.as_slice().len(), output.capacity());
                    break;
                }
                Err(e) => panic!("write_all failed: {}", e),
            }
        }
    }

    #[test]
    fn simple_cctx_output_buffer_full_flush() {
        // Create a simple compression context and an output buffer with a very low capacity.
        //
        // Our goal is to write a bunch of data into the compression context and then manually trigger a flush, and then
        // assert that the call returns `WriteStatus::OutputBufferFull` once it fills it all up.
        let mut cctx = create_simple_cctx().unwrap();

        let mut output_buffer = vec![0; 8];
        let mut output = OutBuffer::around_pos(&mut output_buffer, 0);

        for i in 0..4 {
            // A simple input string with some different data in it. Small enough that it should all be buffered
            // internally and not trigger an implicit flush when writing... but will lead to enough output when we call
            // flush that it will fill up the output buffer.
            let data = format!("{} my {} special {} string", i, i, i);
            let mut input = InBuffer::around(data.as_bytes());

            match cctx.write_all(&mut input, &mut output) {
                Ok(WriteStatus::Ok) => continue,
                Ok(WriteStatus::OutputBufferFull) => {
                    panic!("should not flush to the output buffer during the write phase")
                }
                Err(e) => panic!("write_all failed: {}", e),
            }
        }

        // Now trigger our flush and make sure we fill up the output buffer.
        //
        // Make sure our output buffer is still empty before doing so.
        assert_eq!(output.as_slice().len(), 0);

        match cctx.flush(&mut output) {
            Ok(WriteStatus::Ok) => panic!("should not be able to fully flush to the output buffer"),
            Ok(WriteStatus::OutputBufferFull) => {
                // We have a full output buffer, so let's check the status.
                assert_eq!(output.as_slice().len(), output.capacity());
            }
            Err(e) => panic!("flush failed: {}", e),
        }
    }

    #[test]
    fn simple_cctx_output_buffer_full_finalize() {
        // Create a simple compression context and an output buffer with a very low capacity.
        //
        // Our goal is to write a bunch of data into the compression context and then finalize the compressor, and then
        // assert that the call returns `WriteStatus::OutputBufferFull` once it fills it all up.
        let mut cctx = create_simple_cctx().unwrap();

        let mut output_buffer = vec![0; 8];
        let mut output = OutBuffer::around_pos(&mut output_buffer, 0);

        for i in 0..4 {
            // A simple input string with some different data in it. Small enough that it should all be buffered
            // internally and not trigger an implicit flush when writing... but will lead to enough output when we call
            // flush that it will fill up the output buffer.
            let data = format!("{} my {} special {} string", i, i, i);
            let mut input = InBuffer::around(data.as_bytes());

            match cctx.write_all(&mut input, &mut output) {
                Ok(WriteStatus::Ok) => continue,
                Ok(WriteStatus::OutputBufferFull) => {
                    panic!("should not flush to the output buffer during the write phase")
                }
                Err(e) => panic!("write_all failed: {}", e),
            }
        }

        // Now trigger our finalize and make sure we fill up the output buffer.
        //
        // Make sure our output buffer is still empty before doing so.
        assert_eq!(output.as_slice().len(), 0);

        match cctx.finalize(&mut output) {
            Ok(WriteStatus::Ok) => panic!("should not be able to fully finalize to the output buffer"),
            Ok(WriteStatus::OutputBufferFull) => {
                // We have a full output buffer, so let's check the status.
                assert_eq!(output.as_slice().len(), output.capacity());
            }
            Err(e) => panic!("finalize failed: {}", e),
        }
    }

    #[test_strategy::proptest(async = "tokio")]
    #[cfg_attr(miri, ignore)]
    async fn property_test_obeys_limit(
        #[strategy(1024..16384usize)] max_compressed_size: usize,
        #[strategy(arb_vecdeque(arb_payload(), 16..=512))] mut inputs: VecDeque<String>,
    ) {
        // Calculate the size of our buffer pool based on the maximum size of an item and the maximum number of items
        // we expect to try compressing, such that we should be able to hold 4x the maximum size of the uncompressed
        // data, which should always be sufficient, and allows for scenarios where buffers end up with small,
        // partial fills.
        let buffer_capacity = 1024;
        let buffer_pool_size = ((96 * 512) / buffer_capacity) * 4;
        let buffer_pool = FixedSizeObjectPool::with_builder("test", buffer_pool_size, || {
            FixedSizeVec::with_capacity(buffer_capacity)
        });

        // Create our compressor with a single output buffer that is double the size of the maximum compressed size,
        // which will ensure we never run out of space in the output buffer before hitting the limit.
        let mut compressor = ZstdLimitedCompressor::new(buffer_pool.clone(), max_compressed_size).unwrap();

        let mut uncompressed_data = Vec::new();
        for input in &inputs {
            uncompressed_data.extend_from_slice(input.as_bytes());
        }

        let mut raw_compressed_outputs = Vec::new();

        while let Some(input) = inputs.pop_front() {
            match compressor.write(input.as_bytes()).await {
                Ok(true) => continue,
                Ok(false) => {
                    // We need to finalize the compressor before we can continue writing.
                    let compressed = compressor.finalize().await.unwrap();
                    raw_compressed_outputs.push(compressed);

                    // Push back the input to try again.
                    inputs.push_front(input);
                }
                Err(e) => panic!("write failed: {}", e),
            }
        }

        let compressed = compressor.finalize().await.unwrap();
        raw_compressed_outputs.push(compressed);

        let compressed_outputs = raw_compressed_outputs
            .into_iter()
            .map(|xs| {
                xs.iter().fold(Vec::new(), |mut acc, x| {
                    acc.extend_from_slice(x.as_slice());
                    acc
                })
            })
            .collect::<Vec<_>>();

        // Make sure all of our compressed outputs are within the maximum size.
        for compressed in &compressed_outputs {
            prop_assert!(
                compressed.len() <= max_compressed_size,
                "compressed={} limit={} overage={}",
                compressed.len(),
                max_compressed_size,
                compressed.len() - max_compressed_size
            );
        }

        // Decompress all of our compressed outputs and make sure they match the original inputs.
        let mut decompressed_data = Vec::new();
        for compressed_output in &compressed_outputs {
            let compressed_data_buf = compressed_output.as_slice();
            let decompressed = zstd_decompress(uncompressed_data.len(), compressed_data_buf);
            decompressed_data.extend(decompressed);
        }

        prop_assert_eq!(decompressed_data, uncompressed_data);
    }
}
