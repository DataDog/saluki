use std::sync::atomic::{AtomicUsize, Ordering::{Acquire, Relaxed, AcqRel}};

use sharded_slab::{Entry, Slab};

const SLOT_SHIFT_BITS: u32 = usize::BITS.trailing_zeros();
const INTRA_SLOT_MASK: usize = (1 << SLOT_SHIFT_BITS) - 1;

// The highest bit of the state is reserved for the "we're actively backfilling" flag.
const BACKFILL_ACTIVE_BIT: usize = 1 << (usize::BITS - 1);
const BACKFILL_CLEAR_MASK: usize = !BACKFILL_ACTIVE_BIT;
const STATE_HIGHEST_SLOT_IDX_MASK: usize = BACKFILL_CLEAR_MASK;

const fn indexes_for_bit(bit: usize) -> (usize, usize) {
	(bit >> SLOT_SHIFT_BITS, bit & INTRA_SLOT_MASK)
}

pub struct ConcurrentBitmap {
	slots: Slab<AtomicUsize>,
	state: AtomicUsize,
}

impl ConcurrentBitmap {
	pub fn new() -> Self {
		Self {
			slots: Slab::new(),
			state: AtomicUsize::new(0),
		}
	}

	pub fn set(&self, bit: usize) {
		// Determine which slot our bit falls within, and the intra-slot index.
		let (slot_idx, intra_slot_idx) = indexes_for_bit(bit);

		// Attempt to load the slot.
		//
		// If it doesn't exist, we'll attempt to backfill all the slots necessary to reach it.
		let slot = self.get_or_backfill_slot(slot_idx);
	}

	fn get_or_backfill_slot(&self, slot_idx: usize) -> Option<Entry<'_, AtomicUsize>> {
		loop {
			// Attempt to load the slot.
			match self.slots.get(slot_idx) {
				Some(slot) => Some(slot),
				None => {
					// We need to backfill the slot. This means we might need to backfill _multiple_ slots between the
					// highest slot we've initialized so far and the slot we're trying to access.

					// First, we need to acquire the backfill lock.
					if self.acquire_backfill_lock() {
						// We've acquired the backfill lock. We can now insert the slot, and any backfill any necessary
						// slots to reach it.
						loop {
							let highest_slot_idx = self.highest_slot_idx();
							let new_slot_idx = match self.slots.insert(AtomicUsize::new(0)) {
								Some(new_slot_idx) => {
									assert_eq!(new_slot_idx, highest_slot_idx + 1, "non-contiguous slot backfill: expected to create slot {}, got {}", highest_slot_idx + 1, new_slot_idx);

									// We've inserted a new slot. We need to update the highest slot index in the state.
									//
									// All we need to do is increment the state, since we know our new slot index is the
									// next value after the current highest slot index.
									self.state.fetch_add(1, AcqRel);

									new_slot_idx
								},
								None => {
									// We ran out of room to insert a new slot. This should ideally never happen, but we
									// just need to release the backfill lock before returning.
									self.release_backfill_lock();

									return None
								}
							};

							assert_eq!(new_slot_idx, highest_slot_idx + 1);
							if new_slot_idx == slot_idx {
								// We've inserted the slot we were looking for.
								break;
							}
						}

						// Release the backfill lock and try again. We should now have the slot we're looking for, and if we
						// don't... something is very wrong.
						self.release_backfill_lock();

						Some(self.slots.get(slot_idx).expect("slot should exist after just being inserted"))
					} else {
						// Someone else has the backfill lock, so wait until it's been released and then try again.
						todo!()
					}
				}
			}
		}
	}

	fn highest_slot_idx(&self) -> usize {
		(self.state.load(Acquire) & STATE_HIGHEST_SLOT_IDX_MASK) as usize
	}
	
	fn acquire_backfill_lock(&self) -> bool {
		todo!()
	}
}