use std::sync::Arc;

use async_trait::async_trait;
use saluki_error::GenericError;
use tokio::sync::broadcast::Receiver;

use crate::autodiscovery::{AutodiscoveryEvent, AutodiscoveryProvider};

/// A boxed autodiscovery provider.
#[derive(Clone)]
pub struct BoxedAutodiscoveryProvider {
    inner: Arc<dyn AutodiscoveryProvider<Error = GenericError> + Send + Sync>,
}

impl BoxedAutodiscoveryProvider {
    /// Creates a new `BoxedAutodiscoveryProvider` from the given autodiscovery provider.
    pub fn from_provider<P>(provider: P) -> Self
    where
        P: AutodiscoveryProvider<Error = GenericError> + Send + Sync + 'static,
    {
        let inner = Arc::new(provider);
        Self { inner }
    }
}

#[async_trait]
impl AutodiscoveryProvider for BoxedAutodiscoveryProvider {
    type Error = GenericError;

    async fn subscribe(&self) -> Receiver<AutodiscoveryEvent> {
        self.inner.subscribe().await
    }
}
