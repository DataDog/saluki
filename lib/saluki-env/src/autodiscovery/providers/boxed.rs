use std::sync::Arc;

use async_trait::async_trait;
use saluki_error::GenericError;
use tokio::sync::{mpsc, Mutex};

use crate::autodiscovery::{AutoDiscoveryProvider, AutodiscoveryEvent};

/// A boxed autodiscovery provider.
#[derive(Clone)]
pub struct BoxedAutodiscoveryProvider {
    inner: Arc<Mutex<dyn AutoDiscoveryProvider<Error = GenericError> + Send + Sync>>,
}

impl BoxedAutodiscoveryProvider {
    /// Creates a new `BoxedAutodiscoveryProvider` from the given host provider.
    pub fn from_provider<P>(provider: P) -> Self
    where
        P: AutoDiscoveryProvider<Error = GenericError> + Send + Sync + 'static,
    {
        let inner = Arc::new(Mutex::new(provider));
        Self { inner }
    }
}

#[async_trait]
impl AutoDiscoveryProvider for BoxedAutodiscoveryProvider {
    type Error = GenericError;

    async fn subscribe(&mut self, sender: mpsc::Sender<AutodiscoveryEvent>) -> Result<(), Self::Error> {
        let mut provider = self.inner.lock().await;

        // For the boxed provider, we just pass through to the inner provider,
        // which should handle duplicate prevention if needed
        provider.subscribe(sender).await
    }
}
