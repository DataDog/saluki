use bytes::{Buf, BufMut, Bytes};

use saluki_core::pooling::FixedSizeObjectPool;

mod chunked;
pub use self::chunked::{ChunkedBytesBuffer, ChunkedBytesBufferObjectPool};

mod vec;
pub use self::vec::{BytesBuffer, FixedSizeVec};

/// An I/O buffer that can be read from.
pub trait ReadIoBuffer: Buf {
    // TODO: This is a little restrictive because it doesn't quite allow for the possibly of a buffer that can grow
    // which is the basis of normal buffer types (`Vec<u8>`, `BytesMut`) as well as our own, such as
    // `ChunkedBytesBuffer`.
    //
    // I think it's OK for now to let us implement our behavior in the length-delimited framer, but we might want to
    // revisit this in the future.
    fn capacity(&self) -> usize;
}

impl ReadIoBuffer for Bytes {
    fn capacity(&self) -> usize {
        self.len()
    }
}

/// An I/O buffer that can be written to.
pub trait WriteIoBuffer: BufMut {}

impl<T> WriteIoBuffer for T where T: BufMut {}

/// An I/O buffer that can be read from and written to.
pub trait ReadWriteIoBuffer: ReadIoBuffer + WriteIoBuffer {}

impl<T> ReadWriteIoBuffer for T where T: ReadIoBuffer + WriteIoBuffer {}

/// Creates a new `FixedSizeObjectPool<BytesBuffers>` with the given number of buffers, each with the given buffer size.
///
/// This is an upfront allocation, and will immediately consume `buffers * buffer_size` bytes of memory.
pub fn get_fixed_bytes_buffer_pool(buffers: usize, buffer_size: usize) -> FixedSizeObjectPool<BytesBuffer> {
    FixedSizeObjectPool::with_builder(buffers, || FixedSizeVec::with_capacity(buffer_size))
}
