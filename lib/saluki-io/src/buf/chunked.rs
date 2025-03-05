use std::{
    collections::VecDeque,
    convert::Infallible,
    future::Ready,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::{Buf, BufMut as _};
use http_body::{Body, Frame, SizeHint};
use saluki_core::pooling::ObjectPool;
use tokio::io::AsyncWrite;
use tokio_util::sync::ReusableBoxFuture;

use super::{vec::FrozenBytesBuffer, BytesBuffer, ReadIoBuffer};

enum PollObjectPool<O>
where
    O: ObjectPool<Item = BytesBuffer>,
{
    Inconsistent,
    CapacityAvailable(Arc<O>, ReusableBoxFuture<'static, (Arc<O>, BytesBuffer)>),
    WaitingForBuffer(ReusableBoxFuture<'static, (Arc<O>, BytesBuffer)>),
}

impl<O> PollObjectPool<O>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    pub fn new(buffer_pool: Arc<O>) -> Self {
        Self::CapacityAvailable(buffer_pool, ReusableBoxFuture::new(acquire_buffer_from_pool(None)))
    }

    pub fn poll_acquire(&mut self, cx: &mut Context<'_>) -> Poll<BytesBuffer> {
        loop {
            let state = std::mem::replace(self, Self::Inconsistent);
            *self = match state {
                Self::Inconsistent => unreachable!("invalid state transition"),
                Self::CapacityAvailable(buffer_pool, mut fut) => {
                    fut.set(acquire_buffer_from_pool(Some(buffer_pool)));
                    Self::WaitingForBuffer(fut)
                }
                Self::WaitingForBuffer(mut fut) => {
                    let (buffer_pool, buffer) = match fut.poll(cx) {
                        Poll::Ready(result) => result,
                        Poll::Pending => {
                            *self = Self::WaitingForBuffer(fut);
                            return Poll::Pending;
                        }
                    };
                    *self = Self::CapacityAvailable(buffer_pool, fut);
                    return Poll::Ready(buffer);
                }
            }
        }
    }
}

/// A bytes buffer that write dynamically-sized payloads across multiple fixed-size chunks.
///
/// `ChunkedBytesBuffer` works in concert with [`ChunkedBytesBufferObjectPool`], which is backed by any generic buffer
/// pool that works with [`BytesBuffer`]. As callers write data to `ChunkedBytesBuffer`, it will asynchronously acquire
/// "chunks" (`BytesBuffer`) from the buffer pool as needed, and write the data across these chunks.
///
/// `ChunkedBytesBuffer` implements [`AsyncWrite`] and [`Body`], allowing it to be asynchronously written to and used as
/// the body of an HTTP request without any additional allocations and copying/merging of data into a single buffer.
///
/// ## Missing
///
/// - `Buf` implementation to allow for general reading of the written data
pub struct ChunkedBytesBuffer<O>
where
    O: ObjectPool<Item = BytesBuffer>,
{
    buffer_pool: PollObjectPool<O>,
    chunks: VecDeque<BytesBuffer>,
    remaining_capacity: usize,
    write_chunk_idx: usize,
}

impl<O> ChunkedBytesBuffer<O>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    /// Creates a new `ChunkedBytesBuffer` attached to the given buffer pool.
    pub fn new(buffer_pool: Arc<O>) -> Self {
        Self {
            buffer_pool: PollObjectPool::new(buffer_pool),
            chunks: VecDeque::new(),
            remaining_capacity: 0,
            write_chunk_idx: 0,
        }
    }

    /// Returns `true` if the buffer has no data.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Returns the number of bytes written to the buffer.
    pub fn len(&self) -> usize {
        self.chunks.iter().map(|chunk| chunk.remaining()).sum()
    }

    fn register_chunk(&mut self, chunk: BytesBuffer) {
        self.remaining_capacity += chunk.remaining_mut();
        self.chunks.push_back(chunk);
    }

    /// Consumes this buffer and returns a read-only version of it.
    ///
    /// All existing chunks at the time of calling this method will be present in the read-only buffer.
    pub fn freeze(self) -> FrozenChunkedBytesBuffer {
        FrozenChunkedBytesBuffer {
            chunks: self.chunks.into_iter().map(|chunk| chunk.freeze()).collect(),
        }
    }
}

impl<O> AsyncWrite for ChunkedBytesBuffer<O>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        let mut written = 0;

        while written < buf.len() {
            // If we've used up all chunks or the current chunk is full, try to get a new one
            if self.write_chunk_idx >= self.chunks.len() {
                match self.buffer_pool.poll_acquire(cx) {
                    Poll::Ready(chunk) => {
                        self.register_chunk(chunk);
                    }
                    Poll::Pending => {
                        // If we've written nothing yet, return Pending
                        if written == 0 {
                            return Poll::Pending;
                        }
                        // Otherwise return what we've written so far
                        break;
                    }
                }
            }

            // Grab the current write chunk and figure out how much available capacity it has
            let write_chunk_idx = self.write_chunk_idx;
            let chunk = &mut self.chunks[write_chunk_idx];
            let chunk_available_len = chunk.remaining_mut();
            if chunk_available_len == 0 {
                // No available capacity. Roll over to the next chunk.
                self.write_chunk_idx += 1;
                continue;
            }

            // Figure out how much we have left to write from the original buffer, and how much of that we can fit into
            // the available capacity of the current chunk.
            let remaining_buf_len = buf.len() - written;
            let write_len = std::cmp::min(chunk_available_len, remaining_buf_len);

            // Now write whatever remaining portion of the input buffer we can fit into the chunk.
            let remaining_buf = &buf[written..written + write_len];
            chunk.put(remaining_buf);

            written += write_len;
        }

        if written > 0 {
            self.remaining_capacity -= written;
        }

        Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

/// A frozen, read-only version of [`ChunkedBytesBuffer`].
///
/// `FrozenChunkedBytesBuffer` can be cheaply cloned, and allows for sharing the underlying chunks among multiple tasks.
#[derive(Clone)]
pub struct FrozenChunkedBytesBuffer {
    chunks: VecDeque<FrozenBytesBuffer>,
}

impl FrozenChunkedBytesBuffer {
    /// Returns `true` if the buffer has no data.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Returns the number of bytes written to the buffer.
    pub fn len(&self) -> usize {
        self.chunks.iter().map(|chunk| chunk.len()).sum()
    }
}

impl ReadIoBuffer for FrozenChunkedBytesBuffer {
    fn capacity(&self) -> usize {
        self.chunks.iter().map(|chunk| chunk.capacity()).sum()
    }
}

impl Buf for FrozenChunkedBytesBuffer {
    fn remaining(&self) -> usize {
        self.chunks.iter().map(|chunk| chunk.remaining()).sum()
    }

    fn chunk(&self) -> &[u8] {
        self.chunks.front().map_or(&[], |chunk| chunk.chunk())
    }

    fn advance(&mut self, mut cnt: usize) {
        while cnt > 0 {
            let chunk = self.chunks.front_mut().expect("no chunks left");
            let chunk_remaining = chunk.remaining();
            if cnt < chunk_remaining {
                chunk.advance(cnt);
                break;
            }

            chunk.advance(chunk_remaining);
            cnt -= chunk_remaining;
            self.chunks.pop_front();
        }
    }
}

impl Body for FrozenChunkedBytesBuffer {
    type Data = FrozenBytesBuffer;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>, _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(self.chunks.pop_front().map(|chunk| Ok(Frame::data(chunk))))
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.len() as u64)
    }
}

async fn acquire_buffer_from_pool<O>(buffer_pool: Option<Arc<O>>) -> (Arc<O>, BytesBuffer)
where
    O: ObjectPool<Item = BytesBuffer>,
{
    match buffer_pool {
        Some(buffer_pool) => {
            let buffer = buffer_pool.acquire().await;
            (buffer_pool, buffer)
        }
        None => unreachable!(),
    }
}

/// An object pool for `ChunkedBytesBuffer`.
#[derive(Clone)]
pub struct ChunkedBytesBufferObjectPool<O> {
    buffer_pool: Arc<O>,
}

impl<O> ChunkedBytesBufferObjectPool<O> {
    /// Creates a new `ChunkedBytesBufferObjectPool` with the given buffer pool.
    pub fn new(buffer_pool: O) -> Self {
        let buffer_pool = Arc::new(buffer_pool);
        Self { buffer_pool }
    }
}

impl<O> ObjectPool for ChunkedBytesBufferObjectPool<O>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    type Item = ChunkedBytesBuffer<O>;

    type AcquireFuture = Ready<Self::Item>;

    fn acquire(&self) -> Self::AcquireFuture {
        let buffer_pool = Arc::clone(&self.buffer_pool);
        std::future::ready(ChunkedBytesBuffer::new(buffer_pool))
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use http_body_util::BodyExt as _;
    use saluki_core::pooling::FixedSizeObjectPool;
    use tokio::io::AsyncWriteExt as _;
    use tokio_test::{assert_pending, assert_ready, task::spawn as test_spawn};

    use super::*;
    use crate::buf::{BytesBuffer, FixedSizeVec};

    const TEST_CHUNK_SIZE: usize = 16;
    const TEST_BUF_CHUNK_SIZED: &[u8] = b"hello world!!!!!";
    const TEST_BUF_LESS_THAN_CHUNK_SIZED: &[u8] = b"hello world!";
    const TEST_BUF_GREATER_THAN_CHUNK_SIZED: &[u8] = b"hello world, here i come!";

    fn create_buffer_pool(chunks: usize, chunk_size: usize) -> (Arc<FixedSizeObjectPool<BytesBuffer>>, usize) {
        let buffer_pool = Arc::new(FixedSizeObjectPool::<BytesBuffer>::with_builder(
            "chunked_test",
            chunks,
            || FixedSizeVec::with_capacity(chunk_size),
        ));

        (buffer_pool, chunks * chunk_size)
    }

    #[test]
    fn single_write_fits_within_single_chunk() {
        let (buffer_pool, total_capacity) = create_buffer_pool(1, TEST_CHUNK_SIZE);
        let mut chunked_buffer = ChunkedBytesBuffer::new(buffer_pool);

        // Fits within a single buffer, so it should complete without blocking.i
        let mut fut = test_spawn(chunked_buffer.write(TEST_BUF_LESS_THAN_CHUNK_SIZED));
        let result = assert_ready!(fut.poll());

        let n = result.unwrap();
        assert_eq!(n, TEST_BUF_LESS_THAN_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 1);
        assert_eq!(chunked_buffer.remaining_capacity, total_capacity - n);
    }

    #[test]
    fn single_write_fits_single_chunk_exactly() {
        let (buffer_pool, total_capacity) = create_buffer_pool(1, TEST_CHUNK_SIZE);
        let mut chunked_buffer = ChunkedBytesBuffer::new(buffer_pool);

        // Fits within a single buffer, so it should complete without blocking.
        let mut fut = test_spawn(chunked_buffer.write(TEST_BUF_CHUNK_SIZED));
        let result = assert_ready!(fut.poll());

        let n = result.unwrap();
        assert_eq!(n, TEST_BUF_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 1);
        assert_eq!(chunked_buffer.remaining_capacity, total_capacity - n);
    }

    #[test]
    fn single_write_strides_two_chunks() {
        let (buffer_pool, total_capacity) = create_buffer_pool(2, TEST_CHUNK_SIZE);
        let mut chunked_buffer = ChunkedBytesBuffer::new(buffer_pool);

        // This won't fit in a single chunk, but should fit within two.
        let mut fut = test_spawn(chunked_buffer.write(TEST_BUF_GREATER_THAN_CHUNK_SIZED));
        let result = assert_ready!(fut.poll());

        let n = result.unwrap();
        assert_eq!(n, TEST_BUF_GREATER_THAN_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 2);
        assert_eq!(chunked_buffer.remaining_capacity, total_capacity - n);
    }

    #[test]
    fn two_writes_fit_two_chunks_exactly() {
        let (buffer_pool, _) = create_buffer_pool(2, TEST_CHUNK_SIZE);
        let mut chunked_buffer = ChunkedBytesBuffer::new(buffer_pool);

        // First write acquires one chunk, and fills it up entirely.
        let mut fut = test_spawn(chunked_buffer.write(TEST_BUF_CHUNK_SIZED));
        let result = assert_ready!(fut.poll());

        let first_n = result.unwrap();
        assert_eq!(first_n, TEST_BUF_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 1);
        assert_eq!(chunked_buffer.remaining_capacity, 0);

        // Second write acquires an additional chunk, and also fills it up entirely.
        let mut fut = test_spawn(chunked_buffer.write(TEST_BUF_CHUNK_SIZED));
        let result = assert_ready!(fut.poll());

        let second_n = result.unwrap();
        assert_eq!(second_n, TEST_BUF_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 2);
        assert_eq!(chunked_buffer.remaining_capacity, 0);
    }

    #[test]
    fn write_without_available_chunk() {
        // Create the buffer pool and immediately consume both buffers.
        let (buffer_pool, _) = create_buffer_pool(2, TEST_CHUNK_SIZE);

        let mut buf_fut = test_spawn(buffer_pool.acquire());
        let first_buf = assert_ready!(buf_fut.poll());

        let mut buf_fut = test_spawn(buffer_pool.acquire());
        let second_buf = assert_ready!(buf_fut.poll());

        let buffer_pool = Arc::clone(&buffer_pool);
        let mut chunked_buffer = ChunkedBytesBuffer::new(buffer_pool);
        assert_eq!(chunked_buffer.chunks.len(), 0);
        assert_eq!(chunked_buffer.remaining_capacity, 0);

        // First write should do nothing since there's no existing capacity and we can't
        // yet acquire a buffer chunk:
        let mut fut = test_spawn(chunked_buffer.write(TEST_BUF_CHUNK_SIZED));
        assert_pending!(fut.poll());
        assert!(!fut.is_woken());

        // Return one of the buffers to the pool, which should signal the blocked write:
        drop(first_buf);
        assert!(fut.is_woken());

        // Now try our first write again:
        let result = assert_ready!(fut.poll());
        let first_n = result.unwrap();
        assert_eq!(first_n, TEST_BUF_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 1);
        assert_eq!(chunked_buffer.remaining_capacity, 0);

        // Now try a second write, which should also block since we haven't returned the
        // second buffer back to the pool yet:
        let mut fut = test_spawn(chunked_buffer.write(TEST_BUF_CHUNK_SIZED));
        assert_pending!(fut.poll());
        assert!(!fut.is_woken());

        // Return the second buffer to the pool, which should signal the blocked write:
        drop(second_buf);
        assert!(fut.is_woken());

        // Now try our second write again:
        let result = assert_ready!(fut.poll());
        let second_n = result.unwrap();
        assert_eq!(second_n, TEST_BUF_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 2);
        assert_eq!(chunked_buffer.remaining_capacity, 0);
    }

    #[tokio::test]
    async fn all_chunks_returned_as_body() {
        let test_bufs = &[
            TEST_BUF_LESS_THAN_CHUNK_SIZED,
            TEST_BUF_CHUNK_SIZED,
            TEST_BUF_GREATER_THAN_CHUNK_SIZED,
        ];
        let test_bufs_total_len = test_bufs.iter().map(|buf| buf.len()).sum::<usize>();
        let required_chunks = test_bufs_total_len / TEST_CHUNK_SIZE
            + if test_bufs_total_len % TEST_CHUNK_SIZE > 0 {
                1
            } else {
                0
            };

        let (buffer_pool, total_capacity) = create_buffer_pool(required_chunks, TEST_CHUNK_SIZE);
        let mut chunked_buffer = ChunkedBytesBuffer::new(buffer_pool);

        // Do three writes, using the less than/exactly/greater than-sized test buffers.
        //
        // We'll write these buffers, concatenated, to a single buffer that we'll use at the end to
        // compare the collected `Body`-based output.
        let mut expected_aggregated_body = BytesMut::new();
        let test_bufs = &[
            TEST_BUF_LESS_THAN_CHUNK_SIZED,
            TEST_BUF_CHUNK_SIZED,
            TEST_BUF_GREATER_THAN_CHUNK_SIZED,
        ];
        let mut total_written = 0;
        for test_buf in test_bufs {
            chunked_buffer.write_all(test_buf).await.unwrap();
            expected_aggregated_body.put(*test_buf);
            total_written += test_buf.len();
        }

        assert_eq!(test_bufs_total_len, total_written);
        assert_eq!(chunked_buffer.chunks.len(), required_chunks);
        assert_eq!(chunked_buffer.remaining_capacity, total_capacity - total_written);

        let read_chunked_buffer = chunked_buffer.freeze();

        // We should now be able to collect the chunked buffer as a `Body`, into a single output buffer.
        let actual_aggregated_body = read_chunked_buffer.collect().await.expect("cannot fail").to_bytes();

        assert_eq!(expected_aggregated_body.freeze(), actual_aggregated_body);
    }
}
