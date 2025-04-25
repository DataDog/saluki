use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use saluki_error::GenericError;
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::{debug, error, info, warn};

use crate::autodiscovery::{AutoDiscovery, AutodiscoveryEvent, Config};
use crate::helpers::remote_agent::RemoteAgentClient;

/// An autodiscovery provider that uses the Datadog Agent's internal gRPC API to receive autodiscovery updates.
pub struct RemoteAgentAutoDiscoveryProvider {
    client: RemoteAgentClient,
    // Multiple subscribers
    event_senders: Arc<Mutex<Vec<mpsc::Sender<AutodiscoveryEvent>>>>,
    // Track if we've started the background listener
    listener_started: bool,
}

impl RemoteAgentAutoDiscoveryProvider {
    /// Creates a new `RemoteAgentAutoDiscoveryProvider` from the given client.
    pub fn new(client: RemoteAgentClient) -> Self {
        Self {
            client,
            event_senders: Arc::new(Mutex::new(Vec::new())),
            listener_started: false,
        }
    }

    /// Starts listening for autodiscovery updates in the background
    pub async fn start_background_listener(&mut self) -> Result<(), GenericError> {
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
                            // Convert the protobuf Config to our Config struct
                            let config = Config::from(proto_config.clone());
                            let event = AutodiscoveryEvent { config };

                            // Send to all subscribers
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

/// Broadcasts an event to all subscribers, removing any that have closed their channels
async fn broadcast_event(senders: &Arc<Mutex<Vec<mpsc::Sender<AutodiscoveryEvent>>>>, event: AutodiscoveryEvent) {
    let mut to_remove = Vec::new();

    // Send the event to all subscribers
    {
        let senders_guard = senders.lock().unwrap();
        for (idx, sender) in senders_guard.iter().enumerate() {
            match sender.try_send(event.clone()) {
                Ok(_) => {}
                Err(TrySendError::Closed(_)) => {
                    // Channel is closed, mark for removal
                    to_remove.push(idx);
                }
                Err(TrySendError::Full(_)) => {
                    // Channel is full, this is a backpressure situation
                    warn!("Channel full - subscriber cannot keep up with autodiscovery events");
                }
            }
        }
    }

    // Remove closed senders if any
    if !to_remove.is_empty() {
        let mut senders_guard = senders.lock().unwrap();
        // Remove from back to front to avoid index shifting issues
        to_remove.sort_unstable_by(|a, b| b.cmp(a));
        debug!("Removing {} closed autodiscovery subscribers", to_remove.len());

        for idx in to_remove {
            senders_guard.swap_remove(idx);
        }
    }
}

#[async_trait]
impl AutoDiscovery for RemoteAgentAutoDiscoveryProvider {
    type Error = GenericError;

    async fn subscribe(&mut self, sender: mpsc::Sender<AutodiscoveryEvent>) -> Result<(), Self::Error> {
        // Add to list of subscribers
        {
            let mut senders = self.event_senders.lock().unwrap();
            senders.push(sender);
        }

        // Start the listener if it's not already running
        if !self.listener_started {
            self.start_background_listener().await?;
        }

        Ok(())
    }
}
