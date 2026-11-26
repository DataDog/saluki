//! A sharded cache of resolved contexts with inline idle expiry and live-context reuse.
//!
//! [`ContextResolver`](super::ContextResolver) uses this cache to return one shared [`Context`] for each
//! [`ContextKey`]. The cache bounds the number of resident contexts, expires idle contexts, and keeps weak entries for
//! evicted contexts that another owner still holds.
//!
//! # Design
//!
//! The cache splits its entries over up to 64 shards. Each shard has its own [`RwLock`], so resolver threads that hit
//! different shards do not contend. Each shard starts on its own 64-byte cache line, so two shard locks never share a
//! line.
//!
//! A hit takes only the read lock of its shard. It records the access in the `AtomicU32` epoch of the slot, so hits
//! never need the write lock. A miss takes the write lock, checks the shard again, and then inserts the new context.
//!
//! ## Eviction
//!
//! A full shard evicts the least recently seen slot from a sample of 8 slots. A rotating cursor selects the sample.
//! This is a trade-off between eviction cost and eviction accuracy. The cache keeps no LRU list, so a hit does not
//! write to shared state other than its own epoch. The sample can miss the oldest slot in the shard, so a busy context
//! sometimes loses its slot before an older idle context.
//!
//! ## Idle expiration
//!
//! Each slot stores the second, counted from the creation of the cache, of its last access. The epoch is a `u32`, and
//! it saturates at `u32::MAX` after about 136 years. A sweep runs once each second and removes a slot when its epoch is
//! older than the configured idle time. The configured idle time is a lower bound: a context can stay up to about two
//! seconds longer, because of the epoch resolution and the sweep interval.
//!
//! ## Live-context reuse
//!
//! Eviction or expiration of a context that another owner still holds, such as an aggregation row, keeps a weak entry
//! for it. The next lookup for the same key upgrades the weak entry and moves the context back into a resident slot.
//! Thus, the resolver returns the existing allocation and does not create a duplicate context with the same key.
//!
//! A weak entry whose context has dropped is dead. A dead weak entry keeps the whole context allocation alive, but not
//! the name, tags, or other data that the context owns. This is a trade-off between lookup cost and memory: each
//! miss checks two weak entries, and each sweep checks at least a quarter of them. A dead entry thus stays only until
//! the next check finds it.
//!
//! ## Memory return
//!
//! After a sweep or an eviction pass, a shard shrinks each of its vectors and its index when it uses a quarter or
//! less of its capacity. The new capacity is twice the current length. This hysteresis stops a shard from shrinking
//! and growing again on each small change in its length.
//!
//! # Layout
//!
//! Each shard keeps resident slots and weak slots in two dense vectors. One hash table indexes both vectors. Each index
//! entry is a `u32`. Its high bit, `LIVE_BIT`, selects the vector, and the other 31 bits are the position in that
//! vector:
//!
//! ```text
//!  index entry (u32)
//!  31         30                                0
//!  +----------+---------------------------------+
//!  | LIVE_BIT |            position             |
//!  +----------+---------------------------------+
//!       0 => slots[position]   (resident, strong)
//!       1 => live[position]    (evicted, weak)
//!
//!  AlignedShard (64 B aligned)
//!  +----------------------------------------------------------------------+
//!  | RwLock<Shard>                                                        |
//!  |                                                                      |
//!  |  index: HashTable<u32>       slots: Vec<Slot> (24 B each)            |
//!  |  +------------+              +-----+---------+-----------+           |
//!  |  | 0x00000001 |------------->| key | Context | last_seen | [1]       |
//!  |  | 0x80000000 |------+       +-----+---------+-----------+           |
//!  |  +------------+      |                                               |
//!  |                      |       live: Vec<LiveSlot> (16 B each)         |
//!  |                      |       +-----+--------------------+            |
//!  |                      +------>| key | Weak<ContextInner> | [0]        |
//!  |                              +-----+--------------------+            |
//!  +----------------------------------------------------------------------+
//! ```
//!
//! Removal uses `swap_remove` on the vector and then rewrites the index entry of the slot that moved. Thus, both
//! vectors stay dense, and a position never needs more than 31 bits.
//!
//! # Hash-bit split
//!
//! The shard and the hash table of the shard must use different bits of the 64-bit context hash. If they shared
//! bits, all keys in one shard would have the same value in those bits, and the table would see fewer distinct
//! buckets and control bytes. The low bits of the hash select the shard: 6 bits for 64 shards, and fewer bits for
//! fewer shards. `index_hash` rotates the hash right by 16 bits for the table, so those bits move to bits 48 to 53:
//!
//! ```text
//!  context hash        63                          16 15         6 5       0
//!                      +-----------------------------+------------+---------+
//!                      |                             |            |  shard  |
//!                      +-----------------------------+------------+---------+
//!
//!  index_hash          63      57 56   54 53     48 47                     0
//!  (rotate_right 16)   +---------+-------+---------+------------------------+
//!                      | h2 (7b) |       |  shard  |   bucket bits (h1)     |
//!                      +---------+-------+---------+------------------------+
//! ```
//!
//! `hashbrown` takes its 7-bit control byte from the top bits and its bucket from the low bits. Neither reaches bits
//! 48 to 53 until a table has 2^48 buckets, so the shard bits never bias the table.

use std::{
    hash::BuildHasher as _,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicU32, Ordering::Relaxed},
        Arc, RwLock, Weak,
    },
    time::{Duration, Instant},
};

use hashbrown::HashTable;
use saluki_common::{hash::NoopU64BuildHasher, sync::shutdown::ShutdownHandle};
use tokio::{pin, select, time::sleep};

use super::{Context, ContextInner, ContextKey};
use crate::{
    cache::Telemetry,
    runtime::{self, ScopeGuard},
};

/// The maximum number of shards.
///
/// More shards reduce lock contention between resolver threads. Each shard costs one 64-byte aligned lock and three
/// empty collections, so 64 shards keep the fixed cost of a cache small.
const MAX_SHARDS: usize = 64;

/// The smallest number of slots in one shard.
///
/// The cache adds shards only while each shard keeps at least this many slots. Thus, each shard has enough slots for
/// sampled eviction to find an idle victim. A limit below this value gets one shard.
const MIN_SHARD_CAPACITY: usize = 32;

/// The number of slots that eviction compares to find the least recently seen slot.
///
/// A larger sample evicts more accurately, but each eviction reads more slots under the write lock.
const EVICTION_SAMPLE_SIZE: usize = 8;

/// The time between two idle-expiration or telemetry sweeps.
///
/// This value is also the resolution of the access epoch.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// The high bit of an index entry. It selects which slot vector the entry points to.
///
/// A clear bit points into `Shard::slots`. A set bit points into `Shard::live`.
const LIVE_BIT: u32 = 1 << 31;

/// The maximum number of index entries in one shard.
///
/// A position must fit in the 31 bits below `LIVE_BIT`. A resident slot can become a weak slot when the cache evicts
/// it, so this limit applies to the sum of both vectors.
const SHARD_ENTRY_LIMIT: usize = LIVE_BIT as usize;

/// The number of weak slots that each miss checks for a dropped context.
///
/// A miss already holds the write lock, so this check costs little. It removes dead weak slots between sweeps.
const LIVE_PROBE_BUDGET: usize = 2;

/// The smallest number of weak slots that each sweep checks.
///
/// A sweep checks a quarter of the weak slots, or this number if it is larger.
const LIVE_SWEEP_MINIMUM: usize = 64;

/// A sharded cache of resolved contexts.
///
/// Clones share the same shards. See the module documentation for the design.
#[derive(Clone)]
pub(super) struct ContextCache {
    /// The shards and the shared telemetry. The background tasks hold their own references to this state.
    state: Arc<State>,

    /// The expiration and telemetry tasks of the cache, if it has any.
    ///
    /// Every clone shares this guard. Thus, the tasks stop when the last clone drops.
    _drivers: Option<Arc<ScopeGuard>>,
}

impl ContextCache {
    /// Creates a cache that holds up to `limit` resident contexts.
    ///
    /// The cache splits `limit` over a power-of-two number of shards, and the first shards get one extra slot for the
    /// remainder. If `idle` is set, a background task expires contexts that nobody resolved for that long. If
    /// telemetry is enabled, a background task reports the number of resident contexts once each second.
    pub(super) fn new(
        identifier: String, limit: NonZeroUsize, idle: Option<Duration>, telemetry_enabled: bool,
    ) -> Self {
        let limit = limit.get();
        let shard_count = 1 << (limit / MIN_SHARD_CAPACITY).clamp(1, MAX_SHARDS).ilog2();
        let shards = (0..shard_count)
            .map(|i| {
                let limit = limit / shard_count + usize::from(i < limit % shard_count);
                AlignedShard(RwLock::new(Shard::new(limit.min(SHARD_ENTRY_LIMIT))))
            })
            .collect();
        let telemetry = Telemetry::new(identifier.clone());
        telemetry.weight_limit().set(limit as f64);
        let state = Arc::new(State {
            shards,
            start: Instant::now(),
            telemetry,
            telemetry_enabled,
        });

        let drivers = (idle.is_some() || telemetry_enabled).then(|| {
            let mut drivers = runtime::scope::nested_or_detached(format!("cache_{identifier}"));
            if let Some(idle) = idle {
                let state = Arc::clone(&state);
                drivers.spawn(
                    runtime::worker_with_shutdown("expiration", move |shutdown| {
                        Arc::clone(&state).drive_expiration(idle, shutdown)
                    })
                    .build(),
                );
            }
            if telemetry_enabled {
                let state = Arc::clone(&state);
                drivers.spawn(
                    runtime::worker_with_shutdown("telemetry", move |shutdown| {
                        Arc::clone(&state).drive_telemetry(idle.is_none(), shutdown)
                    })
                    .build(),
                );
            }
            Arc::new(drivers)
        });

        Self {
            state,
            _drivers: drivers,
        }
    }

    /// Returns the context for `key`, and whether the context already existed.
    ///
    /// On a miss, this method upgrades a weak entry for `key` if its context is still alive. Otherwise, it calls
    /// `create` and inserts the result. A full shard evicts one slot first.
    ///
    /// Returns `None` if `create` returns `None`, or if the shard has `SHARD_ENTRY_LIMIT` index entries.
    pub(super) fn get_or_insert_with(
        &self, key: ContextKey, create: impl FnOnce() -> Option<Context>,
    ) -> Option<(Context, bool)> {
        let state = &self.state;
        let hash = index_hash(key);
        // `index_hash` moved the low bits of the key to the top. Rotate them back to select the shard.
        let shard = &state.shards[hash.rotate_left(16) as usize & (state.shards.len() - 1)].0;
        {
            let shard = shard.read().unwrap();
            if let Some(slot) = shard.get(key, hash) {
                slot.touch(epoch(state.start.elapsed()));
                state.telemetry.hits_total().increment(1);
                return Some((slot.context.clone(), true));
            }
        }

        // Another thread can insert the same key between the two locks, so look again under the write lock.
        let mut shard = shard.write().unwrap();
        let position = shard.position(key, hash);
        if let Some(position) = position.filter(|&i| i & LIVE_BIT == 0) {
            let slot = &shard.slots[position as usize];
            slot.touch(epoch(state.start.elapsed()));
            state.telemetry.hits_total().increment(1);
            return Some((slot.context.clone(), true));
        }

        state.telemetry.misses_total().increment(1);
        let live = position.and_then(|i| shard.take_live(i, hash));
        if !shard.live.is_empty() {
            shard.reap_live(LIVE_PROBE_BUDGET);
        }
        let (context, existing) = if let Some(context) = live {
            (context, true)
        } else {
            // Every resident entry can need a weak slot when the cache evicts it. Thus, the index must have room for
            // every resident entry plus every weak entry.
            if shard.index.len() == SHARD_ENTRY_LIMIT {
                return None;
            }
            (create()?, false)
        };
        let evicted = shard.insert(key, hash, context.clone(), epoch(state.start.elapsed()));
        state.telemetry.items_inserted_total().increment(1);
        if evicted && state.telemetry_enabled {
            state.telemetry.items_evicted_total().increment(1);
        }
        Some((context, existing))
    }
}

/// The state that all clones of a [`ContextCache`] and its background tasks share.
struct State {
    /// The shards. The length is a power of two between 1 and `MAX_SHARDS`.
    shards: Box<[AlignedShard]>,

    /// The creation time of the cache. Access epochs count whole seconds from this instant.
    start: Instant,

    /// The cache metrics.
    telemetry: Telemetry,

    /// Whether the cache reports telemetry.
    telemetry_enabled: bool,
}

impl State {
    /// Expires idle contexts once each `SWEEP_INTERVAL` until shutdown.
    ///
    /// Each pass locks one shard at a time. The task yields after each shard that changed, so a long sweep does not
    /// hold a runtime worker.
    async fn drive_expiration(self: Arc<Self>, idle: Duration, shutdown: ShutdownHandle) {
        pin!(shutdown);
        loop {
            select! {
                _ = &mut shutdown => break,
                _ = sleep(SWEEP_INTERVAL) => {}
            }

            let cutoff = expiration_cutoff(self.start.elapsed(), idle);
            let mut expired = 0;
            for shard in &self.shards {
                let (count, reaped) = shard.0.write().unwrap().sweep(cutoff);
                expired += count;
                if count != 0 || reaped != 0 {
                    tokio::task::yield_now().await;
                }
            }
            if expired != 0 {
                self.telemetry.items_expired_total().increment(expired as u64);
                self.telemetry.items_removed_total().increment(expired as u64);
                self.telemetry.items_expired_batch_size().record(expired as f64);
            }
        }
    }

    /// Reports the number of resident contexts once each `SWEEP_INTERVAL` until shutdown.
    ///
    /// If `sweep_live` is `true`, this task also sweeps each shard that has weak entries. The cache sets it when no
    /// expiration task runs, so dead weak entries and spare capacity do not stay forever.
    async fn drive_telemetry(self: Arc<Self>, sweep_live: bool, shutdown: ShutdownHandle) {
        pin!(shutdown);
        loop {
            select! {
                _ = &mut shutdown => break,
                _ = sleep(SWEEP_INTERVAL) => {}
            }

            let items: usize = self
                .shards
                .iter()
                .map(|shard| {
                    let read = shard.0.read().unwrap();
                    if !sweep_live || read.live.is_empty() {
                        return read.slots.len();
                    }
                    drop(read);
                    let mut write = shard.0.write().unwrap();
                    write.sweep(None);
                    write.slots.len()
                })
                .sum();
            self.telemetry.current_items().set(items as f64);
            self.telemetry.current_weight().set(items as f64);
        }
    }
}

/// Converts the time since `State::start` to an access epoch, in whole seconds.
///
/// The epoch saturates at `u32::MAX`, which is about 136 years after the creation of the cache.
fn epoch(elapsed: Duration) -> u32 {
    u32::try_from(elapsed.as_secs()).unwrap_or(u32::MAX)
}

/// Returns the epoch before which a slot is idle, or `None` if no slot can be idle yet.
///
/// The cutoff rounds down. Thus, a slot expires no earlier than `idle` after its last access.
fn expiration_cutoff(elapsed: Duration, idle: Duration) -> Option<u32> {
    elapsed.checked_sub(idle).map(epoch)
}

/// Returns the hash that the hash table of a shard uses for `key`.
///
/// See the module documentation for the bit split between the shard and the table.
fn index_hash(key: ContextKey) -> u64 {
    // Keep the shard bits out of the table's bucket and control-byte selection.
    NoopU64BuildHasher.hash_one(key).rotate_right(16)
}

/// A shard aligned to a 64-byte cache line.
///
/// The alignment keeps the lock words of two shards on different cache lines. Thus, threads that lock different
/// shards do not slow each other down through false sharing.
#[repr(align(64))]
struct AlignedShard(RwLock<Shard>);

/// A resident context.
///
/// A slot is 24 bytes on 64-bit targets.
struct Slot {
    /// The key of the context. The shard compares keys and rebuilds index hashes from this field.
    key: ContextKey,

    /// The strong reference that keeps the context resident.
    context: Context,

    /// The access epoch of the last hit or insert.
    ///
    /// Hits update this field under the read lock, so it is atomic. It only moves forward.
    last_seen: AtomicU32,
}

impl Slot {
    /// Records an access at epoch `now`.
    ///
    /// Concurrent hits can race under the read lock. The update keeps the largest epoch, so a late writer with an
    /// older epoch never moves `last_seen` back. `Relaxed` is enough: the epoch only guides eviction and expiration,
    /// and it orders no other memory.
    fn touch(&self, now: u32) {
        let _ = self
            .last_seen
            .fetch_update(Relaxed, Relaxed, |seen| (seen < now).then_some(now));
    }
}

/// A weak entry for an evicted context that another owner held at eviction time.
///
/// A weak slot is 16 bytes on 64-bit targets. It keeps the context allocation alive, but not the payload of the
/// context. A lookup upgrades it, and `Shard::reap_live` removes it once its context drops.
struct LiveSlot {
    /// The key of the context.
    key: ContextKey,

    /// The weak reference to the context allocation.
    context: Weak<ContextInner>,
}

/// One lock-protected part of a [`ContextCache`].
struct Shard {
    /// The resident contexts, in no specific order.
    slots: Vec<Slot>,

    /// The weak entries, in no specific order.
    live: Vec<LiveSlot>,

    /// Maps a key to its position in `slots` or `live`. See `LIVE_BIT` for the encoding.
    index: HashTable<u32>,

    /// The maximum length of `slots`. Weak entries do not count toward this limit.
    limit: usize,

    /// The start of the next eviction sample in `slots`.
    ///
    /// The cursor moves forward by one sample on each eviction. Thus, successive evictions inspect different slots.
    next_eviction: usize,

    /// The next weak entry that `Shard::reap_live` checks.
    next_live_probe: usize,
}

impl Shard {
    fn new(limit: usize) -> Self {
        Self {
            slots: Vec::new(),
            live: Vec::new(),
            index: HashTable::new(),
            limit,
            next_eviction: 0,
            next_live_probe: 0,
        }
    }

    /// Returns the index entry for `key`, which can point into `slots` or `live`.
    fn position(&self, key: ContextKey, hash: u64) -> Option<u32> {
        self.index
            .find(hash, |&i| slot_key(&self.slots, &self.live, i) == key)
            .copied()
    }

    /// Returns the resident slot for `key`.
    ///
    /// Returns `None` if `key` is absent or has only a weak entry. A weak entry has `LIVE_BIT` set, so its position is
    /// out of range for `slots`.
    fn get(&self, key: ContextKey, hash: u64) -> Option<&Slot> {
        let position = self.position(key, hash)?;
        self.slots.get(position as usize)
    }

    /// Adds an index entry for a new slot at `position`.
    fn index_slot(&mut self, hash: u64, position: u32) {
        self.index
            .insert_unique(hash, position, |&i| index_hash(slot_key(&self.slots, &self.live, i)));
    }

    /// Removes the weak entry at `position`, and upgrades its context if the context is still alive.
    fn take_live(&mut self, position: u32, hash: u64) -> Option<Context> {
        let LiveSlot { context, .. } = self.remove_live(position, hash);
        context.upgrade().map(|inner| Context { inner })
    }

    /// Inserts a resident context, and returns `true` if the shard evicted another slot to make room.
    fn insert(&mut self, key: ContextKey, hash: u64, context: Context, now: u32) -> bool {
        let evicted = self.slots.len() == self.limit;
        if evicted {
            let victim = self.select_victim();
            self.remove(victim);
        }
        let position = self.slots.len() as u32;
        self.slots.push(Slot {
            key,
            context,
            last_seen: AtomicU32::new(now),
        });
        self.index_slot(hash, position);
        evicted
    }

    /// Returns the position of the least recently seen slot in the next eviction sample.
    fn select_victim(&mut self) -> usize {
        let len = self.slots.len();
        let start = self.next_eviction % len;
        let sample = len.min(EVICTION_SAMPLE_SIZE);
        self.next_eviction = start + sample;
        // Inspect a few entries so a busy context is less likely to lose its slot to a one-off context.
        (start..start + sample)
            .map(|i| i % len)
            .min_by_key(|&i| self.slots[i].last_seen.load(Relaxed))
            .unwrap()
    }

    /// Removes the resident slot at `position`.
    ///
    /// If another owner still holds the context, the index entry moves to a new weak entry. Otherwise, the index entry
    /// goes away, and the context drops unless a caller still holds a clone.
    fn remove(&mut self, position: usize) {
        let slot = &self.slots[position];
        let hash = index_hash(slot.key);
        let entry = self.index.find_entry(hash, |&i| i == position as u32).unwrap();
        // Keep weak pointers only for contexts that another owner can still use.
        //
        // A `Relaxed` load is enough here. The write lock stops this cache from handing out new strong references. If
        // the count is 1, the cache owns the only reference, so no other thread can raise it. If the count is larger,
        // another owner can drop its reference right after the load. That leaves a dead weak entry, and
        // `Shard::reap_live` removes it later.
        if Arc::strong_count(&slot.context.inner) > 1 {
            let live_position = self.live.len() as u32 | LIVE_BIT;
            self.live.push(LiveSlot {
                key: slot.key,
                context: Arc::downgrade(&slot.context.inner),
            });
            *entry.into_mut() = live_position;
        } else {
            entry.remove();
        }
        swap_remove_slot(&mut self.slots, &mut self.index, position as u32, |slot| slot.key);
    }

    /// Removes the weak entry at `position`, which has `LIVE_BIT` set, together with its index entry.
    fn remove_live(&mut self, position: u32, hash: u64) -> LiveSlot {
        self.index.find_entry(hash, |&i| i == position).unwrap().remove();
        swap_remove_slot(&mut self.live, &mut self.index, position, |slot| slot.key)
    }

    /// Checks up to `budget` weak entries, and removes each entry whose context has dropped.
    ///
    /// The probe cursor wraps around `live`. Thus, repeated calls with a small budget check every entry in turn.
    /// Returns the number of entries removed.
    fn reap_live(&mut self, budget: usize) -> usize {
        let before = self.live.len();
        for _ in 0..before.min(budget) {
            if self.next_live_probe >= self.live.len() {
                self.next_live_probe = 0;
            }
            let slot = &self.live[self.next_live_probe];
            if slot.context.strong_count() == 0 {
                self.remove_live(self.next_live_probe as u32 | LIVE_BIT, index_hash(slot.key));
            } else {
                self.next_live_probe += 1;
            }
        }
        before - self.live.len()
    }

    /// Expires the slots that were idle before `cutoff`, reaps dead weak entries, and shrinks the shard.
    ///
    /// A `cutoff` of `None` skips expiration. Returns the number of expired slots and the number of reaped weak
    /// entries.
    fn sweep(&mut self, cutoff: Option<u32>) -> (usize, usize) {
        let expired = cutoff.map_or(0, |cutoff| self.expire(cutoff));
        // Check at least a quarter of the live slots during each sweep.
        let reaped = self.reap_live(self.live.len().div_ceil(4).max(LIVE_SWEEP_MINIMUM));
        if self.slots.len() <= self.slots.capacity() / 4 {
            self.slots.shrink_to(self.slots.len() * 2);
        }
        if self.live.len() <= self.live.capacity() / 4 {
            self.live.shrink_to(self.live.len() * 2);
        }
        if self.index.len() <= self.index.capacity() / 4 {
            self.index
                .shrink_to_fit(|&i| index_hash(slot_key(&self.slots, &self.live, i)));
        }
        (expired, reaped)
    }

    /// Removes each resident slot whose last access epoch is older than `cutoff`, and returns the number removed.
    fn expire(&mut self, cutoff: u32) -> usize {
        let before = self.slots.len();
        let mut i = 0;
        while let Some(slot) = self.slots.get_mut(i) {
            // Retain the cutoff epoch because the exact access time is unknown.
            if *slot.last_seen.get_mut() < cutoff {
                self.remove(i);
            } else {
                i += 1;
            }
        }
        before - self.slots.len()
    }
}

/// Returns the key of the slot that the index entry `position` points to.
fn slot_key(slots: &[Slot], live: &[LiveSlot], position: u32) -> ContextKey {
    if position & LIVE_BIT == 0 {
        slots[position as usize].key
    } else {
        live[(position & !LIVE_BIT) as usize].key
    }
}

/// Removes the slot at `position` from `slots`, and moves the last slot into the gap.
///
/// The index entry of the moved slot gets the new position. `position` keeps its `LIVE_BIT`, so this function works for
/// both slot vectors.
///
/// # Panics
///
/// Panics if the index has no entry for the moved slot. The caller removes or redirects the index entry at `position`
/// before it calls this function. Otherwise, the lookup for the moved slot can find the stale entry.
fn swap_remove_slot<T>(
    slots: &mut Vec<T>, index: &mut HashTable<u32>, position: u32, key: impl FnOnce(&T) -> ContextKey,
) -> T {
    let offset = (position & !LIVE_BIT) as usize;
    let slot = slots.swap_remove(offset);
    if let Some(moved) = slots.get(offset) {
        let old_position = slots.len() as u32 | (position & LIVE_BIT);
        *index.find_mut(index_hash(key(moved)), |&i| i == old_position).unwrap() = position;
    }
    slot
}

#[cfg(test)]
mod tests {
    use std::{sync::Barrier, thread};

    use super::*;
    use crate::data_model::event::metric::context::ContextResolverBuilder;

    fn insert(shard: &mut Shard, name: &str, now: u32) -> bool {
        let context = Context::from_parts(name, crate::data_model::tags::TagSet::default());
        let key = context.inner.key;
        shard.insert(key, index_hash(key), context, now)
    }

    fn get<'a>(shard: &'a Shard, name: &str) -> Option<&'a Slot> {
        let key = Context::from_parts(name, crate::data_model::tags::TagSet::default())
            .inner
            .key;
        shard.get(key, index_hash(key))
    }

    #[test]
    fn evicts_at_capacity_and_keeps_hot_entries() {
        let mut shard = Shard::new(3);
        assert!(!insert(&mut shard, "hot", 0));
        assert!(!insert(&mut shard, "cold", 1));
        assert!(!insert(&mut shard, "recent", 2));
        get(&shard, "hot").unwrap().touch(3);

        assert!(insert(&mut shard, "new", 3));

        assert_eq!(shard.slots.len(), 3);
        assert!(get(&shard, "cold").is_none());
        for name in ["hot", "recent", "new"] {
            assert_eq!(get(&shard, name).unwrap().context.name(), name);
        }
    }

    #[test]
    fn idle_expiration_uses_last_access_and_does_not_round_ttl_down() {
        let mut shard = Shard::new(3);
        let idle = Duration::from_millis(20_500);
        assert_eq!(expiration_cutoff(Duration::from_secs(20), idle), None);
        insert(&mut shard, "touched", 1);
        insert(&mut shard, "untouched", 9);
        get(&shard, "touched").unwrap().touch(10);
        get(&shard, "touched").unwrap().touch(10);

        let cutoff = expiration_cutoff(Duration::from_millis(31_499), idle).unwrap();
        assert_eq!(shard.expire(cutoff), 1);
        assert!(get(&shard, "untouched").is_none());
        assert!(get(&shard, "touched").is_some());

        let cutoff = expiration_cutoff(Duration::from_millis(31_500), idle).unwrap();
        assert_eq!(shard.expire(cutoff), 1);
        assert!(get(&shard, "touched").is_none());
    }

    #[test]
    fn drain_shrinks_storage_and_the_index_and_allows_regrowth() {
        let mut shard = Shard::new(256);
        for i in 0..256 {
            insert(&mut shard, &i.to_string(), u32::from(i < 8));
        }
        let slots = shard.slots.capacity();
        let index = shard.index.capacity();

        assert_eq!(shard.sweep(Some(1)), (248, 0));
        assert!(shard.slots.capacity() < slots / 2);
        assert!(shard.index.capacity() < index / 2);
        for i in 0..8 {
            assert!(get(&shard, &i.to_string()).is_some());
        }

        assert_eq!(shard.sweep(Some(2)), (8, 0));
        assert_eq!(shard.slots.capacity(), 0);
        assert_eq!(shard.index.capacity(), 0);
        assert!(!insert(&mut shard, "new", 2));
        assert!(get(&shard, "new").is_some());
    }

    #[test]
    fn reuses_a_live_context_after_capacity_eviction() {
        let mut resolver = ContextResolverBuilder::for_tests()
            .with_cached_contexts_limit(1)
            .build();
        let held = resolver.resolve("metric.a", ["env:prod"], None).unwrap();
        resolver.resolve("metric.b", ["env:prod"], None).unwrap();

        let cache = resolver.context_cache.as_ref().unwrap();
        let key = held.inner.key;
        assert!(cache.state.shards[0]
            .0
            .read()
            .unwrap()
            .get(key, index_hash(key))
            .is_none());
        assert_eq!(Arc::strong_count(&held.inner), 1);

        let resolved = resolver.resolve("metric.a", ["env:prod"], None).unwrap();
        assert!(held.ptr_eq(&resolved));
    }

    #[test]
    fn reuses_a_live_context_after_idle_expiration() {
        let mut resolver = ContextResolverBuilder::for_tests()
            .with_cached_contexts_limit(1)
            .build();
        let held = resolver.resolve("metric.a", ["env:prod"], None).unwrap();
        let cache = resolver.context_cache.as_ref().unwrap();
        {
            let mut shard = cache.state.shards[0].0.write().unwrap();
            let last_seen = shard.slots[0].last_seen.load(Relaxed);
            let cutoff = expiration_cutoff(Duration::from_secs(u64::from(last_seen) + 21), Duration::from_secs(20));
            assert_eq!(shard.sweep(cutoff), (1, 0));
            assert_eq!(shard.slots.capacity(), 0);
            assert_eq!(shard.index.len(), 1);
        }
        assert_eq!(Arc::strong_count(&held.inner), 1);

        let resolved = resolver.resolve("metric.a", ["env:prod"], None).unwrap();
        assert!(held.ptr_eq(&resolved));
    }

    #[test]
    fn misses_and_sweeps_reap_dead_weak_entries() {
        let mut resolver = ContextResolverBuilder::for_tests()
            .with_cached_contexts_limit(1)
            .build();
        let held: Vec<_> = (0..10)
            .map(|i| resolver.resolve(format!("metric.{i}"), ["env:prod"], None).unwrap())
            .collect();
        let cache = resolver.context_cache.as_ref().unwrap().clone();
        let shard = &cache.state.shards[0].0;
        assert_eq!(shard.read().unwrap().live.len(), 9);
        drop(held);

        resolver.resolve("new", std::iter::empty::<&str>(), None).unwrap();
        assert_eq!(shard.read().unwrap().live.len(), 7);

        let mut shard = shard.write().unwrap();
        assert_eq!(shard.sweep(None), (0, 7));
        assert_eq!(shard.live.capacity(), 0);
        assert_eq!(shard.index.len(), 1);
        assert!(get(&shard, "new").is_some());
    }

    #[test]
    fn concurrent_hits_and_misses_share_one_context_per_key() {
        for capacity in [1, usize::MAX] {
            concurrent_resolve(capacity);
        }
    }

    fn concurrent_resolve(capacity: usize) {
        const THREADS: usize = 8;
        let resolver = ContextResolverBuilder::for_tests()
            .with_cached_contexts_limit(capacity)
            .build();
        let barrier = Barrier::new(THREADS);
        let names: Vec<_> = (0..128).map(|i| format!("metric.{i}")).collect();

        thread::scope(|scope| {
            let workers: Vec<_> = (0..THREADS)
                .map(|_| {
                    let mut resolver = resolver.clone();
                    let names = &names;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        names
                            .iter()
                            .map(|name| resolver.resolve(name, ["env:prod"], None).unwrap())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            let mut workers = workers.into_iter();
            let first = workers.next().unwrap().join().unwrap();
            for worker in workers {
                let others = worker.join().unwrap();
                assert!(first.iter().zip(&others).all(|(a, b)| a.ptr_eq(b)));
            }
        });
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn compact_context_cache_layout() {
        use std::mem::size_of;

        assert_eq!(size_of::<ContextCache>(), 16);
        assert_eq!(size_of::<Slot>(), 24);
        assert_eq!(size_of::<LiveSlot>(), 16);
    }
}
