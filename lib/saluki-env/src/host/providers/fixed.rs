use async_trait::async_trait;
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config_tools::GenericConfiguration;
use saluki_error::GenericError;

use crate::HostProvider;

/// Host provider based on a fixed hostname.
#[derive(Clone)]
pub struct FixedHostProvider {
    hostname: String,
}

impl FixedHostProvider {
    /// Creates a new `FixedHostProvider` from a fixed hostname.
    pub fn from_hostname(hostname: impl Into<String>) -> Self {
        Self {
            hostname: hostname.into(),
        }
    }

    /// Creates a new `FixedHostProvider` from the given configuration.
    ///
    /// Depends on the hostname existing in the given configuration under the `hostname` key.
    ///
    /// # Errors
    ///
    /// If the hostname isn't specified in the configuration, an error is returned.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let hostname = config.get_typed::<String>("hostname")?;

        Ok(Self { hostname })
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
