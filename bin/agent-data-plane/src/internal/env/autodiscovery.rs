use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use datadog_agent_commons::ipc::client::RemoteAgentClient;
use futures::StreamExt;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_config_tools::GenericConfiguration;
use saluki_core::runtime::{InitializationError, Supervisable, Supervisor, SupervisorFuture};
use saluki_env::autodiscovery::AutodiscoveryEvent;
use saluki_env::AutodiscoveryProvider;
use saluki_error::GenericError;
use tokio::select;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, warn};

/// Datadog Agent-based autodiscovery provider.
///
/// This provider is based primarily on the remote autodiscovery API exposed by the Datadog Agent, which handles the
/// bulk of the work by detecting changes to underlying environment (such as containers starting and stopping), and
/// determining if they have services associated with them that require checks to be run against them. The remote
/// autodiscovery API operates in a streaming fashion, which the provider uses to then broadcast updates to subscribers.
#[derive(Clone)]
pub struct RemoteAgentAutodiscoveryProvider {
    subscribers: AutodiscoverySubscribers,
}

type AutodiscoverySubscribers = Arc<Mutex<Vec<Sender<AutodiscoveryEvent>>>>;

impl RemoteAgentAutodiscoveryProvider {
    /// Creates a new `RemoteAgentAutodiscoveryProvider` based on the given configuration, along with a [`Supervisor`] that
    /// drives the collection and broadcasting of autodiscovery events.
    ///
    /// # Errors
    ///
    /// If the remote agent client couldn't be created, an error is returned.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<(Self, Supervisor), GenericError> {
        let client = RemoteAgentClient::from_configuration(config).await?;
        let subscribers = Arc::new(Mutex::new(Vec::new()));

        let provider = Self {
            subscribers: subscribers.clone(),
        };

        let mut supervisor = Supervisor::new("autodiscovery")?;
        supervisor.add_worker(AutodiscoveryEventBroadcaster { client, subscribers });

        Ok((provider, supervisor))
    }
}

#[async_trait]
impl AutodiscoveryProvider for RemoteAgentAutodiscoveryProvider {
    async fn subscribe(&self) -> Option<Receiver<AutodiscoveryEvent>> {
        let (sender, receiver) = mpsc::channel::<AutodiscoveryEvent>(16);
        self.subscribers.lock().await.push(sender);
        Some(receiver)
    }
}

struct AutodiscoveryEventBroadcaster {
    client: RemoteAgentClient,
    subscribers: AutodiscoverySubscribers,
}

#[async_trait]
impl Supervisable for AutodiscoveryEventBroadcaster {
    fn name(&self) -> &str {
        "ad-event-broadcaster"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let client = self.client.clone();
        let subscribers = self.subscribers.clone();

        Ok(Box::pin(async move {
            select! {
                _ = process_shutdown => {},
                _ = run_ad_event_broadcaster(client, subscribers) => {},
            }
            Ok(())
        }))
    }
}

async fn run_ad_event_broadcaster(mut client: RemoteAgentClient, subscribers: AutodiscoverySubscribers) {
    debug!("Listening to autodiscovery events from remote agent.");

    loop {
        let mut autodiscovery_stream = client.get_autodiscovery_stream();

        debug!("Polling autodiscovery event stream.");

        while let Some(result) = autodiscovery_stream.next().await {
            match result {
                Ok(response) => {
                    for proto_config in response.configs {
                        let event = AutodiscoveryEvent::from(proto_config);
                        send_to_subscribers(&subscribers, event).await;
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

async fn send_to_subscribers(subscribers: &AutodiscoverySubscribers, event: AutodiscoveryEvent) {
    let mut subscribers = subscribers.lock().await;
    let mut active_subscribers = Vec::with_capacity(subscribers.len());

    for sender in subscribers.drain(..) {
        if sender.send(event.clone()).await.is_ok() {
            active_subscribers.push(sender);
        }
    }

    *subscribers = active_subscribers;
}
