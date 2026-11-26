//! A sharded, bounded symbol dictionary with two-hit admission and 4-byte IDs.
//!
//! [`SymbolTable`] stores recurring strings once and returns a `u32` ID for each one. Owners of an ID read the string
//! without a lock. Each record carries a reference count, and the last release returns its space to a per-shard free
//! list. A string enters the table only on its second sighting in the same thread, so one-off strings use no record
//! space.
//!
//! [`Symbol`] is the safe, owned handle for one reference. [`Symbol::into_id`] converts it into a raw ID for compact
//! storage, and the unsafe methods on [`SymbolTable`] manage raw IDs.

use std::{
    cell::{RefCell, UnsafeCell},
    hash::BuildHasher,
    mem,
    ops::Deref,
    ptr, slice, str,
    sync::{
        atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        RwLock,
    },
};

use foldhash::fast::RandomState;
use hashbrown::HashTable;

use super::helpers::{ReclaimedEntries, ReclaimedEntry};

/// The number of ID and hash bits that select a shard.
///
/// Four bits give 16 shards. This spreads lock contention and leaves 28 bits of each ID for the record offset.
const SHARD_BITS: u32 = 4;

/// The number of shards in each table.
const SHARD_COUNT: usize = 1 << SHARD_BITS;

/// The number of usable record bytes in one chunk.
///
/// A shard allocates its storage one 16 KiB chunk at a time, on demand. A table with few symbols allocates few chunks.
const CHUNK_BYTES: usize = 16 * 1024;

/// The distance in the virtual offset space between the starts of two adjacent chunks.
///
/// The stride is 32 KiB, twice the chunk size. The unused 16 KiB after each chunk keeps the free ranges of adjacent
/// chunks apart. `ReclaimedEntries` merges adjacent free ranges, and without the gap a merged range could span two
/// separate allocations.
const CHUNK_STRIDE: usize = CHUNK_BYTES * 2;

/// The alignment of every record, in bytes.
///
/// This is 4, the alignment of [`Record`]. IDs store offsets divided by this value.
const RECORD_ALIGN: usize = mem::align_of::<Record>();

/// The size of a record header, in bytes.
///
/// This is 8: a 4-byte reference count and a 4-byte length.
const HEADER_BYTES: usize = mem::size_of::<Record>();

/// The size of the virtual offset space that the 28 offset bits of an ID can address in one shard.
///
/// This is 2^28 units of 4 bytes, that is 1 GiB.
const SHARD_ADDRESS_BYTES: usize = (1usize << (32 - SHARD_BITS)) * RECORD_ALIGN;

/// The maximum record capacity of one shard, in bytes.
///
/// This is 512 MiB, half the address space, because the chunk stride doubles each chunk's virtual size. The table
/// clamps its total capacity to 16 shards of 512 MiB, that is 8 GiB.
const MAX_SHARD_BYTES: usize = SHARD_ADDRESS_BYTES / 2;

/// The number of witness slots in each thread.
///
/// Each slot remembers one first sighting. With 280-byte slots, the array of 256 slots uses 71,680 bytes (70 KiB) in
/// each thread that interns.
const WITNESS_COUNT: usize = 256;

/// The maximum length of a string that the table admits, in bytes.
///
/// A witness slot must hold a full copy of the string to confirm a second sighting exactly. Longer strings always get
/// `None` from `try_intern`.
const WITNESS_BYTES: usize = 256;

/// The number of 64-bit words in the `resident` filter of each shard.
///
/// 64 words use 512 bytes in each shard, and 8 KiB across the 16 shards.
const FILTER_WORDS: usize = 64;

/// The minimum number of removals between two rebuilds of a shard's `resident` filter.
///
/// A shard rebuilds its filter after removals equal to half its live entries, or after this many removals if that
/// number is larger. This keeps the rebuild cost amortized when a shard holds few entries.
const MIN_FILTER_REFRESH_REMOVALS: usize = 64;

/// The source of unique table IDs for witness slots.
///
/// IDs start at 1, so the zero ID of an empty witness slot never matches a table. A process never reuses an ID.
static NEXT_TABLE: AtomicUsize = AtomicUsize::new(1);

thread_local! {
    /// The witness slots of the current thread, shared by all tables.
    static WITNESSES: RefCell<[Witness; WITNESS_COUNT]> = const { RefCell::new([Witness::EMPTY; WITNESS_COUNT]) };
}

/// A record of one first sighting of a string in the current thread.
///
/// Each slot holds an exact copy of the string, so a hash collision cannot admit a first sighting. A slot has this
/// layout on 64-bit targets, 280 bytes in total:
///
/// ```text
/// ┏━━━━━━━━━━━┯━━━━━━━━━━━┯━━━━━━━━━━━┯━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
/// ┃ table ID  │ str hash  │  str len  │ str bytes (first `len` are valid) ┃
/// ┃ (8 bytes) │ (8 bytes) │ (8 bytes) │           (256 bytes)             ┃
/// ┗━━━━━━━━━━━┷━━━━━━━━━━━┷━━━━━━━━━━━┷━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
/// ```
#[derive(Clone, Copy)]
struct Witness {
    /// The ID of the table that saw the string, or 0 for an empty slot.
    table: usize,

    /// The full hash of the string.
    hash: u64,

    /// The length of the string, in bytes.
    len: usize,

    /// A copy of the string. Only the first `len` bytes are valid.
    bytes: [u8; WITNESS_BYTES],
}

impl Witness {
    /// An empty slot that matches no table.
    const EMPTY: Self = Self {
        table: 0,
        hash: 0,
        len: 0,
        bytes: [0; WITNESS_BYTES],
    };

    /// Returns `true` if the current thread saw `value` in `table` at the last use of its slot.
    ///
    /// Low hash bits 0 to 7 select the slot. On a miss, the method overwrites the slot with `value` and returns
    /// `false`. A hit leaves the slot as it is. The caller must make sure `value` is at most `WITNESS_BYTES` long.
    fn seen(table: usize, hash: u64, value: &str) -> bool {
        WITNESSES.with_borrow_mut(|slots| {
            let slot = &mut slots[hash as usize % WITNESS_COUNT];
            if slot.hash == hash && slot.table == table && slot.bytes[..slot.len] == *value.as_bytes() {
                return true;
            }
            slot.table = table;
            slot.hash = hash;
            slot.len = value.len();
            slot.bytes[..slot.len].copy_from_slice(value.as_bytes());
            false
        })
    }
}

/// The header of one string record in shard storage.
///
/// The string bytes follow the header directly. Padding rounds each record up to a multiple of `RECORD_ALIGN`, so the
/// next record starts aligned:
///
/// ```text
/// ┏━━━━━━━━━━━┯━━━━━━━━━━━┯━━━━━━━━━━━┯━━━━━━━━━━━━━┓
/// ┃  ref cnt  │  str len  │ str data  │   padding   ┃
/// ┃ (4 bytes) │ (4 bytes) │ (N bytes) │ (0-3 bytes) ┃
/// ┗━━━━━━━━━━━┷━━━━━━━━━━━┷━━━━━━━━━━━┷━━━━━━━━━━━━━┛
/// ▲                       ▲
/// └─────── `Record` ──────┘
///    (4 byte alignment)
/// ```
#[repr(C)]
struct Record {
    /// The number of owned references to this record.
    ///
    /// The count stays above zero while the record is in its shard's index.
    refs: AtomicU32,

    /// The length of the string data, in bytes.
    len: u32,
}

impl Record {
    /// Returns the number of bytes that a record for a string of `len` bytes uses, including header and padding.
    fn allocation_size(len: usize) -> usize {
        (HEADER_BYTES + len).next_multiple_of(RECORD_ALIGN)
    }

    /// Borrows the string data of a record.
    ///
    /// # Safety
    ///
    /// `record` must point to an initialized record with valid UTF-8 data. The record must stay live for `'a`. A caller
    /// meets this rule if it owns a reference to the record, or holds the shard lock while the record is in the index.
    unsafe fn value<'a>(record: *const Self) -> &'a str {
        // SAFETY: The caller guarantees that the header and `len` data bytes after it are initialized and live. The
        // table writes only bytes from a `&str` into records.
        let bytes = slice::from_raw_parts(record.add(1).cast::<u8>(), (*record).len as usize);
        str::from_utf8_unchecked(bytes)
    }
}

/// One of the 16 independent partitions of a table.
///
/// A shard owns its record storage, its index, and its free list. The chunk pointers and the `resident` filter are
/// atomics, so readers use them without the lock.
struct Shard {
    /// The pointers to the record chunks of this shard, or null for chunks that the shard did not allocate yet.
    ///
    /// Chunk `i` covers virtual offsets `i * CHUNK_STRIDE` up to `i * CHUNK_STRIDE + chunk_len(i)`. Chunks never
    /// move or deallocate while the table is live. Records inside a chunk have independent lifetimes.
    chunks: Box<[AtomicPtr<UnsafeCell<u32>>]>,

    /// The maximum number of record bytes in this shard.
    capacity: usize,

    /// A bloom filter of the strings that are resident in this shard.
    ///
    /// Each string sets two bits in one word. A first sighting with a clear bit skips the shard lock. False positives
    /// take a read lock but cannot admit a first sighting. Removals do not clear bits, so `refresh_filter` rebuilds the
    /// filter from time to time. The filter uses 512 bytes.
    resident: [AtomicU64; FILTER_WORDS],

    /// The mutable state of this shard.
    ///
    /// Lookups take the read lock. Admission and the last release of a record take the write lock.
    state: RwLock<ShardState>,
}

/// The lock-protected state of a shard.
struct ShardState {
    /// The index of live records, keyed by the full string hash, with the record offset as the value.
    entries: HashTable<u32>,

    /// The free ranges of released records and of skipped chunk tails.
    reclaimed: ReclaimedEntries,

    /// The virtual offset where the next bump allocation starts.
    next: usize,

    /// The number of bytes in live records, including headers and padding.
    used: usize,

    /// The number of removals since the last rebuild of the `resident` filter.
    removed_since_refresh: usize,
}

impl Shard {
    /// Creates an empty shard with `capacity` record bytes and no allocated chunks.
    fn new(capacity: usize) -> Self {
        Self {
            chunks: (0..capacity.div_ceil(CHUNK_BYTES))
                .map(|_| AtomicPtr::new(ptr::null_mut()))
                .collect(),
            capacity,
            resident: [const { AtomicU64::new(0) }; FILTER_WORDS],
            state: RwLock::new(ShardState {
                entries: HashTable::new(),
                reclaimed: ReclaimedEntries::new(),
                next: 0,
                used: 0,
                removed_since_refresh: 0,
            }),
        }
    }

    /// Returns the usable length of chunk `index`, in bytes.
    ///
    /// Every chunk has `CHUNK_BYTES` bytes, except that the last chunk ends at the shard capacity.
    fn chunk_len(&self, index: usize) -> usize {
        (self.capacity - index * CHUNK_BYTES).min(CHUNK_BYTES)
    }

    /// Converts a virtual offset into a pointer to its record.
    ///
    /// The chunk index is `offset / CHUNK_STRIDE`, and the position in the chunk is `offset % CHUNK_STRIDE`. The
    /// `Acquire` load pairs with the `Release` store in `reserve`, so a reader sees the chunk allocation.
    fn record(&self, offset: usize) -> *mut Record {
        let chunk = self.chunks[offset / CHUNK_STRIDE].load(Ordering::Acquire);
        // SAFETY: Live and reserved offsets refer to an allocated range in this chunk.
        unsafe { chunk.cast::<u8>().add(offset % CHUNK_STRIDE).cast() }
    }

    /// Reserves `size` bytes for a new record and returns their virtual offset.
    ///
    /// The method uses the first free range that is large enough, and returns any excess to the free list. If no free
    /// range fits, it bump-allocates from `next`. A record never crosses a chunk boundary: if the current chunk tail is
    /// too small, the tail goes to the free list and allocation moves to the next chunk. The method allocates a chunk
    /// the first time it uses it. Returns `None` if the shard has no space.
    ///
    /// The caller must hold the shard write lock, which `state` proves.
    fn reserve(&self, state: &mut ShardState, size: usize) -> Option<usize> {
        if let Some(mut free) = state.reclaimed.take_if(|free| free.capacity() >= size) {
            if free.capacity() > size {
                state.reclaimed.insert(free.split_off(size));
            }
            return Some(free.offset());
        }

        let mut chunk = state.next / CHUNK_STRIDE;
        if chunk >= self.chunks.len() {
            return None;
        }
        let mut position = state.next % CHUNK_STRIDE;
        let remaining = self.chunk_len(chunk) - position;
        if size > remaining {
            if position == 0 {
                return None;
            }
            if remaining > 0 {
                state.reclaimed.insert(ReclaimedEntry::new(state.next, remaining));
            }
            chunk += 1;
            position = 0;
            state.next = chunk * CHUNK_STRIDE;
            if chunk >= self.chunks.len() || size > self.chunk_len(chunk) {
                return None;
            }
        }

        let slot = &self.chunks[chunk];
        // The write lock serializes chunk allocation, so a `Relaxed` load sees every earlier store to this slot.
        if slot.load(Ordering::Relaxed).is_null() {
            let storage: Box<[_]> = (0..self.chunk_len(chunk) / RECORD_ALIGN)
                .map(|_| UnsafeCell::new(0))
                .collect();
            slot.store(Box::into_raw(storage).cast::<UnsafeCell<u32>>(), Ordering::Release);
        }
        let offset = chunk * CHUNK_STRIDE + position;
        state.next = offset + size;
        Some(offset)
    }

    /// Returns the `resident` word index and the two-bit mask for a string hash.
    ///
    /// Hash bits 32 to 37 select the word. Hash bits 0 to 5 and 16 to 21 select the two bits in that word.
    fn filter_location(hash: u64) -> (usize, u64) {
        let index = (hash >> 32) as usize % FILTER_WORDS;
        let bits = (1 << (hash & 63)) | (1 << ((hash >> 16) & 63));
        (index, bits)
    }

    /// Counts one removal, and rebuilds the `resident` filter from the index when enough removals add up.
    ///
    /// A rebuild clears the bits of removed strings. The caller must hold the shard write lock, which `state` proves.
    fn refresh_filter(&self, state: &mut ShardState, hasher: &RandomState) {
        state.removed_since_refresh += 1;
        if state.removed_since_refresh < (state.entries.len() / 2).max(MIN_FILTER_REFRESH_REMOVALS) {
            return;
        }
        // Publish complete words so no live entry temporarily loses its bits.
        let mut words = [0; FILTER_WORDS];
        for &offset in &state.entries {
            // SAFETY: The shard write lock keeps indexed records live.
            let text = unsafe { Record::value(self.record(offset as usize)) };
            let (index, bits) = Self::filter_location(hasher.hash_one(text));
            words[index] |= bits;
        }
        for (slot, word) in self.resident.iter().zip(words) {
            slot.store(word, Ordering::Release);
        }
        state.removed_since_refresh = 0;
    }
}

impl Drop for Shard {
    /// Frees every allocated chunk.
    ///
    /// This is the only place where chunk memory returns to the allocator.
    fn drop(&mut self) {
        for (index, slot) in self.chunks.iter().enumerate() {
            let chunk = slot.load(Ordering::Relaxed);
            if !chunk.is_null() {
                // SAFETY: This is the original chunk allocation. No symbol can outlive the table.
                unsafe {
                    drop(Box::from_raw(ptr::slice_from_raw_parts_mut(
                        chunk,
                        self.chunk_len(index) / RECORD_ALIGN,
                    )))
                };
            }
        }
    }
}

/// A bounded string dictionary with exact, two-hit admission and lock-free ID reads.
///
/// IDs stay stable while a reference is live. Their storage is available for reuse when the last reference drops.
/// Admission requires two observations in the same thread-local window. Strings longer than 256 bytes stay out of the
/// dictionary.
///
/// # Design
///
/// `SymbolTable` serves packed contexts, which store many strings in one allocation with a 4-byte handle for each.
/// [`GenericMapInterner`](super::GenericMapInterner) serves callers that need a self-contained handle and admission on
/// the first lookup. The two types have different contracts, so they stay separate.
///
/// ## Shards and IDs
///
/// The table has 16 shards. Each shard has its own lock, index, free list, and record storage. An ID holds the shard
/// index in its top 4 bits. The low 28 bits hold the record's virtual offset divided by 4:
///
/// ```text
/// 31      28 27                                                    0
/// ┏━━━━━━━━━┯━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
/// ┃  shard  │                  virtual offset / 4                  ┃
/// ┃ (4 bits)│                      (28 bits)                       ┃
/// ┗━━━━━━━━━┷━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
/// ```
///
/// The virtual offset space of a shard is 1 GiB. Chunks sit in it at a 32 KiB stride, with 16 KiB of record space
/// and a 16 KiB gap that has no memory:
///
/// ```text
/// 0          16 KiB      32 KiB     48 KiB      64 KiB
/// ┏━━━━━━━━━━━┓┄┄┄┄┄┄┄┄┄┄┄┏━━━━━━━━━━━┓┄┄┄┄┄┄┄┄┄┄┄┏━━━━━━━
/// ┃  chunk 0  ┃    gap    ┃  chunk 1  ┃    gap    ┃  ...
/// ┗━━━━━━━━━━━┛┄┄┄┄┄┄┄┄┄┄┄┗━━━━━━━━━━━┛┄┄┄┄┄┄┄┄┄┄┄┗━━━━━━━
/// ```
///
/// The gap keeps free ranges in adjacent chunks from merging. Because of the gap, a shard holds at most 512 MiB of
/// records. [`SymbolTable::new`] clamps the total capacity to 8 GiB.
///
/// ## Hash bits
///
/// One 64-bit hash of each string drives every decision:
///
/// - Bits 60 to 63 select the shard.
/// - Bits 0 to 7 select the witness slot.
/// - Bits 32 to 37 select the `resident` filter word, and bits 0 to 5 and 16 to 21 select its two bits.
/// - The shard index uses the full hash to place and find the entry.
///
/// ## Lock-free reads
///
/// An owner of an ID reads its string with [`SymbolTable::get_unchecked`], which takes no lock. The record cannot
/// move or change while a reference is live, and its chunk stays allocated until the table drops.
///
/// ## Two-hit admission
///
/// A string enters the table only on its second sighting in the same thread. The first sighting goes into a
/// thread-local witness slot and returns `None`. This keeps one-off strings, such as unique IDs in tags, from filling
/// the capacity. The cost is that the first sighting of a recurring string is not interned, and the caller must store
/// it another way. A string that alternates between threads, or that shares a slot with another string, can need
/// more sightings.
///
/// Each thread that interns keeps 256 witness slots of 280 bytes, that is 70 KiB. A first sighting checks the
/// `resident` filter and the witness without a shard lock.
///
/// ## Memory
///
/// Released records return their space to the shard's free list for reuse. Chunks never return to the allocator until
/// the table drops, so the table keeps its peak chunk usage.
///
/// ## Thread safety
///
/// `SymbolTable` is `Send` and `Sync` through its fields, with no unsafe impl. The shard locks guard all index and
/// free-list changes. Record headers use atomic reference counts, and chunk pointers are atomics.
pub struct SymbolTable {
    /// The unique ID of this table in witness slots.
    table_id: usize,

    /// The hasher for all strings in this table.
    hasher: RandomState,

    /// The shards of this table.
    shards: [Shard; SHARD_COUNT],
}

impl SymbolTable {
    /// Creates a table with at most `capacity` bytes of string records, distributed across 16 shards.
    ///
    /// The table rounds `capacity` down to a multiple of 4 and clamps it to 8 GiB. Each shard gets an equal part, and
    /// the first shards get the remainder. Index and admission-filter storage are separate. Record chunks are
    /// allocated on demand.
    pub fn new(capacity: usize) -> Self {
        let units = (capacity / RECORD_ALIGN).min(MAX_SHARD_BYTES / RECORD_ALIGN * SHARD_COUNT);
        Self {
            table_id: NEXT_TABLE.fetch_add(1, Ordering::Relaxed),
            hasher: RandomState::default(),
            shards: std::array::from_fn(|i| {
                Shard::new((units / SHARD_COUNT + usize::from(i < units % SHARD_COUNT)) * RECORD_ALIGN)
            }),
        }
    }

    /// Returns the number of active symbols.
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.state.read().unwrap().entries.len())
            .sum()
    }

    /// Returns whether the dictionary has any active symbols.
    pub fn is_empty(&self) -> bool {
        self.shards
            .iter()
            .all(|shard| shard.state.read().unwrap().entries.is_empty())
    }

    /// Returns the number of bytes occupied by active records, including their headers and padding.
    pub fn len_bytes(&self) -> usize {
        self.shards.iter().map(|shard| shard.state.read().unwrap().used).sum()
    }

    /// Returns the capacity of the table in record bytes.
    pub fn capacity_bytes(&self) -> usize {
        self.shards.iter().map(|shard| shard.capacity).sum()
    }

    /// Splits an ID into its shard and the virtual offset of its record.
    #[inline]
    fn split_id(&self, id: u32) -> (&Shard, usize) {
        let shard = &self.shards[(id >> (32 - SHARD_BITS)) as usize];
        let offset = id as usize % (SHARD_ADDRESS_BYTES / RECORD_ALIGN) * RECORD_ALIGN;
        (shard, offset)
    }

    /// Builds an ID from a shard index and a virtual offset.
    ///
    /// The offset must be a multiple of `RECORD_ALIGN` and below `SHARD_ADDRESS_BYTES`.
    #[inline]
    fn make_id(shard: usize, offset: usize) -> u32 {
        ((shard << (32 - SHARD_BITS)) | (offset / RECORD_ALIGN)) as u32
    }

    /// Returns the existing symbol, or admits a repeat if there is space.
    ///
    /// Returns `None` in these cases:
    ///
    /// - `value` is longer than 256 bytes.
    /// - The table has no symbol for `value`, and this is not a repeat sighting in the current thread.
    /// - The shard has no space for a new record.
    ///
    /// A first observation never takes a dictionary write lock. It takes a read lock only if the `resident` filter
    /// reports a possible match.
    pub fn try_intern(&self, value: &str) -> Option<Symbol<'_>> {
        if value.len() > WITNESS_BYTES {
            return None;
        }
        let hash = self.hasher.hash_one(value);
        let shard_index = (hash >> (64 - SHARD_BITS)) as usize;
        let shard = &self.shards[shard_index];
        let size = Record::allocation_size(value.len());
        if size > shard.capacity {
            return None;
        }
        let (index, bits) = Shard::filter_location(hash);
        let filter = &shard.resident[index];
        let matches = |offset: &u32| {
            // SAFETY: The shard lock keeps all indexed records live during lookup.
            unsafe { Record::value(shard.record(*offset as usize)) == value }
        };
        if filter.load(Ordering::Acquire) & bits == bits {
            let state = shard.state.read().unwrap();
            if let Some(&offset) = state.entries.find(hash, matches) {
                let id = Self::make_id(shard_index, offset as usize);
                // SAFETY: The shard read lock prevents the record's last reference from dropping.
                unsafe { self.retain(id) };
                return Some(Symbol { table: self, id });
            }
        }
        if !Witness::seen(self.table_id, hash, value) {
            return None;
        }

        let mut state = shard.state.write().unwrap();
        if let Some(&offset) = state.entries.find(hash, matches) {
            let id = Self::make_id(shard_index, offset as usize);
            // SAFETY: The shard write lock prevents the record's last reference from dropping.
            unsafe { self.retain(id) };
            return Some(Symbol { table: self, id });
        }
        let offset = shard.reserve(&mut state, size)?;
        let record = shard.record(offset);
        // SAFETY: This range is reserved under the write lock, aligned, and large enough for the value.
        unsafe {
            record.write(Record {
                refs: AtomicU32::new(1),
                len: value.len() as u32,
            });
            record
                .add(1)
                .cast::<u8>()
                .copy_from_nonoverlapping(value.as_ptr(), value.len());
        }
        state.entries.insert_unique(hash, offset as u32, |&offset| {
            // SAFETY: Only live records are in the index.
            self.hasher
                .hash_one(unsafe { Record::value(shard.record(offset as usize)) })
        });
        state.used += size;
        filter.fetch_or(bits, Ordering::Release);
        Some(Symbol {
            table: self,
            id: Self::make_id(shard_index, offset),
        })
    }

    /// Borrows a string by ID without taking a lock.
    ///
    /// # Safety
    ///
    /// `id` must belong to this table, and an owned reference must remain live for the entire returned borrow.
    #[inline]
    pub unsafe fn get_unchecked(&self, id: u32) -> &str {
        let (shard, offset) = self.split_id(id);
        // SAFETY: The caller's live reference keeps the record initialized and in place for the borrow.
        Record::value(shard.record(offset))
    }

    /// Adds one owned reference to a live ID.
    ///
    /// The count uses a `Relaxed` increment, because the caller already holds a live reference. The process aborts if
    /// the count reaches `i32::MAX`.
    ///
    /// # Safety
    ///
    /// `id` must have a live reference in this table for the duration of the call. Release the new reference once.
    #[inline]
    pub unsafe fn retain(&self, id: u32) {
        let (shard, offset) = self.split_id(id);
        // SAFETY: The caller's live reference keeps the record header initialized.
        if (*shard.record(offset)).refs.fetch_add(1, Ordering::Relaxed) >= i32::MAX as u32 {
            std::process::abort();
        }
    }

    /// Releases one owned reference to an ID.
    ///
    /// The release follows this protocol:
    ///
    /// 1. If the count is above 1, a `Release` compare-and-swap loop decrements it, and the method returns. This fast
    ///    path takes no lock. The `Release` ordering orders the caller's earlier reads before any later reuse.
    /// 2. If the count is 1, the method takes the shard write lock first, and the count stays 1 while it waits.
    /// 3. Under the lock, an `AcqRel` decrement runs. If the old count is not 1, a concurrent `try_intern` retained the
    ///    record during the wait, and the method returns.
    /// 4. If the count reaches zero, this thread frees the record. It removes the record from the index, subtracts its
    ///    size from `used`, and puts its range on the free list.
    /// 5. The removal counts toward the next rebuild of the `resident` filter.
    ///
    /// A lookup or admission finds and retains records only under the shard lock. So a lookup cannot retain a record
    /// after its count reaches zero. A freed range returns to the free list and never to the allocator. Later
    /// admissions can overwrite it.
    ///
    /// # Safety
    ///
    /// The caller must own a reference to `id` in this table, and must not use borrows that depend on it afterwards.
    pub unsafe fn release(&self, id: u32) {
        let (shard, offset) = self.split_id(id);
        let record = shard.record(offset);
        // SAFETY: The caller's reference keeps the record header initialized until the count reaches zero.
        let refs = &(*record).refs;
        // Only the last reference needs the shard lock. The count remains nonzero until that lock is held.
        if refs
            .fetch_update(Ordering::Release, Ordering::Relaxed, |n| (n > 1).then(|| n - 1))
            .is_ok()
        {
            return;
        }
        let mut state = shard.state.write().unwrap();
        if refs.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        // SAFETY: The write lock is held and the record is still in the index, so its bytes are live.
        let value = Record::value(record);
        let hash = self.hasher.hash_one(value);
        let size = Record::allocation_size(value.len());
        state
            .entries
            .find_entry(hash, |entry| *entry as usize == offset)
            .unwrap()
            .remove();
        state.used -= size;
        state.reclaimed.insert(ReclaimedEntry::new(offset, size));
        shard.refresh_filter(&mut state, &self.hasher);
    }
}

/// An owned reference to an interned symbol, tied to the lifetime of its table.
///
/// Cloning a symbol adds a reference, and dropping it releases one. The symbol dereferences to its string without a
/// lock.
pub struct Symbol<'a> {
    /// The table that owns the record.
    table: &'a SymbolTable,

    /// The ID of the record in `table`.
    id: u32,
}

impl Symbol<'_> {
    /// Returns the ID of this symbol in its table.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Transfers this reference to an ID.
    ///
    /// The symbol does not release its reference, so the caller owns one reference to the ID. Call
    /// [`SymbolTable::release`] exactly once for it, on the same table. Until then, [`SymbolTable::get_unchecked`] can
    /// borrow the string. A leaked ID keeps its record in the table until the table drops.
    pub fn into_id(self) -> u32 {
        let id = self.id;
        mem::forget(self);
        id
    }
}

impl Deref for Symbol<'_> {
    type Target = str;

    fn deref(&self) -> &str {
        // SAFETY: The reference owned by this symbol outlives the borrow.
        unsafe { self.table.get_unchecked(self.id) }
    }
}

impl Clone for Symbol<'_> {
    fn clone(&self) -> Self {
        // SAFETY: This symbol owns a live reference.
        unsafe { self.table.retain(self.id) };
        Self {
            table: self.table,
            id: self.id,
        }
    }
}

impl Drop for Symbol<'_> {
    fn drop(&mut self) {
        // SAFETY: This symbol owns the reference it releases.
        unsafe { self.table.release(self.id) };
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn singletons_leave_space_for_late_shared_symbols() {
        let table = SymbolTable::new(8 * 1024 * 1024);
        for n in 0..200_000 {
            let value = format!("correlation_id:12345678-1234-1234-1234-{n:012x}");
            assert_eq!(value.len(), 51);
            assert!(table.try_intern(&value).is_none());
        }
        assert_eq!(table.len_bytes(), 0);
        assert!(table
            .shards
            .iter()
            .all(|shard| shard.chunks.iter().all(|slot| slot.load(Ordering::Relaxed).is_null())));

        let shared = "deployment:service-shared-across-context";
        assert_eq!(shared.len(), 40);
        assert!(table.try_intern(shared).is_none());
        let a = table.try_intern(shared).unwrap();
        let b = table.try_intern(shared).unwrap();
        assert_eq!(a.id(), b.id());
        assert_eq!(&*b, shared);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn concurrent_id_reads_do_not_take_the_shard_lock() {
        let value = "deployment:read-while-the-writer-holds-the-shard";
        let table = SymbolTable::new(64 * 1024);
        assert!(table.try_intern(value).is_none());
        let symbol = table.try_intern(value).unwrap();
        let (shard, _) = table.split_id(symbol.id());
        let _writer = shard.state.write().unwrap();
        thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..10_000 {
                        assert_eq!(&*symbol, value);
                    }
                });
            }
        });
    }

    #[test]
    fn churn_clears_stale_bits_without_hiding_live_symbols() {
        let table = SymbolTable::new(64 * 1024);
        let value = "deployment:highly-reused";
        assert!(table.try_intern(value).is_none());
        let live = table.try_intern(value).unwrap();
        let shard_id = live.id() >> (32 - SHARD_BITS);

        let churn = (0..).map(|n| format!("deployment:one-live-at-a-time:{n}"));
        for text in churn
            .filter(|value| (table.hasher.hash_one(value.as_str()) >> (64 - SHARD_BITS)) as u32 == shard_id)
            .take(2 * MIN_FILTER_REFRESH_REMOVALS)
        {
            assert!(table.try_intern(&text).is_none());
            drop(table.try_intern(&text).unwrap());
        }

        let (shard, _) = table.split_id(live.id());
        let occupied: u32 = shard
            .resident
            .iter()
            .map(|word| word.load(Ordering::Relaxed).count_ones())
            .sum();
        assert!((1..=2).contains(&occupied));
        thread::scope(|scope| {
            scope
                .spawn(|| assert_eq!(table.try_intern(value).unwrap().id(), live.id()))
                .join()
                .unwrap();
        });
    }

    #[test]
    fn final_reference_reclaims_the_exact_record() {
        let value = "deployment:";
        let table = SymbolTable::new(2048);
        assert!(table.try_intern(value).is_none());
        let a = table.try_intern(value).unwrap();
        let id = a.id();
        let b = a.clone();
        let bytes = table.len_bytes();
        drop(a);
        assert_eq!(table.len_bytes(), bytes);
        assert_eq!(&*b, value);
        drop(b);
        assert!(table.is_empty());
        assert_eq!(table.len_bytes(), 0);
        let next = table.try_intern(value).unwrap();
        assert_eq!(next.id(), id);
        assert_eq!(table.len_bytes(), bytes);
    }
}
