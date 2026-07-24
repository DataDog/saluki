use std::{
    future::{ready, Ready},
    sync::Arc,
};

use saluki_common::resource_tracking::ResourceGroupToken;

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
        S: AsRef<str>,
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
        S: AsRef<str>,
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
    resource_group: ResourceGroupToken,
    metrics: PoolMetrics,
}

impl<T: Poolable> OnDemandStrategy<T> {
    fn with_builder<S, B>(pool_name: S, builder: B) -> Self
    where
        S: AsRef<str>,
        B: Fn() -> T::Data + Send + Sync + 'static,
    {
        let builder = Box::new(builder);

        let metrics = PoolMetrics::new(pool_name.as_ref());
        metrics.capacity().set(usize::MAX as f64);

        Self {
            builder,
            resource_group: ResourceGroupToken::current(),
            metrics,
        }
    }

    fn build(&self) -> T::Data {
        self.metrics.created().increment(1);
        self.metrics.in_use().increment(1.0);

        let _ = self.resource_group.enter();
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

#[cfg(test)]
mod tests {
    use saluki_metrics::test::TestRecorder;
    use tokio_test::{assert_ready, task::spawn};

    use super::*;
    use crate::pooled;

    pooled! {
        struct PooledValue {
            value: u32,
        }

        clear => |this| this.value = 0
    }

    impl std::fmt::Debug for PooledValue {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PooledValue").finish_non_exhaustive()
        }
    }

    #[test]
    fn allocates_a_fresh_item_on_every_acquire_without_blocking() {
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let pool = OnDemandObjectPool::<PooledValue>::new("test");

        // Documented contract: this pool performs no actual pooling. Every acquire builds a brand-new
        // item and completes immediately, even while a previously-acquired item is still outstanding
        // (a fixed-size pool would instead block on the second acquire here).
        let mut first_acquire = spawn(pool.acquire());
        let first = assert_ready!(first_acquire.poll());
        let mut second_acquire = spawn(pool.acquire());
        let second = assert_ready!(second_acquire.poll());

        // Both acquisitions allocated on demand, so two items were created and two are in use.
        assert_eq!(
            recorder.counter((PoolMetrics::created_name(), &[("pool_name", "test")])),
            Some(2)
        );
        assert_eq!(
            recorder.gauge((PoolMetrics::in_use_name(), &[("pool_name", "test")])),
            Some(2.0)
        );

        // Releasing items drops them (nothing is retained) and drives the in-use gauge back to zero.
        drop(first);
        drop(second);
        assert_eq!(
            recorder.counter((PoolMetrics::released_name(), &[("pool_name", "test")])),
            Some(2)
        );
        assert_eq!(
            recorder.gauge((PoolMetrics::in_use_name(), &[("pool_name", "test")])),
            Some(0.0)
        );
    }

    #[test]
    fn reports_effectively_unbounded_capacity() {
        // The pool never enforces a ceiling, which it advertises via the capacity gauge as `usize::MAX`.
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let _pool = OnDemandObjectPool::<PooledValue>::new("test");
        assert_eq!(
            recorder.gauge((PoolMetrics::capacity_name(), &[("pool_name", "test")])),
            Some(usize::MAX as f64)
        );
    }
}
