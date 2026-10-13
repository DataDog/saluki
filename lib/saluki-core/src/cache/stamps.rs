//! Change stamps: counters a cache advances whenever one of its keys changes.

use std::{
    hash::{BuildHasher, Hash},
    sync::atomic::{AtomicU32, Ordering},
};

/// The number of key buckets a cache's change stamps cover.
const BUCKETS: usize = 4_096;

/// Counters, one per bucket of keys, that a cache advances after every insertion, removal, eviction, and expiry of a
/// key in the bucket.
///
/// A copy of an entry taken outside the cache, such as by a per-task cache in front of a shared one, is exactly what
/// the cache holds for its key for as long as the key's stamp equals the stamp read before the lookup that produced
/// the copy: every change to the key after that read advances the stamp. Changes to other keys of the same bucket
/// advance it too, so a stamp that moved means only that the copy may be stale.
pub(crate) struct ChangeStamps<H> {
    hasher: H,
    stamps: Box<[AtomicU32]>,
}

impl<H> ChangeStamps<H> {
    /// The bytes change stamps allocate.
    pub(crate) const BYTES: usize = BUCKETS * size_of::<AtomicU32>();
}

impl<H: BuildHasher> ChangeStamps<H> {
    /// Creates change stamps that bucket keys by their hash under `hasher`, the cache's own.
    pub(crate) fn new(hasher: H) -> Self {
        Self {
            hasher,
            stamps: (0..BUCKETS).map(|_| AtomicU32::new(0)).collect(),
        }
    }

    fn bucket<K: Hash + ?Sized>(&self, key: &K) -> &AtomicU32 {
        let hash = self.hasher.hash_one(key);
        &self.stamps[hash as usize & (BUCKETS - 1)]
    }

    /// Returns the current stamp of `key`.
    ///
    /// Read it before looking the key up in the cache: a change after the read then always shows as a different stamp.
    pub(crate) fn current<K: Hash + ?Sized>(&self, key: &K) -> u32 {
        self.bucket(key).load(Ordering::Acquire)
    }

    /// Advances the stamp of `key`, after the cache changed what it holds for it.
    pub(crate) fn advance<K: Hash + ?Sized>(&self, key: &K) {
        self.bucket(key).fetch_add(1, Ordering::AcqRel);
    }
}

/// Advances change stamps by key, for the cache's eviction hook, which does not know the cache's hasher.
pub(crate) trait AdvanceStamp<K>: Send + Sync {
    /// Advances the stamp of `key`.
    fn advance(&self, key: &K);
}

impl<K, H> AdvanceStamp<K> for ChangeStamps<H>
where
    K: Hash,
    H: BuildHasher + Send + Sync,
{
    fn advance(&self, key: &K) {
        ChangeStamps::advance(self, key);
    }
}

#[cfg(test)]
mod tests {
    use saluki_common::hash::NoopU64BuildHasher;

    use super::*;

    #[test]
    fn a_stamp_moves_when_its_key_or_a_key_of_its_bucket_changes_and_only_then() {
        let stamps = ChangeStamps::new(NoopU64BuildHasher);
        let key = 7_u64;
        let same_bucket = key + BUCKETS as u64;
        let other_bucket = key + 1;

        let read = stamps.current(&key);
        stamps.advance(&other_bucket);
        assert_eq!(
            stamps.current(&key),
            read,
            "a change to another bucket leaves the stamp"
        );
        stamps.advance(&key);
        let after_own = stamps.current(&key);
        assert_ne!(after_own, read, "a change to the key moves its stamp");
        stamps.advance(&same_bucket);
        assert_ne!(
            stamps.current(&key),
            after_own,
            "a change to a key of the same bucket moves it too"
        );
    }
}

// Loom model of the change-stamp protocol that a front cache relies on: a reader reads a key's stamp, then looks the
// key up under the cache shard's lock, and keeps the copy with that stamp (`FrontCache::lookup`, then the shared lookup
// in `ContextResolver::resolve`); a writer changes what the cache holds for the key under the lock, then advances the
// stamp (`Cache::insert`, `Cache::remove`, the eviction hook, and `drive_expiration`). A copy whose stamp is still the
// key's, read once the writer's changes happened before the read, must be what the cache holds, or a front cache would
// answer with a context the cache replaced or let go of.
//
// The model transcribes the protocol with loom primitives, since the real cache's shard lock is inside `quick_cache`,
// which loom cannot instrument: the shard is a loom mutex over the key's value, and the stamp a loom atomic with
// `ChangeStamps::current`'s and `ChangeStamps::advance`'s orderings. `read_then_look_up` and `change_then_advance` must
// stay in lockstep with the code above. `look_up_then_read` and `advance_then_change` transcribe the two orders the
// protocol forbids, which the models reject.
#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use loom::{
        sync::{
            atomic::{AtomicU32, Ordering},
            Arc, Mutex,
        },
        thread,
    };

    /// One key of a cache: what the cache holds for it, under the shard's lock, and its change stamp.
    struct Key {
        held: Mutex<Option<u32>>,
        stamp: AtomicU32,
    }

    /// A copy of what the cache held for the key, kept outside it with the stamp read for it.
    struct Copy {
        value: Option<u32>,
        stamp: u32,
    }

    fn read_then_look_up(key: &Key) -> Copy {
        let stamp = key.stamp.load(Ordering::Acquire);
        let value = *key.held.lock().unwrap();
        Copy { value, stamp }
    }

    fn look_up_then_read(key: &Key) -> Copy {
        let value = *key.held.lock().unwrap();
        let stamp = key.stamp.load(Ordering::Acquire);
        Copy { value, stamp }
    }

    fn change_then_advance(key: &Key, value: Option<u32>) {
        *key.held.lock().unwrap() = value;
        key.stamp.fetch_add(1, Ordering::AcqRel);
    }

    fn advance_then_change(key: &Key, value: Option<u32>) {
        key.stamp.fetch_add(1, Ordering::AcqRel);
        *key.held.lock().unwrap() = value;
    }

    /// A writer replaces the key's value and then removes it, while two readers copy it, at every interleaving loom
    /// explores. Once the writer finished, a copy whose stamp is still the key's is what the cache holds.
    fn model(read: fn(&Key) -> Copy, change: fn(&Key, Option<u32>)) {
        loom::model(move || {
            let key = Arc::new(Key {
                held: Mutex::new(Some(1)),
                stamp: AtomicU32::new(0),
            });
            let writer = {
                let key = Arc::clone(&key);
                thread::spawn(move || {
                    change(&key, Some(2));
                    change(&key, None);
                })
            };
            let other_reader = {
                let key = Arc::clone(&key);
                thread::spawn(move || read(&key))
            };
            let copy = read(&key);
            writer.join().unwrap();
            let other_copy = other_reader.join().unwrap();

            let stamp = key.stamp.load(Ordering::Acquire);
            let held = *key.held.lock().unwrap();
            for copy in [copy, other_copy] {
                if copy.stamp == stamp {
                    assert_eq!(copy.value, held, "a copy whose stamp holds is not what the cache holds");
                }
            }
        });
    }

    #[test]
    fn a_copy_whose_stamp_holds_is_what_the_cache_holds() {
        model(read_then_look_up, change_then_advance);
    }

    // Reading the stamp after the lookup can pair the value from before a change with the stamp from after it: loom
    // finds that interleaving.
    #[test]
    #[should_panic(expected = "a copy whose stamp holds is not what the cache holds")]
    fn reading_the_stamp_after_the_lookup_keeps_a_stale_copy() {
        model(look_up_then_read, change_then_advance);
    }

    // Advancing the stamp before the change lets a reader read the new stamp and then the old value: loom finds that
    // interleaving.
    #[test]
    #[should_panic(expected = "a copy whose stamp holds is not what the cache holds")]
    fn advancing_the_stamp_before_the_change_keeps_a_stale_copy() {
        model(read_then_look_up, advance_then_change);
    }
}
