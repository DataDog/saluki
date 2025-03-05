use std::{mem::ManuallyDrop, sync::Arc};

use bytes::{buf::UninitSlice, Buf, BufMut};
use saluki_core::pooling::{helpers::pooled_newtype, Clearable, ReclaimStrategy};
use triomphe::{Arc as TriompheArc, UniqueArc};

use super::{ClearableIoBuffer, CollapsibleReadWriteIoBuffer, ReadIoBuffer};

/// A fixed-size bytes buffer.
///
/// This is a simple wrapper around a `BytesMut` that provides fixed-size semantics by disallowing writes that extend
/// beyond the initial capacity. `FixedSizeVec` cannot be used directly, and must be interacted with via the
/// [`Buf`] and [`BufMut`] traits.
///
/// Additionally, it is designed for use in object pools (implements [`Clearable`]).
pub struct FixedSizeVec {
    data: UniqueArc<Vec<u8>>,
    read_idx: usize,
}

impl FixedSizeVec {
    /// Creates a new `FixedSizeVec` with the given capacity.
    ///
    /// The vector will not grow once all available capacity has been consumed, and must be cleared to be reused.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            data: UniqueArc::new(Vec::with_capacity(capacity)),
            read_idx: 0,
        }
    }

    fn freeze(self) -> FrozenFixedSizeVec {
        FrozenFixedSizeVec {
            data: self.data.shareable(),
            read_idx: self.read_idx,
        }
    }
}

impl Clearable for FixedSizeVec {
    fn clear(&mut self) {
        self.data.clear();
        self.read_idx = 0;
    }
}

struct FrozenFixedSizeVec {
    data: TriompheArc<Vec<u8>>,
    read_idx: usize,
}

impl FrozenFixedSizeVec {
    fn into_unique(self) -> Option<FixedSizeVec> {
        TriompheArc::into_unique(self.data).map(|data| FixedSizeVec { data, read_idx: 0 })
    }
}

impl Clone for FrozenFixedSizeVec {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            read_idx: 0,
        }
    }
}

pooled_newtype! {
    outer => BytesBuffer,
    inner => FixedSizeVec,
}

impl BytesBuffer {
    /// Consumes this buffer and returns a read-only version of it.
    pub fn freeze(mut self) -> FrozenBytesBuffer {
        let data = self.data.take().unwrap().freeze();

        FrozenBytesBuffer {
            strategy_ref: Arc::clone(&self.strategy_ref),
            data: ManuallyDrop::new(data),
        }
    }
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

/// A frozen, read-only version of [`BytesBuffer`].
///
/// `FrozenBytesBuffer` can be cheaply cloned, and allows for sharing an underlying [`BytesBuffer`] among multiple
/// tasks while still maintaining all of the original buffer's object pooling semantics.
// TODO: it's not great that we're manually emulating the internal structure of `BytesBuffer`, since the whole point is
// that those bits are auto-generated for us and meant to be functionally transparent to using `BytesBuffer` in the
// first place... it'd be interesting to consider if we could make this more ergonomic, perhaps by having some sort of
// convenience helper method for converting pooled objects of type T to U where the `Poolable::Data` is identical
// between them, almost along lines of `CoerceUnsized` where the underlying data isn't changing, just the representation
// of it.
#[derive(Clone)]
pub struct FrozenBytesBuffer {
    strategy_ref: Arc<dyn ReclaimStrategy<BytesBuffer> + Send + Sync>,
    data: ManuallyDrop<FrozenFixedSizeVec>,
}

impl FrozenBytesBuffer {
    /// Returns `true` if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.data.read_idx == self.data.data.len()
    }

    /// Returns the number of bytes remaining in the buffer.
    pub fn len(&self) -> usize {
        self.data.data.len() - self.data.read_idx
    }

    /// Returns the total capacity of the buffer.
    pub fn capacity(&self) -> usize {
        self.data.data.capacity()
    }
}

impl Buf for FrozenBytesBuffer {
    fn remaining(&self) -> usize {
        self.data.data.len() - self.data.read_idx
    }

    fn chunk(&self) -> &[u8] {
        &self.data.data[self.data.read_idx..]
    }

    fn advance(&mut self, cnt: usize) {
        assert!(self.data.read_idx + cnt <= self.data.data.len());
        self.data.read_idx += cnt;
    }
}

impl Drop for FrozenBytesBuffer {
    fn drop(&mut self) {
        // If we're the last reference to the buffer, we need to reconstitute it back to a `FixedSizeVec`, and reclaim
        // it to the object pool.
        //
        // SAFETY: Nothing else can be using `self.data` since we're dropping.
        let data = unsafe { ManuallyDrop::take(&mut self.data) };
        if let Some(data) = data.into_unique() {
            self.strategy_ref.reclaim(data);
        }
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
