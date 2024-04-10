use std::sync::Arc;

mod fixed;
use async_trait::async_trait;

pub use self::fixed::FixedSizeBufferPool;

mod helpers;
pub use self::helpers::{buffered, buffered_newtype};

/// An item that can be cleared.
pub trait Clearable {
    fn clear(&mut self) {}
}

/// An item that is bufferable.
///
/// This meta-trait is used to mark a type as being bufferable, where the type itself wraps a piece of data
/// (Self::Data), and the data itself is the actual value which gets buffered and reused.
///
/// While somewhat incestuous, what this unlocks is the ability to use wrapping types to hide away the complexity of
/// ensuring that data which is given from a buffer pool is eventully returned and not lost. Otherwise, trying to do
/// something like using drop logic to send `T` back to a buffer pool would involve _replacing_ the value of `T` with a
/// new value, which is not always possible, at least not without incurring additional allocations, which negate the
/// point of using a buffer pool in the first place.
pub trait Bufferable {
    type Data: Clearable + Send + 'static;

    fn from_data(strategy: Arc<dyn Strategy<Self::Data> + Send + Sync>, data: Self::Data) -> Self;
}

#[async_trait]
pub trait Strategy<T>
where
    T: Clearable,
{
    async fn acquire(&self) -> T;
    fn reclaim(&self, data: T);
}

#[async_trait]
pub trait BufferPool {
    type Buffer;

    async fn acquire(&self) -> Self::Buffer;
}
