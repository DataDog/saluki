//! A concurrent cache with optional expiration and telemetry.

use std::{marker::PhantomData, num::NonZeroUsize, sync::Arc, time::Duration};

use saluki_common::{hash::FastBuildHasher, task::spawn_traced};
use saluki_error::GenericError;
use saluki_metrics::{static_metrics, Counter, Gauge, Histogram};
use tokio::time::sleep;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::debug;

mod accumulated;
pub(crate) use self::accumulated::AccumulatedCounters;

mod expiry;
use self::expiry::{Expiration, ExpirationBuilder, ExpiryCapableLifecycle};

mod stamps;
pub(crate) use self::stamps::ChangeStamps;

pub mod weight;
use self::weight::{ItemCountWeighter, Weighter, WrappedWeighter};

type RawCache<K, V, W, H> = quick_cache::sync::Cache<K, V, WrappedWeighter<W>, H, ExpiryCapableLifecycle<K>>;

#[static_metrics(prefix = cache, labels(cache_id))]
#[derive(Clone)]
struct Telemetry {
    current_items: Gauge,
    current_weight: Gauge,
    weight_limit: Gauge,
    hits_total: Counter,
    misses_total: Counter,
    items_evicted_total: Counter,
    #[metric(level = debug)]
    items_inserted_total: Counter,
    #[metric(level = debug)]
    items_removed_total: Counter,
    #[metric(level = debug)]
    items_expired_total: Counter,
    #[metric(level = trace)]
    items_expired_batch_size: Histogram,
}

/// The counters a cache writes on every lookup, insertion, and removal: its telemetry's handles, or the counts of its
/// own that an accumulating cache keeps.
#[derive(Clone)]
struct OperationCounters {
    hits: Counter,
    misses: Counter,
    inserted: Counter,
    removed: Counter,
}

impl OperationCounters {
    fn of(telemetry: &Telemetry) -> Self {
        Self {
            hits: telemetry.hits_total().clone(),
            misses: telemetry.misses_total().clone(),
            inserted: telemetry.items_inserted_total().clone(),
            removed: telemetry.items_removed_total().clone(),
        }
    }
}

/// Returns the counts of an accumulating cache whose telemetry is `telemetry`, with the counters its operations
/// write and the counter its evictions write.
fn accumulated(telemetry: &Telemetry) -> (AccumulatedCounters, OperationCounters, Counter) {
    let accumulated = AccumulatedCounters::new(vec![
        telemetry.hits_total().clone(),
        telemetry.misses_total().clone(),
        telemetry.items_inserted_total().clone(),
        telemetry.items_removed_total().clone(),
        telemetry.items_evicted_total().clone(),
    ]);
    let counters = OperationCounters {
        hits: accumulated.local(0),
        misses: accumulated.local(1),
        inserted: accumulated.local(2),
        removed: accumulated.local(3),
    };
    let evicted = accumulated.local(4);
    (accumulated, counters, evicted)
}

/// How a cache, or a context resolver, reports the gauges its build and its background task set: a cache's items,
/// weight, and weight limit, and the interner of a context resolver.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GaugeMode {
    /// Each gauge is set to this cache's value, so the gauges of caches that share an identifier show the value of
    /// whichever wrote last.
    #[default]
    Set,

    /// This cache's values are added to each gauge, and withdrawn once its last clone is dropped, so the gauges of the
    /// caches that share an identifier, such as one per task of a component, sum what every one of them holds.
    Summed,
}

/// A value added to a gauge, and withdrawn from it when dropped: what one of several caches that share an identifier
/// contributes to a gauge it set at build.
pub(crate) struct GaugeContribution {
    gauge: Gauge,
    value: f64,
}

impl GaugeContribution {
    /// Adds `value` to `gauge`, until the contribution is dropped.
    pub(crate) fn add(gauge: Gauge, value: f64) -> Self {
        gauge.increment(value);
        Self { gauge, value }
    }
}

impl Drop for GaugeContribution {
    fn drop(&mut self) {
        self.gauge.decrement(self.value);
    }
}

struct InnerCache<K, V, W, H> {
    cache: Arc<RawCache<K, V, W, H>>,
    /// What an accumulating cache counted and has not published; published when the last clone is dropped, too.
    accumulated: Option<AccumulatedCounters>,
    /// The stamps of a cache built with change stamps, which every change to a key advances.
    stamps: Option<Arc<ChangeStamps<H>>>,
    /// What a cache whose gauges are summed adds to its weight limit's gauge, withdrawn with its last clone.
    _weight_limit: Option<GaugeContribution>,
    _task_shutdown_guard: DropGuard,
}

/// Builder for creating a [`Cache`].
pub struct CacheBuilder<K, V, W = ItemCountWeighter, H = FastBuildHasher> {
    identifier: String,
    capacity: NonZeroUsize,
    weighter: W,
    idle_period: Option<Duration>,
    expiration_interval: Option<Duration>,
    telemetry_enabled: bool,
    accumulated_telemetry: bool,
    gauges: GaugeMode,
    change_stamps: bool,
    _key: PhantomData<K>,
    _value: PhantomData<V>,
    _hasher: PhantomData<H>,
}

impl<K, V> CacheBuilder<K, V> {
    /// Creates a new `CacheBuilder` with the given cache identifier.
    ///
    /// The cache identifier _should_ be unique, but it isn't required to be. Metrics for the cache will be emitted
    /// using the given identifier, so in cases where the identifier isn't unique, those metrics will be aggregated
    /// together and it won't be possible to distinguish between the different caches.
    ///
    /// # Errors
    ///
    /// If the given cache identifier is empty, an error is returned.
    pub fn from_identifier<N: Into<String>>(identifier: N) -> Result<CacheBuilder<K, V>, GenericError> {
        let identifier = identifier.into();
        if identifier.is_empty() {
            return Err(GenericError::msg("cache identifier must not be empty"));
        }

        Ok(CacheBuilder {
            identifier,
            capacity: NonZeroUsize::MAX,
            weighter: ItemCountWeighter,
            idle_period: None,
            expiration_interval: None,
            telemetry_enabled: true,
            accumulated_telemetry: false,
            gauges: GaugeMode::Set,
            change_stamps: false,
            _key: PhantomData,
            _value: PhantomData,
            _hasher: PhantomData,
        })
    }

    /// Configures a [`CacheBuilder`] that's suitable for tests.
    ///
    /// This configures the builder with the following defaults:
    ///
    /// - cache identifier of "noop"
    /// - unlimited cache size
    /// - telemetry disabled
    ///
    /// This is generally only useful for testing purposes, and is exposed publicly in order to be used in cross-crate
    /// testing scenarios.
    pub fn for_tests() -> CacheBuilder<K, V> {
        CacheBuilder::from_identifier("noop")
            .expect("cache identifier not empty")
            .with_telemetry(false)
    }
}

impl<K, V, W, H> CacheBuilder<K, V, W, H> {
    /// Sets the capacity for the cache.
    ///
    /// The capacity is used, in conjunction with the item weighter, to determine how many items can be held in the
    /// cache and when items should be evicted to make room for new items.
    ///
    /// See [`with_item_weighter`][Self::with_item_weighter] for more information on how the item weighter is used.
    ///
    /// Defaults to unlimited capacity.
    pub fn with_capacity(mut self, capacity: NonZeroUsize) -> Self {
        self.capacity = capacity;
        self
    }

    /// Enables expiration of cached items based on how long since they were last accessed.
    ///
    /// Items which haven't been accessed within the configured duration will be marked for expiration, and be removed
    /// from the cache shortly thereafter. For the purposes of expiration, "accessed" is either when the item was first
    /// inserted or when it was last read.
    ///
    /// If the given value is `None`, expiration is disabled.
    ///
    /// Defaults to no expiration.
    pub fn with_time_to_idle(mut self, idle_period: Option<Duration>) -> Self {
        self.idle_period = idle_period;

        // Make sure we have an expiration interval set if expiration is enabled.
        if self.idle_period.is_some() {
            self.expiration_interval = self.expiration_interval.or(Some(Duration::from_secs(1)));
        }

        self
    }

    /// Sets the interval at which the expiration process will run.
    ///
    /// This controls how often the expiration process will run to check for expired items. While items become
    /// _eligible_ for expiration after the configured duration, they're not _guaranteed_ to be
    /// removed immediately: the expiration process must still run to actually find the expired items and remove them.
    ///
    /// This means that the rough upper bound for how long an item may be kept alive is the sum of
    /// both the configured expiration duration and the expiration interval.
    ///
    /// This value is only relevant if expiration is enabled.
    ///
    /// Defaults to 1 second.
    pub fn with_expiration_interval(mut self, expiration_interval: Duration) -> Self {
        self.expiration_interval = Some(expiration_interval);
        self
    }

    /// Sets the item weighter for the cache.
    ///
    /// The item weighter is used to determine the "weight" of each item in the cache, which is used during
    /// insertion/eviction to determine if an item can be held in the cache without first having to evict other items to
    /// stay within the configured capacity.
    ///
    /// For example, if the configured capacity is set to 10,000, and the "item count" weighter is used, then the cache
    /// will operate in a way that aims to simply ensure that no more than 10,000 items are held in the cache at any given
    /// time. This allows defining custom weighters that can be used to track other aspects of the items in the cache,
    /// such as their size in bytes, or some other metric that's relevant to the intended caching behavior.
    ///
    /// Defaults to "item count" weighter.
    pub fn with_item_weighter<W2>(self, weighter: W2) -> CacheBuilder<K, V, W2, H> {
        CacheBuilder {
            identifier: self.identifier,
            capacity: self.capacity,
            weighter,
            idle_period: self.idle_period,
            expiration_interval: self.expiration_interval,
            telemetry_enabled: self.telemetry_enabled,
            accumulated_telemetry: self.accumulated_telemetry,
            gauges: self.gauges,
            change_stamps: self.change_stamps,
            _key: PhantomData,
            _value: PhantomData,
            _hasher: PhantomData,
        }
    }

    /// Sets the item hasher for the cache.
    ///
    /// As cache keys are hashed before performing any reads or writes, the chosen hasher can potentially impact the
    /// performance of those operations. In some scenarios, it may be desirable to use a different hasher than the
    /// default one in order to optimize for specific key types or access patterns.
    ///
    /// Defaults to a fast, non-cryptographic hasher: [`FastBuildHasher`].
    pub fn with_hasher<H2>(self) -> CacheBuilder<K, V, W, H2> {
        CacheBuilder {
            identifier: self.identifier,
            capacity: self.capacity,
            weighter: self.weighter,
            idle_period: self.idle_period,
            expiration_interval: self.expiration_interval,
            telemetry_enabled: self.telemetry_enabled,
            accumulated_telemetry: self.accumulated_telemetry,
            gauges: self.gauges,
            change_stamps: self.change_stamps,
            _key: PhantomData,
            _value: PhantomData,
            _hasher: PhantomData,
        }
    }

    /// Sets whether or not to enable telemetry for this cache.
    ///
    /// Reporting the telemetry of the cache requires running an asynchronous task to override adding additional
    /// overhead in the hot path of reading or writing to the cache. In some cases, it may be cumbersome to always
    /// create the cache in an asynchronous context so that the telemetry task can be spawned. This method allows
    /// disabling telemetry reporting in those cases.
    ///
    /// Defaults to telemetry enabled.
    pub fn with_telemetry(mut self, enabled: bool) -> Self {
        self.telemetry_enabled = enabled;
        self
    }

    /// Makes the cache count its hits, misses, insertions, removals, and evictions in counters of its own, which
    /// [`Cache::publish_telemetry`] adds to its telemetry, instead of writing its telemetry on every operation.
    ///
    /// Caches with one identifier share their telemetry, so caches used in parallel, such as one per task of a
    /// component, contend on it on every lookup. Accumulating keeps each cache's operations on its own counters, and
    /// its series still sum what every cache counted, once each publishes. Whatever owns the cache publishes it
    /// periodically, about once a second; what is left is published when the last clone of the cache is dropped.
    /// Between publications, the cache's counters lag its operations. Gauges, which a background task sets, and the
    /// expiration task's counts are unaffected.
    ///
    /// Defaults to writing the telemetry on every operation.
    pub fn with_accumulated_telemetry(mut self) -> Self {
        self.accumulated_telemetry = true;
        self
    }

    /// Sets how the cache reports its items, weight, and weight limit: set to its own values, or summed with the other
    /// caches of its identifier (see [`GaugeMode`]).
    ///
    /// A summed cache adds its weight limit to its gauge when it is built, and its items and weight to theirs once a
    /// second, as their changes, and withdraws all three once its last clone is dropped, so the series of caches used
    /// in parallel under one identifier, such as one per task of a component, show what all of them hold. Without
    /// telemetry, nothing is reported either way.
    ///
    /// Defaults to [`GaugeMode::Set`].
    pub fn with_gauges(mut self, gauges: GaugeMode) -> Self {
        self.gauges = gauges;
        self
    }

    /// Makes the cache advance a change stamp for every insertion, removal, eviction, and expiry, so that a copy of an
    /// entry kept outside the cache can tell whether the cache may have changed it since (see [`ChangeStamps`]).
    ///
    /// Defaults to no change stamps.
    pub(crate) fn with_change_stamps(mut self) -> Self {
        self.change_stamps = true;
        self
    }
}

impl<K, V, W, H> CacheBuilder<K, V, W, H>
where
    K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    W: Weighter<K, V> + Clone + Send + Sync + 'static,
    H: std::hash::BuildHasher + Clone + Default + Send + Sync + 'static,
{
    /// Builds a [`Cache`] from the current configuration.
    pub fn build(self) -> Cache<K, V, W, H> {
        let capacity = self.capacity.get();

        let telemetry = Telemetry::new(self.identifier);
        let summed = self.telemetry_enabled && self.gauges == GaugeMode::Summed;
        let weight_limit = if summed {
            Some(GaugeContribution::add(
                telemetry.weight_limit().clone(),
                capacity as f64,
            ))
        } else {
            telemetry.weight_limit().set(capacity as f64);
            None
        };

        // An accumulating cache writes its operations to counters of its own, which it publishes to its telemetry.
        let (accumulated, counters, accumulated_evictions) = if self.accumulated_telemetry {
            let (accumulated, counters, evictions) = accumulated(&telemetry);
            (Some(accumulated), counters, Some(evictions))
        } else {
            (None, OperationCounters::of(&telemetry), None)
        };

        // Configure expiration if enabled.
        let eviction_counter = match accumulated_evictions {
            _ if !self.telemetry_enabled => Counter::noop(),
            Some(evictions) => evictions,
            None => telemetry.items_evicted_total().clone(),
        };
        let stamps = self.change_stamps.then(|| Arc::new(ChangeStamps::new(H::default())));
        let mut expiration_builder = ExpirationBuilder::new(eviction_counter);
        if let Some(time_to_idle) = self.idle_period {
            expiration_builder = expiration_builder.with_time_to_idle(time_to_idle);
        }
        if let Some(stamps) = &stamps {
            expiration_builder = expiration_builder.with_change_stamps(Arc::clone(stamps) as _);
        }
        let (expiration, expiry_lifecycle) = expiration_builder.build();

        // Create the underlying cache and shutdown signal.
        let shutdown_token = CancellationToken::new();
        let raw_cache = Arc::new(RawCache::with(
            capacity,
            capacity as u64,
            WrappedWeighter::from(self.weighter),
            H::default(),
            expiry_lifecycle,
        ));

        let cache = Cache {
            inner: Arc::new(InnerCache {
                cache: Arc::clone(&raw_cache),
                accumulated,
                stamps: stamps.clone(),
                _weight_limit: weight_limit,
                _task_shutdown_guard: shutdown_token.clone().drop_guard(),
            }),
            expiration: expiration.clone(),
            counters,
        };

        // If expiration is enabled, spawn a background task to actually drive expiration.
        if let Some(expiration_interval) = self.expiration_interval {
            let expiration = expiration.clone();

            spawn_traced(drive_expiration(
                Arc::clone(&raw_cache),
                telemetry.clone(),
                expiration,
                stamps,
                expiration_interval,
                shutdown_token.clone(),
            ));
        }

        // If telemetry is enabled, spawn a background task to drive telemetry reporting.
        if self.telemetry_enabled {
            spawn_traced(drive_telemetry(
                Arc::clone(&raw_cache),
                telemetry,
                summed,
                shutdown_token,
            ));
        }

        cache
    }
}

/// A simple concurrent cache.
#[derive(Clone)]
pub struct Cache<K, V, W = ItemCountWeighter, H = FastBuildHasher> {
    inner: Arc<InnerCache<K, V, W, H>>,
    expiration: Expiration<K>,
    counters: OperationCounters,
}

impl<K, V, W, H> Cache<K, V, W, H>
where
    K: Eq + std::hash::Hash + Clone,
    V: Clone,
    W: Weighter<K, V> + Clone,
    H: std::hash::BuildHasher + Clone,
{
    /// Returns `true` if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.cache.is_empty()
    }

    /// Returns the number of items currently in the cache.
    pub fn len(&self) -> usize {
        self.inner.cache.len()
    }

    /// Returns the total weight of all items in the cache.
    pub fn weight(&self) -> u64 {
        self.inner.cache.weight()
    }

    /// Inserts an item into the cache with the given key and value.
    ///
    /// If an item with the same key already exists, it will be overwritten and the old value will be dropped. If the
    /// cache is full, one or more items will be evicted to make room for the new item, based on the configured item
    /// weighter and the weight of the new item.
    pub fn insert(&self, key: K, value: V) {
        self.inner.cache.insert(key.clone(), value);
        if let Some(stamps) = &self.inner.stamps {
            stamps.advance(&key);
        }
        self.expiration.mark_entry_accessed(key);
        self.counters.inserted.increment(1);
    }

    /// Gets an item from the cache by its key.
    ///
    /// If the item is found, it's cloned and `Some(value)` is returned. Otherwise, `None` is returned.
    pub fn get(&self, key: &K) -> Option<V> {
        let value = self.inner.cache.get(key);
        if value.is_some() {
            self.expiration.mark_entry_accessed(key.clone());
            self.counters.hits.increment(1);
        } else {
            self.counters.misses.increment(1);
        }
        value
    }

    /// Adds what the cache counted since it last published to its telemetry, if it was built with
    /// [accumulated telemetry][CacheBuilder::with_accumulated_telemetry]; otherwise, does nothing.
    ///
    /// Clones of a cache share its counts, so publishing from any of them publishes everything counted once.
    pub fn publish_telemetry(&self) {
        if let Some(accumulated) = &self.inner.accumulated {
            accumulated.publish();
        }
    }

    /// Removes an item from the cache by its key.
    pub fn remove(&self, key: &K) {
        self.inner.cache.remove(key);
        if let Some(stamps) = &self.inner.stamps {
            stamps.advance(key);
        }
        self.expiration.mark_entry_removed(key.clone());
        self.counters.removed.increment(1);
    }

    /// Returns the change stamp of `key`, if the cache was built with change stamps.
    ///
    /// Read it before the lookup whose result is kept outside the cache; see [`ChangeStamps`].
    #[cfg(test)]
    pub(crate) fn change_stamp(&self, key: &K) -> Option<u32> {
        self.inner.stamps.as_ref().map(|stamps| stamps.current(key))
    }

    /// Returns the cache's change stamps, if it was built with them.
    pub(crate) fn change_stamps(&self) -> Option<Arc<ChangeStamps<H>>> {
        self.inner.stamps.clone()
    }

    /// Counts `hits` lookups that hit, made outside the cache on copies of its entries, as its own hits.
    pub(crate) fn count_hits(&self, hits: u64) {
        self.counters.hits.increment(hits);
    }

    /// Returns what the cache holds for `key`, without counting, recording an access, or changing its eviction order.
    #[cfg(test)]
    pub(crate) fn peek(&self, key: &K) -> Option<V> {
        self.inner.cache.peek(key)
    }
}

async fn drive_expiration<K, V, W, H>(
    cache: Arc<RawCache<K, V, W, H>>, telemetry: Telemetry, expiration: Expiration<K>,
    stamps: Option<Arc<ChangeStamps<H>>>, expiration_interval: Duration, shutdown: CancellationToken,
) where
    K: Eq + std::hash::Hash + Clone,
    V: Clone,
    W: Weighter<K, V> + Clone,
    H: std::hash::BuildHasher + Clone,
{
    let mut expired_item_keys = Vec::new();

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = sleep(expiration_interval) => {}
        }

        // Drain all expired items that have been queued up for the cache.
        expiration.drain_expired_items(&mut expired_item_keys);

        let num_expired_items = expired_item_keys.len();
        if num_expired_items != 0 {
            telemetry.items_expired_total().increment(num_expired_items as u64);
            telemetry.items_expired_batch_size().record(num_expired_items as f64);
        }

        debug!(num_expired_items, "Found expired items.");

        for item_key in expired_item_keys.drain(..) {
            cache.remove(&item_key);
            if let Some(stamps) = &stamps {
                stamps.advance(&item_key);
            }
            telemetry.items_removed_total().increment(1);
            expiration.mark_entry_removed(item_key);
        }

        debug!(num_expired_items, "Removed expired items.");
    }
}

/// Reports the cache's items and weight once a second until the cache is dropped: sets them, or, when `summed`, adds
/// their changes to the gauges and withdraws what it added once the cache is dropped.
async fn drive_telemetry<K, V, W, H>(
    cache: Arc<RawCache<K, V, W, H>>, telemetry: Telemetry, summed: bool, shutdown: CancellationToken,
) where
    K: Eq + std::hash::Hash + Clone,
    V: Clone,
    W: Weighter<K, V> + Clone,
    H: std::hash::BuildHasher + Clone,
{
    let mut items = SummedGauge::new(telemetry.current_items().clone(), summed);
    let mut weight = SummedGauge::new(telemetry.current_weight().clone(), summed);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = sleep(Duration::from_secs(1)) => {}
        }

        items.report(cache.len() as f64);
        weight.report(cache.weight() as f64);
    }
}

/// A gauge a background task reports one value of: set to it, or, when summed with the values of the other caches or
/// resolvers of its identifier, changed by what the value changed since the last report, and withdrawn when dropped.
pub(crate) struct SummedGauge {
    gauge: Gauge,
    summed: bool,
    reported: f64,
}

impl SummedGauge {
    /// Creates a reporter of one value to `gauge`, which sets the gauge unless `summed`.
    pub(crate) fn new(gauge: Gauge, summed: bool) -> Self {
        Self {
            gauge,
            summed,
            reported: 0.0,
        }
    }

    /// Reports `value`.
    pub(crate) fn report(&mut self, value: f64) {
        if self.summed {
            self.gauge.increment(value - self.reported);
        } else {
            self.gauge.set(value);
        }
        self.reported = value;
    }
}

impl Drop for SummedGauge {
    fn drop(&mut self) {
        if self.summed {
            self.gauge.decrement(self.reported);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    pub struct ItemValueWeighter;

    impl<K> Weighter<K, usize> for ItemValueWeighter {
        fn item_weight(&self, _key: &K, value: &usize) -> u64 {
            *value as u64
        }
    }

    #[test]
    fn empty_cache_identifier() {
        let result = CacheBuilder::<u64, u64>::from_identifier("");
        assert!(result.is_err(), "expected error for empty cache identifier");
    }

    #[test]
    fn basic() {
        const CACHE_KEY: usize = 42;
        const CACHE_VALUE: &str = "value1";

        let cache = CacheBuilder::for_tests().build();

        assert_eq!(cache.len(), 0);
        assert_eq!(cache.weight(), 0);

        cache.insert(CACHE_KEY, CACHE_VALUE);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.weight(), 1);

        assert_eq!(cache.get(&CACHE_KEY), Some(CACHE_VALUE));

        cache.remove(&CACHE_KEY);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.weight(), 0);
    }

    #[test]
    fn evict_at_capacity() {
        const CAPACITY: usize = 3;

        let cache = CacheBuilder::for_tests()
            .with_capacity(NonZeroUsize::new(CAPACITY).unwrap())
            .build();

        // Insert items up to the capacity.
        for i in 0..CAPACITY {
            cache.insert(i, "value");
        }

        assert_eq!(cache.len(), CAPACITY);
        assert_eq!(cache.weight(), CAPACITY as u64);

        // Inserting another item should evict something else to make room, leaving it such that the cache still has the
        // same number of items.
        cache.insert(CAPACITY, "new_value");
        assert_eq!(cache.len(), CAPACITY);
        assert_eq!(cache.weight(), CAPACITY as u64);

        let mut evicted = false;
        for i in 0..CAPACITY {
            if cache.get(&i).is_none() {
                evicted = true;
                break;
            }
        }
        assert!(evicted, "expected at least one original item to be evicted");
    }

    #[test]
    fn overweight_item() {
        const CAPACITY: usize = 10;

        // Create our cache using an "item value" weighter, which uses the item value itself as the weight.
        let cache = CacheBuilder::for_tests()
            .with_capacity(NonZeroUsize::new(CAPACITY).unwrap())
            .with_item_weighter(ItemValueWeighter)
            .build();

        // We should fail to insert an item that is too heavy for the cache by itself.
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.weight(), 0);

        cache.insert(1, CAPACITY + 1);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.weight(), 0);
        assert_eq!(cache.get(&1), None);
    }

    #[test]
    fn evict_on_insert_by_weight() {
        const CAPACITY: usize = 10;

        // Create our cache using an "item value" weighter, which uses the item value itself as the weight.
        let cache = CacheBuilder::for_tests()
            .with_capacity(NonZeroUsize::new(CAPACITY).unwrap())
            .with_item_weighter(ItemValueWeighter)
            .build();

        // Insert three items which together have a weight equal to the cache capacity.
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.weight(), 0);

        cache.insert(1, 3);
        cache.insert(2, 4);
        cache.insert(3, 3);
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.weight(), CAPACITY as u64);

        // Now try to insert an item that has a weight that is smaller than the cache capacity, but larger than all
        // prior items combined, which should evict all prior items to make room for the new item.
        cache.insert(4, CAPACITY - 1);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.weight(), (CAPACITY - 1) as u64);

        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&3), None);
        assert_eq!(cache.get(&4), Some(CAPACITY - 1));
    }

    /// Returns the counters of the cache `identifier` in `recorder`: hits, misses, insertions, removals, evictions.
    fn operation_counts(recorder: &saluki_metrics::test::TestRecorder, identifier: &'static str) -> [Option<u64>; 5] {
        [
            "hits_total",
            "misses_total",
            "items_inserted_total",
            "items_removed_total",
            "items_evicted_total",
        ]
        .map(|name| {
            let name = format!("cache_{}", name);
            recorder.counter(metrics::Key::from_parts(
                name,
                vec![metrics::Label::new("cache_id", identifier)],
            ))
        })
    }

    /// Runs the same operations on a cache: three insertions into a cache of two, which evicts one, two hits, a miss,
    /// and a removal.
    fn exercise(cache: &Cache<u64, u64>) {
        cache.insert(1, 10);
        cache.insert(2, 20);
        assert_eq!(cache.get(&1), Some(10));
        cache.insert(3, 30);
        let found = (1..=3).filter(|key| cache.get(key).is_some()).count();
        assert_eq!(found, 2, "one of three items was evicted from a cache of two");
        cache.remove(&3);
    }

    #[tokio::test]
    async fn an_accumulating_cache_publishes_what_a_direct_cache_writes_as_it_goes() {
        let recorder = saluki_metrics::test::TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let build = |identifier: &'static str| {
            CacheBuilder::from_identifier(identifier)
                .unwrap()
                .with_capacity(NonZeroUsize::new(2).unwrap())
        };

        let direct = build("direct").build();
        exercise(&direct);
        let written = operation_counts(&recorder, "direct");
        assert_eq!(written, [Some(3), Some(1), Some(3), Some(1), Some(1)]);

        let accumulating = build("accumulating").with_accumulated_telemetry().build();
        exercise(&accumulating);
        assert_eq!(
            operation_counts(&recorder, "accumulating"),
            [Some(0); 5],
            "nothing is written before the cache publishes"
        );
        accumulating.publish_telemetry();
        assert_eq!(operation_counts(&recorder, "accumulating"), written);

        // Counts left unpublished reach the telemetry when the last clone of the cache is dropped, and never twice.
        let clone = accumulating.clone();
        clone.insert(4, 40);
        assert_eq!(accumulating.get(&4), Some(40));
        accumulating.publish_telemetry();
        clone.remove(&4);
        drop(accumulating);
        assert_eq!(
            operation_counts(&recorder, "accumulating")[3],
            Some(1),
            "the clone keeps the counts"
        );
        drop(clone);
        assert_eq!(
            operation_counts(&recorder, "accumulating"),
            [Some(4), Some(1), Some(4), Some(2), Some(1)]
        );
    }

    /// Returns the gauges of the cache `identifier` in `recorder`: items, weight, and weight limit.
    fn gauges(recorder: &saluki_metrics::test::TestRecorder, identifier: &'static str) -> [Option<f64>; 3] {
        ["current_items", "current_weight", "weight_limit"].map(|name| {
            recorder.gauge(metrics::Key::from_parts(
                format!("cache_{}", name),
                vec![metrics::Label::new("cache_id", identifier)],
            ))
        })
    }

    #[tokio::test(start_paused = true)]
    async fn the_gauges_of_summed_caches_of_one_identifier_sum_and_each_cache_withdraws_its_share_when_dropped() {
        let recorder = saluki_metrics::test::TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let build = |identifier: &'static str, capacity: usize, gauges: GaugeMode| {
            CacheBuilder::<u64, u64>::from_identifier(identifier)
                .unwrap()
                .with_capacity(NonZeroUsize::new(capacity).unwrap())
                .with_gauges(gauges)
                .build()
        };
        let (first, second) = (
            build("summed", 4, GaugeMode::Summed),
            build("summed", 6, GaugeMode::Summed),
        );
        assert_eq!(gauges(&recorder, "summed")[2], Some(10.0), "the limits sum at build");
        first.insert(1, 1);
        first.insert(2, 2);
        (10..13).for_each(|key| second.insert(key, key));
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert_eq!(gauges(&recorder, "summed"), [Some(5.0), Some(5.0), Some(10.0)]);
        first.remove(&1);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            gauges(&recorder, "summed"),
            [Some(4.0), Some(4.0), Some(10.0)],
            "only changes are added"
        );

        // A clone keeps the cache's share; the last one dropped withdraws it.
        let clone = first.clone();
        drop(first);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(gauges(&recorder, "summed"), [Some(4.0), Some(4.0), Some(10.0)]);
        drop(clone);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(gauges(&recorder, "summed"), [Some(3.0), Some(3.0), Some(6.0)]);
        drop(second);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(gauges(&recorder, "summed"), [Some(0.0), Some(0.0), Some(0.0)]);

        // Caches whose gauges are set show the last writer's values, and keep them once dropped.
        let (first, second) = (build("set", 4, GaugeMode::Set), build("set", 6, GaugeMode::Set));
        first.insert(1, 1);
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert_eq!(gauges(&recorder, "set")[2], Some(6.0));
        assert!([Some(0.0), Some(1.0)].contains(&gauges(&recorder, "set")[0]));
        drop((first, second));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(gauges(&recorder, "set")[2], Some(6.0));
    }

    #[test]
    fn every_change_to_a_key_advances_its_change_stamp_and_lookups_do_not() {
        // Keys hash to themselves, so that keys below 4,096 apart never share a stamp.
        let cache = CacheBuilder::<u64, u64>::for_tests()
            .with_capacity(NonZeroUsize::new(4).unwrap())
            .with_hasher::<saluki_common::hash::NoopU64BuildHasher>()
            .with_change_stamps()
            .build();

        let before = cache.change_stamp(&1).unwrap();
        cache.insert(1, 10);
        let inserted = cache.change_stamp(&1).unwrap();
        assert_ne!(inserted, before, "an insertion advances the stamp");
        assert_eq!(cache.get(&1), Some(10));
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.change_stamp(&1), Some(inserted), "lookups leave it");
        cache.remove(&1);
        assert_ne!(cache.change_stamp(&1), Some(inserted), "a removal advances it");

        // Evictions: of keys inserted once into a cache of four, exactly those the cache evicted have a stamp that
        // moved since their insertion.
        let inserted = (100..120)
            .map(|key| {
                cache.insert(key, 1);
                (key, cache.change_stamp(&key).unwrap())
            })
            .collect::<Vec<_>>();
        let mut evicted = 0;
        for (key, stamp) in inserted {
            if cache.peek(&key).is_some() {
                assert_eq!(cache.change_stamp(&key), Some(stamp), "nothing changed key {}", key);
            } else {
                assert_ne!(
                    cache.change_stamp(&key),
                    Some(stamp),
                    "the eviction of key {} advanced it",
                    key
                );
                evicted += 1;
            }
        }
        assert!(evicted >= 16, "a cache of four holds at most four of 20 keys");

        let without = CacheBuilder::<u64, u64>::for_tests().build();
        without.insert(1, 10);
        assert_eq!(without.change_stamp(&1), None, "a cache without change stamps has none");
    }

    #[tokio::test]
    async fn an_expired_key_advances_its_change_stamp() {
        let cache = CacheBuilder::<u64, u64>::for_tests()
            .with_time_to_idle(Some(Duration::from_secs(1)))
            .with_expiration_interval(Duration::from_millis(50))
            .with_change_stamps()
            .build();
        cache.insert(1, 10);
        let inserted = cache.change_stamp(&1).unwrap();

        tokio::time::timeout(Duration::from_secs(10), async {
            while cache.peek(&1).is_some() {
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("an idle key expires within a few seconds");
        assert_ne!(cache.change_stamp(&1), Some(inserted));
    }

    #[tokio::test]
    async fn tasks_stop_when_cache_dropped() {
        let cache = CacheBuilder::<u64, u64>::from_identifier("test-drop")
            .expect("valid identifier")
            .with_time_to_idle(Some(Duration::from_secs(60)))
            .with_expiration_interval(Duration::from_millis(50))
            .build();

        // Grab a weak reference to the raw cache data held by the background tasks.
        let weak_cache = Arc::downgrade(&cache.inner.cache);

        drop(cache);

        // When `InnerCache` is dropped, the cancellation token's drop guard is also dropped, which triggers
        // cancellation, so both tasks should wake up immediately and exit, releasing their Arc<RawCache> references.
        //
        // TODO: There's no good way to assert the tasks have shutdown besides sleeping and checking the weak cache is
        // gone. It would be nice if there was a way to asynchronously _and_ fallibly shutdown the runtime with a
        // timeout, such that we could detect if they shutdown cleanly... but alas.
        sleep(Duration::from_millis(100)).await;

        assert!(
            weak_cache.upgrade().is_none(),
            "raw cache should be released after background tasks exit"
        );
    }
}
