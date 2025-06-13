// NOTE: We're wrapping the `Weighter` trait from `quick_cache` to provide an abstracted interface over `quick_cache`,
// so that we can more easily swap out the underlying cache implementation in the future if needed.

/// Calculates the weight of cache items.
pub trait Weighter<K, V> {
    /// Returns the weight of the cache item.
    ///
    /// For performance reasons, this function should be as cheap as possible, as it will be called during the cache
    /// eviction routine. If the weight is expensive to calculate, consider caching it alongside the value.
    ///
    /// Zero (0) weight items are allowed and will be ignored when looking for eviction candidates. Such items can only
    /// be manually removed or overwritten.
    fn item_weight(&self, key: &K, value: &V) -> u64;
}

/// Wrapper struct to translate from `Weighter` to `quick_cache::Weighter`.
#[derive(Clone)]
pub(super) struct WrappedWeighter<W>(W);

impl<W> From<W> for WrappedWeighter<W> {
    fn from(weighter: W) -> Self {
        Self(weighter)
    }
}

impl<W, K, V> quick_cache::Weighter<K, V> for WrappedWeighter<W>
where
    W: Weighter<K, V>,
{
    fn weight(&self, key: &K, value: &V) -> u64 {
        self.0.item_weight(key, value)
    }
}

/// Weights items equally, assigning a weight of one to each item.
///
/// Caches using this weighter will hold `n` items, where `n` is the configured capacity of the cache.
#[derive(Clone, Default)]
pub struct ItemCountWeighter;

impl<K, V> Weighter<K, V> for ItemCountWeighter {
    fn item_weight(&self, _key: &K, _value: &V) -> u64 {
        1
    }
}
