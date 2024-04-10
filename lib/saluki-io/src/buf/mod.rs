use bytes::{Buf, BufMut};

use saluki_core::buffers::FixedSizeBufferPool;

mod chunked;
pub use self::chunked::{ChunkedBytesBuffer, ChunkedBytesBufferPool};

mod vec;
pub use self::vec::{BytesBuffer, FixedSizeVec};

pub trait IoBuffer: Buf + BufMut {}

pub fn get_fixed_bytes_buffer_pool(buffers: usize, buffer_size: usize) -> FixedSizeBufferPool<BytesBuffer> {
    FixedSizeBufferPool::with_builder(buffers, || FixedSizeVec::with_capacity(buffer_size))
}
