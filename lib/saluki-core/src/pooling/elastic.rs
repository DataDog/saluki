use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{
            AtomicUsize,
            Ordering::{AcqRel, Acquire, Relaxed},
        },
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::Duration,
};

use memory_accounting::allocator::AllocationGroupToken;
use pin_project::pin_project;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::sleep,
};
use tokio_util::sync::PollSemaphore;
use tracing::{debug, trace};

use super::{Clearable, ObjectPool, PoolMetrics, Poolable, ReclaimStrategy};

const SHRINKER_SLEEP_DURATION: Duration = Duration::from_secs(1);

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
/// # Missing
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
    on_demand_allocs: AtomicUsize,
    min_capacity: usize,
    max_capacity: usize,
    alloc_group: AllocationGroupToken,
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
            on_demand_allocs: AtomicUsize::new(0),
            min_capacity,
            max_capacity,
            alloc_group: AllocationGroupToken::current(),
            metrics,
        }
    }

    fn acquire_item(&self, permit: OwnedSemaphorePermit) -> T::Data {
        permit.forget();

        let data = { self.items.lock().unwrap().pop_back().unwrap() };

        self.metrics.acquired().increment(1);
        self.metrics.in_use().increment(1.0);

        data
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

/// A [`Future`] that acquires an item from an [`ElasticObjectPool`].
#[pin_project]
pub struct ElasticAcquireFuture<T: Poolable> {
    strategy: Option<Arc<ElasticStrategy<T>>>,
    waiting_slow: bool,
    semaphore: PollSemaphore,
}

impl<T: Poolable> ElasticAcquireFuture<T> {
    fn new(strategy: Arc<ElasticStrategy<T>>) -> Self {
        let semaphore = PollSemaphore::new(Arc::clone(&strategy.available));
        Self {
            strategy: Some(strategy),
            waiting_slow: false,
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
        let strategy = this.strategy.take().unwrap();

        // If we're not waiting from previously going down the slow path, we'll try to acquire a permit in a non-polling
        // fashion, which avoids scheduling a wakeup for this task if we end up trying to allocate an item on demand.
        //
        // If we _are_ waiting from previously going down the slow path, we'll always poll the semaphore to ensure we
        // don't throw away a permit that was given to us while we were waiting.
        while !*this.waiting_slow {
            // Fast path: try to acquire a permit immediately.
            //
            // If we get one, we can acquire an item from the pool. Otherwise, we'll attempt to do a burst allocation if
            // the pool hasn't yet reached its maximum capacity.
            if let Ok(permit) = strategy.available.clone().try_acquire_owned() {
                trace!("Acquired permit on fast path. Acquiring item from pool.");

                let data = strategy.acquire_item(permit);

                return Poll::Ready(T::from_data(strategy, data));
            }

            trace!("No available permits. Attempting to allocate on demand.");

            // If we're at capacity, we can't allocate any more items.
            let active = strategy.active.load(Acquire);
            if active == strategy.max_capacity {
                trace!("Pool at capacity. Falling back to waiting for next available permit.");

                *this.waiting_slow = true;
                break;
            }

            // Try to atomically increment `active` which signals that we still have capacity to allocate another
            // item, and more importantly, that _we_ are authorized to do so.
            if strategy
                .active
                .compare_exchange_weak(active, active + 1, AcqRel, Relaxed)
                .is_err()
            {
                continue;
            }

            trace!("Updated active count. Allocating on demand.");

            let new_item = {
                let _entered = strategy.alloc_group.enter();
                (strategy.builder)()
            };

            strategy.on_demand_allocs.fetch_add(1, Relaxed);
            strategy.metrics.created().increment(1);
            strategy.metrics.capacity().increment(1.0);
            strategy.metrics.in_use().increment(1.0);

            return Poll::Ready(T::from_data(strategy, new_item));
        }

        trace!("Waiting for next available permit.");
        match this.semaphore.poll_acquire(cx) {
            Poll::Ready(Some(permit)) => {
                trace!("Acquired permit. Acquiring item from pool.");

                let data = strategy.acquire_item(permit);

                Poll::Ready(T::from_data(strategy, data))
            }
            Poll::Ready(None) => unreachable!("semaphore should never be closed"),
            Poll::Pending => {
                trace!("Permit not yet available. Waiting for next available permit.");

                this.strategy.replace(strategy);
                Poll::Pending
            }
        }
    }
}

async fn run_background_shrinker<T: Poolable>(strategy: Arc<ElasticStrategy<T>>) {
    // The shrinker continuously tracks the "demand" for items in the pool, and attempts to shrink the pool size such
    // that it stays at the smallest possible size while minimizing the amount of on-demand allocations seen.
    //
    // Every time the shrinker runs, it consumes the current value of `fallback_count`, which gives us a delta of the
    // number of on-demand allocations that have occurred since the last time the shrinker ran, or simply "demand". We
    // track a rolling average of this demand, and if the average demand is less than N, where N is configurable, then
    // we consider the pool eligible to shrink.
    //
    // We only shrink the pool down to its minimum capacity, even if the average demand is less than N. When the pool is
    // eligible to shrink and not yet at the minimum capacity, we remove a single item from the pool per iteration.

    // We use an alpha of 0.1, which provides a fairly strong smoothing effect.
    let mut average_demand = Ewma::new(0.1);
    let min_capacity = strategy.min_capacity;

    loop {
        debug!("Shrinker sleeping.");
        sleep(SHRINKER_SLEEP_DURATION).await;

        // Track the number of available permits, and the number of active items.
        //
        // When we have available permits, and our active count is greater than the minimum capacity, we'll take an item
        // from the pool.
        let active = strategy.active.load(Relaxed);
        if active <= min_capacity {
            debug!("Object pool already at minimum capacity. Nothing to do.");
            continue;
        }

        let delta_demand = strategy.on_demand_allocs.swap(0, Relaxed);
        average_demand.update(delta_demand as f64);
        if average_demand.value() < 1.0 {
            debug!(
                avg_demand = average_demand.value(),
                active, min_capacity, "Pool qualifies for shrinking. Attempting to remove single item..."
            );

            // Try acquiring a permit from the pool, giving us permission to remove an item.
            let permit = strategy
                .available
                .acquire()
                .await
                .expect("semaphore should never be closed");

            // Lock the pool and remove an item, taking care to update the active count while holding the lock.
            let item = {
                let item = strategy.items.lock().unwrap().pop_back().unwrap();
                strategy.active.fetch_sub(1, AcqRel);
                item
            };

            // Drop the item itself, and update our metrics.
            drop(item);
            strategy.metrics.deleted().increment(1);
            strategy.metrics.capacity().decrement(1.0);

            // Forget the permit so that we shrink the overall number of permits attached to the semaphore.
            permit.forget();

            debug!("Shrinker successfully removed an item from the pool.");
        } else {
            debug!(
                avg_demand = average_demand.value(),
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

#[cfg(test)]
mod tests {
    use tokio_test::{assert_pending, assert_ready, task::spawn};

    use super::ElasticObjectPool;
    use crate::{pooled, pooling::ObjectPool as _};

    pooled! {
        struct TestObject {
            value: u32,
        }

        clear => |this| this.value = 0
    }

    impl std::fmt::Debug for TestObject {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TestObject").finish_non_exhaustive()
        }
    }

    #[test]
    fn basic() {
        let (pool, _) = ElasticObjectPool::<TestObject>::with_capacity("test", 1, 2);
        assert_eq!(pool.strategy.available.available_permits(), 1);

        let mut acquire = spawn(pool.acquire());
        let item = assert_ready!(acquire.poll());
        assert_eq!(pool.strategy.available.available_permits(), 0);

        drop(item);
        assert_eq!(pool.strategy.available.available_permits(), 1);
    }

    #[test]
    fn burst_allocation() {
        let (pool, _) = ElasticObjectPool::<TestObject>::with_capacity("test", 1, 2);
        assert_eq!(pool.strategy.available.available_permits(), 1);

        // Acquire the first item, which should already exist.
        let mut first_acquire = spawn(pool.acquire());
        let first_item = assert_ready!(first_acquire.poll());
        assert_eq!(pool.strategy.available.available_permits(), 0);

        // Acquire a second item, which should be allocated on demand since we haven't reached our maximum capacity yet.
        let mut second_acquire = spawn(pool.acquire());
        let second_item = assert_ready!(second_acquire.poll());
        assert_eq!(pool.strategy.available.available_permits(), 0);

        // Try to acquire a third item, which should block because our pool has reached its maximum capacity.
        let mut third_acquire = spawn(pool.acquire());
        assert_pending!(third_acquire.poll());
        assert!(!third_acquire.is_woken());

        // Drop the items to return them to the pool and observe that at least one makes it back, but the semaphore will
        // divert the returned permit for the first item to the pending third acquire.
        drop(first_item);
        assert_eq!(pool.strategy.available.available_permits(), 0);
        drop(second_item);
        assert_eq!(pool.strategy.available.available_permits(), 1);

        // Now our third acquire should have been notified and we should be able to acquire an item.
        assert!(third_acquire.is_woken());
        let third_item = assert_ready!(third_acquire.poll());
        assert_eq!(pool.strategy.available.available_permits(), 1);

        drop(third_item);
        assert_eq!(pool.strategy.available.available_permits(), 2);
    }
}
