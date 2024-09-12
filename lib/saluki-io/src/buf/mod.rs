use std::collections::VecDeque;

use bytes::{Buf, BufMut, Bytes};

mod chunked;
pub use self::chunked::{ChunkedBuffer, ChunkedBufferObjectPool};

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

impl ReadIoBuffer for Bytes {
    fn capacity(&self) -> usize {
        self.len()
    }
}

impl ReadIoBuffer for VecDeque<u8> {
    fn capacity(&self) -> usize {
        self.capacity()
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
