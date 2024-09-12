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
    /// Metrics are emitted for the pool with a tag (`pool_name`) that is set to the value of the given pool name.
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
    /// Metrics are emitted for the pool with a tag (`pool_name`) that is set to the value of the given pool name.
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

        Self {
            items: Mutex::new(items),
            available,
            metrics: PoolMetrics::new(pool_name.into()),
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
            None => unreachable!("semaphore should never be closed"),
        }
    }
}
