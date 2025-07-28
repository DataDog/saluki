use std::{
    future::{ready, Ready},
    sync::Arc,
};

use memory_accounting::allocator::AllocationGroupToken;

use super::{Clearable, ObjectPool, PoolMetrics, Poolable, ReclaimStrategy};

/// An object pool that allocates objects on demand.
///
/// This pool implementation is meant to satisfy the interface of [`ObjectPool`] without performing any actual pooling.
pub struct OnDemandObjectPool<T: Poolable> {
    strategy: Arc<OnDemandStrategy<T>>,
}

impl<T> OnDemandObjectPool<T>
where
    T: Poolable + 'static,
    T::Data: Default,
{
    /// Creates a new `OnDemandObjectPool`.
    pub fn new<S>(pool_name: S) -> Self
    where
        S: Into<String>,
    {
        Self::with_builder(pool_name, T::Data::default)
    }
}

impl<T> OnDemandObjectPool<T>
where
    T: Poolable + 'static,
{
    /// Creates a new `OnDemandObjectPool` with the given item builder.
    ///
    /// `builder` is called to construct each item.
    pub fn with_builder<S, B>(pool_name: S, builder: B) -> Self
    where
        S: Into<String>,
        B: Fn() -> T::Data + Send + Sync + 'static,
    {
        let strategy = Arc::new(OnDemandStrategy::with_builder(pool_name, builder));

        Self { strategy }
    }
}

impl<T: Poolable> Clone for OnDemandObjectPool<T> {
    fn clone(&self) -> Self {
        Self {
            strategy: self.strategy.clone(),
        }
    }
}

impl<T> ObjectPool for OnDemandObjectPool<T>
where
    T: Poolable + Send + Unpin + 'static,
{
    type Item = T;
    type AcquireFuture = Ready<T>;

    fn acquire(&self) -> Self::AcquireFuture {
        let strategy = Arc::clone(&self.strategy);
        let item = strategy.build();
        ready(T::from_data(strategy, item))
    }
}

struct OnDemandStrategy<T: Poolable> {
    builder: Box<dyn Fn() -> T::Data + Send + Sync>,
    alloc_group: AllocationGroupToken,
    metrics: PoolMetrics,
}

impl<T: Poolable> OnDemandStrategy<T> {
    fn with_builder<S, B>(pool_name: S, builder: B) -> Self
    where
        S: Into<String>,
        B: Fn() -> T::Data + Send + Sync + 'static,
    {
        let builder = Box::new(builder);

        let metrics = PoolMetrics::new(pool_name.into());
        metrics.capacity().set(usize::MAX as f64);

        Self {
            builder,
            alloc_group: AllocationGroupToken::current(),
            metrics,
        }
    }

    fn build(&self) -> T::Data {
        self.metrics.created().increment(1);
        self.metrics.in_use().increment(1.0);

        let _ = self.alloc_group.enter();
        (self.builder)()
    }
}

impl<T: Poolable> ReclaimStrategy<T> for OnDemandStrategy<T> {
    fn reclaim(&self, mut data: T::Data) {
        data.clear();
        drop(data);

        self.metrics.released().increment(1);
        self.metrics.in_use().decrement(1.0);
    }
}
