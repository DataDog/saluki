use std::sync::{
    atomic::{
        AtomicUsize,
        Ordering::{AcqRel, Acquire, Release},
    },
    RwLock,
};

const SLOT_BITS_LEN: usize = usize::BITS as usize;
const SLOT_SHIFT_BITS: u32 = usize::BITS.trailing_zeros();
const INTRA_SLOT_MASK: usize = (1 << SLOT_SHIFT_BITS) - 1;

const fn indexes_for_bit(bit: usize) -> (usize, usize) {
    (bit >> SLOT_SHIFT_BITS, bit & INTRA_SLOT_MASK)
}

#[derive(Debug)]
pub struct ConcurrentBitmap {
    slots: RwLock<Vec<AtomicUsize>>,
}

impl ConcurrentBitmap {
    pub fn new() -> Self {
        Self {
            slots: RwLock::new(Vec::with_capacity(4)),
        }
    }

    pub fn is_set(&self, bit: usize) -> bool {
        let (slot_idx, intra_slot_idx) = indexes_for_bit(bit);
        self.with_slot_no_backfill(slot_idx, |slot| slot.load(Acquire) & (1 << intra_slot_idx) != 0)
            .unwrap_or(false)
    }

    pub fn set(&self, bit: usize) {
        let (slot_idx, intra_slot_idx) = indexes_for_bit(bit);
        self.with_slot(slot_idx, |slot| slot.fetch_or(1 << intra_slot_idx, AcqRel));
    }

    pub fn clear_all(&self) {
        // We _specifically_ take a write lock to block other operations while we're clearing the bitmap.
        #[allow(clippy::readonly_write_lock)]
        let slots_write = self.slots.write().unwrap();
        for slot in slots_write.iter() {
            slot.store(0, Release);
        }
    }

    fn with_slot_no_backfill<F, R>(&self, slot_idx: usize, f: F) -> Option<R>
    where
        F: FnOnce(&AtomicUsize) -> R,
    {
        let slots = self.slots.read().unwrap();
        slots.get(slot_idx).map(f)
    }

    fn with_slot<F, R>(&self, slot_idx: usize, f: F) -> R
    where
        F: FnOnce(&AtomicUsize) -> R,
    {
        loop {
            let slots_read = self.slots.read().unwrap();
            match slots_read.get(slot_idx) {
                Some(slot) => return f(slot),
                None => {
                    // We need to backfill the slot. This means we might need to backfill _multiple_ slots between the
                    // highest slot we've initialized so far and the slot we're trying to access.
                    drop(slots_read);
                    let mut slots_write = self.slots.write().unwrap();
                    while slots_write.len() < slot_idx + 1 {
                        let new_slot = AtomicUsize::new(0);
                        slots_write.push(new_slot);
                    }
                }
            }
        }
    }

    fn slots_initialized(&self) -> usize {
        self.slots.read().unwrap().len()
    }

    pub fn iter_rev_unset(&self) -> BitmapUnsetRevIter<'_> {
        BitmapUnsetRevIter::new(self)
    }
}

pub struct BitmapUnsetRevIter<'a> {
    bitmap: &'a ConcurrentBitmap,
    current_slot_idx: usize,
    current_slot: Option<usize>,
    current_intra_slot_idx: usize,
}

impl<'a> BitmapUnsetRevIter<'a> {
    pub fn new(bitmap: &'a ConcurrentBitmap) -> Self {
        // If we have no slots initialized, the iterator is a no-op.
        //
        // Otherwise, we start ourselves at the highest slot, and the highest bit in that slot, as we're iterating in reverse.
        let (current_slot_idx, current_slot, current_intra_slot_idx) = match bitmap.slots_initialized() {
            0 => (0, None, 0),
            n => {
                let slot_idx = n - 1;
                let slot = bitmap
                    .with_slot_no_backfill(slot_idx, |slot| slot.load(Acquire))
                    .expect("slot must exist if slots_initialized() > 0");

                (slot_idx, Some(slot), SLOT_BITS_LEN)
            }
        };

        Self {
            bitmap,
            current_slot_idx,
            current_slot,
            current_intra_slot_idx,
        }
    }
}

impl<'a> Iterator for BitmapUnsetRevIter<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.current_slot {
                Some(slot) => {
                    while self.current_intra_slot_idx != 0 {
                        self.current_intra_slot_idx -= 1;

                        if slot & (1 << self.current_intra_slot_idx) == 0 {
                            return Some((self.current_slot_idx << SLOT_SHIFT_BITS) + self.current_intra_slot_idx);
                        }
                    }

                    // We've exhausted all bits in the current slot, so we need to move to the next slot.
                    //
                    // If we've just iterated over the first slot, then we're done. Otherwise, we move down, reset our
                    // intra-slot index, and get the next slot's value. This doesn't _really_ need to be fallible, since
                    // we know it's going to be there as we don't delete slots... but we use the fallibility of
                    // `self.current_slot` to know when to terminate iteration, so this is just easiest.
                    self.current_slot = if self.current_slot_idx == 0 {
                        None
                    } else {
                        self.current_slot_idx -= 1;
                        self.current_intra_slot_idx = SLOT_BITS_LEN;

                        self.bitmap
                            .with_slot_no_backfill(self.current_slot_idx, |slot| slot.load(Acquire))
                    }
                }

                // We've exhausted all slots.
                None => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_clear_all() {
        let bitmap = ConcurrentBitmap::new();

        assert!(!bitmap.is_set(0));
        assert!(!bitmap.is_set(1));
        assert!(!bitmap.is_set(2));
        assert!(!bitmap.is_set(3));
        assert_eq!(bitmap.slots_initialized(), 0);

        bitmap.set(0);
        bitmap.set(1);
        bitmap.set(2);
        bitmap.set(3);

        assert!(bitmap.is_set(0));
        assert!(bitmap.is_set(1));
        assert!(bitmap.is_set(2));
        assert!(bitmap.is_set(3));
        assert_eq!(bitmap.slots_initialized(), 1);

        bitmap.clear_all();

        assert!(!bitmap.is_set(0));
        assert!(!bitmap.is_set(1));
        assert!(!bitmap.is_set(2));
        assert!(!bitmap.is_set(3));
        assert_eq!(bitmap.slots_initialized(), 1);
    }

    #[test]
    fn sparse_bits() {
        const SECOND_SLOT_BIT: usize = SLOT_BITS_LEN + 1;
        const FAR_AWAY_SLOT_BIT: usize = 1_000_000;
        const SLOTS_FOR_FAR_AWAY_SLOT_BIT: usize = (FAR_AWAY_SLOT_BIT / usize::BITS as usize) + 1;

        let bitmap = ConcurrentBitmap::new();

        bitmap.set(0);
        bitmap.set(2);
        bitmap.set(SECOND_SLOT_BIT);
        bitmap.set(FAR_AWAY_SLOT_BIT);

        assert_eq!(bitmap.slots_initialized(), SLOTS_FOR_FAR_AWAY_SLOT_BIT);

        for bit in 0..=FAR_AWAY_SLOT_BIT {
            let should_be_set = bit == 0 || bit == 2 || bit == SECOND_SLOT_BIT || bit == FAR_AWAY_SLOT_BIT;
            assert_eq!(bitmap.is_set(bit), should_be_set, "bit {}", bit);
        }
    }
}
