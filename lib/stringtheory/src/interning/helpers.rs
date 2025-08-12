use std::{alloc::Layout, collections::BTreeSet, hash::BuildHasher as _, num::NonZeroUsize, sync::LazyLock};

const PACKED_CAP_MASK: usize = usize::MAX << (usize::BITS / 2);
const PACKED_CAP_SHIFT: u32 = usize::BITS / 2;
const PACKED_LEN_MASK: usize = usize::MAX >> (usize::BITS / 2);

/// A packed length/capacity representation in a single `usize`.
///
/// The upper half of the `usize` contains the capacity, while the lower half contains the length. This is useful for
/// tracking both the capacity and length of a value in a more efficient way, when the known upper bound on the size of
/// the values can be represented in half of the bits of `usize`, which on 64-bit platforms means being less than or
/// equal to 4GB.
pub struct PackedLengthCapacity(usize);

impl PackedLengthCapacity {
    /// Creates a new packed length/capacity value.
    pub const fn new(capacity: usize, len: usize) -> Self {
        Self((capacity << PACKED_CAP_SHIFT) | len)
    }

    /// Returns the capacity from the packed value.
    pub const fn capacity(&self) -> usize {
        (self.0 & PACKED_CAP_MASK) >> PACKED_CAP_SHIFT
    }

    /// Returns the length from the packed value.
    pub const fn len(&self) -> usize {
        self.0 & PACKED_LEN_MASK
    }

    /// Returns the maximum capacity or length that can be stored in the packed representation.
    pub const fn maximum_value() -> usize {
        (usize::MAX & PACKED_CAP_MASK) >> PACKED_CAP_SHIFT
    }
}

impl std::fmt::Debug for PackedLengthCapacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackedLengthCapacity")
            .field("cap", &self.capacity())
            .field("len", &self.len())
            .finish()
    }
}

/// A reclaimed entry.
#[derive(Clone, Copy, Debug)]
pub struct ReclaimedEntry {
    pub offset: usize,
    pub capacity: usize,
}

impl ReclaimedEntry {
    /// Creates a new `ReclaimedEntry` with the given offset and capacity.
    pub const fn new(offset: usize, capacity: usize) -> Self {
        Self { offset, capacity }
    }

    /// Returns `true` if `other` comes immediately after (contiguous) `self`.
    pub const fn followed_by(&self, other: &Self) -> bool {
        self.offset + self.capacity == other.offset
    }

    /// Returns the offset of this reclaimed entry in the data buffer.
    pub const fn offset(&self) -> usize {
        self.offset
    }

    /// Returns the size, in bytes, of this reclaimed entry.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Splits the entry into two at the given index.
    ///
    /// Afterwards, `self` will have a new capacity of `len` and its original offset, and the returned `ReclaimedEntry`
    /// will have an offset of `self.offset + len` and a capacity of `self.capacity - len`.
    pub const fn split_off(&mut self, len: usize) -> Self {
        let new_offset = self.offset + len;
        let new_capacity = self.capacity - len;
        self.capacity = len;

        Self::new(new_offset, new_capacity)
    }

    /// Merges `other` into `self`, updating `self` to reflect the new, larger entry.
    pub const fn merge(&mut self, other: Self) {
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

/// A set of reclaimed entries.
#[derive(Debug)]
pub struct ReclaimedEntries(BTreeSet<ReclaimedEntry>);

impl ReclaimedEntries {
    /// Creates a new, empty `ReclaimedEntries`.
    pub fn new() -> Self {
        Self(BTreeSet::new())
    }

    /// Returns `true` if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Inserts a new entry into the reclaimed entries.
    pub fn insert(&mut self, mut current_entry: ReclaimedEntry) -> ReclaimedEntry {
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

    /// Takes the first entry in the set that matches the given predicate.
    ///
    /// If the set is empty, or no entry matches the predicate, `None` is returned.
    pub fn take_if<F>(&mut self, f: F) -> Option<ReclaimedEntry>
    where
        F: Fn(&ReclaimedEntry) -> bool,
    {
        let found_entry = self.0.iter().find(|entry| f(entry)).copied()?;
        self.0.remove(&found_entry);

        Some(found_entry)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[cfg(test)]
    pub fn first(&self) -> Option<ReclaimedEntry> {
        self.0.first().copied()
    }

    #[cfg(test)]
    pub fn iter(&self) -> impl Iterator<Item = &ReclaimedEntry> {
        self.0.iter()
    }
}

#[inline]
pub fn hash_string(s: &str) -> u64 {
    // Single global instance of the hasher state since we need a consistently-seeded state for `hash_single_fast` to
    // consistently hash things across the application.
    static BUILD_HASHER: LazyLock<foldhash::fast::RandomState> = LazyLock::new(foldhash::fast::RandomState::default);

    BUILD_HASHER.hash_one(s)
}

/// Rounds up `len` to the nearest multiple of the alignment of `T`.
pub const fn aligned<T>(len: usize) -> usize {
    let align = std::mem::align_of::<T>();
    len.wrapping_add(align).wrapping_sub(1) & !align.wrapping_sub(1)
}

/// Gets the number of bytes for the given string, rounded up to the nearest multiple of the alignment of `T`.
pub const fn aligned_string<T>(s: &str) -> usize {
    aligned::<T>(s.len())
}

/// Computes the layout for the given `size` aligned to `T`.
///
/// The resulting layout will always be well-aligned for `T`, such that the given size will be rounded up to the nearest
/// multiple of the alignment of `T`. This means that if `size` is smaller than the alignment of `T`, the resulting
/// layout will at least large enough to hold a single `T`.
pub const fn layout_for_data<T>(size: NonZeroUsize) -> Layout {
    let element_len = std::mem::size_of::<T>();
    let element_align = std::mem::align_of::<T>();
    assert!(element_len > 0, "`T` cannot be a zero-sized type");

    // Round up the capacity to the nearest multiple of the element size.
    let size = size.get().div_ceil(element_len) * element_len;

    // SAFETY: The size of the layout cannot be zero (since `capacity` is non-zero and we always divide rounding up,
    // meaning `size` will always be at least `element_len`), and alignment comes directly from `std::mem::align_of`,
    // where alignment preconditions are already upheld.
    //
    // SAFETY: The caller is responsible for ensuring that `capacity`, when rounded up to `element_align`, does not
    // overflow `isize` (i.e. less than or equal to `isize::MAX`).
    unsafe { Layout::from_size_align_unchecked(size, element_align) }
}

#[cfg(test)]
mod tests {
    use super::*;

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
