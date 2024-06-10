// TODO: If we're trying to add a reclaimed entry, and that entry comes immediately before any available capacity, and
// the entry is aligned... we should skip actually tracking it as a reclaimed entry and instead simply decrement `len`,
// adding the entry back to the available capacity at the end of the data buffer.
//
// This avoids specific fragmentation where the reclaimed entry might be too small for a string, requiring us to spill
// to the buffer, and then we waste space doing so when we could have potentially had a more optimal usage if the
// available capacity was simply larger.

#![allow(dead_code)]
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

#[cfg(not(feature = "loom"))]
use std::sync::{atomic::AtomicUsize, Arc, Mutex};

const HEADER_LEN: usize = std::mem::size_of::<EntryHeader>();
const HEADER_ALIGN: usize = std::mem::align_of::<EntryHeader>();

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
    interner: Arc<Mutex<InternerState>>,
    header: NonNull<EntryHeader>,
}

impl StringState {
    #[cfg(test)]
    fn get_entry_header(&self) -> &EntryHeader {
        unsafe { self.header.as_ref() }
    }

    fn get_entry<'a>(&'a self) -> &'a str {
        // NOTE: We're specifically upholding a safety variant here by tying the lifetime of the string reference to our
        // own lifetime, as the lifetime of the string reference *cannot* exceed the lifetime of the interner state,
        // which we keep alive by holding `Arc<Mutex<InternerState>>`.
        get_entry_string::<'a>(self.header.as_ptr())
    }
}

impl Drop for StringState {
    fn drop(&mut self) {
        // SAFETY: We know that `self.header` is well-aligned for `EntryHeader` and is initialized.
        let header = unsafe { self.header.as_ref() };
        if header.refs.fetch_sub(1, AcqRel) == 1 {
            // We decremented the reference count to zero, so try to mark this entry for reclamation.
            let mut interner = self.interner.lock().unwrap();
            interner.mark_for_reclamation(self.header);
        }
    }
}

// SAFETY: We don't take references to the entry header pointer that outlast `StringState`, and we don't use it to
// modify the entry header at all, so we're safe to send it around and share it between threads.
unsafe impl Send for StringState {}
unsafe impl Sync for StringState {}

struct EntryHeader {
    hash: u64,
    refs: AtomicUsize,
    len: usize,
}

impl EntryHeader {
    fn size(&self) -> usize {
        // SAFETY: Our layout for an entry is `EntryHeader` + the string bytes appended directly at the end of the
        // header. Strings are just byte slices, so their alignment is 1. This means that all we need to care about is
        // aligning things properly for `EntryHeader` itself.
        //
        // We also pad the layout so that we know that we can write another entry immediately after this one, and that
        // entry's header will be implicitly well-aligned.
        unsafe {
            Layout::from_size_align_unchecked(HEADER_LEN + self.len, HEADER_ALIGN)
                .pad_to_align()
                .size()
        }
    }

    fn is_active(&self) -> bool {
        self.refs.load(Acquire) != 0
    }
}

#[derive(Clone, Copy, Debug)]
struct ReclaimedEntry {
    start: usize,
    end: usize,
    aligned: bool,
}

impl ReclaimedEntry {
    const fn empty() -> Self {
        Self {
            start: 0,
            end: 0,
            aligned: false,
        }
    }

    const fn new(start: usize, end: usize, aligned: bool) -> Self {
        Self { start, end, aligned }
    }

    const fn is_aligned(&self) -> bool {
        self.aligned
    }

    const fn capacity(&self) -> usize {
        self.end + 1 - self.start
    }

    const fn followed_by(&self, other: &Self) -> bool {
        self.end + 1 == other.start
    }
}

impl PartialEq for ReclaimedEntry {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start && self.end == other.end
    }
}

impl Eq for ReclaimedEntry {}

impl PartialOrd for ReclaimedEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.start.cmp(&other.start))
    }
}

impl Ord for ReclaimedEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.start.cmp(&other.start)
    }
}

#[derive(Debug)]
struct InternerState {
    // Direct pieces of our buffer allocation.
    ptr: NonNull<u8>,
    len: usize,
    capacity: NonZeroUsize,

    // Number of active entries (strings) in the interner.
    entries: usize,

    // Markers for entries that can be reused.
    reclaimed: BTreeSet<ReclaimedEntry>,
}

impl InternerState {
    /// Creates a new `InternerState` with a pre-allocated buffer that has the given capacity.
    pub fn with_capacity(capacity: NonZeroUsize) -> Self {
        // Allocate our data buffer.
        //
        // This holds our entries -- header and string bytes -- and is well-aligned for `EntryHeader`.
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

    fn available(&self) -> usize {
        self.capacity.get() - self.len
    }

    fn find_entry(&self, hash: u64, s: &str) -> Option<NonNull<EntryHeader>> {
        // We'll iterate over all entries in the buffer, reclaimed or not, to see if we can find a match for the given string.
        let ptr = self.ptr.as_ptr();
        let mut offset = 0;
        let mut i = 0;

        while i < self.entries {
            // Construct a pointer to the entry at `offset`, and get a reference to the header value.
            //
            // SAFETY: We base `offset` either on starting at 0, or by adding the size of the previous entry, thus we
            // know that if we don't iterate more times than we have entries, that the offset will always be within the
            // the bounds of our data buffer _and_ that the entry heaer will have been previously initiatlized.
            let header_ptr = unsafe { ptr.add(offset).cast::<EntryHeader>() };
            debug_assert_eq!(
                header_ptr.cast::<u8>().align_offset(HEADER_ALIGN),
                0,
                "entry header pointer must be well-aligned"
            );
            let header = unsafe { header_ptr.as_ref().unwrap() };

            // See if this entry is active or not. If it's active, then we'll quickly check the hash/length of the
            // string to see if this is likely to be a match for `s`.
            if header.is_active() && header.hash == hash && header.len == s.len() {
                // As a final check, we make sure that the entry string and `s` are equal. If they are, then we
                // have an exact match and will return the entry.
                let s_entry = get_entry_string(header_ptr);
                if s_entry == s {
                    // Increment the reference count for this entry so it's not prematurely reclaimed.
                    header.refs.fetch_add(1, AcqRel);

                    // SAFETY: `header_ptr` cannot be null otherwise we would have already panicked when creating a
                    // reference to it above.
                    return Some(unsafe { NonNull::new_unchecked(header_ptr) });
                }
            }

            // Either this was a reclaimed entry or we didn't have a match, so we move on to the next entry.
            offset += header.size();
            i += 1;
        }

        None
    }

    fn write_entry(&mut self, offset: usize, hash: u64, s: &str) -> NonNull<EntryHeader> {
        let s_buf = s.as_bytes();

        // Write the entry header.
        //
        // SAFETY: The caller is responsible for ensuring that `offset` is within the bounds of the data buffer, that
        // the there is enough capacity within the underlying allocation to write `HEADER_LEN` bytes, and that the
        // resulting entry pointer will be well-aligned for `EntryHeader`.
        let header_ptr = unsafe {
            let ptr = self.ptr.as_ptr().add(offset).cast::<EntryHeader>();
            debug_assert_eq!(
                ptr.cast::<u8>().align_offset(HEADER_ALIGN),
                0,
                "entry header pointer must be well-aligned"
            );
            ptr.write(EntryHeader {
                hash,
                refs: AtomicUsize::new(1),
                len: s_buf.len(),
            });

            NonNull::new_unchecked(ptr)
        };

        // Write the string.
        //
        // SAFETY: The caller is responsible for ensuring that `offset` is within the bounds of the data buffer, and
        // that the there is enough capacity within the underlying allocation to write `HEADER_LEN + s.len()` bytes.
        let s_start = offset + HEADER_LEN;
        let entry_s_buf = unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().add(s_start), s_buf.len()) };
        entry_s_buf.copy_from_slice(s_buf);

        // Update the number of entries we have.
        self.entries += 1;

        header_ptr
    }

    fn get_offsets_for_header(&self, header: &EntryHeader) -> (usize, usize) {
        // Zero out the string bytes in the data buffer.
        unsafe {
            // Skip over the header itself so that we just zero out the string bytes.
            let data_ptr = self.ptr.as_ptr();
            let header_end_ptr = NonNull::from(header).as_ptr().add(1).cast::<u8>();
            let str_ptr = data_ptr.offset(header_end_ptr.offset_from(data_ptr.cast_const()));
            let str_buf = std::slice::from_raw_parts_mut(str_ptr, header.len);
            str_buf.fill(0);
        }

        // Get the offset of the header within the data buffer.
        //
        // SAFETY: The caller is responsible for ensuring the entry header reference belongs to this interner. If that
        // is upheld, then we know that entry header belongs to our data buffer, and that the pointer to the entry
        // header is not less than the base pointer of the data buffer, ensuring the offset is non-negative.
        let entry_offset = unsafe {
            NonNull::from(header)
                .cast::<u8>()
                .as_ptr()
                .offset_from(self.ptr.as_ptr().cast_const())
        };
        debug_assert!(entry_offset >= 0, "entry offset must be non-negative");

        (entry_offset as usize, entry_offset as usize + header.size() - 1)
    }

    fn add_reclaimed_from_header(&mut self, header: &EntryHeader) {
        let (start, end) = self.get_offsets_for_header(header);
        self.add_reclaimed(start, end, true);
    }

    fn add_reclaimed(&mut self, start: usize, end: usize, aligned: bool) {
        // Track this reclaimed entry.
        let curr_entry = ReclaimedEntry::new(start, end, aligned);
        self.reclaimed.insert(curr_entry);

        // While we're here, we'll do an incremental merging of reclaimed entries. This lets us avoid fragmentation
        // that, over time, would shrink the effective capacity of the interner in a non-recoverable way.

        // Iterate over all of the reclaimed entries until we find the entry we just added, and then go one more entry
        // past that, if possible.
        //
        // We're looking to find the entries that occur immediate before and after, if they exist, to figure out if
        // they're adjacent or not and can potentially be merged.
        let mut found_current = false;
        let mut maybe_prev_entry = None;
        let mut maybe_next_entry = None;

        for entry in self.reclaimed.iter() {
            match (found_current, entry == &curr_entry) {
                // We haven't found our current entry yet, and this one directly precedes it.
                (false, false) => {
                    if entry.followed_by(&curr_entry) {
                        maybe_prev_entry = Some(*entry);
                    }
                }
                // We've already found our current entry, so we know to stop iterating after one more entry.
                (false, true) => found_current = true,
                // We've already found our current entry, and this one directly follows it.
                (true, false) => {
                    if curr_entry.followed_by(entry) {
                        maybe_next_entry = Some(*entry);
                        break;
                    }
                }
                // Can't have found our current entry and then find it again.
                (true, true) => unreachable!(),
            }
        }

        // If we had no adjacent previous or next entries, then we're done.
        if maybe_prev_entry.is_none() && maybe_next_entry.is_none() {
            return;
        }

        self.reclaimed.remove(&curr_entry);

        let (new_start, new_aligned) = match maybe_prev_entry {
            None => (curr_entry.start, curr_entry.aligned),
            Some(prev_entry) => {
                self.reclaimed.remove(&prev_entry);
                (prev_entry.start, prev_entry.aligned)
            }
        };

        let new_end = match maybe_next_entry {
            None => curr_entry.end,
            Some(next_entry) => {
                self.reclaimed.remove(&next_entry);
                next_entry.end
            }
        };

        self.reclaimed
            .insert(ReclaimedEntry::new(new_start, new_end, new_aligned));
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
        // SAFETY: The caller is responsible for ensuring that `header_ptr` is a valid pointer to an `EntryHeader`
        // value: well-aligned and initialized.
        debug_assert_eq!(
            header_ptr.as_ptr().cast::<u8>().align_offset(HEADER_ALIGN),
            0,
            "entry header pointer must be well-aligned"
        );
        let header = unsafe { header_ptr.as_ref() };
        if !header.is_active() {
            self.entries -= 1;
            self.add_reclaimed_from_header(header);
        }
    }

    fn try_intern(&mut self, s: &str) -> Option<NonNull<EntryHeader>> {
        // Calculate the hash of the string, since we need it whether we find a matching entry or end up adding one.
        let s_hash = hash_string(s);

        // Try and find an existing entry for this string.
        if self.entries != 0 {
            if let Some(existing_entry) = self.find_entry(s_hash, s) {
                return Some(existing_entry);
            }
        }

        // We didn't find an existing entry, so we're going to intern it.
        //
        // First, try and see if we have a reclaimed entry that can fit this string and is aligned. If nothing suitable
        // is found, then we'll just try to fit it in the remaining capacity of our data buffer.
        let entry_layout = layout_for_entry(s.len());

        if !self.reclaimed.is_empty() {
            let maybe_reclaimed_entry = self
                .reclaimed
                .iter()
                .find(|re| re.is_aligned() && re.capacity() >= entry_layout.size())
                .copied();
            if let Some(reclaimed_entry) = maybe_reclaimed_entry {
                // We found a suitable reclaimed entry. Remove it from the list.
                self.reclaimed.remove(&reclaimed_entry);

                // Check to see if the reclaimed entry is _bigger_ than the size of the entry we're about to add. If it is,
                // we'll carve off the remainder and add it back as a new reclaimed entry.
                let leftover = reclaimed_entry.capacity() - entry_layout.size();
                if leftover > 0 {
                    let leftover_start = reclaimed_entry.start + entry_layout.size();
                    let is_aligned = leftover_start % HEADER_ALIGN == 0;

                    self.add_reclaimed(leftover_start, reclaimed_entry.end, is_aligned);
                }

                // Write our entry.
                return Some(self.write_entry(reclaimed_entry.start, s_hash, s));
            }
        }

        // We couldn't find a large enough reclaimed entry, or we had none, so see if we can fit this string within the
        // available capacity of our data buffer.
        if entry_layout.size() <= self.available() {
            let header_ptr = self.write_entry(self.len, s_hash, s);

            // Update our length to reflect the brand-new entry.
            self.len += entry_layout.size();

            Some(header_ptr)
        } else {
            None
        }
    }
}

impl Drop for InternerState {
    fn drop(&mut self) {
        // SAFETY: We allocated this buffer with the global allocator, and we're generating the same layout for it as
        // `InternerState::new` using `layout_for_data`.
        unsafe {
            std::alloc::dealloc(self.ptr.as_ptr(), layout_for_data(self.capacity));
        }
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
/// ┌─────────────────────────────── entry #1 ───────────────────────────────┐ ┌── entry #2 ──┐ ┌── entry .. ──┐
/// ▼                                                                        ▼ ▼              ▼ ▼              ▼
/// ┏━━━━━━━━━━━━━┯━━━━━━━━━━━┯━━━━━━━━━━━━┯━━━━━━━━━━━━━┯━━━━━━━━━━━━━━━━━━━┓ ┏━━━━━━━━━━━━━━┓ ┏━━━━━━━━━━━━━━┓
/// ┃ string hash │  ref cnt  │ string len │ string data │ alignment padding ┃ ┃    header    ┃ ┃    header    ┃
/// ┃  (8 bytes)  │ (8 bytes) │ (8 bytes)  │  (N bytes)  │    (variable)     ┃ ┃   & string   ┃ ┃   & string   ┃
/// ┗━━━━━━━━━━━━━┷━━━━━━━━━━━┷━━━━━━━━━━━━┷━━━━━━━━━━━━━┷━━━━━━━━━━━━━━━━━━━┛ ┗━━━━━━━━━━━━━━┛ ┗━━━━━━━━━━━━━━┛
/// ▲                                      ▲                                 ▲
/// └──────────── `EntryHeader` ───────────┘       alignment matched to ─────┘
///            (8 byte alignment)                  `EntryHeader` via padding    
///
/// The backing buffer is always aligned properly for `EntryHeader`, so that the first entry can be referenced
/// correctly. However, when appending additional entries to the buffer, we need to ensure that those entries also have
/// an aligned start for accessing the header. This is complicated due to the variable number of bytes for the string data.
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
pub struct FixedSizeInterner {
    state: Arc<Mutex<InternerState>>,
}

impl FixedSizeInterner {
    /// Creates a new `FixedSizeInterner` with the given capacity.
    ///
    /// The given capacity will potentially be rounded up by a small number of bytes (up to 7) in order to ensure the
    /// backing buffer is properly aligned.
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            state: Arc::new(Mutex::new(InternerState::with_capacity(capacity))),
        }
    }

    #[cfg(test)]
    fn with_state<F, O>(&self, f: F) -> O
    where
        F: FnOnce(&InternerState) -> O,
    {
        let state = self.state.lock().unwrap();
        f(&state)
    }

    /// Returns `true` if the interner contains no strings.
    pub fn is_empty(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.entries == 0
    }

    /// Returns the number of strings in the interner.
    pub fn len(&self) -> usize {
        let state = self.state.lock().unwrap();
        state.entries
    }

    /// Returns the total number of bytes in the interner.
    pub fn len_bytes(&self) -> usize {
        let state = self.state.lock().unwrap();
        state.len
    }

    /// Returns the total number of bytes the interner can hold.
    pub fn capacity_bytes(&self) -> usize {
        let state = self.state.lock().unwrap();
        state.capacity.get()
    }

    /// Tries to intern the given string.
    ///
    /// If the intern is at capacity and the given string cannot fit, `None` is returned. Otherwise, `Some` is
    /// returned with a reference to the interned string.
    pub fn try_intern(&self, s: &str) -> Option<InternedString> {
        let mut state = self.state.lock().unwrap();
        state.try_intern(s).map(|header| InternedString {
            state: Arc::new(StringState {
                interner: Arc::clone(&self.state),
                header,
            }),
        })
    }
}

struct EntryLayout {
    size: usize,
}

impl EntryLayout {
    fn size(&self) -> usize {
        self.size
    }
}

fn hash_string(s: &str) -> u64 {
    let mut hasher = ahash::AHasher::default();
    hasher.write(s.as_bytes());
    hasher.finish()
}

const fn layout_for_data(capacity: NonZeroUsize) -> Layout {
    // Round up the capacity to the nearest multiple of the header size.
    let size = capacity.get().div_ceil(HEADER_LEN) * HEADER_LEN;

    // SAFETY: The size of the layout cannot be zero (since `capacity` is non-zero and we always divide rounding up,
    // meaning `size` will always be atleast HEADER_LEN) nd is well-aligned for `EntryHeader`.
    unsafe { Layout::from_size_align_unchecked(size, HEADER_ALIGN) }
}

fn layout_for_entry(s_len: usize) -> EntryLayout {
    // SAFETY: There's a few building blocks here that let us do this correctly:
    // - Strings are a slice of bytes, so alignment is always 1
    // - We can use the alignment from the header because it will always be the same or greater than the alignment of
    //   the string (1), so alignment is always satisfied
    // - We can start the string immediately after the header, because the only thing that needs to be properly aligned
    //   _is_ the header
    // - We pad the overall layout to the alignment of the header, so we know that we can immediately follow this layout
    //   with another header and have it be properly aligned
    let layout = unsafe { Layout::from_size_align_unchecked(HEADER_LEN + s_len, HEADER_ALIGN).pad_to_align() };

    EntryLayout { size: layout.size() }
}

fn get_entry_string<'a>(header_ptr: *mut EntryHeader) -> &'a str {
    // SAFETY: The caller is responsible for ensuring that `header_ptr` is a valid pointer to an `EntryHeader` value:
    // non-null, well-aligned, and initialized.
    debug_assert!(!header_ptr.is_null(), "entry header pointer must not be null");
    debug_assert_eq!(
        header_ptr.cast::<u8>().align_offset(HEADER_ALIGN),
        0,
        "entry header pointer must be well-aligned"
    );
    let header = unsafe { &*header_ptr };

    // Advance past the header and get a reference to the string.
    //
    // SAFETY: We know that since we're advancing by one, we're ending up at the end of this entry header, which is
    // still in bounds of the data buffer.
    //
    // We also the string bytes are still in bounds of the data buffer, are well-aligned (alignment of 1, so we can't
    // ever be unaligned), and have been initialized since we had to write them in the first place.
    //
    // Finally, we know the string bytes are valid UTF-8 because we only ever write bytes derived from `&str` values,
    // which are inherently valid UTF-8.
    unsafe {
        let s_ptr = header_ptr.add(1).cast::<u8>();
        let s_buf = std::slice::from_raw_parts(s_ptr, header.len);
        std::str::from_utf8_unchecked(s_buf)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, ops::RangeInclusive};

    use super::*;
    use prop::sample::Index;
    use proptest::{
        collection::{hash_set, vec as arb_vec},
        prelude::*,
    };

    fn get_interned_string_entry_start_end(interner: &FixedSizeInterner, s: &InternedString) -> (usize, usize) {
        let state = interner.state.lock().unwrap();
        let header = unsafe { s.state.header.as_ref() };
        let entry_start = unsafe { s.state.header.as_ptr().cast::<u8>().offset_from(state.ptr.as_ptr()) as usize };
        let entry_end = entry_start + header.size() - 1;
        (entry_start, entry_end)
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
        let interner = FixedSizeInterner::new(NonZeroUsize::new(1024).unwrap());

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
        let interner = FixedSizeInterner::new(NonZeroUsize::new(64).unwrap());

        let s1 = interner.try_intern("hello world!");
        assert!(s1.is_some());

        let s2 = interner.try_intern("hello, world");
        assert!(s2.is_none());
    }

    #[test]
    fn reclaim_after_dropped() {
        let interner = FixedSizeInterner::new(NonZeroUsize::new(1024).unwrap());

        let s1 = interner.try_intern("hello").expect("should not fail to intern");
        let (s1_entry_start, s1_entry_end) = get_interned_string_entry_start_end(&interner, &s1);

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 1);
            assert!(state.reclaimed.is_empty());
        }

        // Drop the interned string, which should decrement the reference count to zero and then reclaim the entry.
        drop(s1);

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 0);
            assert_eq!(state.reclaimed.len(), 1);

            let s1_reclaimed = state.reclaimed.first().unwrap();
            assert_eq!(s1_reclaimed.start, s1_entry_start);
            assert_eq!(s1_reclaimed.end, s1_entry_end);
        }
    }

    #[test]
    fn has_reclaimed_entries_string_fits_exactly() {
        // The interner is large enough for one "hello world!"/"hello, world", but not two of them.
        let interner = FixedSizeInterner::new(NonZeroUsize::new(64).unwrap());

        // Intern the first string, which should fit without issue.
        let s1 = interner.try_intern("hello world!").expect("should not fail to intern");
        let (s1_entry_start, s1_entry_end) = get_interned_string_entry_start_end(&interner, &s1);

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 1);
            assert!(state.reclaimed.is_empty());
        }

        // Try to intern the second string, which should fail as we don't have the space.
        let s2 = interner.try_intern("hello, world");
        assert_eq!(s2, None);

        // Drop the first string, which should decrement the reference count to zero and then reclaim the entry.
        drop(s1);

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 0);
            assert_eq!(state.reclaimed.len(), 1);

            let s1_reclaimed = state.reclaimed.first().unwrap();
            assert_eq!(s1_reclaimed.start, s1_entry_start);
            assert_eq!(s1_reclaimed.end, s1_entry_end);
        }

        // Try again to intern the second string, which should now succeed and take over the reclaimed entry entirely
        // as the strings are identical in length.
        let _s2 = interner.try_intern("hello, world").expect("should not fail to intern");

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 1);
            assert!(state.reclaimed.is_empty());
        }
    }

    #[test]
    fn has_reclaimed_entries_string_smaller() {
        // The interner is large enough for either "hello there, world!" or "hello, world", but not both of them.
        let interner = FixedSizeInterner::new(NonZeroUsize::new(64).unwrap());

        // Intern the first string, which should fit without issue.
        let s1 = interner
            .try_intern("hello there, world!")
            .expect("should not fail to intern");
        let (s1_entry_start, s1_entry_end) = get_interned_string_entry_start_end(&interner, &s1);

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 1);
            assert!(state.reclaimed.is_empty());
        }

        // Try to intern the second string, which should fail as we don't have the space.
        let s2 = interner.try_intern("hello, world");
        assert_eq!(s2, None);

        // Drop the first string, which should decrement the reference count to zero and then reclaim the entry.
        drop(s1);

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 0);
            assert_eq!(state.reclaimed.len(), 1);

            let s1_reclaimed = state.reclaimed.first().unwrap();
            assert_eq!(s1_reclaimed.start, s1_entry_start);
            assert_eq!(s1_reclaimed.end, s1_entry_end);
        }

        // Try again to intern the second string, which should now succeed and take over the reclaimed entry, but should
        // lead to a smaller reclaimed entry being re-added as we don't need the entirety of the original reclaimed entry.
        let s2 = interner.try_intern("hello, world").expect("should not fail to intern");
        let (_, s2_entry_end) = get_interned_string_entry_start_end(&interner, &s2);

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 1);
            assert_eq!(state.reclaimed.len(), 1);

            // The start of the partial reclaimed entry should be right after the end of s2, and the end should be the
            // original end of s1.
            let s1_partial_reclaimed = state.reclaimed.first().unwrap();
            assert_eq!(s1_partial_reclaimed.start, s2_entry_end + 1);
            assert_eq!(s1_partial_reclaimed.end, s1_entry_end);
        }
    }

    #[test]
    fn reclamation_merges_previous_and_current() {
        // NOTE: The drop order matters here, because what we're testing is that the reclamation logic can properly
        // merge adjacent entries regardless of whether we're merging with an existing reclaimed entry that comes before
        // us _or_ after us (or both!).

        let interner = FixedSizeInterner::new(NonZeroUsize::new(1024).unwrap());

        // Intern two strings, back-to-back, which should fit without issue.
        let s1 = interner
            .try_intern("hello there, world!")
            .expect("should not fail to intern");
        let (s1_entry_start, _) = get_interned_string_entry_start_end(&interner, &s1);

        let s2 = interner
            .try_intern("tally ho, chaps!")
            .expect("should not fail to intern");
        let (_, s2_entry_end) = get_interned_string_entry_start_end(&interner, &s2);

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 2);
            assert!(state.reclaimed.is_empty());
        }

        // Drop the both strings, which should lead to both entries being reclaimed and their entries merged as they're adjacent.
        drop(s1);
        drop(s2);

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 0);
            assert_eq!(state.reclaimed.len(), 1);

            let merged_reclaimed = state.reclaimed.first().unwrap();
            assert_eq!(merged_reclaimed.start, s1_entry_start);
            assert_eq!(merged_reclaimed.end, s2_entry_end);
        }
    }

    #[test]
    fn reclamation_merges_current_and_subsequent() {
        // NOTE: The drop order matters here, because what we're testing is that the reclamation logic can properly
        // merge adjacent entries regardless of whether we're merging with an existing reclaimed entry that comes before
        // us _or_ after us (or both!).

        let interner = FixedSizeInterner::new(NonZeroUsize::new(1024).unwrap());

        // Intern two strings, back-to-back, which should fit without issue.
        let s1 = interner
            .try_intern("hello there, world!")
            .expect("should not fail to intern");
        let (s1_entry_start, _) = get_interned_string_entry_start_end(&interner, &s1);

        let s2 = interner
            .try_intern("tally ho, chaps!")
            .expect("should not fail to intern");
        let (_, s2_entry_end) = get_interned_string_entry_start_end(&interner, &s2);

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 2);
            assert!(state.reclaimed.is_empty());
        }

        // Drop the both strings, which should lead to both entries being reclaimed and their entries merged as they're adjacent.
        drop(s2);
        drop(s1);

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 0);
            assert_eq!(state.reclaimed.len(), 1);

            let merged_reclaimed = state.reclaimed.first().unwrap();
            assert_eq!(merged_reclaimed.start, s1_entry_start);
            assert_eq!(merged_reclaimed.end, s2_entry_end);
        }
    }

    #[test]
    fn reclamation_merges_previous_and_current_and_subsequent() {
        // NOTE: The drop order matters here, because what we're testing is that the reclamation logic can properly
        // merge adjacent entries regardless of whether we're merging with an existing reclaimed entry that comes before
        // us _or_ after us (or both!).

        let interner = FixedSizeInterner::new(NonZeroUsize::new(1024).unwrap());

        // Intern three strings, back-to-back-to-back, which should fit without issue.
        let s1 = interner
            .try_intern("hello there, world!")
            .expect("should not fail to intern");
        let (s1_entry_start, _) = get_interned_string_entry_start_end(&interner, &s1);

        let s2 = interner
            .try_intern("tally ho, chaps!")
            .expect("should not fail to intern");

        let s3 = interner
            .try_intern("onward and upward!")
            .expect("should not fail to intern");
        let (_, s3_entry_end) = get_interned_string_entry_start_end(&interner, &s3);

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 3);
            assert!(state.reclaimed.is_empty());
        }

        // Drop the first and last strings, which should not be merged as they're not adjacent.
        drop(s1);
        drop(s3);

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 1);
            assert_eq!(state.reclaimed.len(), 2);
        }

        // Now drop the second string, which is adjacent to both the first and third strings, and should result in a
        // single merged reclamined entry.
        drop(s2);

        {
            let state = interner.state.lock().unwrap();
            assert_eq!(state.entries, 0);
            assert_eq!(state.reclaimed.len(), 1);

            let merged_reclaimed = state.reclaimed.first().unwrap();
            assert_eq!(merged_reclaimed.start, s1_entry_start);
            assert_eq!(merged_reclaimed.end, s3_entry_end);
        }
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
            let interner = FixedSizeInterner::new(NonZeroUsize::new(ENTRY_SIZE * indices.len()).unwrap());

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
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use std::{num::NonZeroUsize, ops::Deref};

    use super::FixedSizeInterner;

    #[test]
    fn concurrent_drop_and_intern() {
        const STRING_TO_INTERN: &str = "hello, world!";

        // This test is meant to explore the thread orderings when one thread is trying to drop (and thus reclaim) the
        // last active reference to an interned string, and another thread is trying to intern that very same string.
        //
        // We accept, as a caveat, that a possible outcome is that we intern the "new" string again, even though an
        // existing entry to that string may have existed in an alternative ordering.
        loom::model(|| {
            let interner = FixedSizeInterner::new(NonZeroUsize::new(1024).unwrap());
            let t2_interner = interner.clone();
            let t1_interned_s = interner
                .try_intern(STRING_TO_INTERN)
                .expect("should not fail to intern");
            assert_eq!(t1_interned_s.deref(), STRING_TO_INTERN);

            // Spawn thread T2, which tries to intern the same string and returns the handle to it.
            let t2_result = loom::thread::spawn(move || {
                t2_interner
                    .try_intern(STRING_TO_INTERN)
                    .expect("should not fail to intern")
            });

            drop(t1_interned_s);

            let t2_interned_s = t2_result.join().expect("should not fail to join T2");
            assert_eq!(t2_interned_s.deref(), STRING_TO_INTERN);

            // What we're checking for here is that either:
            // - there's no reclaimed entries (T2 found the existing entry for the string before T1 dropped it)
            // - there's a reclaimed entry (T2 didn't find the existing entry for the string before T1 marked it as
            //   inactive) but the reclaimed entry does _not_ overlap with the interned string from T2, meaning we
            //   didn't get confused and allow T2 to use an existing entry that T1 then later marked as reclaimed
            let reclaimed_entries = interner.with_state(|state| state.reclaimed.clone());
            let (t2_interned_start, t2_interned_end) =
                interner.with_state(|state| state.get_offsets_for_header(t2_interned_s.state.get_entry_header()));

            if !reclaimed_entries.is_empty() {
                let has_overlap = reclaimed_entries.iter().any(|re| {
                    (re.start <= t2_interned_start && t2_interned_start <= re.end)
                        || (re.start <= t2_interned_end && t2_interned_end <= re.end)
                });

                assert!(
                    !has_overlap,
                    "reclaimed entry must not overlap with live interned string"
                );
            }
        });
    }
}
