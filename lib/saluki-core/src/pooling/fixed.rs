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

use super::{Clearable, ObjectPool, Poolable, ReclaimStrategy};

/// A fixed-size object pool.
///
/// All items for this object pool are created up front, and when the object pool is empty, calls to `acquire` will
/// block until an item is returned to the pool.
pub struct FixedSizeObjectPool<T: Poolable> {
    strategy: Arc<FixedSizeStrategy<T>>,
}

impl<T: Poolable> FixedSizeObjectPool<T>
where
    T::Data: Default,
{
    /// Creates a new `FixedSizeObjectPool` with the given capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            strategy: Arc::new(FixedSizeStrategy::new(capacity)),
        }
    }
}

impl<T: Poolable> FixedSizeObjectPool<T> {
    /// Creates a new `FixedSizeObjectPool` with the given capacity and item builder.
    ///
    /// `builder` is called to construct each item.
    pub fn with_builder<B>(capacity: usize, builder: B) -> Self
    where
        B: Fn() -> T::Data,
    {
        Self {
            strategy: Arc::new(FixedSizeStrategy::with_builder(capacity, builder)),
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
}

impl<T> FixedSizeStrategy<T>
where
    T: Poolable,
    T::Data: Default,
{
    fn new(capacity: usize) -> Self {
        let mut items = VecDeque::with_capacity(capacity);
        items.extend((0..capacity).map(|_| T::Data::default()));
        let available = Arc::new(Semaphore::new(capacity));

        Self {
            items: Mutex::new(items),
            available,
        }
    }
}

impl<T: Poolable> FixedSizeStrategy<T> {
    fn with_builder<B>(capacity: usize, builder: B) -> Self
    where
        B: Fn() -> T::Data,
    {
        let mut items = VecDeque::with_capacity(capacity);
        items.extend((0..capacity).map(|_| builder()));
        let available = Arc::new(Semaphore::new(capacity));

        Self {
            items: Mutex::new(items),
            available,
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
                let data = strategy.items.lock().unwrap().pop_front().unwrap();
                Poll::Ready(T::from_data(strategy, data))
            }
            None => unreachable!("semaphore should never be closed"),
        }
    }
}
