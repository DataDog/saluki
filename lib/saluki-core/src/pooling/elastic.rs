use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{
            AtomicBool, AtomicUsize,
            Ordering::{AcqRel, Acquire, Relaxed, Release},
        },
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::Duration,
};

use pin_project::pin_project;
use saluki_common::resource_tracking::ResourceGroupToken;
use tokio::{
    sync::{futures::OwnedNotified, Notify, OwnedSemaphorePermit, Semaphore, SemaphorePermit},
    time::{sleep, Instant},
};
use tokio_util::sync::PollSemaphore;
use tracing::{debug, trace};

use super::{Clearable, ObjectPool, PoolMetrics, Poolable, ReclaimStrategy};

const SHRINKER_SLEEP_DURATION: Duration = Duration::from_secs(1);
const SHRINK_GRACE_PERIOD: Duration = Duration::from_secs(5);
const SHRINK_BATCH_DIVISOR: usize = 4;

/// An elastic object pool.
///
/// Pools are configured with a minimum and maximum size, and allocate the minimum number of items up front. When an
/// item is requested and the pool is empty, but hasn't yet reached its maximum size, it will allocate the item on
/// demand.
///
/// After a grace period without growth, a background task shrinks idle capacity toward the minimum capacity. Shrinking
/// removes a fraction of the excess idle items each interval, and returned items above the minimum are dropped
/// immediately while shrinking is active. A new on-demand allocation disables shrinking for another grace period so
/// nearby bursts can reuse the existing capacity.
///
/// # Missing
///
/// - Configurability for the shrinking frequency, grace period, and batch fraction.
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
    shrinking_enabled: AtomicBool,
    shrink_state: Mutex<ShrinkState>,
    min_capacity: usize,
    max_capacity: usize,
    resource_group: ResourceGroupToken,
    metrics: PoolMetrics,
}

struct ShrinkState {
    last_growth: Instant,
}

impl<T: Poolable> ElasticStrategy<T> {
    fn with_builder<S, B>(pool_name: S, min_capacity: usize, max_capacity: usize, builder: B) -> Self
    where
        S: Into<String>,
        B: Fn() -> T::Data + Send + Sync + 'static,
    {
        assert!(
            min_capacity <= max_capacity,
            "minimum capacity must not exceed maximum capacity"
        );

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
            shrinking_enabled: AtomicBool::new(false),
            shrink_state: Mutex::new(ShrinkState {
                last_growth: Instant::now(),
            }),
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

    fn try_increase_active_for_growth(&self, active: usize) -> bool {
        let mut shrink_state = self.shrink_state.lock().unwrap();
        if self
            .active
            .compare_exchange_weak(active, active + 1, AcqRel, Relaxed)
            .is_err()
        {
            return false;
        }

        shrink_state.last_growth = Instant::now();
        self.shrinking_enabled.store(false, Release);
        true
    }

    fn enable_shrinking_if_grace_elapsed(&self) -> bool {
        let shrink_state = self.shrink_state.lock().unwrap();
        if shrink_state.last_growth.elapsed() < SHRINK_GRACE_PERIOD {
            return false;
        }

        self.shrinking_enabled.store(true, Release);
        true
    }

    fn try_decrease_active(&self) -> bool {
        self.active
            .fetch_update(AcqRel, Acquire, |active| {
                (active > self.min_capacity).then_some(active - 1)
            })
            .is_ok()
    }

    fn shrinking_is_current(&self, shrink_state: &ShrinkState) -> bool {
        self.shrinking_enabled.load(Acquire) && shrink_state.last_growth.elapsed() >= SHRINK_GRACE_PERIOD
    }

    fn excess_idle_capacity(&self) -> usize {
        let idle_excess = self.available.available_permits().saturating_sub(self.min_capacity);
        let capacity_excess = self.active.load(Acquire).saturating_sub(self.min_capacity);

        idle_excess.min(capacity_excess)
    }

    fn record_deletion(&self) {
        self.metrics.deleted().increment(1);
        self.metrics.capacity().decrement(1.0);
        self.active_decreased.notify_waiters();
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

        self.metrics.released().increment(1);
        self.metrics.in_use().decrement(1.0);

        if self.shrinking_enabled.load(Acquire) {
            let shrink_state = self.shrink_state.lock().unwrap();
            if self.shrinking_is_current(&shrink_state)
                && self.available.available_permits() >= self.min_capacity
                && self.try_decrease_active()
            {
                drop(shrink_state);
                drop(data);
                self.record_deletion();
                trace!("Dropped returned item above the minimum capacity.");
                return;
            }
        }

        self.items.lock().unwrap().push_back(data);
        self.available.add_permits(1);
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

                // Claim capacity and record growth under the same lock that guards shrink completion.
                if !strategy.try_increase_active_for_growth(active) {
                    continue;
                }

                trace!("Updated active count. Allocating on demand.");

                let new_item = {
                    let _entered = strategy.resource_group.enter();
                    (strategy.builder)()
                };

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
    loop {
        debug!("Shrinker sleeping.");
        sleep(SHRINKER_SLEEP_DURATION).await;

        if !strategy.enable_shrinking_if_grace_elapsed() {
            debug!("Object pool is within the post-growth grace period. Skipping shrinking.");
            continue;
        }

        let idle_excess = strategy.excess_idle_capacity();
        if idle_excess == 0 {
            debug!("Object pool has no excess idle capacity. Nothing to shrink.");
            continue;
        }

        let batch_size = idle_excess.div_ceil(SHRINK_BATCH_DIVISOR);
        let removed = try_shrink_available_items(&strategy, batch_size);
        debug!(
            idle_excess,
            batch_size, removed, "Shrank excess idle object pool capacity."
        );
    }
}

fn try_shrink_available_items<T: Poolable>(strategy: &ElasticStrategy<T>, count: usize) -> usize {
    (0..count)
        .take_while(|_| try_shrink_one_available_item(strategy))
        .count()
}

fn try_shrink_one_available_item<T: Poolable>(strategy: &ElasticStrategy<T>) -> bool {
    if strategy.excess_idle_capacity() == 0 {
        return false;
    }

    // Only shrink idle pool capacity. Waiting here can let the shrinker consume the next returned item ahead of
    // application waiters that are already blocked on the same semaphore.
    let Ok(permit) = strategy.available.try_acquire() else {
        debug!("Pool has excess idle capacity, but no idle item is available.");
        return false;
    };

    try_shrink_available_item(strategy, permit)
}

fn try_shrink_available_item<T: Poolable>(strategy: &ElasticStrategy<T>, permit: SemaphorePermit<'_>) -> bool {
    let shrink_state = strategy.shrink_state.lock().unwrap();
    if !strategy.shrinking_is_current(&shrink_state) {
        return false;
    }

    let mut items = strategy.items.lock().unwrap();
    if !strategy.try_decrease_active() {
        return false;
    }
    let item = items.pop_back().unwrap();
    drop(items);
    drop(shrink_state);
    drop(item);

    permit.forget();
    strategy.record_deletion();
    true
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering::{Acquire, Release};

    use tokio_test::{assert_pending, assert_ready, task::spawn};

    use super::{
        try_shrink_available_item, try_shrink_one_available_item, ElasticObjectPool, SHRINKER_SLEEP_DURATION,
        SHRINK_GRACE_PERIOD,
    };
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

    fn make_shrink_eligible(pool: &ElasticObjectPool<TestObject>) {
        pool.strategy.shrink_state.lock().unwrap().last_growth = tokio::time::Instant::now() - SHRINK_GRACE_PERIOD;
        pool.strategy.shrinking_enabled.store(true, Release);
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
        make_shrink_eligible(&pool);

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
        assert!(try_shrink_available_item(&pool.strategy, permit));
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

    #[test]
    fn eligible_pool_drops_returned_items_above_minimum_capacity() {
        let (pool, _) = ElasticObjectPool::<TestObject>::with_builder("test", 1, 4, || TestObjectInner { value: 0 });

        let mut items = Vec::new();
        for _ in 0..4 {
            let mut acquire = spawn(pool.acquire());
            items.push(assert_ready!(acquire.poll()));
        }
        make_shrink_eligible(&pool);

        drop(items);

        assert_eq!(pool.strategy.active.load(Acquire), 1);
        assert_eq!(pool.strategy.available.available_permits(), 1);
        assert_eq!(pool.strategy.items.lock().unwrap().len(), 1);
    }

    #[test]
    fn fresh_growth_cancels_an_in_flight_shrink() {
        let (pool, _) = ElasticObjectPool::<TestObject>::with_builder("test", 1, 4, || TestObjectInner { value: 0 });

        let mut first_acquire = spawn(pool.acquire());
        let first_item = assert_ready!(first_acquire.poll());
        let mut second_acquire = spawn(pool.acquire());
        let second_item = assert_ready!(second_acquire.poll());
        let mut third_acquire = spawn(pool.acquire());
        let third_item = assert_ready!(third_acquire.poll());
        drop(second_item);
        drop(third_item);
        make_shrink_eligible(&pool);

        let shrink_permit = pool
            .strategy
            .available
            .try_acquire()
            .expect("an idle item should be available to the shrinker");
        let mut fourth_acquire = spawn(pool.acquire());
        let fourth_item = assert_ready!(fourth_acquire.poll());
        let mut fifth_acquire = spawn(pool.acquire());
        let fifth_item = assert_ready!(fifth_acquire.poll());

        assert!(!pool.strategy.shrinking_enabled.load(Acquire));
        assert!(
            !try_shrink_available_item(&pool.strategy, shrink_permit),
            "growth must cancel shrink work that sampled an older grace period"
        );
        assert_eq!(pool.strategy.active.load(Acquire), 4);
        assert_eq!(pool.strategy.available.available_permits(), 1);

        drop(first_item);
        drop(fourth_item);
        drop(fifth_item);
    }

    #[tokio::test(start_paused = true)]
    async fn background_shrinker_honors_grace_and_shrinks_idle_excess_in_batches() {
        let (pool, shrinker) =
            ElasticObjectPool::<TestObject>::with_builder("test", 1, 10, || TestObjectInner { value: 0 });

        let mut items = Vec::new();
        for _ in 0..10 {
            let mut acquire = spawn(pool.acquire());
            items.push(assert_ready!(acquire.poll()));
        }
        assert_eq!(pool.strategy.active.load(Acquire), 10);
        drop(items);
        assert_eq!(pool.strategy.available.available_permits(), 10);

        let mut shrinker = spawn(shrinker);
        assert_pending!(shrinker.poll());

        tokio::time::advance(SHRINK_GRACE_PERIOD - SHRINKER_SLEEP_DURATION).await;
        assert_pending!(shrinker.poll());
        assert_eq!(pool.strategy.active.load(Acquire), 10);

        tokio::time::advance(SHRINKER_SLEEP_DURATION).await;
        assert_pending!(shrinker.poll());
        assert_eq!(pool.strategy.active.load(Acquire), 7);
        assert_eq!(pool.strategy.available.available_permits(), 7);

        for _ in 0..8 {
            tokio::time::advance(SHRINKER_SLEEP_DURATION).await;
            assert_pending!(shrinker.poll());
        }
        assert_eq!(pool.strategy.active.load(Acquire), 1);
        assert_eq!(pool.strategy.available.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn on_demand_growth_restarts_shrink_grace_period() {
        let (pool, shrinker) =
            ElasticObjectPool::<TestObject>::with_builder("test", 1, 3, || TestObjectInner { value: 0 });
        let mut shrinker = spawn(shrinker);
        assert_pending!(shrinker.poll());

        tokio::time::advance(SHRINK_GRACE_PERIOD).await;
        assert_pending!(shrinker.poll());
        assert!(pool.strategy.shrinking_enabled.load(Acquire));

        let mut first_acquire = spawn(pool.acquire());
        let first_item = assert_ready!(first_acquire.poll());
        let mut second_acquire = spawn(pool.acquire());
        let second_item = assert_ready!(second_acquire.poll());
        assert!(!pool.strategy.shrinking_enabled.load(Acquire));
        drop(first_item);
        drop(second_item);
        assert_eq!(pool.strategy.active.load(Acquire), 2);

        tokio::time::advance(SHRINK_GRACE_PERIOD - SHRINKER_SLEEP_DURATION).await;
        assert_pending!(shrinker.poll());
        assert_eq!(pool.strategy.active.load(Acquire), 2);

        tokio::time::advance(SHRINKER_SLEEP_DURATION).await;
        assert_pending!(shrinker.poll());
        assert_eq!(pool.strategy.active.load(Acquire), 1);
    }
}
