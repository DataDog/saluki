use std::{marker::PhantomData, num::NonZeroUsize, sync::Arc, time::Duration};

use saluki_error::GenericError;
use saluki_metrics::static_metrics;
use tokio::time::sleep;
use tracing::debug;

use crate::{hash::FastBuildHasher, task::spawn_traced};

mod expiry;
use self::expiry::{Expiration, ExpirationBuilder, ExpiryCapableLifecycle};

pub mod weight;
use self::weight::{ItemCountWeighter, Weighter, WrappedWeighter};

type InnerCache<K, V, W, H> = quick_cache::sync::Cache<K, V, WrappedWeighter<W>, H, ExpiryCapableLifecycle<K>>;

static_metrics! {
    name => Telemetry,
    prefix => cache,
    labels => [cache_id: String],
    metrics => [
        gauge(current_items),
        gauge(current_weight),
        gauge(weight_limit),
        counter(hits_total),
        counter(misses_total),
        counter(items_inserted_total),
        counter(items_removed_total),
        counter(items_expired_total),
        histogram(items_expired_batch_size),
    ],
}

/// Builder for creating a [`Cache`].
pub struct CacheBuilder<K, V, W = ItemCountWeighter, H = FastBuildHasher> {
    identifier: String,
    capacity: NonZeroUsize,
    weighter: W,
    idle_period: Option<Duration>,
    expiration_interval: Option<Duration>,
    telemetry_enabled: bool,
    _key: PhantomData<K>,
    _value: PhantomData<V>,
    _hasher: PhantomData<H>,
}

impl<K, V> CacheBuilder<K, V> {
    /// Creates a new `CacheBuilder` with the given cache identifier.
    ///
    /// The cache identifier _should_ be unique, but it is not required to be. Metrics for the cache will be emitted
    /// using the given identifier, so in cases where the identifier is not unique, those metrics will be aggregated
    /// together and it will not be possible to distinguish between the different caches.
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
            _key: PhantomData,
            _value: PhantomData,
            _hasher: PhantomData,
        })
    }

    /// Configures a [`CacheBuilder`] that is suitable for tests.
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
    /// See [`with_item_weighter`] for more information on how the item weighter is used.
    ///
    /// Defaults to unlimited capacity.
    pub fn with_capacity(mut self, capacity: NonZeroUsize) -> Self {
        self.capacity = capacity;
        self
    }

    /// Enables expiration of cached items based on how long since they were last accessed.
    ///
    /// Items which have not been accessed within the configured duration will be marked for expiration, and be removed
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
    /// _eligible_ for expiration after the configured duration, they are not _guaranteed_ to be
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
    /// such as their size in bytes, or some other metric that is relevant to the intended caching behavior.
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
        telemetry.weight_limit().set(capacity as f64);

        // Configure expiration if enabled.
        let mut expiration_builder = ExpirationBuilder::new();
        if let Some(time_to_idle) = self.idle_period {
            expiration_builder = expiration_builder.with_time_to_idle(time_to_idle);
        }
        let (expiration, expiry_lifecycle) = expiration_builder.build();

        // Create the underlying cache.
        let cache = Cache {
            inner: Arc::new(InnerCache::with(
                capacity,
                capacity as u64,
                WrappedWeighter::from(self.weighter),
                H::default(),
                expiry_lifecycle,
            )),
            expiration: expiration.clone(),
            telemetry: telemetry.clone(),
        };

        // If expiration is enabled, spawn a background task to actually drive expiration.
        if let Some(expiration_interval) = self.expiration_interval {
            let expiration = expiration.clone();

            spawn_traced(drive_expiration(
                cache.clone(),
                telemetry.clone(),
                expiration,
                expiration_interval,
            ));
        }

        // If telemetry is enabled, spawn a background task to drive telemetry reporting.
        if self.telemetry_enabled {
            spawn_traced(drive_telemetry(cache.clone(), telemetry));
        }

        cache
    }
}

/// A simple concurrent cache.
#[derive(Clone)]
pub struct Cache<K, V, W, H> {
    inner: Arc<InnerCache<K, V, W, H>>,
    expiration: Expiration<K>,
    telemetry: Telemetry,
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
        self.inner.is_empty()
    }

    /// Returns the number of items currently in the cache.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns the total weight of all items in the cache.
    pub fn weight(&self) -> u64 {
        self.inner.weight()
    }

    /// Inserts an item into the cache with the given key and value.
    ///
    /// If an item with the same key already exists, it will be overwritten and the old value will be dropped. If the
    /// cache is full, one or more items will be evicted to make room for the new item, based on the configured item
    /// weighter and the weight of the new item.
    pub fn insert(&self, key: K, value: V) {
        self.inner.insert(key.clone(), value);
        self.expiration.mark_entry_accessed(key);
        self.telemetry.items_inserted_total().increment(1);
    }

    /// Gets an item from the cache by its key.
    ///
    /// If the item is found, it is cloned and `Some(value)` is returned. Otherwise, `None` is returned.
    pub fn get(&self, key: &K) -> Option<V> {
        let value = self.inner.get(key);
        if value.is_some() {
            self.expiration.mark_entry_accessed(key.clone());
            self.telemetry.hits_total().increment(1);
        } else {
            self.telemetry.misses_total().increment(1);
        }
        value
    }

    /// Removes an item from the cache by its key.
    pub fn remove(&self, key: &K) {
        self.inner.remove(key);
        self.expiration.mark_entry_removed(key.clone());
        self.telemetry.items_removed_total().increment(1);
    }
}

async fn drive_expiration<K, V, W, H>(
    cache: Cache<K, V, W, H>, telemetry: Telemetry, expiration: Expiration<K>, expiration_interval: Duration,
) where
    K: Eq + std::hash::Hash + Clone,
    V: Clone,
    W: Weighter<K, V> + Clone,
    H: std::hash::BuildHasher + Clone,
{
    let mut expired_item_keys = Vec::new();

    loop {
        sleep(expiration_interval).await;

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
        }

        debug!(num_expired_items, "Removed expired items.");
    }
}

async fn drive_telemetry<K, V, W, H>(cache: Cache<K, V, W, H>, telemetry: Telemetry)
where
    K: Eq + std::hash::Hash + Clone,
    V: Clone,
    W: Weighter<K, V> + Clone,
    H: std::hash::BuildHasher + Clone,
{
    loop {
        sleep(Duration::from_secs(1)).await;

        telemetry.current_items().set(cache.len() as f64);
        telemetry.current_weight().set(cache.weight() as f64);
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
}
