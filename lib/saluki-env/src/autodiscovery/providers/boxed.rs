use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::broadcast::Receiver;

use crate::autodiscovery::{AutodiscoveryEvent, AutodiscoveryProvider};

/// A boxed autodiscovery provider.
#[derive(Clone)]
pub struct BoxedAutodiscoveryProvider {
    inner: Arc<dyn AutodiscoveryProvider + Send + Sync>,
}

impl BoxedAutodiscoveryProvider {
    /// Creates a new `BoxedAutodiscoveryProvider` from the given autodiscovery provider.
    pub fn from_provider<P>(provider: P) -> Self
    where
        P: AutodiscoveryProvider + Send + Sync + 'static,
    {
        let inner = Arc::new(provider);
        Self { inner }
    }
}

#[async_trait]
impl AutodiscoveryProvider for BoxedAutodiscoveryProvider {
    async fn subscribe(&self) -> Receiver<AutodiscoveryEvent> {
        self.inner.subscribe().await
    }
}
