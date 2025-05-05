use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::broadcast::{self, Receiver, Sender};
use tokio::sync::OnceCell;
use tracing::{debug, info, warn};

use crate::autodiscovery::{AutodiscoveryEvent, AutodiscoveryProvider};
use crate::helpers::remote_agent::RemoteAgentClient;

/// An autodiscovery provider that uses the Datadog Agent's internal gRPC API to receive autodiscovery updates.
pub struct RemoteAgentAutodiscoveryProvider {
    client: RemoteAgentClient,
    sender: Sender<AutodiscoveryEvent>,
    listener_init: OnceCell<()>,
}

impl RemoteAgentAutodiscoveryProvider {
    /// Creates a new `RemoteAgentAutodiscoveryProvider` that uses the remote client to receive autodiscovery updates.
    pub fn new(client: RemoteAgentClient) -> Self {
        let (sender, _) = broadcast::channel::<AutodiscoveryEvent>(super::AD_STREAM_CAPACITY);

        Self {
            client,
            sender,
            listener_init: OnceCell::new(),
        }
    }

    async fn start_background_listener(&self) {
        debug!("Starting autodiscovery background listener.");

        let mut client = self.client.clone();
        let sender = self.sender.clone();

        tokio::spawn(async move {
            info!("Listening to autodiscovery events from remote agent.");

            loop {
                let mut autodiscovery_stream = client.get_autodiscovery_stream();

                while let Some(result) = autodiscovery_stream.next().await {
                    match result {
                        Ok(response) => {
                            for proto_config in response.configs {
                                let event = AutodiscoveryEvent::from(proto_config);
                                match sender.send(event) {
                                    Ok(_) => (),
                                    Err(e) => {
                                        warn!("Failed to send autodiscovery event: {}", e);
                                    }
                                }
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
    async fn subscribe(&self) -> Receiver<AutodiscoveryEvent> {
        self.listener_init
            .get_or_init(|| async {
                self.start_background_listener().await;
            })
            .await;

        self.sender.subscribe()
    }
}
