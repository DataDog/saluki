use std::sync::Arc;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_error::GenericError;

use crate::HostProvider;

/// A host provider that exposes memory bounds.
pub trait BoundedHostProvider: HostProvider<Error = GenericError> + MemoryBounds {}

impl<T> BoundedHostProvider for T where T: HostProvider<Error = GenericError> + MemoryBounds {}

/// A boxed host provider.
#[derive(Clone)]
pub struct BoxedHostProvider {
    inner: Arc<dyn BoundedHostProvider<Error = GenericError> + Send + Sync>,
}

impl BoxedHostProvider {
    /// Creates a new `BoxedHostProvider` from the given host provider.
    pub fn from_provider<P>(provider: P) -> Self
    where
        P: BoundedHostProvider + Send + Sync + 'static,
    {
        let inner = Arc::new(provider);
        Self { inner }
    }
}

#[async_trait]
impl HostProvider for BoxedHostProvider {
    type Error = GenericError;

    async fn get_hostname(&self) -> Result<String, Self::Error> {
        self.inner.get_hostname().await
    }
}

impl MemoryBounds for BoxedHostProvider {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        self.inner.specify_bounds(builder);
    }
}
