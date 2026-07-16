use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{ready, Context, Poll},
};

use pin_project::pin_project;
use tokio::sync::Semaphore;
use tokio_util::sync::PollSemaphore;

use super::{Clearable, ObjectPool, PoolMetrics, Poolable, ReclaimStrategy};

/// A fixed-size object pool.
///
/// All items for this object pool are created up front, and when the object pool is empty, calls to `acquire` will
/// block until an item is returned to the pool.
///
/// ## Metrics
///
/// - `object_pool.acquired{pool_name="<pool_name>"}` - total count of the number of objects acquired from the pool (counter)
/// - `object_pool.released{pool_name="<pool_name>"}` - total count of the number of objects released back to the pool (counter)
/// - `object_pool.in_use{pool_name="<pool_name>"}` - number of objects from the pool that are currently in use (gauge)
pub struct FixedSizeObjectPool<T: Poolable> {
    strategy: Arc<FixedSizeStrategy<T>>,
}

impl<T: Poolable> FixedSizeObjectPool<T>
where
    T::Data: Default,
{
    /// Creates a new `FixedSizeObjectPool` with the given capacity.
    ///
    /// Metrics are emitted for the pool with a tag (`pool_name`) that's set to the value of the given pool name.
    pub fn with_capacity<S>(pool_name: S, capacity: usize) -> Self
    where
        S: Into<String>,
    {
        Self {
            strategy: Arc::new(FixedSizeStrategy::new(pool_name, capacity)),
        }
    }
}

impl<T: Poolable> FixedSizeObjectPool<T> {
    /// Creates a new `FixedSizeObjectPool` with the given capacity and item builder.
    ///
    /// `builder` is called to construct each item.
    ///
    /// Metrics are emitted for the pool with a tag (`pool_name`) that's set to the value of the given pool name.
    pub fn with_builder<S, B>(pool_name: S, capacity: usize, builder: B) -> Self
    where
        S: Into<String>,
        B: Fn() -> T::Data,
    {
        Self {
            strategy: Arc::new(FixedSizeStrategy::with_builder(pool_name, capacity, builder)),
        }
    }
}

impl<T: Poolable> Clone for FixedSizeObjectPool<T> {
    fn clone(&self) -> Self {
        Self {
            strategy: self.strategy.clone(),
        }
    }
}

impl<T> ObjectPool for FixedSizeObjectPool<T>
where
    T: Poolable + Send + Unpin + 'static,
{
    type Item = T;
    type AcquireFuture = FixedSizeAcquireFuture<T>;

    fn acquire(&self) -> Self::AcquireFuture {
        FixedSizeStrategy::acquire(&self.strategy)
    }
}

struct FixedSizeStrategy<T: Poolable> {
    items: Mutex<VecDeque<T::Data>>,
    available: Arc<Semaphore>,
    metrics: PoolMetrics,
}

impl<T> FixedSizeStrategy<T>
where
    T: Poolable,
    T::Data: Default,
{
    fn new<S>(pool_name: S, capacity: usize) -> Self
    where
        S: Into<String>,
    {
        let mut items = VecDeque::with_capacity(capacity);
        items.extend((0..capacity).map(|_| T::Data::default()));
        let available = Arc::new(Semaphore::new(capacity));

        Self {
            items: Mutex::new(items),
            available,
            metrics: PoolMetrics::new(pool_name.into()),
        }
    }
}

impl<T: Poolable> FixedSizeStrategy<T> {
    fn with_builder<S, B>(pool_name: S, capacity: usize, builder: B) -> Self
    where
        S: Into<String>,
        B: Fn() -> T::Data,
    {
        let mut items = VecDeque::with_capacity(capacity);
        items.extend((0..capacity).map(|_| builder()));
        let available = Arc::new(Semaphore::new(capacity));

        let metrics = PoolMetrics::new(pool_name.into());
        metrics.created().increment(capacity as u64);
        metrics.capacity().set(capacity as f64);

        Self {
            items: Mutex::new(items),
            available,
            metrics,
        }
    }
}

impl<T> FixedSizeStrategy<T>
where
    T: Poolable,
    T::Data: Send + 'static,
{
    fn acquire(strategy: &Arc<Self>) -> FixedSizeAcquireFuture<T> {
        FixedSizeAcquireFuture::new(Arc::clone(strategy))
    }
}

impl<T: Poolable> ReclaimStrategy<T> for FixedSizeStrategy<T> {
    fn reclaim(&self, mut data: T::Data) {
        data.clear();

        self.items.lock().unwrap().push_back(data);
        self.available.add_permits(1);
        self.metrics.released().increment(1);
        self.metrics.in_use().decrement(1.0);
    }
}

#[pin_project]
pub struct FixedSizeAcquireFuture<T: Poolable> {
    strategy: Option<Arc<FixedSizeStrategy<T>>>,
    semaphore: PollSemaphore,
}

impl<T: Poolable> FixedSizeAcquireFuture<T> {
    fn new(strategy: Arc<FixedSizeStrategy<T>>) -> Self {
        let semaphore = PollSemaphore::new(Arc::clone(&strategy.available));
        Self {
            strategy: Some(strategy),
            semaphore,
        }
    }
}

impl<T> Future for FixedSizeAcquireFuture<T>
where
    T: Poolable + 'static,
{
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        match ready!(this.semaphore.poll_acquire(cx)) {
            Some(permit) => {
                permit.forget();

                let strategy = this.strategy.take().unwrap();
                let data = strategy.items.lock().unwrap().pop_back().unwrap();
                strategy.metrics.acquired().increment(1);
                strategy.metrics.in_use().increment(1.0);
                Poll::Ready(T::from_data(strategy, data))
            }
            None => {
                saluki_antithesis::unreachable!("fixed object pool semaphore closed");
                unreachable!("semaphore should never be closed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use saluki_metrics::test::TestRecorder;
    use tokio_test::{assert_pending, assert_ready, task::spawn};

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
    fn preallocates_capacity_and_blocks_when_pool_is_empty() {
        // Documented contract: all items are created up front, and once the pool is empty `acquire`
        // blocks until an item is returned.
        let pool = FixedSizeObjectPool::<PooledValue>::with_capacity("test", 2);
        assert_eq!(pool.strategy.available.available_permits(), 2);

        let mut first_acquire = spawn(pool.acquire());
        let first = assert_ready!(first_acquire.poll());
        let mut second_acquire = spawn(pool.acquire());
        let second = assert_ready!(second_acquire.poll());
        assert_eq!(pool.strategy.available.available_permits(), 0);

        // The pool is empty, so a third acquire must block rather than allocate a new item.
        let mut third_acquire = spawn(pool.acquire());
        assert_pending!(third_acquire.poll());
        assert!(!third_acquire.is_woken());

        // Returning an item wakes the blocked acquire and lets it complete.
        drop(first);
        assert!(third_acquire.is_woken());
        let third = assert_ready!(third_acquire.poll());
        assert_eq!(pool.strategy.available.available_permits(), 0);

        // Returning the remaining items restores the pool to its full capacity.
        drop(second);
        drop(third);
        assert_eq!(pool.strategy.available.available_permits(), 2);
    }

    #[test]
    fn clears_items_before_returning_them_to_the_pool() {
        // A capacity-1 pool forces reuse of the same backing item, so a value written before release
        // must have been cleared by the time the item is re-acquired.
        let pool = FixedSizeObjectPool::<PooledValue>::with_capacity("test", 1);

        let mut first_acquire = spawn(pool.acquire());
        let mut item = assert_ready!(first_acquire.poll());
        item.data_mut().value = 42;
        drop(item);

        let mut second_acquire = spawn(pool.acquire());
        let item = assert_ready!(second_acquire.poll());
        assert_eq!(item.data().value, 0, "a released item must be cleared before reuse");
        drop(item);
    }

    #[test]
    fn tracks_acquire_and_release_metrics() {
        // Documented metrics: `object_pool_acquired`/`object_pool_released` count acquisitions and
        // releases, and `object_pool_in_use` tracks the number of currently-outstanding items.
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let pool = FixedSizeObjectPool::<PooledValue>::with_capacity("test", 2);

        let mut first_acquire = spawn(pool.acquire());
        let item = assert_ready!(first_acquire.poll());
        assert_eq!(
            recorder.counter((PoolMetrics::acquired_name(), &[("pool_name", "test")])),
            Some(1)
        );
        assert_eq!(
            recorder.gauge((PoolMetrics::in_use_name(), &[("pool_name", "test")])),
            Some(1.0)
        );

        drop(item);
        assert_eq!(
            recorder.counter((PoolMetrics::released_name(), &[("pool_name", "test")])),
            Some(1)
        );
        assert_eq!(
            recorder.gauge((PoolMetrics::in_use_name(), &[("pool_name", "test")])),
            Some(0.0)
        );
    }
}
