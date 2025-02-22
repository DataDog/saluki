use std::collections::VecDeque;

use bytes::{Buf, BufMut, Bytes};

mod chunked;
pub use self::chunked::{ChunkedBytesBuffer, ChunkedBytesBufferObjectPool, FrozenChunkedBytesBuffer};

mod vec;
pub use self::vec::{BytesBuffer, BytesBufferView, BufferView, FixedSizeVec};

/// An I/O buffer that can be read from.
pub trait ReadIoBuffer: Buf {
    fn capacity(&self) -> usize;
}

impl ReadIoBuffer for &[u8] {
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

/// An I/O buffer that can be "collapsed" to regain unused capacity.
pub trait CollapsibleReadWriteIoBuffer: ReadWriteIoBuffer {
    /// Collapses the buffer, shifting any remaining data to the beginning of the buffer.
    ///
    /// This allows for more efficient use of the buffer, as remaining chunks that have yet to be read can be shifted to
    /// the left to allow for subsequent reads to take place in the remaining capacity. This is particularly useful for
    /// dealing with framed protocols, where multiple payloads may be present in a single buffer, and partial reads need
    /// to be completed in order to extract the next frame.
    fn collapse(&mut self);
}

/// A buffer that can be cleared.
pub trait ClearableIoBuffer {
    /// Clears the buffer, setting it back to its initial state.
    fn clear(&mut self);
}
