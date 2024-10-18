use bytes::{buf::UninitSlice, Buf, BufMut};
use saluki_core::pooling::{helpers::pooled_newtype, Clearable};

use super::{ClearableIoBuffer, CollapsibleReadWriteIoBuffer, ReadIoBuffer};

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

impl CollapsibleReadWriteIoBuffer for BytesBuffer {
    fn collapse(&mut self) {
        let remaining = self.remaining();

        // If the buffer is empty, all we have to do is reset the buffer to its initial state.
        if remaining == 0 {
            let inner = self.data_mut();
            inner.read_idx = 0;
            inner.data.clear();
            return;
        }

        // Otherwise, we have to actually shift the remaining data to the front of the buffer and then also update our
        // buffer state.
        let inner = self.data_mut();

        let src_start = inner.read_idx;
        let src_end = inner.data.len();
        inner.data.copy_within(src_start..src_end, 0);
        inner.data.truncate(remaining);
        inner.read_idx = 0;
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

    #[test]
    fn collapsible_empty() {
        let mut buf = get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(13));

        // Buffer is empty.
        assert_eq!(buf.remaining(), 0);
        assert_eq!(buf.remaining_mut(), 13);

        buf.collapse();

        // Buffer is still empty.
        assert_eq!(buf.remaining(), 0);
        assert_eq!(buf.remaining_mut(), 13);
    }

    #[test]
    fn collapsible_remaining_already_collapsed() {
        let mut buf = get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(24));

        // Write a simple string to the buffer.
        buf.put_slice(b"hello, world!");
        assert_eq!(buf.remaining(), 13);
        assert_eq!(buf.remaining_mut(), 11);

        buf.collapse();

        // Buffer is still the same since we never read anything from the buffer.
        assert_eq!(buf.remaining(), 13);
        assert_eq!(buf.remaining_mut(), 11);
    }

    #[test]
    fn collapsible_remaining_not_collapsed_no_overlap() {
        let mut buf = get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(24));

        // Write a simple string to the buffer.
        buf.put_slice(b"hello, world!");
        assert_eq!(buf.remaining(), 13);
        assert_eq!(buf.remaining_mut(), 11);
        assert_eq!(buf.chunk(), b"hello, world!");

        // Write another simple string to the buffer.
        buf.put_slice(b"huzzah!");
        assert_eq!(buf.remaining(), 20);
        assert_eq!(buf.remaining_mut(), 4);
        assert_eq!(buf.chunk(), b"hello, world!huzzah!");

        // Simulate reading the first string from the buffer, which will end up leaving a hole in the buffer, prior to
        // the second string, that is big enough to fit the second string entirely.
        buf.advance(13);
        assert_eq!(buf.remaining(), 7);
        assert_eq!(buf.remaining_mut(), 4);
        assert_eq!(buf.chunk(), b"huzzah!");

        buf.collapse();

        // Buffer should now be collapsed, with the second string at the beginning of the buffer.
        assert_eq!(buf.remaining(), 7);
        assert_eq!(buf.remaining_mut(), 17);
        assert_eq!(buf.chunk(), b"huzzah!");
    }

    #[test]
    fn collapsible_remaining_not_collapsed_with_overlap() {
        let mut buf = get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(24));

        // Write a simple string to the buffer.
        buf.put_slice(b"huzzah!");
        assert_eq!(buf.remaining(), 7);
        assert_eq!(buf.remaining_mut(), 17);
        assert_eq!(buf.chunk(), b"huzzah!");

        // Write another simple string to the buffer.
        buf.put_slice(b"hello, world!");
        assert_eq!(buf.remaining(), 20);
        assert_eq!(buf.remaining_mut(), 4);
        assert_eq!(buf.chunk(), b"huzzah!hello, world!");

        // Simulate reading the first string from the buffer, which will end up leaving a hole in the buffer, prior to
        // the second string, that isn't big enough to fit the second string entirely.
        buf.advance(7);
        assert_eq!(buf.remaining(), 13);
        assert_eq!(buf.remaining_mut(), 4);
        assert_eq!(buf.chunk(), b"hello, world!");

        buf.collapse();

        // Buffer should now be collapsed, with the second string at the beginning of the buffer.
        assert_eq!(buf.remaining(), 13);
        assert_eq!(buf.remaining_mut(), 11);
        assert_eq!(buf.chunk(), b"hello, world!");
    }
}
