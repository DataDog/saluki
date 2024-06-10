use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use tokio::sync::Semaphore;

use super::{Clearable, ObjectPool, Poolable, Strategy};

/// A fixed-size object pool.
///
/// All items for this object pool are created up front, and when the object pool is empty, calls to `acquire` will
/// block until an item is returned to the pool.
pub struct FixedSizeObjectPool<T: Poolable> {
    strategy: Arc<FixedSizeStrategy<T::Data>>,
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

#[async_trait]
impl<T: Poolable> ObjectPool for FixedSizeObjectPool<T> {
    type Item = T;

    async fn acquire(&self) -> Self::Item {
        let data = self.strategy.acquire().await;
        let strategy_ref = Arc::clone(&self.strategy);
        T::from_data(strategy_ref, data)
    }
}

struct FixedSizeStrategy<T: Clearable> {
    items: Mutex<VecDeque<T>>,
    available: Semaphore,
}

impl<T: Clearable + Default> FixedSizeStrategy<T> {
    fn new(capacity: usize) -> Self {
        let mut items = VecDeque::with_capacity(capacity);
        items.extend((0..capacity).map(|_| T::default()));
        let available = Semaphore::new(capacity);

        Self {
            items: Mutex::new(items),
            available,
        }
    }
}

impl<T: Clearable> FixedSizeStrategy<T> {
    fn with_builder<B>(capacity: usize, builder: B) -> Self
    where
        B: Fn() -> T,
    {
        let mut items = VecDeque::with_capacity(capacity);
        items.extend((0..capacity).map(|_| builder()));
        let available = Semaphore::new(capacity);

        Self {
            items: Mutex::new(items),
            available,
        }
    }
}

#[async_trait]
impl<T: Clearable + Send + 'static> Strategy<T> for FixedSizeStrategy<T> {
    async fn acquire(&self) -> T {
        self.available.acquire().await.unwrap().forget();
        self.items.lock().unwrap().pop_back().unwrap()
    }

    fn reclaim(&self, mut data: T) {
        data.clear();

        self.items.lock().unwrap().push_back(data);
        self.available.add_permits(1);
    }
}
