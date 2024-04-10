#[cfg(feature = "agent-like")]
pub mod agent;
pub mod hostname;

use async_trait::async_trait;

#[async_trait]
pub trait HostProvider {
    type Error;

    async fn get_hostname(&self) -> Result<String, Self::Error>;
}
