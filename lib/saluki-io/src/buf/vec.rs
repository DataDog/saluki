use bytes::{buf::UninitSlice, Buf, BufMut};

use saluki_core::pooling::{helpers::pooled_newtype, Clearable};

use super::{ClearableIoBuffer, ReadIoBuffer};

/// A fixed-size byte vector.
///
/// This is a simple wrapper around a `Vec<u8>` that provides fixed-size semantics by disallowing writes that extend
/// beyond the initial capacity. `FixedSizeVec` cannot be used directly, and must be interacted with via the [`Buf`] and
/// [`BufMut`] traits.
///
/// Additionally, it is designed for use in object pools (implements [`Clearable`]).
pub struct FixedSizeVec {
    read_idx: usize,
    data: Vec<u8>,
}

impl FixedSizeVec {
    /// Creates a new `FixedSizeVec` with the given capacity.
    ///
    /// The vector will not grow once all available capacity has been consumed, and must be cleared to be reused.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            read_idx: 0,
            data: Vec::with_capacity(capacity),
        }
    }
}

impl Clearable for FixedSizeVec {
    fn clear(&mut self) {
        self.read_idx = 0;
        self.data.clear();
    }
}

pooled_newtype! {
    outer => BytesBuffer,
    inner => FixedSizeVec,
}

impl Buf for BytesBuffer {
    fn remaining(&self) -> usize {
        self.data().data.len() - self.data().read_idx
    }

    fn chunk(&self) -> &[u8] {
        let data = self.data();
        &data.data[data.read_idx..data.data.len()]
    }

    fn advance(&mut self, cnt: usize) {
        let data = self.data_mut();
        assert!(data.read_idx + cnt <= data.data.len());
        data.read_idx += cnt;
    }
}

unsafe impl BufMut for BytesBuffer {
    fn remaining_mut(&self) -> usize {
        let data = self.data();
        data.data.capacity() - data.data.len()
    }

    fn chunk_mut(&mut self) -> &mut UninitSlice {
        self.data_mut().data.spare_capacity_mut().into()
    }

    unsafe fn advance_mut(&mut self, cnt: usize) {
        let new_len = self.data().data.len() + cnt;
        self.data_mut().data.set_len(new_len);
    }
}

impl ReadIoBuffer for BytesBuffer {
    fn capacity(&self) -> usize {
        self.data().data.capacity()
    }
}

impl ClearableIoBuffer for BytesBuffer {
    fn clear(&mut self) {
        self.data_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::pooling::helpers::get_pooled_object_via_builder;

    use super::*;

    #[test]
    fn basic() {
        let mut buf = get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(13));

        let first_write = b"hello";
        let second_write = b", worl";
        let third_write = b"d!";

        // We start out empty:
        assert_eq!(buf.remaining(), 0);
        assert_eq!(buf.remaining_mut(), 13);

        // Write the first chunk:
        buf.put_slice(first_write);
        assert_eq!(buf.remaining(), 5);
        assert_eq!(buf.remaining_mut(), 8);

        // Write the second chunk:
        buf.put_slice(second_write);
        assert_eq!(buf.remaining(), 11);
        assert_eq!(buf.remaining_mut(), 2);

        // Read 7 bytes worth:
        let first_chunk = buf.chunk();
        assert_eq!(first_chunk.len(), 11);
        assert_eq!(first_chunk, b"hello, worl");

        buf.advance(7);
        assert_eq!(buf.remaining(), 4);
        assert_eq!(buf.remaining_mut(), 2);

        // Write the third chunk:
        buf.put_slice(third_write);
        assert_eq!(buf.remaining(), 6);
        assert_eq!(buf.remaining_mut(), 0);

        // Read the rest:
        let second_chunk = buf.chunk();
        assert_eq!(second_chunk.len(), 6);
        assert_eq!(second_chunk, b"world!");

        buf.advance(6);
        assert_eq!(buf.remaining(), 0);
        assert_eq!(buf.remaining_mut(), 0);

        // Clear the buffer:
        buf.data_mut().clear();
        assert_eq!(buf.remaining(), 0);
        assert_eq!(buf.remaining_mut(), 13);
    }
}
