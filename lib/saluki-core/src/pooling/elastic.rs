use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{
            AtomicUsize,
            Ordering::{AcqRel, Relaxed},
        },
        Arc, Mutex,
    },
    task::{ready, Context, Poll},
};

use pin_project::pin_project;
use tokio::sync::Semaphore;
use tokio_util::sync::PollSemaphore;

use super::{Clearable, ObjectPool, Poolable, ReclaimStrategy};

/// An elastic object pool.
///
/// Pools are configured with a minimum and maximum size, and allocate the minimum number of items up front. When an
/// item is requested and the pool is empty, but has not yet reached its maximum size, it will allocate the item on
/// demand.
///
/// Periodically, a background task will evaluate the utilization of the pool and shrink the pool size in order to
/// attempt to size it more closely to the recent demand. The frequency of this shrinking, as well as how usage demand
/// is captured and rolled off, is configurable.
///
/// ## Missing
///
/// - Actual configurability around the shrinking frequency and usage demand roll-off.
pub struct ElasticObjectPool<T: Poolable> {
    strategy: Arc<ElasticStrategy<T>>,
}

impl<T: Poolable> ElasticObjectPool<T>
where
    T::Data: Default,
{
    /// Creates a new `ElasticObjectPool` with the given minimum and maximum capacity.
    pub fn with_capacity(min_capacity: usize, max_capacity: usize) -> Self {
        Self {
            strategy: Arc::new(ElasticStrategy::new(min_capacity, max_capacity)),
        }
    }
}

impl<T: Poolable> ElasticObjectPool<T> {
    /// Creates a new `ElasticObjectPool` with the given minimum and maximum capacity and item builder.
    ///
    /// `builder` is called to construct each item.
    pub fn with_builder<B>(min_capacity: usize, max_capacity: usize, builder: B) -> Self
    where
        B: Fn() -> T::Data + Send + Sync + 'static,
    {
        Self {
            strategy: Arc::new(ElasticStrategy::with_builder(min_capacity, max_capacity, builder)),
        }
    }
}

impl<T: Poolable> Clone for ElasticObjectPool<T> {
    fn clone(&self) -> Self {
        Self {
            strategy: self.strategy.clone(),
        }
    }
}

impl<T> ObjectPool for ElasticObjectPool<T>
where
    T: Poolable + Send + Unpin + 'static,
{
    type Item = T;
    type AcquireFuture = ElasticAcquireFuture<T>;

    fn acquire(&self) -> Self::AcquireFuture {
        ElasticStrategy::acquire(&self.strategy)
    }
}

struct ElasticStrategy<T: Poolable> {
    items: Mutex<VecDeque<T::Data>>,
    builder: Box<dyn Fn() -> T::Data + Send + Sync>,
    available: Arc<Semaphore>,
    active: AtomicUsize,
    min_capacity: usize,
    max_capacity: usize,
}

impl<T> ElasticStrategy<T>
where
    T: Poolable,
    T::Data: Default,
{
    fn new(min_capacity: usize, max_capacity: usize) -> Self {
        let builder = Box::new(T::Data::default);

        // Allocate enough storage to hold the maximum number of items, but only _build_ the minimum number of items.
        let mut items = VecDeque::with_capacity(max_capacity);
        items.extend((0..min_capacity).map(|_| builder()));
        let available = Arc::new(Semaphore::new(min_capacity));

        Self {
            items: Mutex::new(items),
            builder,
            available,
            active: AtomicUsize::new(min_capacity),
            min_capacity,
            max_capacity,
        }
    }
}

impl<T: Poolable> ElasticStrategy<T> {
    fn with_builder<B>(min_capacity: usize, max_capacity: usize, builder: B) -> Self
    where
        B: Fn() -> T::Data + Send + Sync + 'static,
    {
        let builder = Box::new(builder);

        // Allocate enough storage to hold the maximum number of items, but only _build_ the minimum number of items.
        let mut items = VecDeque::with_capacity(max_capacity);
        items.extend((0..min_capacity).map(|_| builder()));
        let available = Arc::new(Semaphore::new(min_capacity));

        Self {
            items: Mutex::new(items),
            builder,
            available,
            active: AtomicUsize::new(min_capacity),
            min_capacity,
            max_capacity,
        }
    }
}

impl<T> ElasticStrategy<T>
where
    T: Poolable,
    T::Data: Send + 'static,
{
    fn acquire(strategy: &Arc<Self>) -> ElasticAcquireFuture<T> {
        ElasticAcquireFuture::new(Arc::clone(strategy))
    }
}

impl<T: Poolable> ReclaimStrategy<T> for ElasticStrategy<T> {
    fn reclaim(&self, mut data: T::Data) {
        data.clear();

        self.items.lock().unwrap().push_back(data);
        self.available.add_permits(1);
    }
}

#[pin_project]
pub struct ElasticAcquireFuture<T: Poolable> {
    strategy: Option<Arc<ElasticStrategy<T>>>,
    semaphore: PollSemaphore,
}

impl<T: Poolable> ElasticAcquireFuture<T> {
    fn new(strategy: Arc<ElasticStrategy<T>>) -> Self {
        let semaphore = PollSemaphore::new(Arc::clone(&strategy.available));
        Self {
            strategy: Some(strategy),
            semaphore,
        }
    }
}

impl<T> Future for ElasticAcquireFuture<T>
where
    T: Poolable + 'static,
{
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        // See if there are any available permits. If not, we'll try to take the "allocate on demand" path if we haven't
        // reached maximum capacity yet.
        if this.semaphore.available_permits() == 0 {
            let strategy = this.strategy.as_ref().unwrap();

            let mut active = strategy.active.load(Relaxed);
            let max_capacity = strategy.max_capacity;

            loop {
                // If we're at capacity, we can't allocate any more items.
                if active == max_capacity {
                    break;
                }

                // Try to atomically increment `active` which signals that we still have capacity to allocate another
                // item, and more importantly, that _we_ are authorized to do so.
                if let Err(new_active) = strategy
                    .active
                    .compare_exchange_weak(active, active + 1, AcqRel, Relaxed)
                {
                    active = new_active;
                    continue;
                }

                let new_item = (strategy.builder)();
                let strategy = this.strategy.take().unwrap();

                return Poll::Ready(T::from_data(strategy, new_item));
            }
        }

        match ready!(this.semaphore.poll_acquire(cx)) {
            Some(permit) => {
                permit.forget();

                let strategy = this.strategy.take().unwrap();
                let data = strategy.items.lock().unwrap().pop_back().unwrap();
                Poll::Ready(T::from_data(strategy, data))
            }
            None => unreachable!("semaphore should never be closed"),
        }
    }
}
