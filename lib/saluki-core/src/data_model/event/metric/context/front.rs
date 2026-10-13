//! The front cache of a resolver: a small cache of resolved contexts in front of the context cache that clones share.

use std::{num::NonZeroUsize, sync::Arc};

use saluki_common::hash::NoopU64BuildHasher;
use saluki_metrics::Counter;

use super::{resolver::ContextCache, Context, ContextKey};
use crate::cache::ChangeStamps;

/// The lookups over which a front cache judges whether its slots hold the keys it is asked for often enough.
const WINDOW: u32 = 16_384;

/// The fewest lookups of a window whose key a slot held, below which the front cache steps aside.
const USEFUL_MIN: u32 = WINDOW / 16;

/// The lookups a front cache that stepped aside lets go to the shared cache before it tries again.
const BYPASS: u32 = 16 * WINDOW;

/// One entry: a context the shared cache returned for its key, and what is needed to know that it still holds it.
struct Slot {
    key: ContextKey,
    context: Context,

    /// The shared cache's change stamp for the key, read before the lookup that returned the context.
    stamp: u32,

    /// The coarse second in which the shared cache last returned the context.
    confirmed_at: u32,
}

/// What a front cache answers for a key.
pub(super) enum Lookup {
    /// The shared cache holds this context for the key.
    Hit(Context),

    /// Look the key up in the shared cache; if it holds a context, fill the front cache with it and this stamp (none
    /// while the front cache steps aside).
    Miss(Option<u32>),
}

/// A direct-mapped cache of contexts owned by one resolver clone, in front of the context cache that every clone
/// shares.
///
/// The shared cache is safe to use from many tasks at once, and every lookup that hits pays for it: a shard lock, an
/// access record for expiration, and its hit counters, all memory that every task writes. The front cache is owned by
/// one task: a hit compares a key and the shared cache's change stamp for it, then clones the context, and writes
/// nothing shared but the context's reference count. Each context lives in one slot, chosen by its key; a key that maps
/// to an occupied slot replaces what is there.
///
/// A hit gives what a lookup in the shared cache would, and everything the shared cache does on a hit still happens,
/// at most a second later:
///
/// - **Only what the shared cache holds.** A slot keeps the change stamp of its key, read before the lookup that
///   filled it, and is used only while the stamp is unchanged: an insertion, removal, eviction, or expiry of the key
///   advances it, so a slot never outlives the entry it copied (see `ChangeStamps`). A slot that may be stale is
///   dropped on its next use, or at the latest at the next [`maintain`][Self::maintain], so the front cache holds
///   no reference to a context that the shared cache let go of for longer than that.
/// - **Expiry.** A slot's first hit in each coarse second goes to the shared cache instead, which records the access
///   as it records any hit and confirms the slot. Expiration tracks accesses by the coarse second, so a context in
///   use expires exactly when it would without the front cache. (The shared cache's eviction order, under capacity
///   pressure, sees a context in use at least once a second rather than on every hit.)
/// - **Counters.** Hits are counted locally and added to the hit counters of the resolver and the shared cache by
///   [`maintain`][Self::maintain], which the owner calls periodically, and which the front cache also runs itself on
///   its first use in each coarse second; whatever is left is added when it is dropped.
///
/// Misses, and every lookup a slot does not answer, take the shared cache's path unchanged. When the keys asked for
/// are too many for the slots (fewer than one lookup in 16 of a window of 16,384 finds its key in a slot), the front
/// cache steps aside for 16 windows, sending every lookup to the shared cache without filling a slot, then tries again,
/// so that a working set it cannot hold costs little more than having no front cache.
pub(super) struct FrontCache {
    slots: Box<[Option<Slot>]>,
    stamps: Arc<ChangeStamps<NoopU64BuildHasher>>,

    /// Hits answered here and not yet added to the counters.
    hits: u64,
    resolved_existing: Counter,
    cache: ContextCache,

    /// The coarse second of the last maintenance.
    maintained_at: u32,

    /// Lookups in the current window, and how many of them found their key in a slot.
    lookups: u32,
    useful: u32,

    /// Lookups left before a front cache that stepped aside tries again.
    bypass: u32,
}

impl FrontCache {
    /// Creates an empty front cache of `entries` slots, rounded up to a power of two, in front of `cache`, that counts
    /// its hits in `resolved_existing` and in the cache's own hits.
    ///
    /// Returns `None` if `cache` was not built with change stamps, which a front cache needs to know what it holds.
    pub(super) fn new(entries: NonZeroUsize, cache: ContextCache, resolved_existing: Counter) -> Option<Self> {
        let entries = entries.get().checked_next_power_of_two().unwrap_or(usize::MAX / 2 + 1);
        Some(Self {
            slots: (0..entries).map(|_| None).collect(),
            stamps: cache.change_stamps()?,
            hits: 0,
            resolved_existing,
            cache,
            maintained_at: 0,
            lookups: 0,
            useful: 0,
            bypass: 0,
        })
    }

    /// Returns an empty front cache with the size, the cache, and the counters of this one.
    pub(super) fn empty_like(&self) -> Self {
        Self {
            slots: (0..self.slots.len()).map(|_| None).collect(),
            stamps: Arc::clone(&self.stamps),
            hits: 0,
            resolved_existing: self.resolved_existing.clone(),
            cache: self.cache.clone(),
            maintained_at: 0,
            lookups: 0,
            useful: 0,
            bypass: 0,
        }
    }

    /// Returns the bytes the slots of a front cache of `entries` entries take.
    pub(super) fn slot_bytes(entries: NonZeroUsize) -> usize {
        let entries = entries.get().checked_next_power_of_two().unwrap_or(usize::MAX / 2 + 1);
        entries.saturating_mul(std::mem::size_of::<Option<Slot>>())
    }

    fn index(&self, key: ContextKey) -> usize {
        key.as_u64() as usize & (self.slots.len() - 1)
    }

    /// Answers for `key` in the coarse second `now`: the context, if a slot holds it, it is still what the shared cache
    /// holds, and the shared cache confirmed it in this second; otherwise, a miss, which the caller looks up in the
    /// shared cache, with the stamp to fill a slot with, read here, before that lookup.
    pub(super) fn lookup(&mut self, key: ContextKey, now: u32) -> Lookup {
        if now != self.maintained_at {
            self.maintain();
            self.maintained_at = now;
        }
        if self.bypass > 0 {
            self.bypass -= 1;
            return Lookup::Miss(None);
        }
        self.lookups += 1;
        if self.lookups == WINDOW {
            if self.useful < USEFUL_MIN {
                self.bypass = BYPASS;
            }
            self.lookups = 0;
            self.useful = 0;
        }

        let index = self.index(key);
        let stamp = self.stamps.current(&key);
        if let Some(slot) = &self.slots[index] {
            if slot.key == key {
                self.useful += 1;
                if slot.stamp != stamp {
                    // The shared cache may have changed what it holds for the key: forget the copy now, so that it
                    // does not keep a context alive that the shared cache let go of.
                    self.slots[index] = None;
                } else if slot.confirmed_at == now {
                    self.hits += 1;
                    return Lookup::Hit(slot.context.clone());
                }
            }
        }
        Lookup::Miss(Some(stamp))
    }

    /// Fills the slot of `key` with `context`, which the shared cache returned for it in the coarse second `now`, in a
    /// lookup made after reading the key's change stamp `stamp`.
    pub(super) fn fill(&mut self, key: ContextKey, context: &Context, stamp: u32, now: u32) {
        let index = self.index(key);
        self.slots[index] = Some(Slot {
            key,
            context: context.clone(),
            stamp,
            confirmed_at: now,
        });
    }

    /// Adds the hits counted since the last maintenance to the counters, and drops every slot whose key the shared
    /// cache may have changed since the slot was filled.
    pub(super) fn maintain(&mut self) {
        self.publish();
        for slot in self.slots.iter_mut() {
            if let Some(filled) = slot {
                if self.stamps.current(&filled.key) != filled.stamp {
                    *slot = None;
                }
            }
        }
    }

    fn publish(&mut self) {
        let hits = std::mem::take(&mut self.hits);
        if hits > 0 {
            self.resolved_existing.increment(hits);
            self.cache.count_hits(hits);
        }
    }

    #[cfg(test)]
    pub(super) fn occupied(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    #[cfg(test)]
    pub(super) fn unpublished_hits(&self) -> u64 {
        self.hits
    }

    #[cfg(test)]
    pub(super) fn copies(&self) -> Vec<(ContextKey, Context)> {
        self.slots
            .iter()
            .flatten()
            .map(|slot| (slot.key, slot.context.clone()))
            .collect()
    }
}

impl Drop for FrontCache {
    fn drop(&mut self) {
        // Nothing counted is lost: hits not yet published reach the counters now.
        self.publish();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicU64, Ordering::SeqCst},
        Arc,
    };

    use saluki_common::hash::NoopU64BuildHasher;
    use saluki_metrics::reexport::metrics::CounterFn;

    use super::*;
    use crate::cache::CacheBuilder;

    #[derive(Default)]
    struct RecordingCounter(AtomicU64);

    impl CounterFn for RecordingCounter {
        fn increment(&self, value: u64) {
            self.0.fetch_add(value, SeqCst);
        }

        fn absolute(&self, value: u64) {
            self.0.store(value, SeqCst);
        }
    }

    fn shared_cache() -> ContextCache {
        CacheBuilder::from_identifier("front_test")
            .unwrap()
            .with_hasher::<NoopU64BuildHasher>()
            .with_telemetry(false)
            .with_change_stamps()
            .build()
    }

    fn context(name: String) -> Context {
        Context::from_static_name(Box::leak(name.into_boxed_str()))
    }

    /// Fills `front` with what `cache` returns for `context`'s key in the second `now`, as the resolver does.
    fn fill_from(front: &mut FrontCache, cache: &ContextCache, context: &Context, now: u32) {
        let key = context.key();
        let stamp = cache.change_stamp(&key).expect("the cache has change stamps");
        let shared = cache.get(&key).expect("the cache holds the context");
        front.fill(key, &shared, stamp, now);
    }

    impl FrontCache {
        /// Returns the context the front cache answers with for `key` in the second `now`, if any.
        fn get(&mut self, key: ContextKey, now: u32) -> Option<Context> {
            match self.lookup(key, now) {
                Lookup::Hit(context) => Some(context),
                Lookup::Miss(_) => None,
            }
        }
    }

    #[test]
    fn a_slot_answers_only_in_the_second_the_shared_cache_confirmed_it_and_while_its_stamp_holds() {
        let cache = shared_cache();
        let counted = Arc::new(RecordingCounter::default());
        let mut front = FrontCache::new(
            NonZeroUsize::new(16).unwrap(),
            cache.clone(),
            Counter::from_arc(Arc::clone(&counted)),
        )
        .unwrap();
        let metric = context("front.metric".to_string());
        let key = metric.key();
        cache.insert(key, metric.clone());

        assert!(front.get(key, 10).is_none(), "an empty slot answers nothing");
        fill_from(&mut front, &cache, &metric, 10);
        let answered = front.get(key, 10).expect("a confirmed slot answers in its second");
        assert!(answered.ptr_eq(&metric));
        assert!(
            front.get(key, 11).is_none(),
            "the next second goes to the shared cache first"
        );
        assert_eq!(front.occupied(), 1, "a slot waiting for confirmation is kept");
        fill_from(&mut front, &cache, &metric, 11);
        assert!(front.get(key, 11).is_some());

        // Another key of the same slot is not answered by it.
        let other = (0..)
            .map(|index| context(format!("other.{}", index)))
            .find(|other| front.index(other.key()) == front.index(key))
            .unwrap();
        assert!(
            front.get(other.key(), 11).is_none(),
            "a slot answers only for its own key"
        );

        // Once the shared cache let the context go, the slot never answers again, and is dropped.
        cache.remove(&key);
        assert!(front.get(key, 11).is_none());
        assert_eq!(front.occupied(), 0, "the stale copy is dropped at once");

        // Hits are published when the second changes (the first hit, in second 10, at the first use in 11), when
        // the front cache is maintained, and when it is dropped.
        assert_eq!(counted.0.load(SeqCst), 1);
        front.maintain();
        assert_eq!(counted.0.load(SeqCst), 2);
        cache.insert(key, metric.clone());
        fill_from(&mut front, &cache, &metric, 12);
        assert!(front.get(key, 12).is_some());
        assert!(
            front.get(key, 13).is_none(),
            "a new second publishes, then asks for confirmation"
        );
        assert_eq!(counted.0.load(SeqCst), 3);
        fill_from(&mut front, &cache, &metric, 13);
        assert!(front.get(key, 13).is_some());
        drop(front);
        assert_eq!(counted.0.load(SeqCst), 4);
    }

    #[test]
    fn maintenance_drops_every_copy_the_shared_cache_may_have_changed() {
        let cache = shared_cache();
        let mut front = FrontCache::new(NonZeroUsize::new(64).unwrap(), cache.clone(), Counter::noop()).unwrap();
        let contexts = (0..8)
            .map(|index| context(format!("kept.{}", index)))
            .collect::<Vec<_>>();
        for kept in &contexts {
            cache.insert(kept.key(), kept.clone());
        }
        for kept in &contexts {
            fill_from(&mut front, &cache, kept, 5);
        }
        assert!(front.occupied() > 0);

        // Removing every context leaves the front cache holding none of them after one maintenance, used or not.
        for kept in &contexts {
            cache.remove(&kept.key());
        }
        front.maintain();
        assert_eq!(front.occupied(), 0);
    }

    #[test]
    fn a_front_cache_steps_aside_while_its_slots_hold_too_few_of_the_keys_it_is_asked_for() {
        let cache = shared_cache();
        let mut front = FrontCache::new(NonZeroUsize::new(64).unwrap(), cache.clone(), Counter::noop()).unwrap();
        let key = |index: u64| super::super::hash::hash_context(&format!("many.{}", index), ["a:b"], ["o:p"]).0;

        // A window of keys never asked for twice: no slot ever holds the key, so the front cache steps aside.
        for index in 0..u64::from(WINDOW) {
            assert!(matches!(front.lookup(key(index), 1), Lookup::Miss(Some(_))));
        }
        for index in 0..u64::from(BYPASS) {
            assert!(
                matches!(front.lookup(key(index), 1), Lookup::Miss(None)),
                "aside, it neither answers nor asks to be filled"
            );
        }
        assert!(
            matches!(front.lookup(key(0), 1), Lookup::Miss(Some(_))),
            "then it tries again"
        );

        // A working set it holds keeps it in front: a window of lookups over a few contexts it answers, each in a slot
        // of its own, since keys hash with a seed chosen per process and two that share a slot evict each other.
        let mut front = FrontCache::new(NonZeroUsize::new(64).unwrap(), cache.clone(), Counter::noop()).unwrap();
        let mut slots = std::collections::HashSet::new();
        let few = (0..)
            .map(|index| context(format!("few.{}", index)))
            .filter(|kept| slots.insert(front.index(kept.key())))
            .take(4)
            .collect::<Vec<_>>();
        for kept in &few {
            cache.insert(kept.key(), kept.clone());
        }
        for round in 0..u64::from(WINDOW) {
            let kept = &few[(round % 4) as usize];
            if let Lookup::Miss(Some(stamp)) = front.lookup(kept.key(), 1) {
                let shared = cache.get(&kept.key()).unwrap();
                front.fill(kept.key(), &shared, stamp, 1);
            }
        }
        assert_eq!(front.bypass, 0, "a front cache that answers stays in front");
        assert!(front.hits > u64::from(WINDOW) / 2);
    }

    #[test]
    fn an_empty_copy_has_the_size_and_none_of_the_slots() {
        let cache = shared_cache();
        let mut front = FrontCache::new(NonZeroUsize::new(100).unwrap(), cache.clone(), Counter::noop()).unwrap();
        assert_eq!(front.slots.len(), 128, "sizes round up to a power of two");
        let metric = context("copied.metric".to_string());
        cache.insert(metric.key(), metric.clone());
        fill_from(&mut front, &cache, &metric, 1);
        let copy = front.empty_like();
        assert_eq!(copy.slots.len(), 128);
        assert_eq!(copy.occupied(), 0);
        assert_eq!(
            FrontCache::slot_bytes(NonZeroUsize::new(4_096).unwrap()),
            4_096 * std::mem::size_of::<Option<Slot>>()
        );
    }
}
