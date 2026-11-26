#[cfg(not(feature = "loom"))]
use std::sync::{
    atomic::{
        AtomicUsize,
        Ordering::{AcqRel, Acquire, Release},
    },
    Arc, Mutex, MutexGuard,
};
use std::{alloc::Layout, collections::HashMap, num::NonZeroUsize, ptr::NonNull};

#[cfg(feature = "loom")]
use loom::sync::{
    atomic::{
        AtomicUsize,
        Ordering::{AcqRel, Acquire, Release},
    },
    Arc, Mutex, MutexGuard,
};

use super::{
    helpers::{aligned_string, hash_string, PackedLengthCapacity, ReclaimedEntries, ReclaimedEntry},
    InternedString, Interner,
};

const HEADER_LEN: usize = std::mem::size_of::<EntryHeader>();
const HEADER_ALIGN: usize = std::mem::align_of::<EntryHeader>();

/// The preferred size of one chunk, in bytes.
///
/// Each shard allocates its backing storage in chunks of this size, taken from the shared capacity. A chunk is larger
/// when a single entry needs more space. A chunk is smaller when less than this much shared capacity remains.
///
/// Trade-off: small chunks let idle shards hold less unused memory, and let empty chunks return to the shared capacity
/// sooner. Large chunks reduce allocations and leave less space stranded in free regions that cannot merge across
/// chunk boundaries. We use 16 KiB as a balance between these costs.
const CHUNK_SIZE: usize = 16 * 1024;

/// The maximum number of shards in one interner.
///
/// The interner creates one shard for every `2 * CHUNK_SIZE` bytes of capacity, with at least one and at most this
/// many shards. Each shard has its own lock, so more shards reduce lock contention between threads.
///
/// Trade-off: free space in one shard is not available to strings that hash to another shard. Only empty chunks move
/// between shards. More shards therefore strand more free space when the interner is close to full. The value also
/// sets the size of the guard array that `SharedState::lock_shards` returns.
const MAX_SHARDS: usize = 16;

/// The largest string length, in bytes, that the interner accepts.
///
/// This is the maximum value of `PackedLengthCapacity`, rounded down to the alignment of `EntryHeader`. The rounding
/// makes sure that the aligned capacity of an accepted string also fits in `PackedLengthCapacity`. On 64-bit
/// platforms, the limit is approximately 4 GB.
const MAX_STRING_CAPACITY: usize = PackedLengthCapacity::maximum_value() & !(HEADER_ALIGN - 1);

/// The smallest remainder, in bytes, that is worth splitting off a reclaimed entry.
///
/// Each entry has an `EntryHeader` followed by the string data. The string capacity is a multiple of the alignment of
/// `EntryHeader`, so that entries can be written contiguously.
///
/// When a new string reuses a reclaimed entry, the excess space can become a separate reclaimed entry. The excess
/// space must hold a header and at least one alignment unit of string data. A smaller remainder stays part of the
/// reused entry as extra string capacity.
const MINIMUM_ENTRY_LEN: usize = HEADER_LEN + HEADER_ALIGN;

/// A handle to one interned string.
///
/// `StringState` holds two pointers: the shard that owns the entry, and the header of the entry in one of the chunks
/// of that shard. Each handle holds one reference on the entry. The `Arc` keeps the shard, and therefore the chunk,
/// alive while the handle exists.
#[derive(Debug)]
pub(crate) struct StringState {
    interner: Arc<Mutex<ShardState>>,
    header: NonNull<EntryHeader>,
}

impl StringState {
    #[inline]
    pub const fn as_str(&self) -> &str {
        // SAFETY: We ensure `self.header` is well-aligned and points to an initialized `EntryHeader` value when
        // creating `StringState`.
        unsafe { get_entry_string(self.header) }
    }
}

impl PartialEq for StringState {
    fn eq(&self, other: &Self) -> bool {
        self.header == other.header
    }
}

impl Clone for StringState {
    fn clone(&self) -> Self {
        // SAFETY: The caller that creates `StringState` is responsible for ensuring that `self.header` is well-aligned
        // and points to an initialized `EntryHeader` value.
        let header = unsafe { self.header.as_ref() };
        header.increment_active_refs();

        Self {
            interner: self.interner.clone(),
            header: self.header,
        }
    }
}

impl Drop for StringState {
    fn drop(&mut self) {
        // SAFETY: The caller that creates `StringState` is responsible for ensuring that `self.header` is well-aligned
        // and points to an initialized `EntryHeader` value. This handle still holds one reference, so the entry is not
        // reclaimed, and the `Arc` keeps the shard and its chunks alive.
        let header = unsafe { self.header.as_ref() };

        // If other references remain, release ours without taking the shard lock. This never decrements the count
        // to zero, so it cannot race with lookups or reclamation.
        if header.try_decrement_shared_refs() {
            return;
        }

        // We probably hold the last reference. Take the shard lock before the final decrement. Lookups also need this
        // lock to increment the count, so no lookup can see a zero count and resurrect the entry. Clones need a live
        // handle, so they cannot increment the count from zero either.
        let mut shard = self.interner.lock().unwrap();
        if header.decrement_active_refs() {
            // We decremented the reference count to zero while holding the shard lock, so this is the one and only
            // drop that reclaims the entry. After this call, the shard can reuse the space or free the chunk.
            shard.mark_for_reclamation(self.header);
        }
    }
}

/// Metadata about an interner entry.
///
/// `EntryHeader` represents the smallest amount of information about an interned entry that's needed to support both
/// lookup of existing interned strings, as well as the ability to reclaim space in the interner when an entry is no
/// longer in use.
struct EntryHeader {
    /// The number of active references to this entry.
    ///
    /// Incremented by a lookup in the interner or by a clone of a handle. Decremented when a handle is dropped. The
    /// final decrement to zero happens only while the shard lock is held.
    refs: AtomicUsize,

    /// Combined length/capacity of the entry, in terms of the string itself.
    ///
    /// This value excludes the length of the header itself. For example, an entry that holds the string
    /// "hello, world!" has a string length of 13 bytes. The capacity is padded to the 8-byte alignment of
    /// `EntryHeader`, so it is 16 bytes. Thus `EntryHeader::len` reports `13` and `EntryHeader::capacity` reports `16`.
    /// `EntryHeader::entry_len` reports `32`: the string capacity (16) plus the header length (16).
    ///
    /// As the description of `PackedLengthCapacity` explains, strings cannot be larger than approximately 4 GB on
    /// 64-bit platforms. `MAX_STRING_CAPACITY` enforces this limit when a string is interned.
    len_cap: PackedLengthCapacity,
}

impl EntryHeader {
    /// Creates a tombstone entry with the given capacity.
    ///
    /// This marks a reclaimed region in a chunk as unused.
    fn tombstone(entry: ReclaimedEntry) -> Self {
        // The usable capacity for a reclaimed entry is the full capacity minus the size of `EntryHeader` itself, as
        // reclaimed entries represent the _entire_ region in the chunk, but `EntryHeader` only cares about the
        // string portion itself.
        let str_cap = EntryHeader::usable_from_reclaimed(entry);

        Self {
            refs: AtomicUsize::new(0),
            len_cap: PackedLengthCapacity::new(str_cap, 0),
        }
    }

    /// Creates a new entry for the given string.
    fn from_string(s: &str) -> Self {
        // We're dictating the necessary capacity here, which is the length of the string rounded to the nearest
        // multiple of the alignment of `EntryHeader`, which ensures that any subsequent entry will be properly aligned.
        let str_cap = aligned_string::<Self>(s);

        Self {
            refs: AtomicUsize::new(1),
            len_cap: PackedLengthCapacity::new(str_cap, s.len()),
        }
    }

    /// Creates a new entry for the given string, based on the given reclaimed entry.
    ///
    /// This maps the entry header to the underlying capacity of the given reclaimed entry, which is done in cases where
    /// a reclaimed entry is being used when interning a new string, and the reclaimed entry is larger than the string
    /// being interned, but not large enough that we could split the excess capacity into a new reclaimed entry.
    fn from_reclaimed_entry(mut entry: ReclaimedEntry, s: &str) -> (Self, Option<ReclaimedEntry>) {
        // The usable capacity for a reclaimed entry is the full capacity minus the size of `EntryHeader` itself, as
        // reclaimed entries represent the _entire_ region in the chunk, but `EntryHeader` only cares about the
        // string portion itself.
        let entry_str_cap = EntryHeader::usable_from_reclaimed(entry);
        let required_str_cap = aligned_string::<Self>(s);

        // If the reclaimed entry has enough additional space beyond what we need for the string, we'll split it off and
        // return it for the caller to keep around in the reclaimed entries list.
        let remainder = entry_str_cap - required_str_cap;
        let (adjusted_str_cap, maybe_split_entry) = if remainder >= MINIMUM_ENTRY_LEN {
            let entry_len = EntryHeader::len_for(s);
            let split_entry = entry.split_off(entry_len);

            (entry_len - HEADER_LEN, Some(split_entry))
        } else {
            (entry_str_cap, None)
        };

        let header = Self {
            refs: AtomicUsize::new(1),
            len_cap: PackedLengthCapacity::new(adjusted_str_cap, s.len()),
        };

        (header, maybe_split_entry)
    }

    /// Returns the computed length of a complete entry, in bytes, for the given string.
    ///
    /// This includes the size of the entry header itself and the string data, when padded for alignment, and represents
    /// the number of bytes that would be consumed in a chunk.
    const fn len_for(s: &str) -> usize {
        HEADER_LEN + aligned_string::<Self>(s)
    }

    /// Returns the usable capacity of a reclaimed entry, in bytes.
    ///
    /// Usable refers to the number of bytes in a reclaimed entry that could be used for string data, after accounting
    /// for the size of `EntryHeader` itself.
    const fn usable_from_reclaimed(entry: ReclaimedEntry) -> usize {
        entry.capacity() - HEADER_LEN
    }

    /// Returns the size of the string, in bytes, that this entry can hold.
    const fn capacity(&self) -> usize {
        self.len_cap.capacity()
    }

    /// Returns the size of the string, in bytes, that this entry _actually_ holds.
    const fn len(&self) -> usize {
        self.len_cap.len()
    }

    /// Returns the total length of the entry, in bytes.
    ///
    /// This includes the length of the header in addition to the string data.
    const fn entry_len(&self) -> usize {
        HEADER_LEN + self.capacity()
    }

    /// Decrements the reference count by one, only if the count stays above zero.
    ///
    /// Returns `true` if the count was decremented. Returns `false` if this is probably the last reference. In that
    /// case, the caller must take the shard lock and call `decrement_active_refs` instead.
    fn try_decrement_shared_refs(&self) -> bool {
        self.refs
            .try_update(AcqRel, Acquire, |refs| (refs > 1).then(|| refs - 1))
            .is_ok()
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

// SAFETY: We don't take references to the entry header pointer that outlast `StringState`, and the only modification we
// do to the entry header is through atomic operations, so it's safe to both send and share `StringState` between
// threads.
unsafe impl Send for StringState {}
unsafe impl Sync for StringState {}

/// One lazily allocated slab of entry storage.
///
/// A shard allocates a chunk only when no existing chunk in that shard has room for a new entry. A chunk is usually
/// `CHUNK_SIZE` (16 KiB) bytes. It is larger when one entry needs more space, and smaller when less shared capacity
/// remains. Its capacity is always a multiple of the alignment of `EntryHeader`.
///
/// Entries are written contiguously from the start of the chunk. `offset` marks the end of the written region:
///
/// ```text
///   ptr                                          ptr + offset                       ptr + capacity
///    ▼                                                 ▼                                   ▼
///    ┏━━━━━━━━━━━━┯━━━━━━━━━━━━━━━━━┯━━━━━━━━━━━━━━━━━━┳━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
///    ┃  entry #1  │ reclaimed entry │     entry #3     ┃            unoccupied             ┃
///    ┃ (16+ B)    │ (tombstone)     │ (16+ B)          ┃                                   ┃
///    ┗━━━━━━━━━━━━┷━━━━━━━━━━━━━━━━━┷━━━━━━━━━━━━━━━━━━┻━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
/// ```
///
/// A chunk owns its allocation and its list of reclaimed entries. It does not track the bytes in use; the owning
/// `ShardState` tracks `len_bytes` across all of its chunks. A chunk has no lock of its own. The mutex around the
/// owning `ShardState` guards every access to the chunk, except for the atomic reference count in each entry header.
/// When `offset` returns to zero, the chunk is empty, and `SharedState::intern_under_pressure` can free it.
#[derive(Debug)]
struct Chunk {
    /// Start of the allocation, aligned for `EntryHeader`.
    ptr: NonNull<u8>,

    /// Length of the written region, in bytes, including reclaimed entries inside it.
    offset: usize,

    /// Size of the allocation, in bytes.
    capacity: usize,

    /// Free regions inside the written region that new entries can reuse.
    ///
    /// Adjacent reclaimed entries merge. A reclaimed entry that ends at `offset` is removed and `offset` moves back.
    reclaimed: ReclaimedEntries,
}

impl Chunk {
    /// Returns the allocation layout of a chunk with the given capacity.
    ///
    /// `GenericMapInterner::new` limits the total capacity to `isize::MAX`, so the layout is always valid.
    fn layout(capacity: usize) -> Layout {
        Layout::from_size_align(capacity, HEADER_ALIGN).unwrap()
    }

    /// Allocates a new, empty chunk with the given capacity.
    ///
    /// The caller reserves `capacity` from `SharedState::unallocated` before calling this method. The capacity is at
    /// least the length of the first entry that the caller writes into the chunk.
    fn new(capacity: usize) -> Self {
        // Allocate the chunk. It is aligned for `EntryHeader`, so an entry can start at offset zero.
        let data_layout = Self::layout(capacity);

        // SAFETY: The caller always reserves room for at least one entry, which is at least `HEADER_LEN` bytes, so the
        // layout is non-zero.
        let data_ptr = unsafe { std::alloc::alloc(data_layout) };
        let ptr = match NonNull::new(data_ptr) {
            Some(ptr) => ptr,
            None => std::alloc::handle_alloc_error(data_layout),
        };

        Self {
            ptr,
            offset: 0,
            capacity,
            reclaimed: ReclaimedEntries::new(),
        }
    }

    /// Returns the total number of bytes of contiguous, unoccupied space at the end of the chunk.
    fn available_unoccupied(&self) -> usize {
        self.capacity - self.offset
    }

    /// Returns a pointer to the entry header at the given offset in this chunk.
    fn get_entry_ptr(&self, offset: usize) -> NonNull<EntryHeader> {
        debug_assert!(
            offset + HEADER_LEN <= self.capacity,
            "offset would point to entry that cannot possibly avoid extending past end of data buffer"
        );

        // SAFETY: The caller is responsible for ensuring that `offset` is within the bounds of the chunk, and
        // that `offset` is well-aligned for `EntryHeader`.
        let entry_ptr = unsafe { self.ptr.as_ptr().add(offset).cast::<EntryHeader>() };
        debug_assert!(entry_ptr.is_aligned(), "entry header pointer must be well-aligned");

        // SAFETY: `entry_ptr` is derived from `self.ptr`, which itself is `NonNull<u8>`, and the caller is responsible
        // for ensuring that `offset` is within the bounds of the chunk, so we know `entry_ptr` is non-null.
        unsafe { NonNull::new_unchecked(entry_ptr) }
    }

    /// Writes the header and string bytes of a new entry at the given offset.
    ///
    /// The caller must make sure that the region at `offset` is unused and is at least `entry_header.entry_len()`
    /// bytes long.
    fn write_entry(&mut self, offset: usize, entry_header: EntryHeader, s: &str) -> NonNull<EntryHeader> {
        debug_assert_eq!(
            entry_header.len(),
            s.len(),
            "entry header length must match string length"
        );

        let entry_ptr = self.get_entry_ptr(offset);

        // Write the entry header.
        //
        // SAFETY: `entry_ptr` is in bounds and well-aligned for `EntryHeader`, and the caller makes sure the region is
        // unused, so no live reference points into it.
        unsafe { entry_ptr.as_ptr().write(entry_header) };

        let s_buf = s.as_bytes();

        // Write the string.
        //
        // SAFETY: The region holds the header plus at least `s.len()` bytes of string capacity, so the slice stays
        // inside the chunk. The region is unused, so no other reference aliases the slice.
        let entry_s_buf = unsafe {
            // Take the entry pointer and add 1, which sets our pointer to right _after_ the header.
            let entry_s_ptr = entry_ptr.as_ptr().add(1).cast::<u8>();
            std::slice::from_raw_parts_mut(entry_s_ptr, s_buf.len())
        };
        entry_s_buf.copy_from_slice(s_buf);

        entry_ptr
    }

    /// Appends an entry for `s` to the unoccupied space at the end of the chunk.
    ///
    /// The caller must make sure that `available_unoccupied` is at least `EntryHeader::len_for(s)`.
    fn write_to_unoccupied(&mut self, s: &str) -> NonNull<EntryHeader> {
        let entry_header = EntryHeader::from_string(s);

        // Write the entry to the end of the chunk.
        let entry_offset = self.offset;
        self.offset += entry_header.entry_len();

        self.write_entry(entry_offset, entry_header, s)
    }

    /// Writes an entry for `s` into a reclaimed entry, and keeps any large remainder as a new reclaimed entry.
    fn write_to_reclaimed_entry(&mut self, entry: ReclaimedEntry, s: &str) -> NonNull<EntryHeader> {
        let entry_offset = entry.offset();
        let (entry_header, maybe_split_entry) = EntryHeader::from_reclaimed_entry(entry, s);

        // If we had enough capacity in the reclaimed entry to hold this string _and_ potentially hold another entry, we
        // split it off and store that remainder entry.
        if let Some(split_entry) = maybe_split_entry {
            self.add_reclaimed(split_entry);
        }

        // Write the entry in place of the reclaimed entry.
        self.write_entry(entry_offset, entry_header, s)
    }

    /// Adds a reclaimed entry to this chunk, and merges it with adjacent free space.
    fn add_reclaimed(&mut self, entry: ReclaimedEntry) {
        // Reclamation is a two-step process: first, we have to actually keep track of the reclaimed entry, which
        // potentially involves merging adjacent reclaimed entries, and then once all of that has happened, we tombstone
        // the entry (whether merged or not).
        //
        // However, if the merged reclaimed entry immediately precedes any available capacity, we can skip tombstoning
        // it, since we can just wind back `offset` to reclaim the space.
        let merged_entry = self.reclaimed.insert(entry);
        if merged_entry.offset() + merged_entry.capacity() == self.offset {
            self.offset -= merged_entry.capacity();
            self.reclaimed.remove(&merged_entry);
            return;
        }

        self.clear_reclaimed_entry(merged_entry);
    }

    /// Writes a tombstone header and a fill pattern over a reclaimed entry.
    fn clear_reclaimed_entry(&mut self, entry: ReclaimedEntry) {
        let entry_ptr = self.get_entry_ptr(entry.offset);

        // Write the tombstone header, which marks the region as having no active references.
        //
        // SAFETY: We know that `entry_ptr` is valid for writes (reclaimed entries are, by definition, inactive regions
        // in the chunk) and is well-aligned for `EntryHeader`.
        let tombstone = EntryHeader::tombstone(entry);
        let str_cap = tombstone.capacity();

        unsafe {
            entry_ptr.as_ptr().write(tombstone);
        }

        // Write a magic value to the entire string capacity for the entry. This ensures that there's a known repeating
        // value which, in the case of debugging issues, can be a signal that offsets/reclaimed entries are incorrect
        // and overlapping with active entries.
        //
        // SAFETY: Like above, the caller is responsible for ensuring that `offset` is within the bounds of the chunk,
        // and that `offset + capacity` does not extend past the bounds of the chunk.
        unsafe {
            // Take the entry pointer and add 1, which sets our pointer to right _after_ the header.
            let str_ptr = entry_ptr.as_ptr().add(1).cast::<u8>();
            let str_buf = std::slice::from_raw_parts_mut(str_ptr, str_cap);
            str_buf.fill(0x21);
        }
    }

    /// Returns the byte offset of the given entry header within the chunk.
    fn offset_of(&self, header_ptr: NonNull<EntryHeader>) -> usize {
        // SAFETY: The caller is responsible for ensuring the entry header belongs to this chunk. If that is upheld,
        // then the pointer to the entry header is not less than the base pointer of the chunk, ensuring the offset is
        // non-negative.
        let entry_offset = unsafe {
            header_ptr
                .cast::<u8>()
                .as_ptr()
                .offset_from(self.ptr.as_ptr().cast_const())
        };
        debug_assert!(entry_offset >= 0, "entry offset must be non-negative");

        entry_offset as usize
    }

    /// Writes an entry for `s` into this chunk, if the chunk has room.
    ///
    /// `required` is `EntryHeader::len_for(s)`. A reclaimed entry that is large enough is used first. Otherwise, the
    /// entry goes into the unoccupied space at the end of the chunk. Returns `None` if neither has room.
    fn try_intern(&mut self, s: &str, required: usize) -> Option<NonNull<EntryHeader>> {
        if let Some(entry) = self.reclaimed.take_if(|entry| entry.capacity() >= required) {
            saluki_antithesis::sometimes!(true, "reclaimed interner slot reused for a new string");
            Some(self.write_to_reclaimed_entry(entry, s))
        } else if required <= self.available_unoccupied() {
            Some(self.write_to_unoccupied(s))
        } else {
            None
        }
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        // SAFETY: We allocated this chunk with the global allocator, and we're generating the same layout that was
        // used to allocate it in the first place.
        unsafe {
            std::alloc::dealloc(self.ptr.as_ptr(), Self::layout(self.capacity));
        }
    }
}

/// The state of one shard: its chunks, its entries map, and its byte count.
///
/// Each shard lives in an `Arc<Mutex<ShardState>>`. `SharedState` holds one `Arc` for each shard, and every
/// `StringState` handle for a string in the shard holds another. The mutex guards every field and every chunk. Only
/// the reference count in each entry header changes without the lock, and only while the count stays above zero.
///
/// A shard owns its chunks. It does not own its capacity budget: it reserves chunk capacity from
/// `SharedState::unallocated`, and only `SharedState::intern_under_pressure` returns that capacity.
#[derive(Debug, Default)]
struct ShardState {
    /// Active entries in this shard, keyed by string.
    ///
    /// Each value holds the entry header and the index of its chunk in `chunks`. These keys borrow the strings in
    /// `chunks`. Remove the key before reusing its storage.
    entries: HashMap<&'static str, (NonNull<EntryHeader>, usize)>,

    /// Chunks that this shard has allocated.
    ///
    /// A freed chunk leaves a `None` slot, so that the chunk indices in `entries` stay valid. A new chunk fills the
    /// first `None` slot before the vector grows.
    chunks: Vec<Option<Chunk>>,

    /// Total length of all active entries in this shard, in bytes, including headers and padding.
    ///
    /// This counts bytes across all chunks. It excludes reclaimed entries and unoccupied space.
    len_bytes: usize,
}

impl ShardState {
    /// Reclaims the storage of an entry whose reference count dropped to zero.
    ///
    /// Reclamation must happen exactly once for each entry. A second reclamation of the same entry would subtract its
    /// length from `len_bytes` twice. It would also add an overlapping reclaimed entry to the chunk's free list. A
    /// later intern could then write two strings into the same storage.
    ///
    /// `StringState::drop` makes the final decrement while it holds this shard's lock. A lookup needs the same lock
    /// to increment the count, and a clone needs a live handle. Thus no thread can resurrect the entry after the count
    /// reaches zero. Exactly one drop observes the transition to zero, so exactly one call reaches this method.
    ///
    /// The entry is removed from `entries` before its storage is reclaimed. Reclamation overwrites the string bytes,
    /// and the map key borrows those bytes.
    ///
    /// # Invariants
    ///
    /// The caller must hold this shard's lock, which `&mut self` proves. The caller must have decremented the
    /// reference count of `header` to zero while it held that lock. `header` must point to an entry that is still in
    /// `entries`. If these conditions are not met, this method panics or corrupts the chunk's free list.
    fn mark_for_reclamation(&mut self, header: NonNull<EntryHeader>) {
        // SAFETY: The last reference was released under this shard's lock, so no other thread can reclaim or reuse the
        // entry. The entry is still in the map, so its header and string bytes are still initialized.
        let entry_str = unsafe { get_entry_string(header) };
        let (_, index) = self.entries.remove(entry_str).unwrap();
        let chunk = self.chunks[index].as_mut().unwrap();
        // SAFETY: The header is still initialized. `add_reclaimed` below is the first write to this region.
        let len = unsafe { header.as_ref() }.entry_len();
        self.len_bytes -= len;
        chunk.add_reclaimed(ReclaimedEntry::new(chunk.offset_of(header), len));
    }

    /// Looks up `s`, or writes it into an existing chunk of this shard.
    ///
    /// An existing entry gets one more reference. Otherwise, the method tries each allocated chunk. Returns `None` if
    /// the string is not present and no chunk has room for `required` bytes.
    fn try_intern_in_chunks(&mut self, s: &str, required: usize) -> Option<NonNull<EntryHeader>> {
        if let Some(&(header, _)) = self.entries.get(s) {
            // SAFETY: Each mapped entry stays alive while we hold this shard's lock.
            unsafe { header.as_ref() }.increment_active_refs();
            return Some(header);
        }

        // Check the end of the chunk list first, where appends usually have room.
        for (index, chunk) in self.chunks.iter_mut().enumerate().rev() {
            if let Some(header) = chunk.as_mut().and_then(|chunk| chunk.try_intern(s, required)) {
                self.insert_entry(header, index);
                return Some(header);
            }
        }
        None
    }

    /// Allocates a new chunk with the given capacity and writes `s` into it.
    ///
    /// The caller must have reserved `capacity` from `SharedState::unallocated`, and `capacity` must be at least
    /// `EntryHeader::len_for(s)`.
    fn intern_in_new_chunk(&mut self, s: &str, capacity: usize) -> NonNull<EntryHeader> {
        let mut chunk = Chunk::new(capacity);
        let header = chunk.write_to_unoccupied(s);
        let index = if let Some(index) = self.chunks.iter().position(Option::is_none) {
            self.chunks[index] = Some(chunk);
            index
        } else {
            let index = self.chunks.len();
            self.chunks.push(Some(chunk));
            index
        };
        self.insert_entry(header, index);
        header
    }

    /// Adds a newly written entry to `entries`, and adds its length to `len_bytes`.
    fn insert_entry(&mut self, header: NonNull<EntryHeader>, index: usize) {
        // SAFETY: The new entry is initialized and its chunk cannot move. We create a `'static` key, but we never
        // expose that lifetime outside the shard. `mark_for_reclamation` removes the key before the storage is reused.
        self.len_bytes += unsafe { header.as_ref() }.entry_len();
        self.entries
            .insert(unsafe { get_entry_string(header) }, (header, index));
    }
}

// SAFETY: We don't take references to chunk memory that outlast `ShardState`, other than handles that keep the shard
// alive. All access to `ShardState` goes through a mutex, so we can send it between threads. `Mutex<ShardState>` is
// then `Sync` without a separate `Sync` impl.
unsafe impl Send for ShardState {}

/// The state that all clones of one `GenericMapInterner` share.
///
/// `SharedState` owns the shard list and the capacity budget. It does not own any chunk; each shard owns its own
/// chunks. The shard list never changes after construction, so reading it needs no lock.
///
/// Capacity protocol: `capacity` is the fixed limit that `capacity_bytes` reports. `unallocated` is the part of the
/// limit, rounded down to the alignment of `EntryHeader`, that no chunk holds yet. A thread that holds one shard lock
/// reserves a chunk with an `AcqRel` compare-and-swap in `reserve_chunk`. Only `intern_under_pressure` returns
/// capacity. It holds every shard lock, so no reservation can run at the same time. It therefore reads `unallocated`
/// with `Acquire`, frees empty chunks, and writes the new value with a plain `Release` store. The sum of `unallocated`
/// and all chunk capacities never exceeds the rounded-down capacity.
#[derive(Debug)]
struct SharedState {
    /// The shards, in lock order.
    shards: Vec<Arc<Mutex<ShardState>>>,

    /// The total capacity, in bytes, that the caller requested.
    capacity: usize,

    /// The capacity, in bytes, that is not yet allocated to any chunk.
    unallocated: AtomicUsize,
}

impl SharedState {
    /// Returns the index of the shard that owns `s`.
    fn shard_index(&self, s: &str) -> usize {
        if self.shards.len() == 1 {
            0
        } else {
            (hash_string(s) as usize) % self.shards.len()
        }
    }

    /// Locks every shard in index order.
    ///
    /// All code that holds more than one shard lock uses this method, so the shared lock order prevents deadlocks.
    /// Shards beyond `shards.len()` are `None`.
    fn lock_shards(&self) -> [Option<MutexGuard<'_, ShardState>>; MAX_SHARDS] {
        std::array::from_fn(|index| self.shards.get(index).map(|shard| shard.lock().unwrap()))
    }

    /// Reserves capacity for a new chunk from `unallocated`.
    ///
    /// The chunk gets `CHUNK_SIZE` bytes, or `required` bytes if that is larger. If less capacity remains, the chunk
    /// gets all remaining capacity, but only if that is at least `required`. Returns the reserved capacity, or `None`
    /// if less than `required` bytes remain. The caller must hold a shard lock, which `intern_under_pressure` relies
    /// on.
    fn reserve_chunk(&self, required: usize) -> Option<usize> {
        let preferred = CHUNK_SIZE.max(required);
        let available = self
            .unallocated
            .try_update(AcqRel, Acquire, |available| {
                (available >= required).then(|| available - preferred.min(available))
            })
            .ok()?;
        Some(preferred.min(available))
    }

    /// Interns `s` after the fast path found no room, by freeing empty chunks in all shards.
    ///
    /// The method locks every shard, so it is slow and blocks all other interning. The caller must not hold a shard
    /// lock. First, it retries `try_intern_in_chunks` in the target shard. Next, it frees empty chunks until at least
    /// `required` bytes are unallocated. If enough capacity is free, it allocates a new chunk in the target shard.
    /// Returns `None` if the interner cannot make room for the string.
    #[cold]
    fn intern_under_pressure(&self, index: usize, s: &str, required: usize) -> Option<NonNull<EntryHeader>> {
        // Acquire locks in the same order as the metrics methods. Another thread may have inserted the string or made
        // room after we released the original lock.
        let mut shards = self.lock_shards();
        if let Some(header) = shards[index].as_mut().unwrap().try_intern_in_chunks(s, required) {
            return Some(header);
        }

        // Return empty chunks to the shared budget before denying an admission. Freeing a chunk leaves a `None`
        // slot, so the offsets of occupied chunks and their indices in the entries map stay unchanged.
        let mut available = self.unallocated.load(Acquire);
        'shards: for shard in shards.iter_mut().flatten() {
            for slot in &mut shard.chunks {
                if available >= required {
                    break 'shards;
                }
                if let Some(chunk) = slot.as_ref().filter(|chunk| chunk.offset == 0) {
                    available += chunk.capacity;
                    *slot = None;
                }
            }
        }
        if available < required {
            self.unallocated.store(available, Release);
            return None;
        }
        let capacity = CHUNK_SIZE.max(required).min(available);
        self.unallocated.store(available - capacity, Release);
        let mut shard = shards[index].take().unwrap();
        drop(shards);
        Some(shard.intern_in_new_chunk(s, capacity))
    }
}

/// A sharded string interner with bounded backing storage and support for reclamation.
///
/// ## Overview
///
/// A string's hash selects one of up to `MAX_SHARDS` shards. Each shard has its own lock, entries map, and chunks.
/// Shards allocate chunks on demand from a shared capacity budget, so an idle interner uses little memory. The
/// interner never allocates more chunk memory than its capacity allows. Once all references to an interned string
/// are dropped, the storage for that string is reclaimed and can hold new strings.
///
/// ## Storage layout
///
/// Each chunk stores entries contiguously. Each entry has a header followed by the string bytes. The header holds
/// the reference count and the packed length and capacity of the string.
///
/// The layout of an entry is as follows:
///
/// ```text
/// ┌───────────────────── entry #1 ──────────────────────┐ ┌─ entry #2 ─┐ ┌─ entry .. ─┐
/// ▼                                                     ▼ ▼            ▼ ▼            ▼
/// ┏━━━━━━━━━━━┯━━━━━━━━━━━━━━━┯━━━━━━━━━━━┯━━━━━━━━━━━━━┓ ┏━━━━━━━━━━━━┓ ┏━━━━━━━━━━━━┓
/// ┃  ref cnt  │ str len & cap │ str data  │   padding   ┃ ┃   header   ┃ ┃   header   ┃
/// ┃ (8 bytes) │   (8 bytes)   │ (N bytes) │ (0-7 bytes) ┃ ┃  & string  ┃ ┃  & string  ┃
/// ┗━━━━━━━━━━━┷━━━━━━━━━━━━━━━┷━━━━━━━━━━━┷━━━━━━━━━━━━━┛ ┗━━━━━━━━━━━━┛ ┗━━━━━━━━━━━━┛
/// ▲                           ▲                           ▲
/// └────── `EntryHeader` ──────┘                           └── aligned for `EntryHeader`
///      (8 byte alignment)                                     via trailing padding
/// ```
///
/// Each chunk is aligned for `EntryHeader`, so the first entry in a chunk is aligned. The string data has a variable
/// length, so each entry ends with padding. The padding makes sure the next entry also starts aligned for
/// `EntryHeader`. In the worst case, an entry wastes 7 bytes on padding.
///
/// ## `InternedString`
///
/// An `InternedString` holds a pointer to the entry header and a reference to the shard that owns the entry. Cloning
/// it increments the reference count in the header. Dereferencing it reads the length from the header and the string
/// bytes that follow the header.
///
/// When an `InternedString` is dropped, it decrements the reference count. The drop that releases the last reference
/// takes the shard lock first, and then marks the entry for reclamation.
///
/// ## Reclamation
///
/// The interner bounds its memory use, so unused strings must not fill it up. Reclamation makes the storage of
/// unused strings available for new strings.
///
/// When the last reference to a string is dropped, the shard removes the string from its entries map. The entry's
/// region becomes a reclaimed entry in the free list of its chunk. A reclaimed entry is a marker: the offset and
/// length of a free region in the chunk. When the shard interns a new string, it searches the free list for a region
/// that can hold the string.
///
/// Adjacent reclaimed entries in the same chunk merge. This reduces fragmentation, although not as well as re-packing
/// the entries. Free regions cannot merge across chunk or shard boundaries. When a chunk becomes empty, it stays
/// allocated until the interner needs capacity for a new chunk. At that time, any shard can free its empty chunks and
/// return their capacity to the shared budget.
#[derive(Clone, Debug)]
pub struct GenericMapInterner {
    state: Arc<SharedState>,
}

impl GenericMapInterner {
    /// Creates an interner that allocates at most `capacity` bytes of backing storage.
    ///
    /// The interner allocates no chunk until it interns the first string. The number of shards depends on the
    /// capacity: one shard for every `2 * CHUNK_SIZE` bytes, from 1 to `MAX_SHARDS`. The capacity available to chunks
    /// is rounded down to a multiple of 8 bytes, the alignment of entries.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is larger than `isize::MAX`.
    pub fn new(capacity: NonZeroUsize) -> Self {
        let capacity = capacity.get();
        assert!(capacity <= isize::MAX as usize, "capacity exceeds allocation limits");

        let shard_count = (capacity / (2 * CHUNK_SIZE)).clamp(1, MAX_SHARDS);
        Self {
            state: Arc::new(SharedState {
                shards: (0..shard_count)
                    .map(|_| Arc::new(Mutex::new(ShardState::default())))
                    .collect(),
                capacity,
                unallocated: AtomicUsize::new(capacity & !(HEADER_ALIGN - 1)),
            }),
        }
    }
}

// Metrics: `len` counts active entries, `len_bytes` sums the bytes of active entries (headers, strings, and padding),
// and `is_empty` checks for zero active entries. Each method takes every shard lock in index order, so it reads a
// consistent snapshot of all shards. The value can change as soon as the locks are released. `capacity_bytes` reads a
// constant and takes no lock.
impl Interner for GenericMapInterner {
    fn is_empty(&self) -> bool {
        self.state
            .lock_shards()
            .iter()
            .flatten()
            .all(|shard| shard.entries.is_empty())
    }

    fn len(&self) -> usize {
        self.state
            .lock_shards()
            .iter()
            .flatten()
            .map(|shard| shard.entries.len())
            .sum()
    }

    fn len_bytes(&self) -> usize {
        self.state
            .lock_shards()
            .iter()
            .flatten()
            .map(|shard| shard.len_bytes)
            .sum()
    }

    fn capacity_bytes(&self) -> usize {
        self.state.capacity
    }

    fn try_intern(&self, s: &str) -> Option<InternedString> {
        // We can only intern strings with a capacity that fits within a packed length/capacity value. If `s` is
        // larger than that, we can't intern it, and there's no existing entry we could have for it either.
        if s.len() > MAX_STRING_CAPACITY {
            return None;
        }
        let required = EntryHeader::len_for(s);
        // An entry larger than the total capacity can never fit, so skip the shard locks entirely.
        if required > self.capacity_bytes() {
            saluki_antithesis::sometimes!(true, "interner full — no capacity to intern");
            return None;
        }

        let index = self.state.shard_index(s);
        let interner = &self.state.shards[index];
        // Fast path: hold only the target shard's lock. Look up the string, try the existing chunks, and then try to
        // reserve a new chunk from the shared budget. The lock is released at the end of this block.
        let header = {
            let mut shard = interner.lock().unwrap();
            shard.try_intern_in_chunks(s, required).or_else(|| {
                let capacity = self.state.reserve_chunk(required)?;
                Some(shard.intern_in_new_chunk(s, capacity))
            })
        };
        // Slow path: lock every shard and free empty chunks to make room.
        let Some(header) = header.or_else(|| self.state.intern_under_pressure(index, s, required)) else {
            saluki_antithesis::sometimes!(true, "interner full — no capacity to intern");
            return None;
        };
        Some(InternedString::from(StringState {
            interner: Arc::clone(interner),
            header,
        }))
    }
}

/// Returns the pointer to the string bytes of an entry, and the string length.
///
/// # Safety
///
/// `header_ptr` must be well-aligned and point to an initialized `EntryHeader` value. The string bytes must follow the
/// header in the same allocation.
#[inline]
const unsafe fn get_entry_string_parts(header_ptr: NonNull<EntryHeader>) -> (NonNull<u8>, usize) {
    // SAFETY: The caller is responsible for ensuring that `header_ptr` is well-aligned and points to an initialized
    // `EntryHeader` value.
    let header = header_ptr.as_ref();

    // Advance past the header and get the pointer to the string.
    //
    // SAFETY: We know that we're simply skipping over the header by advancing the pointer by one when it's still typed
    // as `*mut EntryHeader`.
    let s_ptr = header_ptr.add(1).cast::<u8>();
    (s_ptr, header.len())
}

/// Returns the string that an entry holds.
///
/// # Safety
///
/// The caller must meet the safety requirements of `get_entry_string_parts`. The string bytes must be valid UTF-8.
/// The caller chooses the lifetime `'a`, and must make sure the entry is not reclaimed while the string is in use.
#[inline]
const unsafe fn get_entry_string<'a>(header_ptr: NonNull<EntryHeader>) -> &'a str {
    let (s_ptr, s_len) = get_entry_string_parts(header_ptr);

    // SAFETY: We depend on `get_entry_string_parts` to give us a valid pointer and length for the string.
    std::str::from_utf8_unchecked(std::slice::from_raw_parts(s_ptr.as_ptr() as *const _, s_len))
}

#[cfg(test)]
mod tests {
    use std::ops::Deref as _;

    use prop::sample::Index;
    use proptest::{collection::vec as arb_vec, prelude::*};

    use super::*;
    use crate::interning::{test_support, InternedStringState};

    pub(super) fn create_interner(capacity: usize) -> GenericMapInterner {
        assert!(capacity > 0, "capacity must be greater than zero");
        GenericMapInterner::new(NonZeroUsize::new(capacity).unwrap())
    }

    fn available_len(interner: &GenericMapInterner) -> usize {
        interner.capacity_bytes() - interner.len_bytes()
    }

    fn reclaimed_len(interner: &GenericMapInterner) -> usize {
        let shard = interner.state.shards[0].lock().unwrap();
        shard.chunks[0].as_ref().unwrap().reclaimed.len()
    }

    fn first_reclaimed_entry(interner: &GenericMapInterner) -> ReclaimedEntry {
        let shard = interner.state.shards[0].lock().unwrap();
        shard.chunks[0].as_ref().unwrap().reclaimed.first().unwrap()
    }

    const fn entry_len(s: &str) -> usize {
        EntryHeader::len_for(s)
    }

    pub(super) fn get_reclaimed_entry_for_string(s: &InternedString) -> ReclaimedEntry {
        let state = match &s.state {
            InternedStringState::GenericMap(state) => state,
            _ => panic!("unexpected string state"),
        };

        let shard = state.interner.lock().unwrap();
        let (_, index) = shard.entries[s.deref()];
        let chunk = shard.chunks[index].as_ref().unwrap();
        let header = unsafe { state.header.as_ref() };
        let offset = chunk.offset_of(state.header);
        ReclaimedEntry::new(offset, header.entry_len())
    }

    fn allocated_chunks(interner: &GenericMapInterner) -> (usize, usize) {
        interner
            .state
            .lock_shards()
            .iter()
            .flatten()
            .flat_map(|shard| shard.chunks.iter().flatten())
            .fold((0, 0), |(count, bytes), chunk| (count + 1, bytes + chunk.capacity))
    }

    #[test]
    fn allocates_chunks_only_as_needed() {
        let interner = create_interner(8 * 1024 * 1024);
        assert_eq!(allocated_chunks(&interner), (0, 0));
        #[cfg(not(feature = "loom"))]
        assert_eq!(std::mem::size_of::<StringState>(), 16);

        let first = interner.try_intern("first").unwrap();
        assert_eq!(allocated_chunks(&interner), (1, CHUNK_SIZE));
        assert_eq!(interner.len_bytes(), entry_len("first"));

        let strings = ["second", "third", "fourth"].map(|s| interner.try_intern(s).unwrap());
        let (count, bytes) = allocated_chunks(&interner);
        assert!((1..=4).contains(&count));
        assert_eq!(bytes, count * CHUNK_SIZE);
        assert_eq!(interner.len(), 4);
        drop((first, strings));
        assert!(interner.is_empty());
    }

    #[test]
    fn allocates_large_entry_within_exact_capacity() {
        let capacity = 2 * CHUNK_SIZE + 3;
        let interner = create_interner(capacity);
        let large = "x".repeat(2 * CHUNK_SIZE - HEADER_LEN);
        assert!(interner.try_intern(&format!("{large}x")).is_none());
        assert_eq!(allocated_chunks(&interner), (0, 0));

        // An empty small chunk must not prevent the full-size entry from fitting.
        drop(interner.try_intern("small").unwrap());
        let large = interner.try_intern(&large).unwrap();
        assert_eq!(interner.capacity_bytes(), capacity);
        assert_eq!(interner.len_bytes(), 2 * CHUNK_SIZE);
        assert_eq!(allocated_chunks(&interner), (1, 2 * CHUNK_SIZE));
        assert!(interner.try_intern("").is_none());
        drop(large);
        assert_eq!(interner.len_bytes(), 0);
    }

    #[test]
    fn reclaimed_regions_do_not_merge_across_chunks() {
        let interner = create_interner(2 * CHUNK_SIZE);
        let strings = ['a', 'b', 'c', 'd'].map(|c| c.to_string().repeat(CHUNK_SIZE / 2 - HEADER_LEN));
        let [first, second, third, fourth] = strings.each_ref().map(|s| interner.try_intern(s).unwrap());
        assert_eq!(allocated_chunks(&interner), (2, 2 * CHUNK_SIZE));

        drop((second, third));
        // These slots were adjacent in insertion order, but no chunk has a large enough region.
        let oversized = "x".repeat(CHUNK_SIZE - HEADER_LEN);
        assert!(interner.try_intern(&oversized).is_none());
        assert_eq!(interner.len_bytes(), CHUNK_SIZE);
        let second = interner.try_intern(&strings[1]).unwrap();
        let third = interner.try_intern(&strings[2]).unwrap();
        assert_eq!(interner.len_bytes(), 2 * CHUNK_SIZE);
        assert_eq!(first.deref(), strings[0]);
        assert_eq!(fourth.deref(), strings[3]);
        drop((first, second, third, fourth));
        assert!(interner.is_empty());
    }

    #[test]
    fn empty_string_fits_in_header_only_capacity() {
        let interner = create_interner(HEADER_LEN);
        let empty = interner.try_intern("").unwrap();
        assert!(empty.is_empty());
        assert_eq!(interner.len_bytes(), HEADER_LEN);
        assert!(interner.try_intern("x").is_none());
        drop(empty);
        assert!(interner.is_empty());
    }

    #[cfg(not(feature = "loom"))]
    #[test]
    fn reclaims_across_shards_after_concurrent_intern_and_drop() {
        let capacity = 4 * CHUNK_SIZE;
        let interner = create_interner(capacity);
        assert_eq!(interner.state.shards.len(), 2);

        let mut candidates = (0..).map(|n| format!("{n:08x}{}", "x".repeat(CHUNK_SIZE - HEADER_LEN - 8)));
        let [left, right, extra] = [0, 1, 0].map(|shard| {
            std::array::from_fn::<_, 2, _>(|_| candidates.find(|s| interner.state.shard_index(s) == shard).unwrap())
        });

        let [left, right] = std::thread::scope(|scope| {
            [left, right]
                .map(|strings| {
                    let interner = interner.clone();
                    scope.spawn(move || {
                        strings.map(|s| {
                            let handle = interner.try_intern(&s).unwrap();
                            let clone = handle.clone();
                            drop(handle);
                            assert_eq!(clone.deref(), s);
                            clone
                        })
                    })
                })
                .map(|thread| thread.join().unwrap())
        });
        assert_eq!(interner.capacity_bytes(), capacity);
        assert_eq!(interner.len(), 4);
        assert_eq!(interner.len_bytes(), capacity);
        assert_eq!(allocated_chunks(&interner), (4, capacity));
        assert!(interner.try_intern(&extra[0]).is_none());

        std::thread::scope(|scope| scope.spawn(move || drop(right)).join().unwrap());
        assert_eq!(interner.len(), 2);
        assert_eq!(interner.len_bytes(), capacity / 2);
        let replacements = extra.map(|s| interner.try_intern(&s).unwrap());
        assert_eq!(interner.len(), 4);
        assert_eq!(interner.len_bytes(), capacity);
        assert_eq!(allocated_chunks(&interner), (4, capacity));

        drop((left, replacements));
        assert!(interner.is_empty());
        assert_eq!(interner.len(), 0);
        assert_eq!(interner.len_bytes(), 0);
    }

    #[test]
    fn basic() {
        let interner = create_interner(1024);

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
        let interner = create_interner(48);

        let s1 = interner.try_intern("hello world!");
        assert!(s1.is_some());

        let s2 = interner.try_intern("hello, world");
        assert!(s2.is_none());
    }

    #[test]
    fn reclaim_after_dropped() {
        let interner = create_interner(1024);

        let s1 = interner.try_intern("hello world!").expect("should not fail to intern");
        let s1_entry_len = entry_len(&s1);

        assert_eq!(interner.len(), 1);
        assert_eq!(available_len(&interner), 1024 - s1_entry_len);
        assert_eq!(reclaimed_len(&interner), 0);

        // Drop the interned string, which should decrement the reference count to zero and then reclaim the entry.
        drop(s1);

        assert_eq!(interner.len(), 0);
        assert_eq!(available_len(&interner), 1024);
        assert_eq!(reclaimed_len(&interner), 0);
    }

    #[test]
    fn interns_to_reclaimed_entry_with_leftover() {
        // We want to intern a string initially, which takes up almost all of the capacity, and then drop it so it gets
        // reclaimed. After that, we'll intern a much smaller string which should lead to utilizing that reclaimed
        // entry, but only a part of it. Finally, we'll intern another new string.
        //
        // The point is to demonstrate that our reclamation logic is sound in terms of allowing reclaimed entries to be
        // split while the search/insertion logic is operating.
        let capacity = 256;
        let interner = create_interner(capacity);

        // We craft four strings such that the first two (`s_large` and `s_medium1`) will take up enough capacity that
        // `s_small` can't possibly be interned in the available capacity. We'll also craft `s_medium2` so it can fit
        // within the reclaimed entry for `s_large` but takes enough capacity that `s_small` cannot fit in the leftover
        // reclaimed entry that we split off.
        let s_large = "99 bottles of beer on the wall, 99 bottles of beer! take one down, pass it around, 98 bottles of beer on the wall!";
        let s_medium1 = "no act of kindness, no matter how small, is ever wasted";
        let s_medium2 = "if you want to go fast, go alone; if you want to go far, go together";
        let s_small = "are you there god? it's me, margaret";

        let phase1_available_capacity = capacity - entry_len(s_large) - entry_len(s_medium1);
        assert!(phase1_available_capacity < entry_len(s_small));
        assert!((entry_len(s_large) - entry_len(s_medium2)) < entry_len(s_small));
        assert!(entry_len(s_medium2) < entry_len(s_large));

        // Phase 1: intern our two larger strings.
        let s1 = interner.try_intern(s_large).expect("should not fail to intern");
        let s2 = interner.try_intern(s_medium1).expect("should not fail to intern");

        assert_eq!(interner.len(), 2);
        assert_eq!(available_len(&interner), phase1_available_capacity);
        assert_eq!(reclaimed_len(&interner), 0);

        // Phase 2: drop `s_large` so it gets reclaimed.
        drop(s1);
        assert_eq!(interner.len(), 1);
        assert_eq!(reclaimed_len(&interner), 1);

        // Phase 3: intern `s_medium2`, which should fit in the reclaimed entry for `s_large`. This should leave a
        // small, split off reclaimed entry.
        let s3 = interner.try_intern(s_medium2).expect("should not fail to intern");

        assert_eq!(interner.len(), 2);
        assert_eq!(reclaimed_len(&interner), 1);

        // Phase 4: intern `s_small`, which should not fit in the leftover reclaimed entry from `s_large` _or_ the
        // available capacity.
        let s4 = interner.try_intern(s_small);
        assert_eq!(s4, None);

        assert_eq!(interner.len(), 2);
        assert_eq!(reclaimed_len(&interner), 1);

        // And make sure we can still dereference the interned strings we _do_ have left:
        assert_eq!(s2.deref(), s_medium1);
        assert_eq!(s3.deref(), s_medium2);
    }

    #[test]
    fn has_reclaimed_entries_string_fits_exactly() {
        // The interner is large enough to fit two of the identically-sized strings, but not all three. We show that
        // when we drop an entry and it is reclaimed, a string of identical size should always be able to reuse that
        // reclaimed entry.
        const S1_VALUE: &str = "hello world!";
        const S2_VALUE: &str = "hello, world";
        const S3_VALUE: &str = "hello--world";

        let interner = create_interner(80);

        // Intern the first two strings, which should fit without issue.
        let s1 = interner.try_intern(S1_VALUE).expect("should not fail to intern");
        let s1_reclaimed_expected = get_reclaimed_entry_for_string(&s1);
        let _s2 = interner.try_intern(S2_VALUE).expect("should not fail to intern");

        assert_eq!(interner.len(), 2);
        assert_eq!(reclaimed_len(&interner), 0);

        // Try to intern a third string, which should fail as we don't have the space.
        let s3 = interner.try_intern(S3_VALUE);
        assert_eq!(s3, None);

        // Drop the first string, which should decrement the reference count to zero and then reclaim the entry.
        drop(s1);

        assert_eq!(interner.len(), 1);
        assert_eq!(reclaimed_len(&interner), 1);

        let s1_reclaimed = first_reclaimed_entry(&interner);
        assert_eq!(s1_reclaimed_expected, s1_reclaimed);

        // Try again to intern the third string, which should now succeed and take over the reclaimed entry entirely
        // as the strings are identical in length.
        let _s3 = interner.try_intern(S3_VALUE).expect("should not fail to intern");

        assert_eq!(interner.len(), 2);
        assert_eq!(reclaimed_len(&interner), 0);
    }

    #[test]
    fn reclaimed_entry_reuse_split_too_small() {
        // This situation is slightly contrived, but: we want to test that when there's a reclaimed entry of a certain
        // size, reusing that reclaimed entry won't lead to it being split if the resulting split entry would be too
        // "small": unable to hold another minimum-sized entry.
        //
        // We have to intern three strings to do this because we only track reclaimed entries when they're followed by
        // in-use entries, and the string we drop to create a reclaimed entry has to be big enough, but not _too_ big,
        // to hold the string we want to intern after it.
        let interner = create_interner(128);

        // Declare our strings to intern and just check some preconditions by hand.
        let s_one = "a horse, a horse, my kingdom for a horse!";
        let s_one_entry_len = entry_len(s_one);
        let s_two = "why hello there, beautiful";
        let s_two_entry_len = entry_len(s_two);
        let s_three = "real gs move in silence like lasagna";
        let s_three_entry_len = entry_len(s_three);

        assert!(s_one_entry_len <= interner.capacity_bytes());
        assert!(s_two_entry_len <= interner.capacity_bytes());
        assert!(s_one_entry_len + s_two_entry_len + s_three_entry_len > interner.capacity_bytes());
        assert!(s_one_entry_len > s_two_entry_len);
        assert!(s_three_entry_len > s_two_entry_len);
        assert!((s_one_entry_len - s_three_entry_len) < MINIMUM_ENTRY_LEN);

        // Intern the first two strings, which should fit without issue.
        let s1 = interner.try_intern(s_one).expect("should not fail to intern");
        let s1_reclaimed_expected = get_reclaimed_entry_for_string(&s1);
        let _s2 = interner.try_intern(s_two).expect("should not fail to intern");

        assert_eq!(interner.len(), 2);
        assert_eq!(reclaimed_len(&interner), 0);

        // Try to intern the third string, which should fail as we don't have the space.
        let s3 = interner.try_intern(s_three);
        assert_eq!(s3, None);

        // Drop the first string, which should decrement the reference count to zero and then reclaim the entry.
        drop(s1);

        assert_eq!(interner.len(), 1);
        assert_eq!(reclaimed_len(&interner), 1);

        let s1_reclaimed = first_reclaimed_entry(&interner);
        assert_eq!(s1_reclaimed_expected, s1_reclaimed);

        // Try again to intern the third string, which should now succeed and take over the reclaimed entry, but since
        // the remainder of the reclaimed entry after taking the necessary capacity for `s_three` is not large enough
        // (`MINIMUM_ENTRY_LEN`), we shouldn't end up splitting the reclaimed entry, and instead, `s3` should consume
        // the entire reclaimed entry.
        let s3 = interner.try_intern(s_three).expect("should not fail to intern");
        let s3_reclaimed_expected = get_reclaimed_entry_for_string(&s3);

        assert_eq!(interner.len(), 2);
        assert_eq!(reclaimed_len(&interner), 0);
        assert_eq!(s1_reclaimed_expected, s3_reclaimed_expected);
    }

    #[test]
    fn reclaimed_entry_adjacent_to_spare_capacity() {
        let interner = create_interner(128);

        // Intern two smallish strings that fit without issue.
        let s1 = interner.try_intern("hello, world!").expect("should not fail to intern");
        let s2 = interner
            .try_intern("cheeeeeehooooo!")
            .expect("should not fail to intern");
        let s1_entry_len = entry_len(&s1);
        let s2_entry_len = entry_len(&s2);

        assert_eq!(reclaimed_len(&interner), 0);
        assert_eq!(available_len(&interner), 128 - s1_entry_len - s2_entry_len);

        drop(s2);
        assert_eq!(reclaimed_len(&interner), 0);
        assert_eq!(available_len(&interner), 128 - s1_entry_len);

        drop(s1);
        assert_eq!(reclaimed_len(&interner), 0);
        assert_eq!(available_len(&interner), 128);
    }

    #[test]
    fn len_bytes_reps_active_interned_entries() {
        let interner = create_interner(256);
        let mut active_interned_entries_len = 0;
        let string_1 = "hello world!";
        let string_2 = "hello again, world";
        let string_3 = "this is another string";

        // Intern a string.
        let s1 = interner.try_intern(string_1).expect("should not fail to intern");
        active_interned_entries_len += entry_len(string_1);
        assert_eq!(interner.len(), 1);
        assert_eq!(interner.len_bytes(), active_interned_entries_len);

        // Intern a second string.
        let _s2 = interner.try_intern(string_2).expect("should not fail to intern");
        active_interned_entries_len += entry_len(string_2);
        assert_eq!(interner.len(), 2);
        assert_eq!(interner.len_bytes(), active_interned_entries_len);

        // Drop the first string.
        drop(s1);
        active_interned_entries_len -= entry_len(string_1);
        assert_eq!(interner.len(), 1);
        assert_eq!(interner.len_bytes(), active_interned_entries_len);

        // Intern a new string.
        let _s3 = interner.try_intern(string_3).expect("should not fail to intern");
        active_interned_entries_len += entry_len(string_3);
        assert_eq!(interner.len(), 2);
        assert_eq!(interner.len_bytes(), active_interned_entries_len);
    }

    proptest! {
        #[test]
        #[cfg_attr(miri, ignore)]
        fn property_test_entry_count_accurate(
            strs in test_support::arb_alphanum_strings(1..=128, 16..=512),
            indices in arb_vec(any::<Index>(), 1..=1000),
        ) {
            // Create an interner with enough capacity to hold all of the strings we've generated. This is the maximum
            // string size multiplied by the number of strings we've generated... plus a little constant factor per
            // string to account for the entry header. The header size differs between interner implementations, which
            // is exactly why the interner construction (and thus this `property_test_`-named entry point) stays
            // per-implementation while the invariant check is shared.
            const ENTRY_SIZE: usize = 128 + HEADER_LEN;
            let interner = create_interner(ENTRY_SIZE * indices.len());

            test_support::assert_entry_count_matches_unique_strings(&interner, &strs, &indices)?;
        }
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use std::ops::Deref;

    use super::{
        tests::{create_interner, get_reclaimed_entry_for_string},
        *,
    };

    #[test]
    fn concurrent_drop_and_intern() {
        fn reclaimed_entries(interner: &GenericMapInterner) -> Vec<ReclaimedEntry> {
            interner.state.shards[0].lock().unwrap().chunks[0]
                .as_ref()
                .unwrap()
                .reclaimed
                .iter()
                .copied()
                .collect()
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
            let interner = create_interner(1024);
            let t2_interner = interner.clone();

            // Intern the string from thread T1.
            let t1_interned_s = interner
                .try_intern(STRING_TO_INTERN)
                .expect("should not fail to intern");
            assert_eq!(t1_interned_s.deref(), STRING_TO_INTERN);
            let t1_reclaimed_entry = get_reclaimed_entry_for_string(&t1_interned_s);

            // Spawn thread T2, which tries to intern the same string and returns the handle to it.
            let t2_result = loom::thread::spawn(move || {
                let interned_s = t2_interner
                    .try_intern(STRING_TO_INTERN)
                    .expect("should not fail to intern");
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
            let reclaimed_entries = reclaimed_entries(&interner);
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

    #[test]
    fn concurrent_resurrect_and_double_drop() {
        // Regression test for a double-reclamation race.
        //
        // Unlike `concurrent_drop_and_intern`, where the second thread keeps its handle alive, here the second thread
        // interns the string _and_ drops it within the race window. This models the dangerous interleaving that a
        // drop could cause if it decremented the count to zero before it took the shard lock:
        //
        //   1. T1 drops the last reference: `refs` 1 -> 0 (but T1 hasn't taken the shard lock yet).
        //   2. T2 calls `try_intern`, finds the still-mapped entry, and resurrects it: `refs` 0 -> 1.
        //   3. T2 drops its handle: `refs` 1 -> 0.
        //   4. T1 and T2 each take the lock, and BOTH mark the same slot for reclamation.
        //
        // `StringState::drop` prevents this, because it makes the final decrement while it holds the shard lock. A
        // double reclamation either underflows `len_bytes` (a panic under overflow-checks, which the loom test profile
        // enables) or inserts a spurious/overlapping reclaimed entry. After every handle to the single interned string
        // has been dropped, the only correct end state is a fully empty interner.
        const STRING_TO_INTERN: &str = "hello, world!";

        loom::model(|| {
            let interner = create_interner(1024);
            let t2_interner = interner.clone();

            // T1 interns the string and holds the only reference.
            let t1_interned_s = interner
                .try_intern(STRING_TO_INTERN)
                .expect("should not fail to intern");

            // T2 interns the same string and immediately drops it. The interesting orderings are those where this runs
            // while T1's drop is between its failed shared decrement and its final decrement under the shard lock.
            let t2 = loom::thread::spawn(move || {
                let t2_interned_s = t2_interner
                    .try_intern(STRING_TO_INTERN)
                    .expect("should not fail to intern");
                assert_eq!(t2_interned_s.deref(), STRING_TO_INTERN);
                drop(t2_interned_s);
            });

            // T1 drops its (originally last) reference.
            drop(t1_interned_s);

            t2.join().expect("should not fail to join T2");

            // Every handle has been dropped, so the slot must have been reclaimed exactly once: the interner is fully
            // empty, with consistent accounting and no leftover reclaimed entries. A double reclaim breaks at least one
            // of these (and typically panics first through the `len_bytes` underflow or the `entries.remove` unwrap).
            let state = interner.state.shards[0].lock().unwrap();
            let chunk = state.chunks[0].as_ref().unwrap();
            assert!(state.entries.is_empty(), "entries map should be empty");
            assert_eq!(
                state.len_bytes, 0,
                "len_bytes should be zero (it is double-subtracted on a double reclaim)"
            );
            assert_eq!(chunk.offset, 0, "offset should wind back to zero");
            assert!(
                chunk.reclaimed.is_empty(),
                "no reclaimed entries should remain (a double reclaim inserts a spurious/overlapping entry)"
            );
        });
    }
}
