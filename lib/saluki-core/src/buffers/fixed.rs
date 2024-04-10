use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use tokio::sync::Semaphore;

use super::{BufferPool, Bufferable, Clearable, Strategy};

pub struct FixedSizeBufferPool<T: Bufferable> {
    strategy: Arc<FixedSizeStrategy<T::Data>>,
}

impl<T: Bufferable> FixedSizeBufferPool<T>
where
    T::Data: Default,
{
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            strategy: Arc::new(FixedSizeStrategy::new(capacity)),
        }
    }
}

impl<T: Bufferable> FixedSizeBufferPool<T> {
    pub fn with_builder<B>(capacity: usize, builder: B) -> Self
    where
        B: Fn() -> T::Data,
    {
        Self {
            strategy: Arc::new(FixedSizeStrategy::with_builder(capacity, builder)),
        }
    }

    pub async fn acquire(&self) -> T {
        let data = self.strategy.acquire().await;
        let strategy_ref = Arc::clone(&self.strategy);
        T::from_data(strategy_ref, data)
    }
}

impl<T: Bufferable> Clone for FixedSizeBufferPool<T> {
    fn clone(&self) -> Self {
        Self {
            strategy: self.strategy.clone(),
        }
    }
}

#[async_trait]
impl<T: Bufferable> BufferPool for FixedSizeBufferPool<T> {
    type Buffer = T;

    async fn acquire(&self) -> Self::Buffer {
        self.acquire().await
    }
}

pub struct FixedSizeStrategy<T: Clearable> {
    items: Mutex<VecDeque<T>>,
    available: Semaphore,
}

impl<T: Clearable + Default> FixedSizeStrategy<T> {
    pub fn new(capacity: usize) -> Self {
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
    pub fn with_builder<B>(capacity: usize, builder: B) -> Self
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
