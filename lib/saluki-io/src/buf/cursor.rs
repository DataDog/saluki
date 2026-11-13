/// A cursor over the unread bytes of a buffer, which advances the buffer as bytes are taken.
///
/// The cursor borrows the buffer's bytes immutably and its read index exclusively. Bytes taken with
/// [`take`](Self::take) therefore stay valid for the whole borrow, even after the cursor is dropped, and the buffer
/// can't be written to, collapsed, or cleared until they're gone. Taking bytes is the only way to get bytes that
/// outlive the cursor, so any such bytes have already been advanced past.
pub struct ReadCursor<'a> {
    bytes: &'a [u8],
    read_idx: &'a mut usize,
    capacity: usize,
}

impl<'a> ReadCursor<'a> {
    /// Creates a cursor over `bytes`, starting at `*read_idx`.
    ///
    /// `read_idx` must be the buffer's own read index, and `capacity` the largest number of bytes the buffer can hold.
    /// Crate-private, so the only cursors are ones that advance a real buffer.
    ///
    /// # Panics
    ///
    /// Panics if `*read_idx` is greater than `bytes.len()`, or `capacity` is less than `bytes.len()`.
    pub(crate) fn new(bytes: &'a [u8], read_idx: &'a mut usize, capacity: usize) -> Self {
        assert!(*read_idx <= bytes.len(), "read index past the end of the buffer");
        assert!(capacity >= bytes.len(), "capacity smaller than the buffer");
        Self {
            bytes,
            read_idx,
            capacity,
        }
    }

    /// Returns the unread bytes.
    ///
    /// The returned slice borrows the cursor, so it can't be held while bytes are taken.
    pub fn unread(&self) -> &[u8] {
        &self.bytes[*self.read_idx..]
    }

    /// Returns the largest number of bytes the underlying buffer can hold.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Advances past the first `n` unread bytes, and returns them.
    ///
    /// # Panics
    ///
    /// Panics if `n` is greater than the number of unread bytes. The buffer is left unchanged.
    pub fn take(&mut self, n: usize) -> &'a [u8] {
        let start = *self.read_idx;
        assert!(n <= self.bytes.len() - start, "took past the end of the buffer");
        *self.read_idx = start + n;
        &self.bytes[start..start + n]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_advances_and_outlives_the_cursor() {
        let bytes = b"hello, world!";
        let mut read_idx = 0;

        let (first, second) = {
            let mut cursor = ReadCursor::new(bytes, &mut read_idx, bytes.len());
            let first = cursor.take(7);
            assert_eq!(cursor.unread(), b"world!");
            (first, cursor.take(6))
        };

        assert_eq!(first, b"hello, ");
        assert_eq!(second, b"world!");
        assert_eq!(read_idx, bytes.len());
    }

    #[test]
    fn take_past_end_panics_without_advancing() {
        let bytes = b"hello";
        let mut read_idx = 1;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ReadCursor::new(bytes, &mut read_idx, bytes.len()).take(5);
        }));
        assert!(result.is_err());
        assert_eq!(read_idx, 1);
    }
}
