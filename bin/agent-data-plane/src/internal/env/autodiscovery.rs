use std::time::Duration;

use async_trait::async_trait;
use datadog_agent_commons::ipc::client::RemoteAgentClient;
use futures::StreamExt;
use saluki_config::GenericConfiguration;
use saluki_core::runtime::{InitializationError, ProcessShutdown, Supervisable, Supervisor, SupervisorFuture};
use saluki_env::autodiscovery::AutodiscoveryEvent;
use saluki_env::AutodiscoveryProvider;
use saluki_error::GenericError;
use std::sync::Arc;
use tokio::select;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, warn};

/// Datadog Agent-based autodiscovery provider.
///
/// This provider is based primarily on the remote autodiscovery API exposed by the Datadog Agent, which handles the
/// bulk of the work by detecting changes to underlying environment (such as containers starting and stopping), and
/// determing if they have services associated with them that require checks to be run against them. The remote
/// autodiscovery API operates in a streaming fashion, which the provider uses to then broadcast updates to subscribers.
#[derive(Clone)]
pub struct RemoteAgentAutodiscoveryProvider {
    sender: Sender<AutodiscoveryEvent>,
    receiver: Arc<Mutex<Option<Receiver<AutodiscoveryEvent>>>>,
}

impl RemoteAgentAutodiscoveryProvider {
    /// Creates a new `RemoteAgentAutodiscoveryProvider` based on the given configuration, along with a [`Supervisor`] that
    /// drives the collection and broadcasting of autodiscovery events.
    ///
    /// # Errors
    ///
    /// If the remote agent client couldn't be created, an error is returned.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<(Self, Supervisor), GenericError> {
        let client = RemoteAgentClient::from_configuration(config).await?;
        let (sender, receiver) = mpsc::channel::<AutodiscoveryEvent>(16);

        let provider = Self {
            sender: sender.clone(),
            receiver: Arc::new(Mutex::new(Some(receiver))),
        };

        let mut supervisor = Supervisor::new("autodiscovery")?;
        supervisor.add_worker(AutodiscoveryEventBroadcaster { client, sender });

        Ok((provider, supervisor))
    }
}

#[async_trait]
impl AutodiscoveryProvider for RemoteAgentAutodiscoveryProvider {
    async fn subscribe(&self) -> Option<Receiver<AutodiscoveryEvent>> {
        self.receiver.lock().await.take()
    }
}

struct AutodiscoveryEventBroadcaster {
    client: RemoteAgentClient,
    sender: Sender<AutodiscoveryEvent>,
}

#[async_trait]
impl Supervisable for AutodiscoveryEventBroadcaster {
    fn name(&self) -> &str {
        "ad-event-broadcaster"
    }

    async fn initialize(&self, process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        let client = self.client.clone();
        let sender = self.sender.clone();

        Ok(Box::pin(async move {
            select! {
                _ = process_shutdown => {},
                _ = run_ad_event_broadcaster(client, sender) => {},
            }
            Ok(())
        }))
    }
}

async fn run_ad_event_broadcaster(mut client: RemoteAgentClient, sender: Sender<AutodiscoveryEvent>) {
    debug!("Listening to autodiscovery events from remote agent.");

    loop {
        let mut autodiscovery_stream = client.get_autodiscovery_stream();

        debug!("Polling autodiscovery event stream.");

        while let Some(result) = autodiscovery_stream.next().await {
            match result {
                Ok(response) => {
                    for proto_config in response.configs {
                        let event = AutodiscoveryEvent::from(proto_config);
                        let _ = sender.send(event).await;
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
