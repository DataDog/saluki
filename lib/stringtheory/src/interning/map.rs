// TODO: If we're trying to add a reclaimed entry, and that entry comes immediately before any available capacity, and
// the entry is aligned... we should skip actually tracking it as a reclaimed entry and instead simply decrement `len`,
// adding the entry back to the available capacity at the end of the data buffer.
//
// This avoids specific fragmentation where the reclaimed entry might be too small for a string, requiring us to spill
// to the buffer, and then we waste space doing so when we could have potentially had a more optimal usage if the
// available capacity was simply larger.

#[cfg(not(feature = "loom"))]
use std::sync::{
    atomic::{
        AtomicUsize,
        Ordering::{AcqRel, Acquire},
    },
    Arc, Mutex,
};
use std::{collections::HashMap, num::NonZeroUsize, ptr::NonNull};

#[cfg(feature = "loom")]
use loom::sync::{
    atomic::{
        AtomicUsize,
        Ordering::{AcqRel, Acquire},
    },
    Arc, Mutex,
};

use super::{
    helpers::{layout_for_data, PackedLengthCapacity},
    InternedString, Interner,
};
use crate::interning::helpers::{aligned_string, ReclaimedEntries, ReclaimedEntry};

const HEADER_LEN: usize = std::mem::size_of::<EntryHeader>();
const HEADER_ALIGN: usize = std::mem::align_of::<EntryHeader>();

/// The minimum possible length of an entry.
///
/// For any entry in the interner, there is already an `EntryHeader` followed by the string data itself. In order to
/// ensure that entries can be written contiguously, we additionally ensure that the number of bytes we utilize for the
/// string data is aligned at least as much as `EntryHeader` itself.
///
/// This means that the minimum possible length of an entry, or the minimum number of bytes a valid entry could consume,
/// is the length of the header plus the alignment of the header.
const MINIMUM_ENTRY_LEN: usize = HEADER_LEN + HEADER_ALIGN;

#[derive(Debug)]
pub(crate) struct StringState {
    interner: Arc<Mutex<InternerState>>,
    header: NonNull<EntryHeader>,
}

impl StringState {
    #[inline]
    pub const fn as_str(&self) -> &str {
        // SAFETY: We ensure `self.header` is well-aligned and points to an initialized `EntryHeader` value when creating `StringState`.
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
        // and points to an initialized `EntryHeader` value.
        let header = unsafe { self.header.as_ref() };
        if header.decrement_active_refs() {
            // We decremented the reference count to zero, so try to mark this entry for reclamation.
            let mut interner = self.interner.lock().unwrap();
            interner.mark_for_reclamation(self.header);
        }
    }
}

/// Metadata about an interner entry.
///
/// `EntryHeader` represents the smallest amount of information about an interned entry that is needed to support both
/// lookup of existing interned strings, as well as the ability to reclaim space in the interner when an entry is no
/// longer in use.
struct EntryHeader {
    /// The number of active references to this entry.
    ///
    /// Only incremented by the interner itself, and decremented by `InternedString` when it is dropped.
    refs: AtomicUsize,

    /// Combined length/capacity of the entry, in terms of the string itself.
    ///
    /// Notably, this does _not_ include the length of the header itself. For example, an entry holding the string
    /// "hello, world!" has a string length of 13 bytes, but since we have to pad out to meet our alignment requirements
    /// for `EntryHeader`, we would end up with a capacity of 16 bytes. As such, `EntryHeader::len` would report `13`,
    /// while `EntryHeader::capacity` would report `16`. Likewise, `EntryHeader::entry_len` would report `40`,
    /// accounting for the string capacity (16) as well as the header length itself (24).
    ///
    /// As explained in the description of `PackedLengthCapacity`, this does mean strings can't be larger than ~4GB on
    /// 64-bit platforms, which is not a problem we have.
    len_cap: PackedLengthCapacity,
}

impl EntryHeader {
    /// Creates a tombstone entry with the given capacity.
    ///
    /// This is to allow for updating a region in the data buffer, which has been reclaimed, such that it is
    /// identifiable as being unused.
    fn tombstone(entry: ReclaimedEntry) -> Self {
        // The usable capacity for a reclaimed entry is the full capacity minus the size of `EntryHeader` itself, as
        // reclaimed entries represent the _entire_ region in the data buffer, but `EntryHeader` only cares about the
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
        // reclaimed entries represent the _entire_ region in the data buffer, but `EntryHeader` only cares about the
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
    /// the number of bytes that would be consumed in the data buffer.
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

// SAFETY: We don't take references to the entry header pointer that outlast `StringState`, and the only modification we
// do to the entry header is through atomic operations, so it's safe to both send and share `StringState` between
// threads.
unsafe impl Send for StringState {}
unsafe impl Sync for StringState {}

#[derive(Debug)]
struct InternerStorage {
    // Direct pieces of our buffer allocation.
    ptr: NonNull<u8>,
    offset: usize,
    capacity: NonZeroUsize,
    len: usize,

    // Markers for entries that can be reused.
    reclaimed: ReclaimedEntries,
}

impl InternerStorage {
    fn with_capacity(capacity: NonZeroUsize) -> Self {
        assert!(
            capacity.get() <= isize::MAX as usize,
            "capacity would overflow isize::MAX, which violates layout constraints"
        );

        // Allocate our data buffer. This is the main backing allocation for all interned strings, and is well-aligned
        // for `EntryHeader`.
        //
        // SAFETY: `layout_for_data` ensures the layout is non-zero.
        let data_layout = layout_for_data::<EntryHeader>(capacity);
        let data_ptr = unsafe { std::alloc::alloc(data_layout) };
        let ptr = match NonNull::new(data_ptr) {
            Some(ptr) => ptr,
            None => std::alloc::handle_alloc_error(data_layout),
        };

        Self {
            ptr,
            offset: 0,
            capacity,
            len: 0,
            reclaimed: ReclaimedEntries::new(),
        }
    }

    #[cfg(test)]
    /// Returns the total number of unused bytes that are available for interning.
    fn available(&self) -> usize {
        self.capacity.get() - self.len
    }

    /// Returns the total number of bytes of continguous, unoccupied space at the end of the data buffer.
    fn available_unoccupied(&self) -> usize {
        self.capacity.get() - self.offset
    }

    fn get_entry_ptr(&self, offset: usize) -> NonNull<EntryHeader> {
        debug_assert!(
            offset + MINIMUM_ENTRY_LEN <= self.capacity.get(),
            "offset would point to entry that cannot possibly avoid extending past end of data buffer"
        );

        // SAFETY: The caller is responsible for ensuring that `offset` is within the bounds of the data buffer, and
        // that `offset` is well-aligned for `EntryHeader`.
        let entry_ptr = unsafe { self.ptr.as_ptr().add(offset).cast::<EntryHeader>() };
        debug_assert!(entry_ptr.is_aligned(), "entry header pointer must be well-aligned");

        // SAFETY: `entry_ptr` is derived from `self.ptr`, which itself is `NonNull<u8>`, and the caller is responsible
        // for ensuring that `offset` is within the bounds of the data buffer, so we know `entry_ptr` is non-null.
        unsafe { NonNull::new_unchecked(entry_ptr) }
    }

    fn write_entry(&mut self, offset: usize, entry_header: EntryHeader, s: &str) -> NonNull<EntryHeader> {
        debug_assert_eq!(
            entry_header.len(),
            s.len(),
            "entry header length must match string length"
        );

        let entry_ptr = self.get_entry_ptr(offset);
        let entry_len = entry_header.entry_len();

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

        self.len += entry_len;

        entry_ptr
    }

    fn write_to_unoccupied(&mut self, s: &str) -> (usize, NonNull<EntryHeader>) {
        let entry_header = EntryHeader::from_string(s);

        // Write the entry to the end of the data buffer.
        let entry_offset = self.offset;
        self.offset += entry_header.entry_len();

        (entry_offset, self.write_entry(entry_offset, entry_header, s))
    }

    fn write_to_reclaimed_entry(&mut self, entry: ReclaimedEntry, s: &str) -> (usize, NonNull<EntryHeader>) {
        let entry_offset = entry.offset();
        let (entry_header, maybe_split_entry) = EntryHeader::from_reclaimed_entry(entry, s);

        // If we had enough capacity in the reclaimed entry to hold this string _and_ potentially hold another entry, we
        // split it off and store that remainder entry.
        if let Some(split_entry) = maybe_split_entry {
            self.add_reclaimed(split_entry);
        }

        // Write the entry in place of the reclaimed entry.
        (entry_offset, self.write_entry(entry_offset, entry_header, s))
    }

    fn add_reclaimed(&mut self, entry: ReclaimedEntry) {
        // Reclamation is a two-step process: first, we have to actually keep track of the reclaimed entry, which
        // potentially involves merging adjacent reclaimed entries, and then once all of that has happened, we tombstone
        // the entry (whether merged or not).
        let merged_entry = self.reclaimed.insert(entry);
        self.clear_reclaimed_entry(merged_entry);
    }

    fn clear_reclaimed_entry(&mut self, entry: ReclaimedEntry) {
        let entry_ptr = self.get_entry_ptr(entry.offset);

        // Write the entry tombstone itself, which clears out the hash and sets the reference count to zero.
        //
        // SAFETY: We know that `entry_ptr` is valid for writes (reclaimed entries are, by definition, inactive regions
        // in the data buffer) and is well-aligned for `EntryHeader`.
        let tombstone = EntryHeader::tombstone(entry);
        let str_cap = tombstone.capacity();

        unsafe {
            entry_ptr.as_ptr().write(EntryHeader::tombstone(entry));
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
            let str_buf = std::slice::from_raw_parts_mut(str_ptr, str_cap);
            str_buf.fill(0x21);
        }
    }

    fn mark_for_reclamation(&mut self, header_ptr: NonNull<EntryHeader>) {
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
        let entry_len = header.entry_len();

        let entry = ReclaimedEntry::new(entry_offset as usize, entry_len);
        self.add_reclaimed(entry);
        self.len -= entry_len;
    }
}

impl Drop for InternerStorage {
    fn drop(&mut self) {
        // SAFETY: We allocated this buffer with the global allocator, and we're generating the same layout that was
        // used to allocate it in the first place.
        unsafe {
            std::alloc::dealloc(self.ptr.as_ptr(), layout_for_data::<EntryHeader>(self.capacity));
        }
    }
}

#[derive(Debug)]
struct InternerState {
    // Backing storage for the interned strings.
    storage: InternerStorage,

    // Active entries in the interner.
    entries: HashMap<&'static str, usize>,
}

impl InternerState {
    /// Creates a new `InternerState` with a pre-allocated buffer that has the given capacity.
    pub fn with_capacity(capacity: NonZeroUsize) -> Self {
        Self {
            storage: InternerStorage::with_capacity(capacity),
            entries: HashMap::new(),
        }
    }

    fn mark_for_reclamation(&mut self, header_ptr: NonNull<EntryHeader>) {
        // See if the reference count is zero.
        //
        // Only interned string values (the frontend handle that wraps the pointer to a specific entry) can decrement
        // the reference count for their specific entry when dropped, and only `InternerState` -- with its access
        // mediated through a mutex -- can increment the reference count for entries. This means that if the reference
        // count is zero, then we know that nobody else is holding a reference to this entry, and no concurrent call to
        // `try_intern` could be updating the reference count, either... so it's safe to be marked as reclaimed.
        //
        // SAFETY: The caller is responsible for ensuring that `header_ptr` is well-aligned and points to an initialized
        // `EntryHeader` value.
        let header = unsafe { header_ptr.as_ref() };
        if !header.is_active() {
            // Remove the entry from the entries map first before we reclaim the entry, since doing so overwrites the
            // entry data in the data buffer.
            //
            // SAFETY: The caller is responsible for ensuring that they've given us a pointer to an `EntryHeader` that
            // was acquired from this interner.
            let entry_str = unsafe { get_entry_string(header_ptr) };
            self.entries.remove(entry_str);

            self.storage.mark_for_reclamation(header_ptr);
        }
    }

    fn try_intern(&mut self, s: &str) -> Option<NonNull<EntryHeader>> {
        // We can only intern strings with a size that fits within a packed length/capacity value, so if `s` is larger
        // than that, we can't intern it, and there's existing entry we could have for it either.
        if s.len() > PackedLengthCapacity::maximum_value() {
            return None;
        }

        // Try and find an existing entry for this string.
        if let Some(entry_offset) = self.entries.get(s) {
            let entry_ptr = self.storage.get_entry_ptr(*entry_offset);

            let header = unsafe { entry_ptr.as_ref() };
            header.increment_active_refs();

            return Some(entry_ptr);
        }

        let required_cap = EntryHeader::len_for(s);

        // We didn't find an existing entry, so we're going to intern it.
        //
        // First, try and see if we have a reclaimed entry that can fit this string. If nothing suitable is found, or we
        // have no reclaimed entries, then we'll just try to fit it in the remaining capacity of our data buffer.
        let maybe_reclaimed_entry = self.storage.reclaimed.take_if(|entry| entry.capacity() >= required_cap);
        let (entry_offset, entry_ptr) = if let Some(reclaimed_entry) = maybe_reclaimed_entry {
            self.storage.write_to_reclaimed_entry(reclaimed_entry, s)
        } else if required_cap <= self.storage.available_unoccupied() {
            self.storage.write_to_unoccupied(s)
        } else {
            // We don't have enough space to intern this string at all, so we'll just return `None`.
            return None;
        };

        // SAFETY: Callers of `get_entry_string` are responsible for ensuring that the chosen lifetime of the string is
        // valid with respect to ensuring that the underlying entry lives long enough, and that the string data is valid
        // UTF-8 from as long as the reference is live.
        //
        // We're creating a `'static` reference here, which is generally frowned upon when the data is in fact not
        // `'static`, but this is safe because we never leak this lifetime outside of the interner, and we ensure that
        // we don't actually keep this reference around longer than the entry itself, as we only need a `'static`
        // reference to use it as the key in the entries map.
        let entry_str = unsafe { get_entry_string(entry_ptr) };
        self.entries.insert(entry_str, entry_offset);

        Some(entry_ptr)
    }
}

// SAFETY: We don't take references to the data buffer pointer that outlast `InternerState`, and all access to
// `InternerState` itself is mediated through a mutex, so we're safe to send it around and share it between threads.
unsafe impl Send for InternerState {}
unsafe impl Sync for InternerState {}

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
pub struct GenericMapInterner {
    state: Arc<Mutex<InternerState>>,
}

impl GenericMapInterner {
    /// Creates a new `GenericMapInterner` with the given capacity.
    ///
    /// The given capacity will potentially be rounded up by a small number of bytes (up to 7) in order to ensure the
    /// backing buffer is properly aligned.
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            state: Arc::new(Mutex::new(InternerState::with_capacity(capacity))),
        }
    }
}

impl Interner for GenericMapInterner {
    fn is_empty(&self) -> bool {
        self.state.lock().unwrap().entries.is_empty()
    }

    fn len(&self) -> usize {
        self.state.lock().unwrap().entries.len()
    }

    fn len_bytes(&self) -> usize {
        self.state.lock().unwrap().storage.len
    }

    fn capacity_bytes(&self) -> usize {
        self.state.lock().unwrap().storage.capacity.get()
    }

    fn try_intern(&self, s: &str) -> Option<InternedString> {
        let header = {
            let mut state = self.state.lock().unwrap();
            state.try_intern(s)?
        };

        Some(InternedString::from(StringState {
            interner: Arc::clone(&self.state),
            header,
        }))
    }
}

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

#[inline]
const unsafe fn get_entry_string<'a>(header_ptr: NonNull<EntryHeader>) -> &'a str {
    let (s_ptr, s_len) = get_entry_string_parts(header_ptr);

    // SAFETY: We depend on `get_entry_string_parts` to give us a valid pointer and length for the string.
    std::str::from_utf8_unchecked(std::slice::from_raw_parts(s_ptr.as_ptr() as *const _, s_len))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        ops::{Deref as _, RangeInclusive},
    };

    use prop::sample::Index;
    use proptest::{
        collection::{hash_set, vec as arb_vec},
        prelude::*,
    };

    use super::*;
    use crate::interning::InternedStringState;

    fn create_interner(capacity: usize) -> GenericMapInterner {
        assert!(capacity > 0, "capacity must be greater than zero");
        GenericMapInterner::new(NonZeroUsize::new(capacity).unwrap())
    }

    fn available_len(interner: &GenericMapInterner) -> usize {
        interner.state.lock().unwrap().storage.available()
    }

    fn reclaimed_len(interner: &GenericMapInterner) -> usize {
        interner.state.lock().unwrap().storage.reclaimed.len()
    }

    fn first_reclaimed_entry(interner: &GenericMapInterner) -> ReclaimedEntry {
        interner.state.lock().unwrap().storage.reclaimed.first().unwrap()
    }

    const fn entry_len(s: &str) -> usize {
        EntryHeader::len_for(s)
    }

    fn get_reclaimed_entry_for_string(s: &InternedString) -> ReclaimedEntry {
        let state = match &s.state {
            InternedStringState::GenericMap(state) => state,
            _ => panic!("unexpected string state"),
        };

        let ptr = state.interner.lock().unwrap().storage.ptr.as_ptr();
        let header = unsafe { state.header.as_ref() };
        let offset = unsafe { state.header.as_ptr().cast::<u8>().offset_from(ptr) as usize };
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
        let s1_reclaimed_expected = get_reclaimed_entry_for_string(&s1);

        assert_eq!(interner.len(), 1);
        assert_eq!(reclaimed_len(&interner), 0);

        // Drop the interned string, which should decrement the reference count to zero and then reclaim the entry.
        drop(s1);

        assert_eq!(interner.len(), 0);
        assert_eq!(reclaimed_len(&interner), 1);

        let s1_reclaimed = first_reclaimed_entry(&interner);
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
        // The shard is large enough for one "hello world!"/"hello, world", but not two of them.
        let interner = create_interner(48);

        // Intern the first string, which should fit without issue.
        let s1 = interner.try_intern("hello world!").expect("should not fail to intern");
        let s1_reclaimed_expected = get_reclaimed_entry_for_string(&s1);

        assert_eq!(interner.len(), 1);
        assert_eq!(reclaimed_len(&interner), 0);

        // Try to intern the second string, which should fail as we don't have the space.
        let s2 = interner.try_intern("hello, world");
        assert_eq!(s2, None);

        // Drop the first string, which should decrement the reference count to zero and then reclaim the entry.
        drop(s1);

        assert_eq!(interner.len(), 0);
        assert_eq!(reclaimed_len(&interner), 1);

        let s1_reclaimed = first_reclaimed_entry(&interner);
        assert_eq!(s1_reclaimed_expected, s1_reclaimed);

        // Try again to intern the second string, which should now succeed and take over the reclaimed entry entirely
        // as the strings are identical in length.
        let _s2 = interner.try_intern("hello, world").expect("should not fail to intern");

        assert_eq!(interner.len(), 1);
        assert_eq!(reclaimed_len(&interner), 0);
    }

    #[test]
    fn reclaimed_entry_reuse_split_too_small() {
        // Create an interner that's big enough to fit either string individually, but not at the same time.
        let interner = create_interner(72);

        // Declare our strings to intern and just check some preconditions by hand.
        let s_one = "a horse, a horse, my kingdom for a horse!";
        let s_one_entry_len = entry_len(s_one);
        let s_two = "why hello there, beautiful";
        let s_two_entry_len = entry_len(s_two);

        assert!(s_one_entry_len <= interner.capacity_bytes());
        assert!(s_two_entry_len <= interner.capacity_bytes());
        assert!(s_one_entry_len + s_two_entry_len > interner.capacity_bytes());
        assert!(s_one_entry_len > s_two_entry_len);
        assert!((s_one_entry_len - s_two_entry_len) < MINIMUM_ENTRY_LEN);

        // Intern the first string, which should fit without issue.
        let s1 = interner.try_intern(s_one).expect("should not fail to intern");
        let s1_reclaimed_expected = get_reclaimed_entry_for_string(&s1);

        assert_eq!(interner.len(), 1);
        assert_eq!(reclaimed_len(&interner), 0);

        // Try to intern the second string, which should fail as we don't have the space.
        let s2 = interner.try_intern(s_two);
        assert_eq!(s2, None);

        // Drop the first string, which should decrement the reference count to zero and then reclaim the entry.
        drop(s1);

        assert_eq!(interner.len(), 0);
        assert_eq!(reclaimed_len(&interner), 1);

        let s1_reclaimed = first_reclaimed_entry(&interner);
        assert_eq!(s1_reclaimed_expected, s1_reclaimed);

        // Try again to intern the second string, which should now succeed and take over the reclaimed entry, but since
        // the remainder of the reclaimed entry after taking the necessary capacity for `s_two` is not large enough
        // (`MINIMUM_ENTRY_LEN`), we shouldn't end up splitting the reclaimed entry, and instead, `s2` should consume
        // the entire reclaimed entry.
        let s2 = interner.try_intern(s_two).expect("should not fail to intern");
        let s2_reclaimed_expected = get_reclaimed_entry_for_string(&s2);

        assert_eq!(interner.len(), 1);
        assert_eq!(reclaimed_len(&interner), 0);
        assert_eq!(s1_reclaimed_expected, s2_reclaimed_expected);
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
            let interner = create_interner(ENTRY_SIZE * indices.len());

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
        fn reclaimed_entries(interner: &GenericMapInterner) -> Vec<ReclaimedEntry> {
            interner
                .state
                .lock()
                .unwrap()
                .storage
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
}
