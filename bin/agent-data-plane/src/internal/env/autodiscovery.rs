use std::time::Duration;

use async_trait::async_trait;
use datadog_agent_commons::ipc::client::RemoteAgentClient;
use futures::StreamExt;
use saluki_config::GenericConfiguration;
use saluki_env::autodiscovery::AutodiscoveryEvent;
use saluki_env::AutodiscoveryProvider;
use saluki_error::GenericError;
use tokio::sync::broadcast::{self, Receiver, Sender};
use tokio::sync::OnceCell;
use tracing::{debug, info, warn};

/// An autodiscovery provider that uses the Datadog Agent's internal gRPC API to receive autodiscovery updates.
pub struct RemoteAgentAutodiscoveryProvider {
    client: RemoteAgentClient,
    sender: Sender<AutodiscoveryEvent>,
    listener_init: OnceCell<()>,
}

impl RemoteAgentAutodiscoveryProvider {
    /// Creates a new `RemoteAgentAutodiscoveryProvider` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If the remote agent client could not be created, an error is returned.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let client = RemoteAgentClient::from_configuration(config).await?;
        let (sender, _) = broadcast::channel::<AutodiscoveryEvent>(16);

        Ok(Self {
            client,
            sender,
            listener_init: OnceCell::new(),
        })
    }

    async fn start_background_listener(&self) {
        debug!("Starting autodiscovery background listener.");

        let mut client = self.client.clone();
        let sender = self.sender.clone();

        tokio::spawn(async move {
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
                                "Encountered error while listening for autodiscovery events.  Retrying in 1 second..."
                            );
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            break;
                        }
                    }
                }
            }
        });
    }
}

#[async_trait]
impl AutodiscoveryProvider for RemoteAgentAutodiscoveryProvider {
    async fn subscribe(&self) -> Option<Receiver<AutodiscoveryEvent>> {
        self.listener_init
            .get_or_init(|| async {
                self.start_background_listener().await;
            })
            .await;

        Some(self.sender.subscribe())
    }
}
