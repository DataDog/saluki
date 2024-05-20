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
    sync::{
        atomic::{
            AtomicUsize,
            Ordering::{AcqRel, Acquire},
        },
        Arc, Mutex,
    },
};

const HEADER_LEN: usize = std::mem::size_of::<EntryHeader>();
const HEADER_ALIGN: usize = std::mem::align_of::<EntryHeader>();

/// An interned string.
#[derive(Debug, Eq, PartialEq)]
pub struct InternedString {
    state: Arc<StringState>,
}

impl Deref for InternedString {
    type Target = str;

    fn deref(&self) -> &str {
        self.state.get_entry()
    }
}

#[derive(Debug)]
struct StringState {
    interner: Arc<Mutex<InternerState>>,
    header: NonNull<EntryHeader>,
}

impl StringState {
    fn get_entry<'a>(&'a self) -> &'a str {
        // NOTE: We're specifically upholding a safety variant here by tying the lifetime of the string reference to our
        // own lifetime, as the lifetime of the string reference *cannot* exceed the lifetime of the interner state,
        // which we keep alive by holding `Arc<Mutex<InternerState>>`.
        get_entry_string::<'a>(self.header.as_ptr())
    }
}

impl Drop for StringState {
    fn drop(&mut self) {
        // TODO: We need to decrement the reference count on the entry, and if it's at zero, we try to add a tombstone
        // to the interner state for it. That might fail if another caller is trying to intern the same string before we
        // can get the lock to the interner state, but that's OK: we only care about at least _trying_ to tombstone it.

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

impl PartialEq for StringState {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.interner, &other.interner) && self.header == other.header
    }
}

impl Eq for StringState {}

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
        // We'll iterate over all entries in the buffer, tombstoned or not, to see if we can find a match for the given string.
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
            let header = unsafe { header_ptr.as_ref().unwrap() };

            // See if this entry is a tombstone or not. If it's not -- it's active and not slated to be reclaimed --
            // then we'll quickly check the hash/length of the string to see if this is likely to be a match for `s`.
            if header.is_active() && header.hash == hash && header.len == s.len() {
                // As a final check, we make sure that the entry string and `s` are equal. If they are, then we
                // have an exact match and will return the entry.
                let s_entry = get_entry_string(header_ptr);
                if s_entry == s {
                    // Increment the reference count for this entry so it's not prematurely tombstoned/reclaimed.
                    header.refs.fetch_add(1, AcqRel);

                    // SAFETY: `header_ptr` cannot be null otherwise we would have already panicked when creating a
                    // reference to it above.
                    return Some(unsafe { NonNull::new_unchecked(header_ptr) });
                }
            }

            // Either this was a tombstone or we didn't have a match, so we move on to the next entry.
            offset += header.size();
            i += 1;
        }

        None
    }

    fn write_entry(&mut self, offset: usize, hash: u64, s: &str) -> NonNull<EntryHeader> {
        let s_buf = s.as_bytes();

        // Write out the entry header and then the string.
        //
        // SAFETY: We know that `offset` is within the bounds of the data buffer because the caller is responsible for
        // ensuring that.
        let header_ptr = unsafe {
            let ptr = self.ptr.as_ptr().add(offset).cast::<EntryHeader>();
            ptr.write(EntryHeader {
                hash,
                refs: AtomicUsize::new(1),
                len: s_buf.len(),
            });

            NonNull::new_unchecked(ptr)
        };

        // SAFETY: We know that `s_start` is within the bounds of the data buffer, because it's derived from `offset`,
        // which is checked to ensure that the start and end are both within the bounds of our data buffer.
        let s_start = offset + HEADER_LEN;
        let entry_s_buf = unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().add(s_start), s_buf.len()) };
        entry_s_buf.copy_from_slice(s_buf);

        // Update the number of entries we have.
        self.entries += 1;

        header_ptr
    }

    fn add_reclaimed_from_header(&mut self, header: &EntryHeader) {
        // SAFETY: We know both pointers are within bounds of our data buffer, and are pointers derived from the
        // same pointer, and the delta is a multiple of `T` (which is `u8`, so size of 1) and all of that.
        //
        // Additionally, our cast to `usize` is logically sound because a header pointer could never be less than
        // the base pointer of the data buffer.
        let entry_offset = unsafe {
            NonNull::from(header)
                .cast::<u8>()
                .as_ptr()
                .offset_from(self.ptr.as_ptr().cast_const()) as usize
        };

        self.add_reclaimed(entry_offset, entry_offset + header.size() - 1, true);
    }

    fn add_reclaimed(&mut self, start: usize, end: usize, aligned: bool) {
        let reclaimed_entry = ReclaimedEntry::new(start, end, aligned);
        self.reclaimed.insert(reclaimed_entry);

        // While we're here, we'll do an incremental merging of reclaimed entries. This lets us avoid fragmentation
        // that, over time, would shrink the effective capacity of the interner in a non-recoverable way.

        // We iterate over the reclaimed entries, storing the last three seen entries in a small ring buffer, and stop
        // iterating once we've gotten to the entry right _after_ the one we just inserted, or the one we just inserted
        // itself.
        //
        // We do this because there's no need to check any entries after that, because they couldn't be adjacent
        // otherwise they would have already been merged when they were reclaimed.
        let mut last_seen = [None; 3];
        let mut found_current = false;
        let mut seen_idx = 0;
        let mut current_entry_idx = 0;

        for entry in self.reclaimed.iter() {
            last_seen[seen_idx % 3] = Some(*entry);

            if entry == &reclaimed_entry {
                // We found the entry we just reclaimed, so mark that down.
                found_current = true;
                current_entry_idx = seen_idx;
            } else if found_current {
                // We've already seen the entry we just inserted, and now we've seen the entry _after_ it, so we can
                // break out of the loop.
                //
                // First, though, we increment `seen_idx` so that we can properly determine how many entries we've seen.
                seen_idx += 1;
                break;
            }

            seen_idx += 1;
        }

        // Now check to see if we can merge the reclaimed entry we just added with either the one before it, the one
        // after it or both, if either of them exist.
        if seen_idx == 2 {
            // We only have two entries, which can only mean that our just-added entry is the second one, so see if we
            // can merge with the previous entry.
            let previous_entry = &last_seen[(current_entry_idx - 1) % 3];
            let current_entry = &last_seen[current_entry_idx % 3];

            if previous_entry.end + 1 == current_entry.start {
                // We can merge the previous entry with the current one.
                let merged_entry =
                    ReclaimedEntry::new(previous_entry.start, current_entry.end, previous_entry.is_aligned());
                self.reclaimed.remove(previous_entry);
                self.reclaimed.remove(current_entry);
                self.reclaimed.insert(merged_entry);
            }
        } else if seen_idx >= 3 {
            // We visited three or more entries, so check to see if either the previous or next entry can be merged.
            let previous_entry = &last_seen[(current_entry_idx - 1) % 3];
            let current_entry = &last_seen[current_entry_idx % 3];
            let next_entry = &last_seen[(current_entry_idx + 1) % 3];

            let mut merged_entry_start = None;
            let mut merged_entry_end = None;
            let mut merged_entry_aligned = None;

            if previous_entry.end + 1 == current_entry.start {
                // We can merge the previous entry with the current one.
                merged_entry_start = Some(previous_entry.start);
                merged_entry_aligned = Some(previous_entry.is_aligned());
                self.reclaimed.remove(previous_entry);
            }

            if current_entry.end + 1 == next_entry.start {
                // We can merge the current entry with the next one.
                merged_entry_end = Some(next_entry.end);
                self.reclaimed.remove(next_entry);
            }

            if merged_entry_start.is_some() || merged_entry_end.is_some() {
                // We can merge at least one entry, so remove the entries we're merging and add the new merged entry.
                self.reclaimed.remove(current_entry);

                let merged_entry = ReclaimedEntry::new(
                    merged_entry_start.unwrap_or(current_entry.start),
                    merged_entry_end.unwrap_or(current_entry.end),
                    merged_entry_aligned.unwrap_or(current_entry.is_aligned()),
                );
                self.reclaimed.insert(merged_entry);
            }
        }
    }

    fn mark_for_reclamation(&mut self, header_ptr: NonNull<EntryHeader>) {
        // See if the reference count is zero.
        //
        // Only interned string values (the frontend handle that wraps the pointer to a specific entry) can decrement
        // the reference count for their specific entry when dropped, and only `InternerState` -- with its access mediated through a
        // mutex -- can increment the reference count for entries. This means that if the reference count is zero, then
        // we know that nobody else is holding a reference to this entry, and no concurrent call to `try_intern` could
        // be updating the reference count, either... so it's safe to be marked as reclaimed.
        //
        // SAFETY: We know the pointer is well-aligned for `EntryHeader` and is initialized.
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

        // We couldn't find a large enough tombstone, or we had none, so see if we can fit this string within the
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

impl PartialEq for InternerState {
    fn eq(&self, other: &Self) -> bool {
        // Comparing by pointer alone should be enough, but we're just doubling up by also checking that the capacity is
        // the same as well.
        std::ptr::eq(self.ptr.as_ptr(), other.ptr.as_ptr())
            && self.capacity == other.capacity
    }
}

impl Eq for InternerState {}

/// A string interner basd on a single, fixed-size backing buffer with support for reclamation.
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
/// ┌─────────────────────────────── entry #1 ───────────────────────────────┐ ┌──  entry #2 ──┐ ┌──  entry .. ──┐
/// ▼                                                                        ▼ ▼               ▼ ▼               ▼
/// ┏━━━━━━━━━━━━━┯━━━━━━━━━━━┯━━━━━━━━━━━━┯━━━━━━━━━━━━━┯━━━━━━━━━━━━━━━━━━━┓ ┏━━━━━━━━━━━━━━━┓ ┏━━━━━━━━━━━━━━━┓
/// ┃ string hash │  ref cnt  │ string len │ string data │ alignment padding ┃ ┃     header    ┃ ┃     header    ┃
/// ┃  (8 bytes)  │ (8 bytes) │ (8 bytes)  │  (N bytes)  │    (variable)     ┃ ┃    & string   ┃ ┃    & string   ┃
/// ┗━━━━━━━━━━━━━┷━━━━━━━━━━━┷━━━━━━━━━━━━┷━━━━━━━━━━━━━┷━━━━━━━━━━━━━━━━━━━┛ ┗━━━━━━━━━━━━━━━┛ ┗━━━━━━━━━━━━━━━┛
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
/// When a string is interned, the entry header tracks how mny active references there are to it. When that reference
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
struct Interner {
    state: Arc<Mutex<InternerState>>,
}

impl Interner {
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            state: Arc::new(Mutex::new(InternerState::with_capacity(capacity))),
        }
    }

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
    // SAFETY: We know that `header_ptr` is non-null, well-aligned for `EntryHeader`, and initialized.
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
    use super::*;

    fn get_interned_string_entry_start_end(interner: &Interner, s: &InternedString) -> (usize, usize) {
        let state = interner.state.lock().unwrap();
        let header = unsafe { s.state.header.as_ref() };
        let entry_start = unsafe { s.state.header.as_ptr().cast::<u8>().offset_from(state.ptr.as_ptr()) as usize };
        let entry_end = entry_start + header.size() - 1;
        (entry_start, entry_end)
    }

    #[test]
    fn basic() {
        let interner = Interner::new(NonZeroUsize::new(1024).unwrap());

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
        let interner = Interner::new(NonZeroUsize::new(64).unwrap());

        let s1 = interner.try_intern("hello world!");
        assert!(s1.is_some());

        let s2 = interner.try_intern("hello, world");
        assert!(s2.is_none());
    }

    #[test]
    fn reclaim_after_dropped() {
        let interner = Interner::new(NonZeroUsize::new(1024).unwrap());

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
        let interner = Interner::new(NonZeroUsize::new(64).unwrap());

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
        let interner = Interner::new(NonZeroUsize::new(64).unwrap());

        // Intern the first string, which should fit without issue.
        let s1 = interner.try_intern("hello there, world!").expect("should not fail to intern");
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

        let interner = Interner::new(NonZeroUsize::new(1024).unwrap());

        // Intern two strings, back-to-back, which should fit without issue.
        let s1 = interner.try_intern("hello there, world!").expect("should not fail to intern");
        let (s1_entry_start, _) = get_interned_string_entry_start_end(&interner, &s1);

        let s2 = interner.try_intern("tally ho, chaps!").expect("should not fail to intern");
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

        let interner = Interner::new(NonZeroUsize::new(1024).unwrap());

        // Intern two strings, back-to-back, which should fit without issue.
        let s1 = interner.try_intern("hello there, world!").expect("should not fail to intern");
        let (s1_entry_start, _) = get_interned_string_entry_start_end(&interner, &s1);

        let s2 = interner.try_intern("tally ho, chaps!").expect("should not fail to intern");
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

        let interner = Interner::new(NonZeroUsize::new(1024).unwrap());

        // Intern three strings, back-to-back-to-back, which should fit without issue.
        let s1 = interner.try_intern("hello there, world!").expect("should not fail to intern");
        let (s1_entry_start, _) = get_interned_string_entry_start_end(&interner, &s1);

        let s2 = interner.try_intern("tally ho, chaps!").expect("should not fail to intern");

        let s3 = interner.try_intern("onward and upward!").expect("should not fail to intern");
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
}
