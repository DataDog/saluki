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

use pin_project::pin_project;
use resource_accounting::ResourceGroupToken;
use tokio::{
    sync::{futures::OwnedNotified, Notify, OwnedSemaphorePermit, Semaphore, SemaphorePermit},
    time::sleep,
};
use tokio_util::sync::PollSemaphore;
use tracing::{debug, trace};

use super::{Clearable, ObjectPool, PoolMetrics, Poolable, ReclaimStrategy};

const SHRINKER_SLEEP_DURATION: Duration = Duration::from_secs(1);

/// An elastic object pool.
///
/// Pools are configured with a minimum and maximum size, and allocate the minimum number of items up front. When an
/// item is requested and the pool is empty, but hasn't yet reached its maximum size, it will allocate the item on
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
    active_decreased: Arc<Notify>,
    active: AtomicUsize,
    on_demand_allocs: AtomicUsize,
    min_capacity: usize,
    max_capacity: usize,
    resource_group: ResourceGroupToken,
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
            active_decreased: Arc::new(Notify::new()),
            active: AtomicUsize::new(min_capacity),
            on_demand_allocs: AtomicUsize::new(0),
            min_capacity,
            max_capacity,
            resource_group: ResourceGroupToken::current(),
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
    #[pin]
    active_decreased: OwnedNotified,
}

impl<T: Poolable> ElasticAcquireFuture<T> {
    fn new(strategy: Arc<ElasticStrategy<T>>) -> Self {
        let semaphore = PollSemaphore::new(Arc::clone(&strategy.available));
        let active_decreased = strategy.active_decreased.clone().notified_owned();
        Self {
            strategy: Some(strategy),
            waiting_slow: false,
            semaphore,
            active_decreased,
        }
    }
}

impl<T> Future for ElasticAcquireFuture<T>
where
    T: Poolable + 'static,
{
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let strategy = this.strategy.take().unwrap();

        loop {
            // If we're not waiting from previously going down the slow path, we'll try to acquire a permit in a
            // non-polling fashion, which avoids scheduling a wakeup for this task if we end up trying to allocate an
            // item on demand.
            //
            // If we are waiting from previously going down the slow path, we'll always poll the semaphore to ensure
            // we don't throw away a permit that was given to us while we were waiting.
            while !*this.waiting_slow {
                // Fast path: try to acquire a permit immediately.
                //
                // If we get one, we can acquire an item from the pool. Otherwise, we'll attempt to do a burst
                // allocation if the pool hasn't yet reached its maximum capacity.
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
                    let _entered = strategy.resource_group.enter();
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

                    return Poll::Ready(T::from_data(strategy, data));
                }
                Poll::Ready(None) => {
                    saluki_antithesis::unreachable!("elastic object pool semaphore closed");
                    unreachable!("semaphore should never be closed")
                }
                Poll::Pending => {
                    trace!("Permit not yet available. Waiting for next available permit.");
                }
            }

            match this.active_decreased.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    trace!("Active count decreased. Retrying acquisition.");

                    // Shrinking lowered `active`, so retry the fast/on-demand path where this waiter may claim the
                    // newly available capacity and allocate a replacement item.
                    this.active_decreased
                        .set(strategy.active_decreased.clone().notified_owned());
                    *this.semaphore = PollSemaphore::new(Arc::clone(&strategy.available));
                    *this.waiting_slow = false;
                }
                Poll::Pending => {
                    this.strategy.replace(strategy);
                    return Poll::Pending;
                }
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

            try_shrink_one_available_item(&strategy);
        } else {
            debug!(
                avg_demand = average_demand.value(),
                active, min_capacity, "Pool does not qualify for shrinking."
            );
        }
    }
}

fn try_shrink_one_available_item<T: Poolable>(strategy: &ElasticStrategy<T>) -> bool {
    // Only shrink idle pool capacity. Waiting here can let the shrinker consume the next returned item ahead of
    // application waiters that are already blocked on the same semaphore.
    let Ok(permit) = strategy.available.try_acquire() else {
        debug!("Pool qualifies for shrinking, but no idle item is available.");
        return false;
    };

    // Keep shrink completion separate so tests can reproduce interleavings after the shrinker takes a permit.
    shrink_available_item(strategy, permit);
    true
}

fn shrink_available_item<T: Poolable>(strategy: &ElasticStrategy<T>, permit: SemaphorePermit<'_>) {
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

    strategy.active_decreased.notify_waiters();

    debug!("Shrinker successfully removed an item from the pool.");
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
    use std::sync::atomic::Ordering::Acquire;

    use tokio_test::{assert_pending, assert_ready, task::spawn};

    use super::{shrink_available_item, try_shrink_one_available_item, ElasticObjectPool};
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

    #[test]
    fn shrinker_does_not_wait_for_returned_items() {
        let (pool, _) = ElasticObjectPool::<TestObject>::with_capacity("test", 1, 2);

        let mut first_acquire = spawn(pool.acquire());
        let first_item = assert_ready!(first_acquire.poll());

        let mut second_acquire = spawn(pool.acquire());
        let second_item = assert_ready!(second_acquire.poll());

        let mut third_acquire = spawn(pool.acquire());
        assert_pending!(third_acquire.poll());
        assert!(!third_acquire.is_woken());

        assert!(!try_shrink_one_available_item(&pool.strategy));

        drop(first_item);
        assert!(third_acquire.is_woken());

        let third_item = assert_ready!(third_acquire.poll());
        assert_eq!(pool.strategy.active.load(Acquire), 2);

        drop(second_item);
        drop(third_item);
    }

    #[test]
    fn slow_waiter_retries_allocation_when_shrink_reduces_active() {
        let (pool, _) = ElasticObjectPool::<TestObject>::with_capacity("test", 1, 2);

        // Fill the pool to its maximum size: active = 2, permits = 0.
        let mut first_acquire = spawn(pool.acquire());
        let first_item = assert_ready!(first_acquire.poll());

        let mut second_acquire = spawn(pool.acquire());
        let second_item = assert_ready!(second_acquire.poll());

        // Return one item while the pool is still at max capacity: active = 2, permits = 1.
        drop(second_item);
        assert_eq!(pool.strategy.active.load(Acquire), 2);
        assert_eq!(pool.strategy.available.available_permits(), 1);

        // Simulate the shrinker winning the race to the idle permit before a new acquire starts waiting:
        // active = 2, permits = 0, shrinker holds the permit.
        let permit = pool
            .strategy
            .available
            .try_acquire()
            .expect("returned item should leave one idle permit for the shrinker");
        assert_eq!(pool.strategy.available.available_permits(), 0);

        // The new acquire sees active == max_capacity and permits = 0, so it waits on the semaphore.
        let mut third_acquire = spawn(pool.acquire());
        assert_pending!(third_acquire.poll());
        assert!(!third_acquire.is_woken());

        // Shrinking removes the idle item and lowers active: active = 1, permits = 0.
        shrink_available_item(&pool.strategy, permit);
        assert_eq!(pool.strategy.active.load(Acquire), 1);
        assert_eq!(pool.strategy.available.available_permits(), 0);

        // The waiter must be woken by the active count change, otherwise it remains asleep even though
        // active < max_capacity means it could allocate a replacement item.
        assert!(
            third_acquire.is_woken(),
            "slow-path waiters must wake when shrinking creates on-demand allocation capacity"
        );

        let third_item = assert_ready!(third_acquire.poll());
        assert_eq!(pool.strategy.active.load(Acquire), 2);

        drop(first_item);
        drop(third_item);
    }
}
