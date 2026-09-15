use async_trait::async_trait;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_error::GenericError;

use crate::HostProvider;

/// Host provider based on a fixed hostname.
#[derive(Clone)]
pub struct FixedHostProvider {
    hostname: String,
}

impl FixedHostProvider {
    /// Creates a new `FixedHostProvider` that reports the given hostname.
    pub fn new(hostname: String) -> Self {
        Self { hostname }
    }
}

#[async_trait]
impl HostProvider for FixedHostProvider {
    type Error = GenericError;

    async fn get_hostname(&self) -> Result<String, Self::Error> {
        Ok(self.hostname.clone())
    }
}

impl MemoryBounds for FixedHostProvider {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<Self>("component struct");
    }
}
