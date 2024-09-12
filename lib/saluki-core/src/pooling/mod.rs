//! Object pooling.
use std::{future::Future, sync::Arc};

mod fixed;

use saluki_metrics::static_metrics;

pub use self::fixed::FixedSizeObjectPool;

pub mod helpers;

/// An item that can be cleared.
pub trait Clearable {
    /// Clears the item.
    fn clear(&mut self) {}
}

/// An item that is poolable.
///
/// This meta-trait is used to mark a type as being poolable, where the type itself wraps a piece of data
/// (Self::Data), and the data itself is the actual value which gets pooled and reused.
///
/// While somewhat incestuous, what this unlocks is the ability to use wrapping types to hide away the complexity of
/// ensuring that data which is given from an objext pool is eventully returned and not lost. Otherwise, trying to do
/// something like using drop logic to send `T` back to an object pool would involve _replacing_ the value of `T` with a
/// new value, which is not always possible, at least not without incurring additional allocations, which would negate
/// the use of an object pool in the first place.
pub trait Poolable {
    /// The inner data value that is stored in the object pool.
    type Data: Clearable + Send + 'static;

    /// Creates a new `Self` from the object pool strategy and data value.
    fn from_data(strategy: Arc<dyn ReclaimStrategy<Self> + Send + Sync>, data: Self::Data) -> Self;
}

/// Object pool reclaimation strategy.
///
/// This trait is used to define the strategy for reclaiming items to an object pool.
pub trait ReclaimStrategy<T>
where
    T: Poolable,
{
    /// Returns an item to the object pool.
    fn reclaim(&self, data: T::Data);
}

/// An object pool.
pub trait ObjectPool: Send + Sync {
    /// The pooled value.
    type Item: Send + Unpin;

    /// Type of future returned by `acquire`.
    type AcquireFuture: Future<Output = Self::Item> + Send;

    /// Acquires an item from the object pool.
    fn acquire(&self) -> Self::AcquireFuture;
}

static_metrics! {
    name => PoolMetrics,
    prefix => object_pool,
    labels => [pool_name: String],
    metrics => [
        counter(acquired),
        counter(released),
        gauge(in_use),
    ],
}
