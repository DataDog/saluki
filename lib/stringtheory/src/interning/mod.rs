//! Interning utilities.
use std::{collections::BTreeSet, fmt, ops::Deref, ptr::NonNull};

mod fixed_size;
pub use self::fixed_size::FixedSizeInterner;

mod helpers;

mod map;
pub use self::map::GenericMapInterner;

pub(crate) struct InternerVtable {
    /// Name of the interner implementation that this string was interned with.
    pub interner_name: &'static str,

    /// Gets the raw parts of the interned string for reassembly.
    ///
    /// This is structured as such so that the caller can tie the appropriate lifetime to the string reference, as it
    /// cannot be done as part of the vtable signature itself.
    pub as_raw_parts: unsafe fn(NonNull<()>) -> (NonNull<u8>, usize),

    /// Clones the interned string.
    pub clone: unsafe fn(NonNull<()>) -> NonNull<()>,

    /// Drops the interned string.
    pub drop: unsafe fn(NonNull<()>),
}

/// An interned string.
///
/// This string type is read-only, and dereferences to `&str` for ergonomic usage. It is cheap to clone (16 bytes), but
/// generally will not be interacted with directly. Instead, most usages should be wrapped in `MetaString`.
pub struct InternedString {
    state: NonNull<()>,
    vtable: &'static InternerVtable,
}

impl Clone for InternedString {
    fn clone(&self) -> Self {
        // SAFETY: The virtual table is responsible for ensuring that the returned pointer is non-null and valid.
        let new_state = unsafe { (self.vtable.clone)(self.state) };
        Self {
            state: new_state,
            vtable: self.vtable,
        }
    }
}

impl fmt::Debug for InternedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InternedString")
            .field("state", &self.state)
            .field("vtable", &self.vtable.interner_name)
            .finish()
    }
}

impl Deref for InternedString {
    type Target = str;

    fn deref(&self) -> &str {
        // SAFETY: The virtual table is responsible for ensuring that the returned pointer is non-null and valid,
        // pointing to a slice of the given length, with valid UTF-8.
        unsafe {
            let (ptr, len) = (self.vtable.as_raw_parts)(self.state);
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr.as_ptr() as *const _, len))
        }
    }
}

impl PartialEq for InternedString {
    fn eq(&self, other: &Self) -> bool {
        self.state == other.state
    }
}

impl Drop for InternedString {
    fn drop(&mut self) {
        // SAFETY: The virtual table is responsible for ensuring that the pointer is non-null and valid.
        unsafe {
            (self.vtable.drop)(self.state);
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ReclaimedEntry {
    offset: usize,
    capacity: usize,
}

impl ReclaimedEntry {
    /// Creates a new `ReclaimedEntry` with the given offset and capacity.
    const fn new(offset: usize, capacity: usize) -> Self {
        Self { offset, capacity }
    }

    /// Returns `true` if `other` comes immediately after (contiguous) `self`.
    const fn followed_by(&self, other: &Self) -> bool {
        self.offset + self.capacity == other.offset
    }

    /// Returns the offset of this reclaimed entry in the data buffer.
    const fn offset(&self) -> usize {
        self.offset
    }

    /// Returns the size, in bytes, of this reclaimed entry.
    const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Splits the entry into two at the given index.
    ///
    /// Afterwards, `self` will have a new capacity of `len` and its original offset, and the returned `ReclaimedEntry`
    /// will have an offset of `self.offset + len` and a capacity of `self.capacity - len`.
    fn split_off(&mut self, len: usize) -> Self {
        let new_offset = self.offset + len;
        let new_capacity = self.capacity - len;
        self.capacity = len;

        Self::new(new_offset, new_capacity)
    }

    /// Merges `other` into `self`, updating `self` to reflect the new, larger entry.
    fn merge(&mut self, other: Self) {
        debug_assert!(
            self.followed_by(&other) || other.followed_by(self),
            "`merge` should never be called for non-adjacent entries"
        );

        // We'll try and merge regardless of which side is adjacent, but we'll always merge `other` into `self`.
        if self.offset < other.offset {
            self.capacity += other.capacity;
        } else {
            self.offset = other.offset;
            self.capacity += other.capacity;
        }
    }
}

impl PartialEq for ReclaimedEntry {
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset && self.capacity == other.capacity
    }
}

impl Eq for ReclaimedEntry {}

impl PartialOrd for ReclaimedEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.offset.cmp(&other.offset))
    }
}

impl Ord for ReclaimedEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.offset.cmp(&other.offset)
    }
}

#[derive(Debug)]
struct ReclaimedEntries(BTreeSet<ReclaimedEntry>);

impl ReclaimedEntries {
    /// Creates a new, empty `ReclaimedEntries`.
    fn new() -> Self {
        Self(BTreeSet::new())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Inserts a new entry into the reclaimed entries.
    fn insert(&mut self, mut current_entry: ReclaimedEntry) -> ReclaimedEntry {
        // Reclamation is a two-step process: first, we try to find adjacent reclaimed entries to the one being added,
        // and merge them together if possible, and secondly, we tombstone the entry (whether merged or not).

        // First, try and find adjacent reclaimed entries.
        //
        // Essentially, if we have two (or more) contiguous entries, we want to merge them together... so that, for
        // example, instead of 2 or 3 entries that are only 64 bytes large (which means 40 bytes usable for strings), we
        // can have a single entry that's 128-192 bytes large (which means 104-168 bytes usable for strings).
        //
        // We iterate over the existing reclaimed entries to see if we can find any that come either directly before
        // _or_ after the current entry we're trying to add, and that are adjacent to the current entry, and make a note
        // of them if found.
        let mut maybe_prev_entry = None;
        let mut maybe_next_entry = None;

        for entry in self.0.iter() {
            if entry.followed_by(&current_entry) {
                // We found an adjacent entry that comes right _before_ our current entry.
                maybe_prev_entry = Some(*entry);
            } else if current_entry.followed_by(entry) {
                // We found an adjacent entry that comes right _after_ our current entry.
                maybe_next_entry = Some(*entry);
                break;
            } else {
                // We're past the current entry, so we can break early.
                if entry.offset > current_entry.offset {
                    break;
                }
            }
        }

        // We found adjacent entries, so remove them from the overall list of reclaimed entries and merge them into our
        // current entry before adding the current entry to the list.
        if let Some(prev_entry) = maybe_prev_entry {
            self.0.remove(&prev_entry);
            current_entry.merge(prev_entry);
        }

        if let Some(next_entry) = maybe_next_entry {
            self.0.remove(&next_entry);
            current_entry.merge(next_entry);
        }

        self.0.insert(current_entry);

        // Now that we've ensured we have the biggest possible reclaimed entry after merging adjacent entries, we return
        // that to the caller so they know what the resulting entry is after any potential merging.
        //
        // This allows for the caller to write tombstone markers and so on.
        current_entry
    }

    #[cfg(test)]
    fn first(&self) -> Option<ReclaimedEntry> {
        self.0.first().copied()
    }

    fn take_if<F>(&mut self, f: F) -> Option<ReclaimedEntry>
    where
        F: Fn(&ReclaimedEntry) -> bool,
    {
        let found_entry = self.0.iter().find(|entry| f(entry)).copied()?;
        self.0.remove(&found_entry);

        Some(found_entry)
    }

    fn remove(&mut self, entry: &ReclaimedEntry) {
        self.0.remove(entry);
    }

    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = &ReclaimedEntry> {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_of_interned_string() {
        // We're asserting that `InternedString` itself is 16 bytes: the thin pointer to the underlying interned
        // string's representation, and the vtable pointer.
        assert_eq!(std::mem::size_of::<InternedString>(), 16);
    }

    #[test]
    fn reclamation_merges_previous_and_current() {
        // NOTE: The order of how we insert entries matters here, because what we're testing is that the reclamation logic can properly
        // merge adjacent entries regardless of whether we're merging with an existing reclaimed entry that comes before
        // us _or_ after us (or both!).

        let mut entries = ReclaimedEntries::new();
        assert_eq!(entries.len(), 0);

        // Insert two reclaimed entries that are not adjacent.
        let entry1 = ReclaimedEntry::new(0, 64);
        let entry2 = ReclaimedEntry::new(128, 64);
        entries.insert(entry1);
        entries.insert(entry2);
        assert_eq!(entries.len(), 2);

        // Insert a third entry that is adjacent to `entry1` but not adjacent to `entry2`.
        let entry3 = ReclaimedEntry::new(64, 32);
        entries.insert(entry3);

        // We should now have two entries: one that represents the combined range of `entry1` and `entry2`, and then the
        // original `entry3`.
        assert_eq!(entries.len(), 2);

        let mut entries_iter = entries.iter();

        let first_entry = entries_iter.next().unwrap();
        assert_eq!(first_entry.offset, 0);
        assert_eq!(first_entry.capacity, 96);

        let second_entry = entries_iter.next().unwrap();
        assert_eq!(second_entry.offset, 128);
        assert_eq!(second_entry.capacity, 64);
    }

    #[test]
    fn reclamation_merges_current_and_subsequent() {
        // NOTE: The order of how we insert entries matters here, because what we're testing is that the reclamation logic can properly
        // merge adjacent entries regardless of whether we're merging with an existing reclaimed entry that comes before
        // us _or_ after us (or both!).

        let mut entries = ReclaimedEntries::new();
        assert_eq!(entries.len(), 0);

        // Insert two reclaimed entries that are not adjacent.
        let entry1 = ReclaimedEntry::new(0, 64);
        let entry2 = ReclaimedEntry::new(128, 64);
        entries.insert(entry1);
        entries.insert(entry2);
        assert_eq!(entries.len(), 2);

        // Insert a third entry that is adjacent to `entry1` but not adjacent to `entry2`.
        let entry3 = ReclaimedEntry::new(96, 32);
        entries.insert(entry3);

        // We should now have two entries: one that represents the original `entry`, and one that represents the
        // combined range of `entry2` and `entry3`.
        assert_eq!(entries.len(), 2);

        let mut entries_iter = entries.iter();

        let first_entry = entries_iter.next().unwrap();
        assert_eq!(first_entry.offset, 0);
        assert_eq!(first_entry.capacity, 64);

        let second_entry = entries_iter.next().unwrap();
        assert_eq!(second_entry.offset, 96);
        assert_eq!(second_entry.capacity, 96);
    }

    #[test]
    fn reclamation_merges_previous_and_current_and_subsequent() {
        // NOTE: The order of how we insert entries matters here, because what we're testing is that the reclamation logic can properly
        // merge adjacent entries regardless of whether we're merging with an existing reclaimed entry that comes before
        // us _or_ after us (or both!).

        let mut entries = ReclaimedEntries::new();
        assert_eq!(entries.len(), 0);

        // Insert two reclaimed entries that are not adjacent.
        let entry1 = ReclaimedEntry::new(0, 64);
        let entry2 = ReclaimedEntry::new(128, 64);
        entries.insert(entry1);
        entries.insert(entry2);
        assert_eq!(entries.len(), 2);

        // Insert a third entry that bridges the gap between `entry1` and `entry2`.
        let entry3 = ReclaimedEntry::new(64, 64);
        entries.insert(entry3);

        // We should now have a single entry that represents the combined range of `entry1`, `entry2`, and `entry3`.
        assert_eq!(entries.len(), 1);

        let first_entry = entries.iter().next().unwrap();
        assert_eq!(first_entry.offset, 0);
        assert_eq!(first_entry.capacity, 192);
    }
}
