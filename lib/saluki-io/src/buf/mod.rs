use bytes::{Buf, BufMut};

use saluki_core::pooling::FixedSizeObjectPool;

mod chunked;
pub use self::chunked::{ChunkedBytesBuffer, ChunkedBytesBufferObjectPool};

mod vec;
pub use self::vec::{BytesBuffer, FixedSizeVec};

/// An I/O buffer that can be read from.
pub trait ReadIoBuffer: Buf {
    fn capacity(&self) -> usize;
}

impl<'a> ReadIoBuffer for &'a [u8] {
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

/// A buffer that can be cleared.
pub trait ClearableIoBuffer {
    /// Clears the buffer, setting it back to its initial state.
    fn clear(&mut self);
}

/// Creates a new `FixedSizeObjectPool<BytesBuffers>` with the given number of buffers, each with the given buffer size.
///
/// This is an upfront allocation, and will immediately consume `buffers * buffer_size` bytes of memory.
pub fn get_fixed_bytes_buffer_pool(buffers: usize, buffer_size: usize) -> FixedSizeObjectPool<BytesBuffer> {
    FixedSizeObjectPool::with_builder(buffers, || FixedSizeVec::with_capacity(buffer_size))
}
