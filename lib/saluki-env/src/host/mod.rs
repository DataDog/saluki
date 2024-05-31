pub mod hostname;
pub mod providers;

use async_trait::async_trait;

#[async_trait]
pub trait HostProvider {
    type Error;

    async fn get_hostname(&self) -> Result<String, Self::Error>;
}
