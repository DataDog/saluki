// TODO: If we're trying to add a reclaimed entry, and that entry comes immediately before any available capacity, and
// the entry is aligned... we should skip actually tracking it as a reclaimed entry and instead simply decrement `len`,
// adding the entry back to the available capacity at the end of the data buffer.
//
// This avoids specific fragmentation where the reclaimed entry might be too small for a string, requiring us to spill
// to the buffer, and then we waste space doing so when we could have potentially had a more optimal usage if the
// available capacity was simply larger.

#[cfg(not(feature = "loom"))]
use std::sync::{atomic::AtomicUsize, Arc, Mutex};
use std::{
    alloc::Layout,
    collections::BTreeSet,
    hash::Hasher as _,
    num::NonZeroUsize,
    ops::Deref,
    ptr::NonNull,
    sync::atomic::Ordering::{AcqRel, Acquire},
};

#[cfg(feature = "loom")]
use loom::sync::{atomic::AtomicUsize, Arc, Mutex};

const HEADER_LEN: usize = std::mem::size_of::<EntryHeader>();
const HEADER_ALIGN: usize = std::mem::align_of::<EntryHeader>();
const ENTRY_CAP_MASK: usize = usize::MAX << (usize::BITS / 2);
const ENTRY_CAP_SHIFT: u32 = usize::BITS / 2;
const ENTRY_LEN_MASK: usize = usize::MAX >> (usize::BITS / 2);

// In `EntryHeader`, we use the `len` field to store both the actual length of the string in the entry, as well as the
// size of the entry itself (minus the header length). We do this so that when interning into a reclaimed entry, whose
// size was large enough to have space left over, but not large enough to hold another string, we can properly track how
// large the entry is overall, allowing us to properly seek to the start of the next possible entry... rather than
// simply seek to the end of the current entry's string data (modulo padding).
//
// Effectively, we split a `usize` in half: the top half is the entry's maximum possible string length, and the bottom
// half is the actual string length.
const MAX_INTERNABLE_STRING_LENGTH: usize = ENTRY_CAP_MASK;

// When reusing a reclaimed entry that's too big for the string we're interning, we check to see if we should "split"
// it: take a portion of unneeded capacity at the end of the entry and turn it into another reclaimed entry.
//
// 32 bytes is the minimum size that such a split would need to be.. This accounts for the header itself (24 bytes) plus
// the smallest possible string capacity (8 bytes, since we always align to 8 bytes).
const MINIMUM_ENTRY_LEN: usize = HEADER_LEN + HEADER_ALIGN;

/// An interned string.
///
/// This string type is read-only, and dereferences to `&str` for ergonomic usage. It is cheap to clone (8 bytes), but
/// generally will not be interacted with directly. Instead, most usages should be wrapped in `MetaString`.
#[derive(Clone, Debug)]
pub struct InternedString {
    state: Arc<StringState>,
}

impl Deref for InternedString {
    type Target = str;

    fn deref(&self) -> &str {
        self.state.get_entry()
    }
}

impl PartialEq for InternedString {
    fn eq(&self, other: &Self) -> bool {
        (Arc::ptr_eq(&self.state.interner, &other.state.interner) && self.state.header == other.state.header)
            || self.state.get_entry() == other.state.get_entry()
    }
}

impl Eq for InternedString {}

#[derive(Debug)]
struct StringState {
    interner: Arc<Mutex<InternerShardState>>,
    header: NonNull<EntryHeader>,
}

impl StringState {
    fn get_entry<'a>(&'a self) -> &'a str {
        // NOTE: We're specifically upholding a safety variant here by tying the lifetime of the string reference to our
        // own lifetime, as the lifetime of the string reference *cannot* exceed the lifetime of the interner state,
        // which we keep alive by holding `Arc<Mutex<InternerShardState>>`.
        get_entry_string::<'a>(self.header)
    }
}

impl Drop for StringState {
    fn drop(&mut self) {
        // SAFETY: The caller that creates `StringState` is responsible for ensuring that `self.header` is well-aligned
        // and points to an initialized `EntryHeader` value.
        let header = unsafe { self.header.as_ref() };
        if header.decrement_active_refs() {
            // We decremented the reference count to zero, so try to mark this entry for reclamation.
            let mut interner = self.interner.lock().unwrap();
            interner.mark_for_reclamation(self.header);
        }
    }
}

// SAFETY: We don't take references to the entry header pointer that outlast `StringState`, and the only modification we
// do to the entry header is through atomic operations, so it's safe to both send and share `StringState` between
// threads.
unsafe impl Send for StringState {}
unsafe impl Sync for StringState {}

/// Metadata about an interner entry.
///
/// `EntryHeader` represents the smallest amount of information about an interned entry that is needed to support both
/// lookup of existing interned strings, as well as the ability to reclaim space in the interner when an entry is no
/// longer in use.
struct EntryHeader {
    /// The hash of the string that this entry represents.
    hash: u64,

    /// The number of active references to this entry.
    ///
    /// Only incremented by the interner itself, and decremented by `InternedString` when it is dropped.
    refs: AtomicUsize,

    /// Combined length/capacity of the entry, in terms of the string itself.
    ///
    /// This is a `usize` where the top half is the capacity of the entry, and the bottom half is the length of the
    /// entry, both in bytes. Since we often have extra padding bytes at the end of the entry, this allows us to track
    /// the full size of the entry separately from how long the entry's string actually is... but in a single field.
    ///
    /// Notably, this does _not_ include the length of the header itself. For example, an entry holding the string
    /// "hello, world!" has a string length of 13 bytes, but since we have to pad out to meet our alignment requirements
    /// for `EntryHeader`, we would end up with a capacity of 16 bytes. As such, `EntryHeader::len` would report `13`,
    /// while `EntryHeader::capacity` would report `16`. Likewise, `EntryHeader::entry_len` would report `40`,
    /// accounting for the string capacity (16) as well as the header length itself (24).
    ///
    /// Practically, this does mean strings can only be as long as `usize::MAX / 2` bytes, but that's a lot of bytes
    /// even on 32-bit platforms.
    len_cap: usize,
}

impl EntryHeader {
    /// Creates a tombstone entry with the given capacity.
    ///
    /// This is to allow for updating a region in the data buffer, which has been reclaimed, so that iteration over
    /// entries in the data buffer can properly skip over this entry.
    fn tombstone(capacity: usize) -> Self {
        Self {
            hash: 0,
            refs: AtomicUsize::new(0),
            len_cap: capacity << ENTRY_CAP_SHIFT,
        }
    }

    /// Creates a new `EntryHeader`.`
    fn new(hash: u64, s: &str) -> Self {
        let entry = ReclaimedEntry::new(0, Self::entry_len_for_str(s));

        Self::from_reclaimed_entry(entry, hash, s)
    }

    /// Creates a new `EntryHeader` based on the given reclaimed entry.
    ///
    /// This maps the entry header to the underlying capacity of the given reclaimed entry, which is done in cases where
    /// a reclaimed entry is being used when interning a new string, and the reclaimed entry is larger than the string
    /// being interned, but not large enough that we could split the excess capacity into a new reclaimed entry.
    fn from_reclaimed_entry(reclaimed_entry: ReclaimedEntry, hash: u64, s: &str) -> Self {
        let len_cap = (reclaimed_entry.str_capacity() << ENTRY_CAP_SHIFT) | s.len();
        Self {
            hash,
            refs: AtomicUsize::new(1),
            len_cap,
        }
    }

    const fn entry_len_for_str(s: &str) -> usize {
        let padded_s_len = aligned_str_len(s);
        HEADER_LEN + padded_s_len
    }

    /// Returns the total number of bytes that this entry takes up in the data buffer.
    const fn entry_len(&self) -> usize {
        HEADER_LEN + self.capacity()
    }

    /// Returns the size of the string, in bytes, that this entry can hold.
    const fn capacity(&self) -> usize {
        (self.len_cap & ENTRY_CAP_MASK) >> ENTRY_CAP_SHIFT
    }

    /// Returns the size of the string, in bytes, that this entry _actually_ holds.
    const fn len(&self) -> usize {
        self.len_cap & ENTRY_LEN_MASK
    }

    /// Returns `true` if this entry is currently referenced.
    fn is_active(&self) -> bool {
        self.refs.load(Acquire) != 0
    }

    /// Increments the active reference count by one.
    fn increment_active_refs(&self) {
        self.refs.fetch_add(1, AcqRel);
    }

    /// Decrements the active reference count by one.
    ///
    /// Returns `true` if the active reference count is zero _after_ calling this method.
    fn decrement_active_refs(&self) -> bool {
        self.refs.fetch_sub(1, AcqRel) == 1
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

    /// Returns the maximum string size, in bytes, that the reclaimed entry could hold.
    const fn str_capacity(&self) -> usize {
        self.capacity - HEADER_LEN
    }

    /// Returns `true` if `other` comes immediately after (contiguous) `self`.
    const fn followed_by(&self, other: &Self) -> bool {
        self.offset + self.capacity == other.offset
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
struct InternerShardState {
    // Direct pieces of our buffer allocation.
    ptr: NonNull<u8>,
    len: usize,
    capacity: NonZeroUsize,

    // Number of active entries (strings) in the interner.
    entries: usize,

    // Markers for entries that can be reused.
    reclaimed: BTreeSet<ReclaimedEntry>,
}

impl InternerShardState {
    /// Creates a new `InternerShardState` with a pre-allocated buffer that has the given capacity.
    pub fn with_capacity(capacity: NonZeroUsize) -> Self {
        assert!(
            aligned(capacity.get(), HEADER_ALIGN) <= isize::MAX as usize,
            "capacity would overflow isize::MAX, which violates layout constraints"
        );

        // Allocate our data buffer. This is the main backing allocation for all interned strings, and is well-aligned
        // for `EntryHeader`.
        //
        // SAFETY: `layout_for_data` ensures the layout is non-zero.
        let data_layout = layout_for_data(capacity);
        let data_ptr = unsafe { std::alloc::alloc(data_layout) };
        let ptr = match NonNull::new(data_ptr) {
            Some(ptr) => ptr,
            None => std::alloc::handle_alloc_error(data_layout),
        };

        Self {
            ptr,
            len: 0,
            capacity,
            entries: 0,
            reclaimed: BTreeSet::new(),
        }
    }

    /// Returns the total number of unused bytes that are available for interning.
    fn available(&self) -> usize {
        self.capacity.get() - self.len
    }

    fn get_entry_ptr(&self, offset: usize) -> NonNull<EntryHeader> {
        debug_assert!(
            offset + MINIMUM_ENTRY_LEN <= self.capacity.get(),
            "offset would point to entry that cannot possibly avoid extending past end of data buffer"
        );

        // SAFETY: The caller is responsible for ensuring that `offset` is within the bounds of the data buffer, and
        // that `offset` is well-aligned for `EntryHeader`.
        let entry_ptr = unsafe { self.ptr.as_ptr().add(offset) };
        debug_assert!(
            entry_ptr.align_offset(HEADER_ALIGN) == 0,
            "entry header pointer must be well-aligned"
        );

        // SAFETY: `entry_ptr` is derived from `self.ptr`, which itself is `NonNull<u8>`, and the caller is responsible
        // for ensuring that `offset` is within the bounds of the data buffer, so we know `entry_ptr` is non-null.
        unsafe { NonNull::new_unchecked(entry_ptr.cast::<EntryHeader>()) }
    }

    fn find_entry(&self, hash: u64, s: &str) -> Option<NonNull<EntryHeader>> {
        let mut offset = 0;

        while offset < self.len {
            // Construct a pointer to the entry at `offset`, and get a reference to the header value.
            let header_ptr = self.get_entry_ptr(offset);
            let header = unsafe { header_ptr.as_ref() };

            // See if this entry is active or not. If it's active, then we'll quickly check the hash/length of the
            // string to see if this is likely to be a match for `s`.
            if header.is_active() && header.hash == hash && header.len() == s.len() {
                // As a final check, we make sure that the entry string and `s` are equal. If they are, then we
                // have an exact match and will return the entry.
                let s_entry = get_entry_string(header_ptr);
                if s_entry == s {
                    // Increment the reference count for this entry so it's not prematurely reclaimed.
                    header.increment_active_refs();

                    return Some(header_ptr);
                }
            }

            // Either this was a reclaimed entry or we didn't have a match, so we move on to the next entry.
            offset += header.entry_len();
        }

        None
    }

    fn write_entry(&mut self, offset: usize, entry_header: EntryHeader, s: &str) -> NonNull<EntryHeader> {
        debug_assert_eq!(
            entry_header.len(),
            s.len(),
            "entry header length must match string length"
        );

        let entry_ptr = self.get_entry_ptr(offset);

        // Write the entry header.
        unsafe { entry_ptr.as_ptr().write(entry_header) };

        let s_buf = s.as_bytes();

        // Write the string.
        let entry_s_buf = unsafe {
            // Take the entry pointer and add 1, which sets our pointer to right _after_ the header.
            let entry_s_ptr = entry_ptr.as_ptr().add(1).cast::<u8>();
            std::slice::from_raw_parts_mut(entry_s_ptr, s_buf.len())
        };
        entry_s_buf.copy_from_slice(s_buf);

        // Update the number of entries we have.
        self.entries += 1;

        entry_ptr
    }

    fn write_to_unoccupied(&mut self, s_hash: u64, s: &str) -> NonNull<EntryHeader> {
        let entry_header = EntryHeader::new(s_hash, s);
        let entry_len = entry_header.entry_len();

        // Write the entry to the end of the data buffer.
        let entry_offset = self.len;
        self.len += entry_len;

        self.write_entry(entry_offset, entry_header, s)
    }

    fn write_to_reclaimed_entry(
        &mut self, mut reclaimed_entry: ReclaimedEntry, s_hash: u64, s: &str,
    ) -> NonNull<EntryHeader> {
        let entry_len = EntryHeader::entry_len_for_str(s);
        let entry_offset = reclaimed_entry.offset;

        // First, figure out if we can/should split the reclaimed entry.
        let remainder = reclaimed_entry.capacity - entry_len;
        if remainder >= MINIMUM_ENTRY_LEN {
            // We can split the reclaimed entry, so we'll update the reclaimed entry to reflect the new entry that's
            // been written, and then add the remainder as a new reclaimed entry.
            let split_reclaimed_entry = reclaimed_entry.split_off(entry_len);
            self.add_reclaimed(split_reclaimed_entry);
        }

        let entry_header = EntryHeader::from_reclaimed_entry(reclaimed_entry, s_hash, s);

        // Write the entry in place of the reclaimed entry.
        self.write_entry(entry_offset, entry_header, s)
    }

    fn add_reclaimed_from_header(&mut self, header_ptr: NonNull<EntryHeader>) {
        // Get the offset of the header within the data buffer.
        //
        // SAFETY: The caller is responsible for ensuring the entry header reference belongs to this interner. If that
        // is upheld, then we know that entry header belongs to our data buffer, and that the pointer to the entry
        // header is not less than the base pointer of the data buffer, ensuring the offset is non-negative.
        let entry_offset = unsafe {
            header_ptr
                .cast::<u8>()
                .as_ptr()
                .offset_from(self.ptr.as_ptr().cast_const())
        };
        debug_assert!(entry_offset >= 0, "entry offset must be non-negative");

        let header = unsafe { header_ptr.as_ref() };

        let reclaimed_entry = ReclaimedEntry::new(entry_offset as usize, header.entry_len());
        self.add_reclaimed(reclaimed_entry);
    }

    fn add_reclaimed(&mut self, mut current_entry: ReclaimedEntry) {
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

        for entry in self.reclaimed.iter() {
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
            self.reclaimed.remove(&prev_entry);
            current_entry.merge(prev_entry);
        }

        if let Some(next_entry) = maybe_next_entry {
            self.reclaimed.remove(&next_entry);
            current_entry.merge(next_entry);
        }

        self.reclaimed.insert(current_entry);

        // Now that we've ensured we have the biggest possible reclaimed entry after merging adjacent entries, we can
        // finally tombstone it.
        self.write_entry_tombstone(current_entry);
    }

    fn mark_for_reclamation(&mut self, header_ptr: NonNull<EntryHeader>) {
        // See if the reference count is zero.
        //
        // Only interned string values (the frontend handle that wraps the pointer to a specific entry) can decrement
        // the reference count for their specific entry when dropped, and only `InternerShardState` -- with its access
        // mediated through a mutex -- can increment the reference count for entries. This means that if the reference
        // count is zero, then we know that nobody else is holding a reference to this entry, and no concurrent call to
        // `try_intern` could be updating the reference count, either... so it's safe to be marked as reclaimed.
        //
        // SAFETY: The caller is responsible for ensuring that `header_ptr` is well-aligned and points to an initialized
        // `EntryHeader` value.
        let header = unsafe { header_ptr.as_ref() };
        if !header.is_active() {
            self.entries -= 1;
            self.add_reclaimed_from_header(header_ptr);
        }
    }

    fn write_entry_tombstone(&mut self, reclaimed_entry: ReclaimedEntry) {
        let entry_ptr = self.get_entry_ptr(reclaimed_entry.offset);

        // Write the entry tombstone itself.
        //
        // This zeroes out the hash, active references, and the string _length_, but leaves the string _capacity_, which
        // is required for proper iteration in `find_entry`.
        //
        // SAFETY: We know that `entry_ptr` is valid for writes (reclaimed entries are, by definition, inactive regions
        // in the data buffer) and is well-aligned for `EntryHeader`.
        unsafe {
            entry_ptr
                .as_ptr()
                .write(EntryHeader::tombstone(reclaimed_entry.str_capacity()));
        }

        // Write a magic value to the entire string capacity for the entry. This ensures that there's a known repeating
        // value which, in the case of debugging issues, can be a signal that offsets/reclaimed entries are incorrect
        // and overlapping with active entries.
        //
        // SAFETY: Like above, the caller is responsible for ensuring that `offset` is within the bounds of the data
        // buffer, and that `offset + capacity` does not extend past the bounds of the data buffer.
        unsafe {
            // Take the entry pointer and add 1, which sets our pointer to right _after_ the header.
            let str_ptr = entry_ptr.as_ptr().add(1).cast::<u8>();
            let str_buf = std::slice::from_raw_parts_mut(str_ptr, reclaimed_entry.str_capacity());
            str_buf.fill(0x25);
        }
    }

    fn try_intern(&mut self, s_hash: u64, s: &str) -> Option<NonNull<EntryHeader>> {
        // We can only intern strings with a size of `MAX_INTERNABLE_STRING_LENGTH` bytes or less. See the description
        // of the constant for details.
        if s.len() > MAX_INTERNABLE_STRING_LENGTH {
            return None;
        }

        // Try and find an existing entry for this string.
        if self.entries != 0 {
            if let Some(existing_entry) = self.find_entry(s_hash, s) {
                return Some(existing_entry);
            }
        }

        let entry_len = EntryHeader::entry_len_for_str(s);

        // We didn't find an existing entry, so we're going to intern it.
        //
        // First, try and see if we have a reclaimed entry that can fit this string and is aligned. If nothing suitable
        // is found, then we'll just try to fit it in the remaining capacity of our data buffer.
        if !self.reclaimed.is_empty() {
            let maybe_reclaimed_entry = self.reclaimed.iter().find(|re| re.capacity >= entry_len).copied();
            if let Some(reclaimed_entry) = maybe_reclaimed_entry {
                // We found a suitable reclaimed entry, so remove it from the list and then write into it.
                self.reclaimed.remove(&reclaimed_entry);

                return Some(self.write_to_reclaimed_entry(reclaimed_entry, s_hash, s));
            }
        }

        // We couldn't find a large enough reclaimed entry, or we had none, so see if we can fit this string within the
        // available capacity of our data buffer.
        if entry_len <= self.available() {
            Some(self.write_to_unoccupied(s_hash, s))
        } else {
            None
        }
    }
}

impl Drop for InternerShardState {
    fn drop(&mut self) {
        // SAFETY: We allocated this buffer with the global allocator, and we're generating the same layout for it as
        // `InternerShardState::new` using `layout_for_data`.
        unsafe {
            std::alloc::dealloc(self.ptr.as_ptr(), layout_for_data(self.capacity));
        }
    }
}

// SAFETY: We don't take references to the data buffer pointer that outlast `InternerShardState`, and all access to
// `InternerShardState` itself is mediated through a mutex, so we're safe to send it around and share it between
// threads.
unsafe impl Send for InternerShardState {}
unsafe impl Sync for InternerShardState {}

#[derive(Debug)]
struct InternerState<const SHARD_FACTOR: usize> {
    shards: [Arc<Mutex<InternerShardState>>; SHARD_FACTOR],
    capacity: usize,
}

impl<const SHARD_FACTOR: usize> InternerState<SHARD_FACTOR> {
    // Ensure our shard factor is a power of two at compile, so that we can just use a mask to get the shard index.
    const _POWER_OF_TWO_SHARD_FACTOR: () = {
        if !SHARD_FACTOR.is_power_of_two() {
            panic!("shard factor must be a power of two")
        }
    };

    pub fn with_capacity(capacity: NonZeroUsize) -> Self {
        let shard_capacity = match NonZeroUsize::new(capacity.get() / SHARD_FACTOR) {
            Some(shard_capacity) => shard_capacity,
            // If we can't divide the capacity evenly, we just specify a capacity of one which will force every shard to
            // upsize the capacity so that a single entry can fit, and satisfies the need for `NonZeroUsize`.
            //
            // SAFETY: One is obviously not zero.
            None => unsafe { NonZeroUsize::new_unchecked(1) },
        };

        let shards = std::iter::repeat_with(|| Arc::new(Mutex::new(InternerShardState::with_capacity(shard_capacity))))
            .take(SHARD_FACTOR)
            .collect::<Vec<_>>();
        let capacity = shards.iter().map(|shard| shard.lock().unwrap().capacity.get()).sum();
        let shards: [Arc<Mutex<InternerShardState>>; SHARD_FACTOR] =
            shards.try_into().expect("should not fail to convert to array");

        Self { shards, capacity }
    }

    fn is_empty(&self) -> bool {
        self.shards.iter().any(|shard| shard.lock().unwrap().entries != 0)
    }

    fn len(&self) -> usize {
        self.shards.iter().map(|shard| shard.lock().unwrap().entries).sum()
    }

    fn len_bytes(&self) -> usize {
        self.shards.iter().map(|shard| shard.lock().unwrap().len).sum()
    }

    fn capacity_bytes(&self) -> usize {
        self.capacity
    }

    fn try_intern(&self, s: &str) -> Option<InternedString> {
        let hash = hash_string(s);
        let shard_idx = (hash as usize) & (SHARD_FACTOR - 1);

        let shard = &self.shards[shard_idx];
        intern_with_shard_and_hash(shard, hash, s)
    }
}

/// A string interner based on a single, fixed-size backing buffer with support for reclamation.
///
/// ## Overview
///
/// This interner uses a single, fixed-size backing buffer where interned strings are stored contiguously. This provides
/// bounded memory usage, and the interner will not allocate additional memory for new strings once the buffer is full.
/// Since interned strings are not likely to need to live for the life of the program, the interner supports
/// reclamation. Once all references to an interned string have been dropped, the storage for that string is reclaimed
/// and can be used to hold new strings.
///
/// ## Storage layout
///
/// The backing buffer stores strings contiguously, with an entry "header" prepended to each string. The header contains
/// relevant data -- hash of the string, reference count, and length of the string -- needed to work with the entry
/// either when searching for existing entries or when using the entry itself.
///
/// The layout of an entry is as follows:
///
/// ```text
/// ┌───────────────────────── entry #1 ──────────────────────────┐ ┌─ entry #2 ─┐ ┌─ entry .. ─┐
/// ▼                                                             ▼ ▼            ▼ ▼            ▼
/// ┏━━━━━━━━━━━┯━━━━━━━━━━━┯━━━━━━━━━━━┯━━━━━━━━━━━┯━━━━━━━━━━━━━┓ ┏━━━━━━━━━━━━┓ ┏━━━━━━━━━━━━┓
/// ┃ str hash  │  ref cnt  │  str len  │ str data  │   padding   ┃ ┃   header   ┃ ┃   header   ┃
/// ┃ (8 bytes) │ (8 bytes) │ (8 bytes) │ (N bytes) │ (1-7 bytes) ┃ ┃  & string  ┃ ┃  & string  ┃
/// ┗━━━━━━━━━━━┷━━━━━━━━━━━┷━━━━━━━━━━━┷━━━━━━━━━━━┷━━━━━━━━━━━━━┛ ┗━━━━━━━━━━━━┛ ┗━━━━━━━━━━━━┛
/// ▲                                   ▲                           ▲
/// └────────── `EntryHeader` ──────────┘                           └── aligned for `EntryHeader`
///          (8 byte alignment)                                         via trailing padding    
/// ```
///
/// The backing buffer is always aligned properly for `EntryHeader`, so that the first entry can be referenced
/// correctly. However, when appending additional entries to the buffer, we need to ensure that those entries also have
/// an aligned start for accessing the header. This is complicated due to the variable number of bytes for the string
/// data.
///
/// Alignment padding is added to the end of the entry to ensure that when appending the next entry, the start of the
/// entry is properly aligned for `EntryHeader`. In the worst case, up to 7 bytes could be added (and thus wasted) on
/// this alignment padding.
///
/// ## `InternedString`
///
/// The `InternedString` type is a handle to the entry header, and thus the string data, for an interned string. It is
/// designed to be small -- 8 bytes! -- and cheap to clone, as it contains an atomic reference to the entry header and a
/// reference to the interner that owns the string. It dereferences to the underlying string with relatively low
/// overhead: two pointer indirections.
///
/// When an `InternedString` is dropped, it decrements the reference count for the entry it points to. If the reference
/// count drops to zero, it will attempt to mark the entry for reclamation.
///
/// ## Reclamation
///
/// As we want to bound the memory used by the interner, but also not allow it to be filled up with strings that
/// eventually end up going entirely unused, we need a way to remove those unused strings so their underlying storage
/// can be used for new strings. This is where reclamation comes in.
///
/// When a string is interned, the entry header tracks how many active references there are to it. When that reference
/// count drops to zero, the last reference to the string attempts to mark the entry for reclamation. Assuming no other
/// reference has been taken out on the entry in the meantime, the entry gets added to a list of "reclaimed" entries.
///
/// Reclaimed entries are simple markers -- start and end position in the data buffer -- which are stored in a freelist.
/// When attempting to intern a new string, this freelist is searched to see if there's an entry large enough to fit the
/// new string, and if so, it is used.
///
/// Additionally, when entries are reclaimed, adjacent entries are merged together where possible. This helps to avoid
/// unnecessary fragmentation over time, although not as effectively as reconstructing the data buffer to re-pack
/// entries.
#[derive(Clone, Debug)]
pub struct FixedSizeInterner<const SHARD_FACTOR: usize> {
    state: Arc<InternerState<SHARD_FACTOR>>,
}

impl<const SHARD_FACTOR: usize> FixedSizeInterner<SHARD_FACTOR> {
    /// Creates a new `FixedSizeInterner` with the given capacity.
    ///
    /// The given capacity will potentially be rounded up by a small number of bytes (up to 7) in order to ensure the
    /// backing buffer is properly aligned.
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            state: Arc::new(InternerState::with_capacity(capacity)),
        }
    }

    /// Returns `true` if the interner contains no strings.
    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }

    /// Returns the number of strings in the interner.
    pub fn len(&self) -> usize {
        self.state.len()
    }

    /// Returns the total number of bytes in the interner.
    pub fn len_bytes(&self) -> usize {
        self.state.len_bytes()
    }

    /// Returns the total number of bytes the interner can hold.
    pub fn capacity_bytes(&self) -> usize {
        self.state.capacity_bytes()
    }

    /// Tries to intern the given string.
    ///
    /// If the intern is at capacity and the given string cannot fit, `None` is returned. Otherwise, `Some` is
    /// returned with a reference to the interned string.
    pub fn try_intern(&self, s: &str) -> Option<InternedString> {
        self.state.try_intern(s)
    }
}

fn hash_string(s: &str) -> u64 {
    let mut hasher = ahash::AHasher::default();
    hasher.write(s.as_bytes());
    hasher.finish()
}

/// Rounds up `len` to the nearest multiple of `align`.
const fn aligned(len: usize, align: usize) -> usize {
    len.wrapping_add(align).wrapping_sub(1) & !align.wrapping_sub(1)
}

/// Gets the number of bytes for the given string, rounded up to the nearest multiple of `HEADER_ALIGN`.
const fn aligned_str_len(s: &str) -> usize {
    aligned(s.len(), HEADER_ALIGN)
}

const fn layout_for_data(capacity: NonZeroUsize) -> Layout {
    // Round up the capacity to the nearest multiple of the header size.
    let size = capacity.get().div_ceil(HEADER_LEN) * HEADER_LEN;

    // SAFETY: The size of the layout cannot be zero (since `capacity` is non-zero and we always divide rounding up,
    // meaning `size` will always be atleast `HEADER_LEN`), and alignment comes directly from `std::mem::align_of`,
    // where alignment preconditions are already upheld.
    //
    // SAFETY: The caller is responsible for ensuring that `capacity`, when rounded up to `HEADER_ALIGN`, does not
    // overflow `isize` (i.e. less than or equal to `isize::MAX`).
    unsafe { Layout::from_size_align_unchecked(size, HEADER_ALIGN) }
}

fn get_entry_string<'a>(header_ptr: NonNull<EntryHeader>) -> &'a str {
    // SAFETY: The caller is responsible for ensuring that `header_ptr` is well-aligned and points to an initialized
    // `EntryHeader` value.
    let header = unsafe { header_ptr.as_ref() };

    // Advance past the header and get a reference to the string.
    //
    // SAFETY: The caller is responsible for ensuring that `header_ptr` can be advanced by the length of the entry
    // header _and_ the length of the string, without exceeding the bounds of the data buffer.
    //
    // SAFETY: The string pointer is derived from `NonNull<EntryHeader>`, which is guaranteed to be non-null, and the
    // alignment required for `u8` is one, so the pointer is always well-aligned.
    //
    // SAFETY: Entries are only ever written from string references, which are valid UTF-8, so we know that the string
    // data is safe to coerce directly to `&str`.
    unsafe {
        let s_ptr = header_ptr.as_ptr().add(1).cast::<u8>();
        let s_buf = std::slice::from_raw_parts(s_ptr, header.len());
        std::str::from_utf8_unchecked(s_buf)
    }
}

fn intern_with_shard_and_hash(shard: &Arc<Mutex<InternerShardState>>, hash: u64, s: &str) -> Option<InternedString> {
    let header = {
        let mut shard = shard.lock().unwrap();
        shard.try_intern(hash, s)?
    };

    Some(InternedString {
        state: Arc::new(StringState {
            interner: Arc::clone(shard),
            header,
        }),
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, ops::RangeInclusive};

    use prop::sample::Index;
    use proptest::{
        collection::{hash_set, vec as arb_vec},
        prelude::*,
    };

    use super::*;

    fn create_shard(capacity: NonZeroUsize) -> Arc<Mutex<InternerShardState>> {
        Arc::new(Mutex::new(InternerShardState::with_capacity(capacity)))
    }

    fn intern_for_shard(shard: &Arc<Mutex<InternerShardState>>, s: &str) -> Option<InternedString> {
        let hash = hash_string(s);
        intern_with_shard_and_hash(shard, hash, s)
    }

    fn shard_capacity(shard: &Arc<Mutex<InternerShardState>>) -> usize {
        shard.lock().unwrap().capacity.get()
    }

    fn shard_available(shard: &Arc<Mutex<InternerShardState>>) -> usize {
        shard.lock().unwrap().available()
    }

    fn shard_entries(shard: &Arc<Mutex<InternerShardState>>) -> usize {
        shard.lock().unwrap().entries
    }

    fn shard_reclaimed_len(shard: &Arc<Mutex<InternerShardState>>) -> usize {
        shard.lock().unwrap().reclaimed.len()
    }

    fn shard_first_reclaimed_entry(shard: &Arc<Mutex<InternerShardState>>) -> ReclaimedEntry {
        shard.lock().unwrap().reclaimed.first().copied().unwrap()
    }

    fn entry_len(s: &str) -> usize {
        EntryHeader::entry_len_for_str(s)
    }

    fn get_reclaimed_entry_for_string(s: &InternedString) -> ReclaimedEntry {
        let ptr = s.state.interner.lock().unwrap().ptr.as_ptr();
        let header = unsafe { s.state.header.as_ref() };
        let offset = unsafe { s.state.header.as_ptr().cast::<u8>().offset_from(ptr) as usize };
        ReclaimedEntry::new(offset, header.entry_len())
    }

    fn arb_alphanum_strings(
        str_len: RangeInclusive<usize>, unique_strs: RangeInclusive<usize>,
    ) -> impl Strategy<Value = Vec<String>> {
        // Create characters between 0x20 (32) and 0x7E (126), which are all printable ASCII characters.
        let char_gen = any::<u8>().prop_map(|c| std::cmp::max(c % 127, 32));

        let str_gen = any::<usize>()
            .prop_map(move |n| std::cmp::max(n % *str_len.end(), *str_len.start()))
            .prop_flat_map(move |len| arb_vec(char_gen.clone(), len))
            // SAFETY: We know our characters are all valid UTF-8 because they're in the ASCII range.
            .prop_map(|xs| unsafe { String::from_utf8_unchecked(xs) });

        // Create a hash set, which handles the deduplication aspect for us, ensuring we have N unique strings where N
        // is within the `unique_strs` range... and then convert it to `Vec<String>` for easier consumption.
        hash_set(str_gen, unique_strs).prop_map(|unique_strs| unique_strs.into_iter().collect::<Vec<_>>())
    }

    #[test]
    fn size_of_interned_string() {
        // We're asserting that `InternedString` itself is 8 bytes: the size of `Arc<T>`.
        //
        // While we still end up doing pointer indirection to actually _load_ the underlying string, making
        // `InternedString` super lightweight is important to performance.
        assert_eq!(std::mem::size_of::<InternedString>(), 8);
    }

    #[test]
    fn basic() {
        let interner = FixedSizeInterner::<1>::new(NonZeroUsize::new(1024).unwrap());

        let s1 = interner.try_intern("hello").unwrap();
        let s2 = interner.try_intern("world").unwrap();
        let s3 = interner.try_intern("hello").unwrap();

        assert_eq!(s1.deref(), "hello");
        assert_eq!(s2.deref(), "world");
        assert_eq!(s3.deref(), "hello");

        // The pointers from the interned strings should be the same, but not between the interned string and a pointer
        // to an equivalent (but not interned) string:
        assert!(std::ptr::eq(s1.deref() as *const _, s3.deref() as *const _));

        let local_hello = "hello";
        assert!(!std::ptr::eq(s1.deref() as *const _, local_hello as *const _));
    }

    #[test]
    fn try_intern_without_capacity() {
        // Big enough to fit a single "hello world!" string, but not big enough to fit two.
        let interner = FixedSizeInterner::<1>::new(NonZeroUsize::new(64).unwrap());

        let s1 = interner.try_intern("hello world!");
        assert!(s1.is_some());

        let s2 = interner.try_intern("hello, world");
        assert!(s2.is_none());
    }

    #[test]
    fn reclaim_after_dropped() {
        let shard = create_shard(NonZeroUsize::new(1024).unwrap());

        let s1 = intern_for_shard(&shard, "hello").expect("should not fail to intern");
        let s1_reclaimed_expected = get_reclaimed_entry_for_string(&s1);

        assert_eq!(shard_entries(&shard), 1);
        assert_eq!(shard_reclaimed_len(&shard), 0);

        // Drop the interned string, which should decrement the reference count to zero and then reclaim the entry.
        drop(s1);

        assert_eq!(shard_entries(&shard), 0);
        assert_eq!(shard_reclaimed_len(&shard), 1);

        let s1_reclaimed = shard_first_reclaimed_entry(&shard);
        assert_eq!(s1_reclaimed_expected, s1_reclaimed);
    }

    #[test]
    fn interns_to_reclaimed_entry_with_leftover() {
        // We want to intern a string initially, which takes up almost all of the capacity, and then drop it so it gets
        // reclaimed. After that, we'll intern a much smaller string which should lead to utilizing that reclaimed
        // entry, but only a part of it. Finally, we'll intern another new string.
        //
        // The point is to demonstrate that our reclamation logic is sound in terms of allowing reclaimed entries to be
        // split while the search/insertion logic is operating.
        let shard_capacity = NonZeroUsize::new(256).unwrap();
        let shard = create_shard(shard_capacity);

        // We craft four strings such that the first two (`s_large` and `s_medium1`) will take up enough capacity that
        // `s_small` can't possibly be interned in the available capacity. We'll also craft `s_medium2` so it can fit
        // within the reclaimed entry for `s_large` but takes enough capacity that `s_small` cannot fit in the leftover
        // reclaimed entry that we split off.
        let s_large = "99 bottles of beer on the wall, 99 bottles of beer! take one down, pass it around, 98 bottles of beer on the wall!";
        let s_medium1 = "no act of kindness, no matter how small, is ever wasted";
        let s_medium2 = "if you want to go fast, go alone; if you want to go far, go together";
        let s_small = "are you there god? it's me, margaret";

        let phase1_available_capacity = shard_capacity.get() - entry_len(s_large) - entry_len(s_medium1);
        assert!(phase1_available_capacity < entry_len(s_small));
        assert!((entry_len(s_large) - entry_len(s_medium2)) < entry_len(s_small));
        assert!(entry_len(s_medium2) < entry_len(s_large));

        // Phase 1: intern our two larger strings.
        let s1 = intern_for_shard(&shard, s_large).expect("should not fail to intern");
        let s2 = intern_for_shard(&shard, s_medium1).expect("should not fail to intern");

        assert_eq!(shard_entries(&shard), 2);
        assert_eq!(shard_available(&shard), phase1_available_capacity);
        assert_eq!(shard_reclaimed_len(&shard), 0);

        // Phase 2: drop `s_large` so it gets reclaimed.
        drop(s1);
        assert_eq!(shard_entries(&shard), 1);
        assert_eq!(shard_reclaimed_len(&shard), 1);

        // Phase 3: intern `s_medium2`, which should fit in the reclaimed entry for `s_large`. This should leave a
        // small, split off reclaimed entry.
        let s3 = intern_for_shard(&shard, s_medium2).expect("should not fail to intern");

        assert_eq!(shard_entries(&shard), 2);
        assert_eq!(shard_reclaimed_len(&shard), 1);

        // Phase 4: intern `s_small`, which should not fit in the leftover reclaimed entry from `s_large` _or_ the
        // available capacity.
        let s4 = intern_for_shard(&shard, s_small);
        assert_eq!(s4, None);

        assert_eq!(shard_entries(&shard), 2);
        assert_eq!(shard_reclaimed_len(&shard), 1);

        // And make sure we can still dereference the interned strings we _do_ have left:
        assert_eq!(s2.deref(), s_medium1);
        assert_eq!(s3.deref(), s_medium2);
    }

    #[test]
    fn has_reclaimed_entries_string_fits_exactly() {
        // The shard is large enough for one "hello world!"/"hello, world", but not two of them.
        let shard = create_shard(NonZeroUsize::new(64).unwrap());

        // Intern the first string, which should fit without issue.
        let s1 = intern_for_shard(&shard, "hello world!").expect("should not fail to intern");
        let s1_reclaimed_expected = get_reclaimed_entry_for_string(&s1);

        assert_eq!(shard_entries(&shard), 1);
        assert_eq!(shard_reclaimed_len(&shard), 0);

        // Try to intern the second string, which should fail as we don't have the space.
        let s2 = intern_for_shard(&shard, "hello, world");
        assert_eq!(s2, None);

        // Drop the first string, which should decrement the reference count to zero and then reclaim the entry.
        drop(s1);

        assert_eq!(shard_entries(&shard), 0);
        assert_eq!(shard_reclaimed_len(&shard), 1);

        let s1_reclaimed = shard_first_reclaimed_entry(&shard);
        assert_eq!(s1_reclaimed_expected, s1_reclaimed);

        // Try again to intern the second string, which should now succeed and take over the reclaimed entry entirely
        // as the strings are identical in length.
        let _s2 = intern_for_shard(&shard, "hello, world").expect("should not fail to intern");

        assert_eq!(shard_entries(&shard), 1);
        assert_eq!(shard_reclaimed_len(&shard), 0);
    }

    #[test]
    fn reclaimed_entry_reuse_split_too_small() {
        // Create a shard that's big enough to fit either string individually, but not at the same time.
        let shard = create_shard(NonZeroUsize::new(72).unwrap());

        // Declare our strings to intern and just check some preconditions by hand.
        let s_one = "a horse, a horse, my kingdom for a horse!";
        let s_one_entry_len = entry_len(s_one);
        let s_two = "hello there, world!";
        let s_two_entry_len = entry_len(s_two);

        assert!(s_one_entry_len <= shard_capacity(&shard));
        assert!(s_two_entry_len <= shard_capacity(&shard));
        assert!(s_one_entry_len + s_two_entry_len > shard_capacity(&shard));
        assert!(s_one_entry_len > s_two_entry_len && (s_one_entry_len - s_two_entry_len) < MINIMUM_ENTRY_LEN);

        // Intern the first string, which should fit without issue.
        let s1 = intern_for_shard(&shard, s_one).expect("should not fail to intern");
        let s1_reclaimed_expected = get_reclaimed_entry_for_string(&s1);

        assert_eq!(shard_entries(&shard), 1);
        assert_eq!(shard_reclaimed_len(&shard), 0);

        // Try to intern the second string, which should fail as we don't have the space.
        let s2 = intern_for_shard(&shard, s_two);
        assert_eq!(s2, None);

        // Drop the first string, which should decrement the reference count to zero and then reclaim the entry.
        drop(s1);

        assert_eq!(shard_entries(&shard), 0);
        assert_eq!(shard_reclaimed_len(&shard), 1);

        let s1_reclaimed = shard_first_reclaimed_entry(&shard);
        assert_eq!(s1_reclaimed_expected, s1_reclaimed);

        // Try again to intern the second string, which should now succeed and take over the reclaimed entry, but since
        // the remainder of the reclaimed entry after taking the necessary capacity for `s_two` is not large enough
        // (`MINIMUM_ENTRY_LEN`), we shouldn't end up splitting the reclaimed entry, and instead, `s2` should consume
        // the entire reclaimed entry.
        let s2 = intern_for_shard(&shard, s_two).expect("should not fail to intern");
        let s2_reclaimed_expected = get_reclaimed_entry_for_string(&s2);

        assert_eq!(shard_entries(&shard), 1);
        assert_eq!(shard_reclaimed_len(&shard), 0);
        assert_eq!(s1_reclaimed_expected, s2_reclaimed_expected);
    }

    #[test]
    fn reclamation_merges_previous_and_current() {
        // NOTE: The drop order matters here, because what we're testing is that the reclamation logic can properly
        // merge adjacent entries regardless of whether we're merging with an existing reclaimed entry that comes before
        // us _or_ after us (or both!).

        let shard = create_shard(NonZeroUsize::new(1024).unwrap());

        // Intern two strings, back-to-back, which should fit without issue.
        let s1 = intern_for_shard(&shard, "hello there, world!").expect("should not fail to intern");
        let s1_reclaimed_expected = get_reclaimed_entry_for_string(&s1);

        let s2 = intern_for_shard(&shard, "tally ho, chaps!").expect("should not fail to intern");
        let s2_reclaimed_expected = get_reclaimed_entry_for_string(&s2);

        assert_eq!(shard_entries(&shard), 2);
        assert_eq!(shard_reclaimed_len(&shard), 0);

        // Drop the both strings, which should lead to both entries being reclaimed and their entries merged as they're adjacent.
        drop(s1);
        drop(s2);

        assert_eq!(shard_entries(&shard), 0);
        assert_eq!(shard_reclaimed_len(&shard), 1);

        // The reclaimed entry should be the sum of the reclaimed entries for both `s1` and `s2`.
        let merged_reclaimed = shard_first_reclaimed_entry(&shard);
        let mut expected_reclaimed = s1_reclaimed_expected;
        expected_reclaimed.merge(s2_reclaimed_expected);

        assert_eq!(expected_reclaimed, merged_reclaimed);
    }

    #[test]
    fn reclamation_merges_current_and_subsequent() {
        // NOTE: The drop order matters here, because what we're testing is that the reclamation logic can properly
        // merge adjacent entries regardless of whether we're merging with an existing reclaimed entry that comes before
        // us _or_ after us (or both!).

        let shard = create_shard(NonZeroUsize::new(1024).unwrap());

        // Intern two strings, back-to-back, which should fit without issue.
        let s1 = intern_for_shard(&shard, "hello there, world!").expect("should not fail to intern");
        let s1_reclaimed_expected = get_reclaimed_entry_for_string(&s1);

        let s2 = intern_for_shard(&shard, "tally ho, chaps!").expect("should not fail to intern");
        let s2_reclaimed_expected = get_reclaimed_entry_for_string(&s2);

        assert_eq!(shard_entries(&shard), 2);
        assert_eq!(shard_reclaimed_len(&shard), 0);

        // Drop the both strings, which should lead to both entries being reclaimed and their entries merged as they're adjacent.
        drop(s2);
        drop(s1);

        assert_eq!(shard_entries(&shard), 0);
        assert_eq!(shard_reclaimed_len(&shard), 1);

        // The reclaimed entry should be the sum of the reclaimed entries for both `s1` and `s2`.
        let merged_reclaimed = shard_first_reclaimed_entry(&shard);
        let mut expected_reclaimed = s1_reclaimed_expected;
        expected_reclaimed.merge(s2_reclaimed_expected);

        assert_eq!(expected_reclaimed, merged_reclaimed);
    }

    #[test]
    fn reclamation_merges_previous_and_current_and_subsequent() {
        // NOTE: The drop order matters here, because what we're testing is that the reclamation logic can properly
        // merge adjacent entries regardless of whether we're merging with an existing reclaimed entry that comes before
        // us _or_ after us (or both!).

        let shard = create_shard(NonZeroUsize::new(1024).unwrap());

        // Intern three strings, back-to-back-to-back, which should fit without issue.
        let s1 = intern_for_shard(&shard, "hello there, world!").expect("should not fail to intern");
        let s1_reclaimed_expected = get_reclaimed_entry_for_string(&s1);

        let s2 = intern_for_shard(&shard, "tally ho, chaps!").expect("should not fail to intern");
        let s2_reclaimed_expected = get_reclaimed_entry_for_string(&s2);

        let s3 = intern_for_shard(&shard, "onward and upward!").expect("should not fail to intern");
        let s3_reclaimed_expected = get_reclaimed_entry_for_string(&s3);

        assert_eq!(shard_entries(&shard), 3);
        assert_eq!(shard_reclaimed_len(&shard), 0);

        // Drop the first and last strings, which should not be merged as they're not adjacent.
        drop(s1);
        drop(s3);

        assert_eq!(shard_entries(&shard), 1);
        assert_eq!(shard_reclaimed_len(&shard), 2);

        // Now drop the second string, which is adjacent to both the first and third strings, and should result in a
        // single merged reclamined entry.
        drop(s2);

        assert_eq!(shard_entries(&shard), 0);
        assert_eq!(shard_reclaimed_len(&shard), 1);

        // The reclaimed entry should be the sum of the reclaimed entries for `s1`, `s2`, and `s3`.
        let merged_reclaimed = shard_first_reclaimed_entry(&shard);

        let mut expected_reclaimed = s1_reclaimed_expected;
        expected_reclaimed.merge(s2_reclaimed_expected);
        expected_reclaimed.merge(s3_reclaimed_expected);

        assert_eq!(expected_reclaimed, merged_reclaimed);
    }

    proptest! {
        #[test]
        #[cfg_attr(miri, ignore)]
        fn property_test_entry_count_accurate(
            strs in arb_alphanum_strings(1..=128, 16..=512),
            indices in arb_vec(any::<Index>(), 1..=1000),
        ) {
            // We ask `proptest` to generate a bunch of unique strings of varying lengths (1-128 bytes, 16-512 unique
            // strings) which we then randomly select out of those strings which strings we want to intern. The goal
            // here is to potentially select the same string multiple times, to exercise the actual interning logic...
            // but practically, to ensure that when we intern a string that has already been interned, we're not
            // incrementing the entries count again.

            // Create an interner with enough capacity to hold all of the strings we've generated. This is the maximum
            // string size multiplied by the number of strings we've generated... plus a little constant factor per
            // string to account for the entry header.
            const ENTRY_SIZE: usize = 128 + HEADER_LEN;
            let interner = FixedSizeInterner::<1>::new(NonZeroUsize::new(ENTRY_SIZE * indices.len()).unwrap());

            // For each index, pull out the string and both track it in `unique_strs` and intern it. We hold on to the
            // interned string handle to make sure the interned string is actually kept alive, keeping the entry count
            // stable.
            let mut interned = Vec::new();
            let mut unique_strs = HashSet::new();
            for index in &indices {
                let s = index.get(&strs);
                unique_strs.insert(s);

                let s_interned = interner.try_intern(s).expect("should never fail to intern");
                interned.push(s_interned);
            }

            assert_eq!(unique_strs.len(), interner.len());
        }
    }

    #[cfg(feature = "loom")]
    #[test]
    fn concurrent_drop_and_intern() {
        fn shard_reclaimed_entries(shard: &Arc<Mutex<InternerShardState>>) -> Vec<ReclaimedEntry> {
            shard.lock().unwrap().reclaimed.iter().copied().collect()
        }

        fn do_reclaimed_entries_overlap(a: ReclaimedEntry, b: ReclaimedEntry) -> bool {
            let a_start = a.offset;
            let a_end = a.offset + a.capacity - 1;

            let b_start = b.offset;
            let b_end = b.offset + b.capacity - 1;

            (a_start <= b_start && b_start <= a_end) || (a_start <= b_end && b_end <= a_end)
        }

        const STRING_TO_INTERN: &str = "hello, world!";

        // This test is meant to explore the thread orderings when one thread is trying to drop (and thus reclaim) the
        // last active reference to an interned string, and another thread is trying to intern that very same string.
        //
        // We accept, as a caveat, that a possible outcome is that we intern the "new" string again, even though an
        // existing entry to that string may have existed in an alternative ordering.
        loom::model(|| {
            let shard = create_shard(NonZeroUsize::new(1024).unwrap());
            let t2_shard = Arc::clone(&shard);

            // Intern the string from thread T1.
            let t1_interned_s = intern_for_shard(&shard, STRING_TO_INTERN).expect("should not fail to intern");
            assert_eq!(t1_interned_s.deref(), STRING_TO_INTERN);
            let t1_reclaimed_entry = get_reclaimed_entry_for_string(&t1_interned_s);

            // Spawn thread T2, which tries to intern the same string and returns the handle to it.
            let t2_result = loom::thread::spawn(move || {
                let interned_s = intern_for_shard(&t2_shard, STRING_TO_INTERN).expect("should not fail to intern");
                let reclaimed_entry = get_reclaimed_entry_for_string(&interned_s);

                (interned_s, reclaimed_entry)
            });

            drop(t1_interned_s);

            let (t2_interned_s, t2_reclaimed_entry) = t2_result.join().expect("should not fail to join T2");
            assert_eq!(t2_interned_s.deref(), STRING_TO_INTERN);

            // What we're checking for here is that either:
            // - there's no reclaimed entries (T2 found the existing entry for the string before T1 dropped it)
            // - there's a reclaimed entry (T2 didn't find the existing entry for the string before T1 marked it as
            //   inactive) but the reclaimed entry does _not_ overlap with the interned string from T2, meaning we
            //   didn't get confused and allow T2 to use an existing entry that T1 then later marked as reclaimed
            let reclaimed_entries = shard_reclaimed_entries(&shard);
            assert!(reclaimed_entries.len() <= 1, "should have at most one reclaimed entry");

            if !reclaimed_entries.is_empty() {
                // If we do have a reclaimed entry, it needs to match exactly with only one of the interned strings.
                let is_t1_entry = reclaimed_entries.first().unwrap() == &t1_reclaimed_entry;
                let is_t2_entry = reclaimed_entries.first().unwrap() == &t2_reclaimed_entry;

                assert!(
                    (is_t1_entry || is_t2_entry) && !(is_t1_entry && is_t2_entry),
                    "should only match one interned string"
                );

                // Additionally, we ensure that the reclaimed entry does not overlap with the other interned string.
                assert!(
                    !do_reclaimed_entries_overlap(t1_reclaimed_entry, t2_reclaimed_entry),
                    "reclaimed entry should not overlap with remaining interned string"
                );
            }
        });
    }
}
