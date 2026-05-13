use std::time::Duration;

use async_trait::async_trait;
use datadog_agent_commons::ipc::client::RemoteAgentClient;
use futures::StreamExt;
use saluki_config::GenericConfiguration;
use saluki_core::runtime::{InitializationError, ProcessShutdown, Supervisable, SupervisorFuture};
use saluki_env::autodiscovery::AutodiscoveryEvent;
use saluki_env::AutodiscoveryProvider;
use saluki_error::GenericError;
use tokio::select;
use tokio::sync::broadcast::{self, Receiver, Sender};
use tokio::time::sleep;
use tracing::{debug, info, warn};

/// An autodiscovery provider that uses the Datadog Agent's internal gRPC API to receive autodiscovery updates.
///
/// The provider only exposes a broadcast subscription. The gRPC stream that feeds it is driven by a separate
/// [`RemoteAgentAutodiscoveryListener`] worker so the work participates in the supervision tree. The listener is
/// returned alongside the provider from [`RemoteAgentAutodiscoveryProvider::from_configuration`].
#[derive(Clone)]
pub struct RemoteAgentAutodiscoveryProvider {
    sender: Sender<AutodiscoveryEvent>,
}

impl RemoteAgentAutodiscoveryProvider {
    /// Creates a new `RemoteAgentAutodiscoveryProvider` from the given configuration, along with the listener worker
    /// that drives the upstream gRPC stream.
    ///
    /// ## Errors
    ///
    /// If the remote agent client could not be created, an error is returned.
    pub async fn from_configuration(
        config: &GenericConfiguration,
    ) -> Result<(Self, RemoteAgentAutodiscoveryListener), GenericError> {
        let client = RemoteAgentClient::from_configuration(config).await?;
        let (sender, _) = broadcast::channel::<AutodiscoveryEvent>(16);

        let provider = Self { sender: sender.clone() };
        let listener = RemoteAgentAutodiscoveryListener { client, sender };

        Ok((provider, listener))
    }
}

#[async_trait]
impl AutodiscoveryProvider for RemoteAgentAutodiscoveryProvider {
    async fn subscribe(&self) -> Option<Receiver<AutodiscoveryEvent>> {
        Some(self.sender.subscribe())
    }
}

/// Supervised worker that reads autodiscovery events from the Datadog Agent's gRPC API and fans them out to all
/// subscribers of the paired [`RemoteAgentAutodiscoveryProvider`].
pub struct RemoteAgentAutodiscoveryListener {
    client: RemoteAgentClient,
    sender: Sender<AutodiscoveryEvent>,
}

#[async_trait]
impl Supervisable for RemoteAgentAutodiscoveryListener {
    fn name(&self) -> &str {
        "autodiscovery-listener"
    }

    async fn initialize(&self, mut process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        let client = self.client.clone();
        let sender = self.sender.clone();

        Ok(Box::pin(async move {
            select! {
                _ = process_shutdown.wait_for_shutdown() => {},
                _ = run_autodiscovery_listener(client, sender) => {},
            }
            Ok(())
        }))
    }
}

async fn run_autodiscovery_listener(mut client: RemoteAgentClient, sender: Sender<AutodiscoveryEvent>) {
    info!("Listening to autodiscovery events from remote agent.");

    loop {
        let mut autodiscovery_stream = client.get_autodiscovery_stream();

        debug!("Polling autodiscovery event stream.");

        while let Some(result) = autodiscovery_stream.next().await {
            match result {
                Ok(response) => {
                    for proto_config in response.configs {
                        let event = AutodiscoveryEvent::from(proto_config);
                        let _ = sender.send(event);
                    }
                }
                Err(status) => {
                    warn!(
                        ?status,
                        "Encountered error while listening for autodiscovery events. Retrying in 1 second...",
                    );
                    sleep(Duration::from_secs(1)).await;
                    break;
                }
            }
        }

        debug!("Autodiscovery event stream ended. Reconnecting...");
    }
}
