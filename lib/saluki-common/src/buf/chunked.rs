use std::{
    collections::VecDeque,
    convert::Infallible,
    io,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{buf::UninitSlice, Buf, BufMut, Bytes, BytesMut};
use http_body::{Body, Frame, SizeHint};
use tokio::io::AsyncWrite;
use tracing::info;

/// A bytes buffer that write dynamically-sized payloads across multiple fixed-size chunks.
///
/// As callers write data to `ChunkedBytesBuffer`, it will allocate additional chunks as needed, and write the data
/// across these chunks. This allows for predictable memory usage by avoiding reallocations that overestimate the
/// necessary additional capacity, and provides mechnical sympathy to the allocator by using consistently-sized chunks.
///
/// `ChunkedBytesBuffer` implements [`AsyncWrite`] and [`Body`], allowing it to be asynchronously written to and used as
/// the body of an HTTP request without any additional allocations and copying/merging of data into a single buffer.
pub struct ChunkedBytesBuffer {
    chunks: VecDeque<BytesMut>,
    chunk_size: usize,
    remaining_capacity: usize,
}

impl ChunkedBytesBuffer {
    /// Creates a new `ChunkedBytesBuffer`, configured to use chunks of the given size.
    pub fn new(chunk_size: usize) -> Self {
        let s = Self {
            chunks: VecDeque::new(),
            chunk_size,
            remaining_capacity: 0,
        };
        info!("WACKTEST6: chunked_bytes_buffer_new chunk_size={}", chunk_size);
        s
    }

    /// Returns `true` if the buffer has no data.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Returns the number of bytes written to the buffer.
    pub fn len(&self) -> usize {
        self.chunks.iter().map(|chunk| chunk.remaining()).sum()
    }

    fn register_new_chunk(&mut self) {
        self.remaining_capacity += self.chunk_size;
        self.chunks.push_back(BytesMut::with_capacity(self.chunk_size));
        info!(
            "WACKTEST6: cbb_register_new_chunk chunks={} chunk_size={} remaining_capacity={}",
            self.chunks.len(),
            self.chunk_size,
            self.remaining_capacity
        );
    }

    fn ensure_capacity_for_write(&mut self) {
        if self.remaining_capacity == 0 {
            self.register_new_chunk();
        }
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

impl AsyncWrite for ChunkedBytesBuffer {
    fn poll_write(mut self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        let before_len = self.len();
        let before_chunks = self.chunks.len();
        self.put_slice(buf);
        info!(
            "WACKTEST6: cbb_poll_write wrote={} len_before={} len_after={} chunks_before={} chunks_after={}",
            buf.len(),
            before_len,
            self.len(),
            before_chunks,
            self.chunks.len()
        );
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

unsafe impl BufMut for ChunkedBytesBuffer {
    fn remaining_mut(&self) -> usize {
        usize::MAX - self.len()
    }

    fn chunk_mut(&mut self) -> &mut UninitSlice {
        self.ensure_capacity_for_write();
        self.chunks.back_mut().unwrap().spare_capacity_mut().into()
    }

    unsafe fn advance_mut(&mut self, cnt: usize) {
        self.chunks.back_mut().unwrap().advance_mut(cnt);
        self.remaining_capacity -= cnt;
    }
}

/// A frozen, read-only version of [`ChunkedBytesBuffer`].
///
/// `FrozenChunkedBytesBuffer` can be cheaply cloned, and allows for sharing the underlying chunks among multiple tasks.
#[derive(Clone)]
pub struct FrozenChunkedBytesBuffer {
    chunks: VecDeque<Bytes>,
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

    /// Returns the number of underlying chunks.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
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
    type Data = Bytes;
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

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use http_body_util::BodyExt as _;
    use tokio::io::AsyncWriteExt as _;
    use tokio_test::{assert_ready, task::spawn as test_spawn};

    use super::*;

    const TEST_CHUNK_SIZE: usize = 16;
    const TEST_BUF_CHUNK_SIZED: &[u8] = b"hello world!!!!!";
    const TEST_BUF_LESS_THAN_CHUNK_SIZED: &[u8] = b"hello world!";
    const TEST_BUF_GREATER_THAN_CHUNK_SIZED: &[u8] = b"hello world, here i come!";

    #[test]
    fn single_write_fits_within_single_chunk() {
        let mut chunked_buffer = ChunkedBytesBuffer::new(TEST_CHUNK_SIZE);

        // Fits within a single buffer, so it should complete without blocking.i
        let mut fut = test_spawn(chunked_buffer.write(TEST_BUF_LESS_THAN_CHUNK_SIZED));
        let result = assert_ready!(fut.poll());

        let n = result.unwrap();
        assert_eq!(n, TEST_BUF_LESS_THAN_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 1);

        let total_capacity = chunked_buffer.chunks.len() * TEST_CHUNK_SIZE;
        assert_eq!(chunked_buffer.remaining_capacity, total_capacity - n);
    }

    #[test]
    fn single_write_fits_single_chunk_exactly() {
        let mut chunked_buffer = ChunkedBytesBuffer::new(TEST_CHUNK_SIZE);

        // Fits within a single buffer, so it should complete without blocking.
        let mut fut = test_spawn(chunked_buffer.write(TEST_BUF_CHUNK_SIZED));
        let result = assert_ready!(fut.poll());

        let n = result.unwrap();
        assert_eq!(n, TEST_BUF_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 1);

        let total_capacity = chunked_buffer.chunks.len() * TEST_CHUNK_SIZE;
        assert_eq!(chunked_buffer.remaining_capacity, total_capacity - n);
    }

    #[test]
    fn single_write_strides_two_chunks() {
        let mut chunked_buffer = ChunkedBytesBuffer::new(TEST_CHUNK_SIZE);

        // This won't fit in a single chunk, but should fit within two.
        let mut fut = test_spawn(chunked_buffer.write(TEST_BUF_GREATER_THAN_CHUNK_SIZED));
        let result = assert_ready!(fut.poll());

        let n = result.unwrap();
        assert_eq!(n, TEST_BUF_GREATER_THAN_CHUNK_SIZED.len());
        assert_eq!(chunked_buffer.chunks.len(), 2);

        let total_capacity = chunked_buffer.chunks.len() * TEST_CHUNK_SIZE;
        assert_eq!(chunked_buffer.remaining_capacity, total_capacity - n);
    }

    #[test]
    fn two_writes_fit_two_chunks_exactly() {
        let mut chunked_buffer = ChunkedBytesBuffer::new(TEST_CHUNK_SIZE);

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

        let mut chunked_buffer = ChunkedBytesBuffer::new(TEST_CHUNK_SIZE);
        let total_capacity = required_chunks * TEST_CHUNK_SIZE;

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
