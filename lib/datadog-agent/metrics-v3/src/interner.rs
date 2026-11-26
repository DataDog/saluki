//! Exact dictionary indexing over the writer's contiguous column buffers.
//!
//! V3 dictionary IDs start at 1 for each payload. ID 0 represents an empty or absent value.
//!
//! # Design
//!
//! The caller stores keys in an append-only arena. The index stores offsets, not pointers, so the arena can grow.
//! This avoids a second copy of each key but requires the caller to keep indexed ranges intact until reset.
//! Reset both the index and arena between payloads to reuse their allocations and start IDs again at 1.

use std::{
    any::Any,
    hash::{BuildHasher, Hash},
    sync::Arc,
};

use hashbrown::{hash_table::Entry, HashTable};

#[derive(Clone, Copy, Debug)]
struct Record {
    hash: u64,
    start: usize,
    len: usize,
    id: i64,
}

type SymbolHint = (usize, u32);
const SYMBOL_CACHE_SIZE: usize = 256;

#[derive(Clone, Copy, Debug)]
struct SymbolAlias {
    hint: SymbolHint,
    record: Record,
}

/// An exact dictionary index for one caller-owned arena.
#[derive(Debug, Default)]
pub(crate) struct Interner {
    index: HashTable<Record>,
    hasher: foldhash::fast::RandomState,
    symbols: Option<Box<[Option<SymbolAlias>; SYMBOL_CACHE_SIZE]>>,
}

impl Interner {
    /// Looks up the 1-based ID for a key or assigns the next ID on a miss.
    ///
    /// Returns `(id, is_new)`, where `is_new` is `true` only when this call inserts the key.
    /// On a miss, `prefix` appends the framing and the interner appends the key bytes or elements.
    /// Keep the same arena and preserve its indexed ranges until `clear`.
    pub fn get_or_insert<T: Copy + Eq + Hash>(
        &mut self, key: &[T], arena: &mut Vec<T>, prefix: impl FnOnce(&mut Vec<T>),
    ) -> (i64, bool) {
        let (record, is_new) = self.intern(key, arena, prefix);
        (record.id, is_new)
    }

    /// Looks up a byte key with a symbol hint that can avoid hashing on a cache hit.
    ///
    /// The hint does not determine identity. The cached key must also match the bytes in the arena.
    pub fn intern_symbol(
        &mut self, hint: SymbolHint, key: &[u8], arena: &mut Vec<u8>, prefix: impl FnOnce(&mut Vec<u8>),
    ) -> i64 {
        let (table, id) = hint;
        let slot = ((table >> 6) ^ id as usize ^ (id >> 28) as usize) % SYMBOL_CACHE_SIZE;
        if let Some(alias) = self.symbols.as_ref().and_then(|symbols| symbols[slot]) {
            let record = alias.record;
            // A symbol ID may be reclaimed while the writer is still live.
            if alias.hint == hint && arena[record.start..record.start + record.len] == *key {
                return record.id;
            }
        }

        let (record, _) = self.intern(key, arena, prefix);
        self.symbols.get_or_insert_with(|| Box::new([None; SYMBOL_CACHE_SIZE]))[slot] =
            Some(SymbolAlias { hint, record });
        record.id
    }

    fn intern<T: Copy + Eq + Hash>(
        &mut self, key: &[T], arena: &mut Vec<T>, prefix: impl FnOnce(&mut Vec<T>),
    ) -> (Record, bool) {
        let hash = self.hasher.hash_one(key);
        let next_id = self.index.len() as i64 + 1;
        match self.index.entry(
            hash,
            |record| record.hash == hash && arena[record.start..record.start + record.len] == *key,
            |record| record.hash,
        ) {
            Entry::Occupied(entry) => (*entry.get(), false),
            Entry::Vacant(entry) => {
                prefix(arena);
                let start = arena.len();
                arena.extend_from_slice(key);
                let record = Record {
                    hash,
                    start,
                    len: key.len(),
                    id: next_id,
                };
                entry.insert(record);
                (record, true)
            }
        }
    }

    /// Clears dictionary IDs and symbol aliases without releasing their backing storage.
    ///
    /// Clear the arena before starting the next payload.
    pub fn clear(&mut self) {
        self.index.clear();
        if let Some(symbols) = &mut self.symbols {
            symbols.fill(None);
        }
    }
}

const PINNED_SYMBOL_SLOTS: usize = 512;
const PINNED_SYMBOL_PROBES: usize = 4;

pub(crate) type SymbolOwner = Arc<dyn Any + Send + Sync>;

#[derive(Debug)]
struct PinnedSymbol {
    hint: SymbolHint,
    address: usize,
    len: usize,
    id: i64,
    _owner: SymbolOwner,
}

#[derive(Debug, Default)]
pub(crate) struct PinnedSymbols {
    slots: Vec<Option<PinnedSymbol>>,
}

impl PinnedSymbols {
    pub fn intern(
        &mut self, hint: SymbolHint, text: &str, intern: impl FnOnce() -> i64, retain: impl FnOnce() -> SymbolOwner,
    ) -> i64 {
        if self.slots.is_empty() {
            self.slots.resize_with(PINNED_SYMBOL_SLOTS, || None);
        }
        let (_, symbol) = hint;
        let mixed = (symbol ^ symbol.rotate_right(15)).wrapping_mul(0x9e37_79b9);
        let start = (mixed >> (u32::BITS - PINNED_SYMBOL_SLOTS.ilog2())) as usize;
        let address = text.as_ptr().addr();
        let mut destination = start;
        for probe in 0..PINNED_SYMBOL_PROBES {
            let idx = (start + probe) & (PINNED_SYMBOL_SLOTS - 1);
            match &self.slots[idx] {
                Some(slot) if slot.hint == hint && slot.address == address && slot.len == text.len() => return slot.id,
                Some(_) => {}
                None => {
                    destination = idx;
                    break;
                }
            }
        }
        let id = intern();
        self.slots[destination] = Some(PinnedSymbol {
            hint,
            address,
            len: text.len(),
            id,
            _owner: retain(),
        });
        id
    }

    pub fn clear(&mut self) {
        self.slots.iter_mut().for_each(|slot| *slot = None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_exact_slices_across_growth_and_clear() {
        let mut index = Interner::default();
        let mut arena = Vec::new();
        let prefix = |arena: &mut Vec<i64>| arena.push(-1);
        // IDs start at 1. A key that is a prefix of another key gets its own ID.
        assert_eq!(index.get_or_insert(&[1, 2], &mut arena, prefix), (1, true));
        assert_eq!(index.get_or_insert(&[1], &mut arena, prefix), (2, true));
        for n in 3..256 {
            assert_eq!(index.get_or_insert(&[n, n + 1], &mut arena, prefix), (n, true));
        }
        // Arena and index growth do not invalidate an earlier key or duplicate its framing.
        assert_eq!(index.get_or_insert(&[1, 2], &mut arena, prefix), (1, false));
        assert_eq!(&arena[..5], &[-1, 1, 2, -1, 1]);

        // Clearing both structures restarts the IDs and preserves the backing allocations for reuse.
        index.clear();
        arena.clear();
        assert_eq!(index.get_or_insert(&[1], &mut arena, prefix), (1, true));
        assert_eq!(arena, [-1, 1]);
    }

    #[test]
    fn recycled_symbol_id_does_not_alias_a_different_string() {
        let mut index = Interner::default();
        let mut arena = Vec::new();
        let hint = (11, 123);
        assert_eq!(index.intern_symbol(hint, b"old", &mut arena, |_| {}), 1);
        assert_eq!(index.intern_symbol(hint, b"new", &mut arena, |_| {}), 2);
        assert_eq!(index.get_or_insert(b"old", &mut arena, |_| {}), (1, false));
        assert_eq!(index.intern_symbol(hint, b"new", &mut arena, |_| {}), 2);
        assert_eq!(arena, b"oldnew");
    }
}
