use std::{
    collections::VecDeque,
    convert::Infallible,
    io,
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
};

use async_trait::async_trait;
use bytes::{Buf as _, BufMut as _};
use http_body::{Body, Frame};
use tokio::io::AsyncWrite;
use tokio_util::sync::ReusableBoxFuture;

use saluki_core::buffers::BufferPool;

use super::BytesBuffer;

type SharedBufferPool = Arc<dyn BufferPool<Buffer = BytesBuffer> + Send + Sync>;

enum PollBufferPool {
    Inconsistent,
    CapacityAvailable(
        SharedBufferPool,
        ReusableBoxFuture<'static, (SharedBufferPool, BytesBuffer)>,
    ),
    WaitingForBuffer(ReusableBoxFuture<'static, (SharedBufferPool, BytesBuffer)>),
}

impl PollBufferPool {
    pub fn new(buffer_pool: SharedBufferPool) -> Self {
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

pub struct ChunkedBytesBuffer {
    buffer_pool: PollBufferPool,
    chunks: VecDeque<BytesBuffer>,
    remaining_capacity: usize,
    write_chunk_idx: usize,
}

impl ChunkedBytesBuffer {
    pub fn new(buffer_pool: SharedBufferPool) -> Self {
        Self {
            buffer_pool: PollBufferPool::new(buffer_pool),
            chunks: VecDeque::new(),
            remaining_capacity: 0,
            write_chunk_idx: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.chunks.iter().map(|chunk| chunk.remaining()).sum()
    }

    fn register_chunk(&mut self, chunk: BytesBuffer) {
        self.remaining_capacity += chunk.remaining_mut();
        self.chunks.push_back(chunk);
    }

    fn pop_chunk(&mut self) -> Option<BytesBuffer> {
        match self.chunks.pop_front() {
            Some(chunk) => {
                self.remaining_capacity -= chunk.remaining_mut();

                // We do a saturating subtraction here so that when we pop the last chunk, we don't underflow. We just
                // end up back at the default state, when the buffer has no initial chunks.
                self.write_chunk_idx = self.write_chunk_idx.saturating_sub(1);
                Some(chunk)
            }
            None => None,
        }
    }
}

impl AsyncWrite for ChunkedBytesBuffer {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        while self.remaining_capacity < buf.len() {
            let chunk = ready!(self.buffer_pool.poll_acquire(cx));
            self.register_chunk(chunk);
        }

        let mut written = 0;

        while written < buf.len() && self.write_chunk_idx < self.chunks.len() {
            // Grab the current write chunk and figure out how much available capacity it has.
            let write_chunk_idx = self.write_chunk_idx;
            let chunk = &mut self.chunks[write_chunk_idx];
            let chunk_available_len = chunk.remaining_mut();
            if chunk_available_len == 0 {
                // No available capacity. Roll over to the next chunk. If there is no next chunk, the loop terminates
                // and the caller needs to allocate another chunk before proceeding.
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

impl Body for ChunkedBytesBuffer {
    type Data = BytesBuffer;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>, _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(self.pop_chunk().map(|chunk| Ok(Frame::data(chunk))))
    }
}

async fn acquire_buffer_from_pool(buffer_pool: Option<SharedBufferPool>) -> (SharedBufferPool, BytesBuffer) {
    match buffer_pool {
        Some(buffer_pool) => {
            let buffer = buffer_pool.acquire().await;
            (buffer_pool, buffer)
        }
        None => unreachable!(),
    }
}

#[derive(Clone)]
pub struct ChunkedBytesBufferPool {
    buffer_pool: Arc<dyn BufferPool<Buffer = BytesBuffer> + Send + Sync>,
}

impl ChunkedBytesBufferPool {
    pub fn new<B>(buffer_pool: B) -> Self
    where
        B: BufferPool<Buffer = BytesBuffer> + Send + Sync + 'static,
    {
        let buffer_pool = Arc::new(buffer_pool);
        Self { buffer_pool }
    }
}

#[async_trait]
impl BufferPool for ChunkedBytesBufferPool {
    type Buffer = ChunkedBytesBuffer;

    async fn acquire(&self) -> Self::Buffer {
        let buffer_pool = Arc::clone(&self.buffer_pool);
        ChunkedBytesBuffer::new(buffer_pool)
    }
}

// TODO: Rework these tests.
#[cfg(foo)]
mod tests {
    use std::io::Write as _;

    use bytes::BytesMut;
    use http_body_util::BodyExt as _;

    use super::*;
    use crate::{io::buf::vec::FixedSizeVec, util::buffer_pool::FixedSizeBufferPool};

    const TEST_CHUNK_SIZE: usize = 16;
    const TEST_BUF_CHUNK_SIZED: &[u8] = b"hello world!!!!!";
    const TEST_BUF_LESS_THAN_CHUNK_SIZED: &[u8] = b"hello world!";
    const TEST_BUF_GREATER_THAN_CHUNK_SIZED: &[u8] = b"hello world, here i come!";

    fn create_buffer_pool(chunks: usize, chunk_size: usize) -> (Arc<FixedSizeBufferPool<BytesBuffer>>, usize) {
        let buffer_pool = Arc::new(FixedSizeBufferPool::<BytesBuffer>::with_builder(chunks, || {
            FixedSizeVec::with_capacity(chunk_size)
        }));

        (buffer_pool, chunks * chunk_size)
    }

    #[tokio::test]
    async fn single_write_fits_within_single_chunk() {
        let (buffer_pool, total_capacity) = create_buffer_pool(1, TEST_CHUNK_SIZE);
        let mut chunked_buffer = ChunkedBytesBuffer::new(buffer_pool);
        chunked_buffer.reserve(TEST_BUF_LESS_THAN_CHUNK_SIZED.len()).await;

        // Fits within a single buffer, so it should complete in a single call.
        let n = chunked_buffer.write(TEST_BUF_LESS_THAN_CHUNK_SIZED).unwrap();
        assert_eq!(n, TEST_BUF_LESS_THAN_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 1);
        assert_eq!(chunked_buffer.remaining_capacity, total_capacity - n);
    }

    #[tokio::test]
    async fn single_write_fits_single_chunk_exactly() {
        let (buffer_pool, total_capacity) = create_buffer_pool(1, TEST_CHUNK_SIZE);
        let mut chunked_buffer = ChunkedBytesBuffer::new(buffer_pool);
        chunked_buffer.reserve(TEST_BUF_CHUNK_SIZED.len()).await;

        // Fits within a single buffer, so it should complete in a single call.
        let n = chunked_buffer.write(TEST_BUF_CHUNK_SIZED).unwrap();
        assert_eq!(n, TEST_BUF_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 1);
        assert_eq!(chunked_buffer.remaining_capacity, total_capacity - n);
    }

    #[tokio::test]
    async fn single_write_strides_two_chunks() {
        let (buffer_pool, total_capacity) = create_buffer_pool(2, TEST_CHUNK_SIZE);
        let mut chunked_buffer = ChunkedBytesBuffer::new(buffer_pool);
        chunked_buffer.reserve(TEST_BUF_GREATER_THAN_CHUNK_SIZED.len()).await;

        // This won't fit in a single chunk, but should fit within two.
        let n = chunked_buffer.write(TEST_BUF_GREATER_THAN_CHUNK_SIZED).unwrap();
        assert_eq!(n, TEST_BUF_GREATER_THAN_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 2);
        assert_eq!(chunked_buffer.remaining_capacity, total_capacity - n);
    }

    #[tokio::test]
    async fn two_writes_fit_two_chunks_exactly() {
        let (buffer_pool, _) = create_buffer_pool(2, TEST_CHUNK_SIZE);
        let mut chunked_buffer = ChunkedBytesBuffer::new(buffer_pool);
        chunked_buffer.reserve(TEST_BUF_CHUNK_SIZED.len() * 2).await;

        // First write acquires one chunk, and fills it up entirely.
        let first_n = chunked_buffer.write(TEST_BUF_CHUNK_SIZED).unwrap();
        assert_eq!(first_n, TEST_BUF_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 2);
        assert_eq!(chunked_buffer.remaining_capacity, TEST_BUF_CHUNK_SIZED.len());

        // Second write acquires one chunk, and also fills it up entirely.
        let second_n = chunked_buffer.write(TEST_BUF_CHUNK_SIZED).unwrap();
        assert_eq!(second_n, TEST_BUF_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 2);
        assert_eq!(chunked_buffer.remaining_capacity, 0);
    }

    #[tokio::test]
    async fn write_without_available_chunk() {
        let (buffer_pool, _) = create_buffer_pool(2, TEST_CHUNK_SIZE);
        let mut chunked_buffer = ChunkedBytesBuffer::new(buffer_pool);

        // First write should do nothing since we have no capacity.
        let first_n = chunked_buffer.write(TEST_BUF_CHUNK_SIZED).unwrap();
        assert_eq!(first_n, 0);
        assert_eq!(chunked_buffer.chunks.len(), 0);
        assert_eq!(chunked_buffer.remaining_capacity, 0);

        // Reserve some capacity for the write.
        chunked_buffer.reserve(TEST_BUF_CHUNK_SIZED.len()).await;
        assert_eq!(chunked_buffer.chunks.len(), 1);
        assert_eq!(chunked_buffer.remaining_capacity, TEST_BUF_CHUNK_SIZED.len());

        // Now we can write.
        let second_n = chunked_buffer.write(TEST_BUF_CHUNK_SIZED).unwrap();
        assert_eq!(second_n, TEST_BUF_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 1);
        assert_eq!(chunked_buffer.remaining_capacity, 0);

        // Try another two writes without any capacity.
        let third_n = chunked_buffer.write(TEST_BUF_CHUNK_SIZED).unwrap();
        assert_eq!(third_n, 0);

        let fourth_n = chunked_buffer.write(TEST_BUF_CHUNK_SIZED).unwrap();
        assert_eq!(fourth_n, 0);

        assert_eq!(chunked_buffer.chunks.len(), 1);
        assert_eq!(chunked_buffer.remaining_capacity, 0);

        // Now reserve some additional capacity.
        chunked_buffer.reserve(TEST_BUF_CHUNK_SIZED.len()).await;
        assert_eq!(chunked_buffer.chunks.len(), 2);
        assert_eq!(chunked_buffer.remaining_capacity, TEST_BUF_CHUNK_SIZED.len());

        // No we can write... again.
        let fifth_n = chunked_buffer.write(TEST_BUF_CHUNK_SIZED).unwrap();
        assert_eq!(fifth_n, TEST_BUF_CHUNK_SIZED.len());
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
        chunked_buffer.reserve(test_bufs_total_len).await;

        // Do three writes, using the less than/exactly/greater than-sized test buffers.
        //
        // We'll write these buffers, concatentated, to a single buffer that we'll use at the end to
        // compare the collected `Body`-based output.
        let mut expected_aggregated_body = BytesMut::new();
        let test_bufs = &[
            TEST_BUF_LESS_THAN_CHUNK_SIZED,
            TEST_BUF_CHUNK_SIZED,
            TEST_BUF_GREATER_THAN_CHUNK_SIZED,
        ];
        let mut total_written = 0;
        for i in 0..3 {
            chunked_buffer.write(test_bufs[i]).unwrap();
            expected_aggregated_body.put(test_bufs[i]);
            total_written += test_bufs[i].len();
        }

        assert_eq!(test_bufs_total_len, total_written);
        assert_eq!(chunked_buffer.chunks.len(), required_chunks);
        assert_eq!(chunked_buffer.remaining_capacity, total_capacity - total_written);

        // We should now be able to collect the chunked buffer as a `Body`, into a single output buffer.
        let actual_aggregated_body = chunked_buffer.collect().await.expect("cannot fail").to_bytes();

        assert_eq!(expected_aggregated_body.freeze(), actual_aggregated_body);
    }
}
