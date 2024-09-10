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
    time::Duration,
};

use pin_project::pin_project;
use tokio::{sync::Semaphore, time::sleep};
use tokio_util::sync::PollSemaphore;
use tracing::debug;

use super::{Clearable, ObjectPool, PoolMetrics, Poolable, ReclaimStrategy};

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

impl<T> ElasticObjectPool<T>
where
    T: Poolable + 'static,
    T::Data: Default,
{
    /// Creates a new `ElasticObjectPool` with the given minimum and maximum capacity.
    pub fn with_capacity<S>(pool_name: S, min_capacity: usize, max_capacity: usize) -> (Self, impl Future<Output = ()>)
    where
        S: Into<String>,
    {
        Self::with_builder(pool_name, min_capacity, max_capacity, T::Data::default)
    }
}

impl<T> ElasticObjectPool<T>
where
    T: Poolable + 'static,
{
    /// Creates a new `ElasticObjectPool` with the given minimum and maximum capacity and item builder.
    ///
    /// `builder` is called to construct each item.
    pub fn with_builder<S, B>(
        pool_name: S, min_capacity: usize, max_capacity: usize, builder: B,
    ) -> (Self, impl Future<Output = ()>)
    where
        S: Into<String>,
        B: Fn() -> T::Data + Send + Sync + 'static,
    {
        let strategy = Arc::new(ElasticStrategy::with_builder(
            pool_name,
            min_capacity,
            max_capacity,
            builder,
        ));
        let shrinker = run_background_shrinker(Arc::clone(&strategy));

        (Self { strategy }, shrinker)
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
    metrics: PoolMetrics,
}

impl<T: Poolable> ElasticStrategy<T> {
    fn with_builder<S, B>(pool_name: S, min_capacity: usize, max_capacity: usize, builder: B) -> Self
    where
        S: Into<String>,
        B: Fn() -> T::Data + Send + Sync + 'static,
    {
        let builder = Box::new(builder);

        // Allocate enough storage to hold the maximum number of items, but only _build_ the minimum number of items.
        let mut items = VecDeque::with_capacity(max_capacity);
        items.extend((0..min_capacity).map(|_| builder()));
        let available = Arc::new(Semaphore::new(min_capacity));

        let metrics = PoolMetrics::new(pool_name.into());
        metrics.capacity().set(min_capacity as f64);
        metrics.created().increment(min_capacity as u64);

        Self {
            items: Mutex::new(items),
            builder,
            available,
            active: AtomicUsize::new(min_capacity),
            min_capacity,
            max_capacity,
            metrics,
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
        self.metrics.released().increment(1);
        self.metrics.in_use().decrement(1.0);
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

                strategy.metrics.created().increment(1);
                strategy.metrics.capacity().increment(1.0);
                strategy.metrics.in_use().increment(1.0);

                return Poll::Ready(T::from_data(strategy, new_item));
            }
        }

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

async fn run_background_shrinker<T: Poolable>(strategy: Arc<ElasticStrategy<T>>) {
    // The shrinker continuously analyzes the object pool statistics, tracking both the active pool size (total number
    // of allocated items belonging to the pool) and an exponentially weighted moving average of the number of
    // _available_ items in the pool.
    //
    // When the active pool size is greater than the minimum pool size, _and_ the average number of available items is
    // greater than 1, we consider the pool "eligible" for shrinking. We'll attempt to reduce the pool size by one,
    // consuming an item and adjusting all of the relevant counts. We repeat this process on each iteration until the
    // pool is at the minimum size or the average number of available items is less than 1.

    // We use an alpha of 0.1, which provides a fairly strong smoothing effect.
    let mut average_available = Ewma::new(0.1);
    let min_capacity = strategy.min_capacity;

    loop {
        debug!("Shrinker sleeping.");
        sleep(Duration::from_secs(1)).await;

        // Track the number of available permits, and the number of active items.
        //
        // When we have available permits, and our active count is greater than the minimum capacity, we'll take an item
        // from the pool.
        let active = strategy.active.load(Relaxed);
        if active <= min_capacity {
            debug!("Object pool already at minimum capacity. Nothing to do.");
            continue;
        }

        let available = strategy.available.available_permits();
        average_available.update(available as f64);

        if average_available.value() > 1.0 {
            debug!(
                avg_avail = average_available.value(),
                active, min_capacity, "Pool qualifies for shrinking. Attempting to remove single item..."
            );

            // First try acquiring an item from the pool. If we can't, then maybe we hit a temporary burst of demand,
            // and we'll just try again later.
            let removed = {
                let mut items = strategy.items.lock().unwrap();
                match items.pop_back() {
                    Some(item) => {
                        // Explicitly drop the item here to make sure it's cleaned up and gone.
                        drop(item);

                        // Now that we've removed an item from the pool, decrement the active count and the available permits to
                        // reflect it.
                        strategy.active.fetch_sub(1, AcqRel);
                        strategy.available.forget_permits(1);

                        strategy.metrics.deleted().increment(1);
                        strategy.metrics.capacity().decrement(1.0);
                        strategy.metrics.in_use().decrement(1.0);

                        true
                    }
                    None => false,
                }
            };

            if removed {
                debug!("Shrinker successfully removed an item from the pool.");
            } else {
                debug!("Outside caller acquired the last item before we could shrink.");
            }
        } else {
            debug!(
                avg_avail = average_available.value(),
                active, min_capacity, "Pool does not qualify for shrinking."
            );
        }
    }
}

struct Ewma {
    value: f64,
    alpha: f64,
}

impl Ewma {
    fn new(alpha: f64) -> Self {
        Self { value: 0.0, alpha }
    }

    fn update(&mut self, new_value: f64) {
        self.value = (1.0 - self.alpha) * self.value + self.alpha * new_value;
    }

    fn value(&self) -> f64 {
        self.value
    }
}
