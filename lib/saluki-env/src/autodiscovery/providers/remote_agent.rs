use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use saluki_error::GenericError;
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::{debug, error, info, warn};

use crate::autodiscovery::{AutodiscoveryEvent, AutodiscoveryProvider, Config};
use crate::helpers::remote_agent::RemoteAgentClient;

/// An autodiscovery provider that uses the Datadog Agent's internal gRPC API to receive autodiscovery updates.
pub struct RemoteAgentAutoDiscoveryProvider {
    client: RemoteAgentClient,
    event_senders: Arc<Mutex<Vec<mpsc::Sender<AutodiscoveryEvent>>>>,
    listener_started: bool,
}

impl RemoteAgentAutoDiscoveryProvider {
    /// Creates a new `RemoteAgentAutoDiscoveryProvider` that uses the remote client to receive autodiscovery updates.
    pub fn new(client: RemoteAgentClient) -> Self {
        Self {
            client,
            event_senders: Arc::new(Mutex::new(Vec::new())),
            listener_started: false,
        }
    }

    async fn start_background_listener(&mut self) -> Result<(), GenericError> {
        if self.event_senders.lock().unwrap().is_empty() {
            return Err(GenericError::msg(
                "Cannot start background listener without subscribers",
            ));
        }

        if self.listener_started {
            return Ok(());
        }

        debug!("Starting autodiscovery background listener");

        let mut client = self.client.clone();
        let event_senders = self.event_senders.clone();

        tokio::spawn(async move {
            let mut autodiscovery_stream = client.get_autodiscovery_stream();

            info!("Autodiscovery stream established");

            while let Some(result) = autodiscovery_stream.next().await {
                match result {
                    Ok(response) => {
                        for proto_config in response.configs {
                            let config = Config::from(proto_config.clone());
                            let event = AutodiscoveryEvent { config };

                            broadcast_event(&event_senders, event).await;
                        }
                    }
                    Err(status) => {
                        error!("Error from autodiscovery stream: {}", status);
                        break;
                    }
                }
            }

            warn!("Autodiscovery stream closed");
        });

        self.listener_started = true;
        Ok(())
    }
}

async fn broadcast_event(senders: &Arc<Mutex<Vec<mpsc::Sender<AutodiscoveryEvent>>>>, event: AutodiscoveryEvent) {
    let mut to_remove = Vec::new();

    {
        let senders_guard = senders.lock().unwrap();
        for (idx, sender) in senders_guard.iter().enumerate() {
            match sender.try_send(event.clone()) {
                Ok(_) => {}
                Err(TrySendError::Closed(_)) => {
                    to_remove.push(idx);
                }
                Err(TrySendError::Full(_)) => {
                    warn!("Channel full - subscriber cannot keep up with autodiscovery events");
                }
            }
        }
    }

    if !to_remove.is_empty() {
        let mut senders_guard = senders.lock().unwrap();

        to_remove.sort_unstable_by(|a, b| b.cmp(a));
        debug!("Removing {} closed autodiscovery subscribers", to_remove.len());

        for idx in to_remove {
            senders_guard.swap_remove(idx);
        }
    }
}

#[async_trait]
impl AutodiscoveryProvider for RemoteAgentAutoDiscoveryProvider {
    type Error = GenericError;

    async fn subscribe(&mut self, sender: mpsc::Sender<AutodiscoveryEvent>) -> Result<(), Self::Error> {
        {
            let mut senders = self.event_senders.lock().unwrap();

            let sender_addr = format!("{:p}", &sender);
            if senders.iter().any(|s| format!("{:p}", s) == sender_addr) {
                debug!("Attempt to subscribe with duplicate sender - ignoring");
                return Ok(());
            }

            senders.push(sender);
        }

        if !self.listener_started {
            self.start_background_listener().await?;
        }

        Ok(())
    }
}
