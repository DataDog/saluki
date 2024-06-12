//! Object pooling.
use std::sync::Arc;

mod fixed;
use async_trait::async_trait;

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
    fn from_data(strategy: Arc<dyn Strategy<Self::Data> + Send + Sync>, data: Self::Data) -> Self;
}

/// Object pool strategy.
///
/// This trait is used to define the strategy for acquiring and releasing items from an object pool.
#[async_trait]
pub trait Strategy<T>
where
    T: Clearable,
{
    /// Acquires an item from the object pool.
    async fn acquire(&self) -> T;

    /// Returns an item to the object pool.
    fn reclaim(&self, data: T);
}

/// An object pool.
#[async_trait]
pub trait ObjectPool {
    /// The pooled value.
    type Item;

    /// Acquires an item from the object pool.
    async fn acquire(&self) -> Self::Item;
}
