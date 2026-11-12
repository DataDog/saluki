use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use datadog_agent_commons::ipc::{
    client::{RemoteAgentClient, StreamingResponse},
    config::RemoteAgentClientConfiguration,
};
use datadog_protos::agent::AutodiscoveryStreamResponse;
use futures::{Stream, StreamExt};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{InitializationError, Supervisable, Supervisor, SupervisorFuture};
use saluki_env::autodiscovery::AutodiscoveryEvent;
use saluki_env::AutodiscoveryProvider;
use saluki_error::GenericError;
use tokio::select;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::{Mutex, Notify};
use tokio::time::sleep;
use tonic::Status;
use tracing::{debug, warn};

/// Datadog Agent-based autodiscovery provider.
///
/// This provider is based primarily on the remote autodiscovery API exposed by the Datadog Agent, which handles the
/// bulk of the work by detecting changes to underlying environment (such as containers starting and stopping), and
/// determining if they have services associated with them that require checks to be run against them. The remote
/// autodiscovery API operates in a streaming fashion, which the provider uses to then broadcast updates to subscribers.
///
/// # Subscribers
///
/// Each autodiscovery stream starts with a snapshot of the configurations that the Datadog Agent has already
/// scheduled, followed by changes as they happen. The provider doesn't open the stream until the first subscriber
/// exists, so that subscriber receives the snapshot. If nothing ever subscribes, the stream is never opened.
///
/// Once the stream is open, it stays open even if every subscriber goes away. A subscriber that arrives after that
/// only receives the events that follow, and misses the snapshot. After a reconnect, the new stream starts with the
/// snapshot again, so subscribers that already received it receive it again.
#[derive(Clone)]
pub struct RemoteAgentAutodiscoveryProvider {
    subscribers: Arc<AutodiscoverySubscribers>,
}

impl RemoteAgentAutodiscoveryProvider {
    /// Creates a new `RemoteAgentAutodiscoveryProvider` based on the given configuration, along with a [`Supervisor`] that
    /// drives the collection and broadcasting of autodiscovery events.
    ///
    /// # Errors
    ///
    /// If the remote agent client couldn't be created, an error is returned.
    pub async fn new(client_config: &RemoteAgentClientConfiguration) -> Result<(Self, Supervisor), GenericError> {
        let client = RemoteAgentClient::connect(client_config).await?;
        Self::from_source(client)
    }

    fn from_source<S: AutodiscoveryStreamSource>(source: S) -> Result<(Self, Supervisor), GenericError> {
        let subscribers = Arc::new(AutodiscoverySubscribers::default());

        let provider = Self {
            subscribers: Arc::clone(&subscribers),
        };

        let mut supervisor = Supervisor::new("autodiscovery")?;
        supervisor.add_worker(AutodiscoveryEventBroadcaster { source, subscribers });

        Ok((provider, supervisor))
    }
}

#[async_trait]
impl AutodiscoveryProvider for RemoteAgentAutodiscoveryProvider {
    async fn subscribe(&self) -> Option<Receiver<AutodiscoveryEvent>> {
        let (sender, receiver) = mpsc::channel::<AutodiscoveryEvent>(16);
        self.subscribers.add(sender).await;
        Some(receiver)
    }
}

#[derive(Default)]
struct AutodiscoverySubscribers {
    senders: Mutex<Vec<Sender<AutodiscoveryEvent>>>,
    added: Notify,
}

impl AutodiscoverySubscribers {
    async fn add(&self, sender: Sender<AutodiscoveryEvent>) {
        self.senders.lock().await.push(sender);
        self.added.notify_one();
    }

    async fn wait_for_first(&self) {
        // `notify_one` keeps a permit when nothing is waiting, so a subscriber added after the check isn't missed.
        while self.senders.lock().await.is_empty() {
            self.added.notified().await;
        }
    }

    async fn send(&self, event: AutodiscoveryEvent) {
        let mut senders = self.senders.lock().await;
        let mut active_senders = Vec::with_capacity(senders.len());

        for sender in senders.drain(..) {
            if sender.send(event.clone()).await.is_ok() {
                active_senders.push(sender);
            }
        }

        *senders = active_senders;
    }
}

/// Opens autodiscovery streams, so that tests can stand in for the Datadog Agent.
trait AutodiscoveryStreamSource: Clone + Send + Sync + 'static {
    type Stream: Stream<Item = Result<AutodiscoveryStreamResponse, Status>> + Send + Unpin;

    fn open_stream(&mut self) -> Self::Stream;
}

impl AutodiscoveryStreamSource for RemoteAgentClient {
    type Stream = StreamingResponse<AutodiscoveryStreamResponse>;

    fn open_stream(&mut self) -> Self::Stream {
        self.get_autodiscovery_stream()
    }
}

struct AutodiscoveryEventBroadcaster<S> {
    source: S,
    subscribers: Arc<AutodiscoverySubscribers>,
}

#[async_trait]
impl<S: AutodiscoveryStreamSource> Supervisable for AutodiscoveryEventBroadcaster<S> {
    fn name(&self) -> &str {
        "ad-event-broadcaster"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let source = self.source.clone();
        let subscribers = Arc::clone(&self.subscribers);

        Ok(Box::pin(async move {
            select! {
                _ = process_shutdown => {},
                _ = run_ad_event_broadcaster(source, subscribers) => {},
            }
            Ok(())
        }))
    }
}

async fn run_ad_event_broadcaster<S: AutodiscoveryStreamSource>(
    mut source: S, subscribers: Arc<AutodiscoverySubscribers>,
) {
    debug!("Waiting for the first autodiscovery subscriber.");
    subscribers.wait_for_first().await;

    debug!("Listening to autodiscovery events from remote agent.");

    loop {
        let mut autodiscovery_stream = source.open_stream();

        debug!("Polling autodiscovery event stream.");

        while let Some(result) = autodiscovery_stream.next().await {
            match result {
                Ok(response) => {
                    for proto_config in response.configs {
                        let event = AutodiscoveryEvent::from(proto_config);
                        subscribers.send(event).await;
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use datadog_protos::agent::Config as ProtoConfig;
    use futures::stream::{self, BoxStream};
    use saluki_core::runtime::test_util::TestSupervisor;
    use tokio::sync::broadcast;
    use tokio::time::timeout;

    use super::*;

    /// Stands in for the Datadog Agent: each stream yields the snapshot, then the updates pushed while it's open.
    #[derive(Clone)]
    struct FakeAgent {
        snapshot: Vec<&'static str>,
        opens: Arc<AtomicUsize>,
        // `None` ends the open stream.
        updates: broadcast::Sender<Option<&'static str>>,
    }

    impl FakeAgent {
        fn new(snapshot: &[&'static str]) -> Self {
            Self {
                snapshot: snapshot.to_vec(),
                opens: Arc::new(AtomicUsize::new(0)),
                updates: broadcast::channel(16).0,
            }
        }

        fn opens(&self) -> usize {
            self.opens.load(Ordering::SeqCst)
        }

        fn push_update(&self, name: &'static str) {
            self.updates.send(Some(name)).expect("no open stream");
        }

        fn end_stream(&self) {
            self.updates.send(None).expect("no open stream");
        }
    }

    fn response(names: &[&'static str]) -> AutodiscoveryStreamResponse {
        let configs = names
            .iter()
            .map(|name| ProtoConfig {
                name: name.to_string(),
                ..Default::default()
            })
            .collect();
        AutodiscoveryStreamResponse { configs }
    }

    impl AutodiscoveryStreamSource for FakeAgent {
        type Stream = BoxStream<'static, Result<AutodiscoveryStreamResponse, Status>>;

        fn open_stream(&mut self) -> Self::Stream {
            self.opens.fetch_add(1, Ordering::SeqCst);
            let updates = stream::unfold(self.updates.subscribe(), |mut updates| async move {
                match updates.recv().await {
                    Ok(Some(name)) => Some((Ok(response(&[name])), updates)),
                    _ => None,
                }
            });
            stream::iter([Ok(response(&self.snapshot))]).chain(updates).boxed()
        }
    }

    async fn start(agent: &FakeAgent) -> (RemoteAgentAutodiscoveryProvider, TestSupervisor) {
        let (provider, supervisor) = RemoteAgentAutodiscoveryProvider::from_source(agent.clone()).unwrap();
        let supervisor = TestSupervisor::start_with("test-root", |root| root.add_worker(supervisor)).await;
        (provider, supervisor)
    }

    async fn next_scheduled_config(events: &mut Receiver<AutodiscoveryEvent>) -> String {
        let event = timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("timed out waiting for an autodiscovery event")
            .expect("broadcaster dropped the subscriber");
        match event {
            AutodiscoveryEvent::Schedule { config } => config.name.to_string(),
            other => panic!("expected a schedule event, got {other:?}"),
        }
    }

    // Regression test: the broadcaster used to open the stream as soon as its worker started, and dropped the snapshot
    // while nothing had subscribed. It now waits for the first subscriber before opening the stream.
    #[tokio::test(start_paused = true)]
    async fn subscriber_after_worker_start_receives_initial_snapshot() {
        let agent = FakeAgent::new(&["redis", "nginx"]);
        let (provider, supervisor) = start(&agent).await;

        // Let the worker start and settle before anything subscribes.
        sleep(Duration::from_secs(1)).await;

        let mut events = provider.subscribe().await.expect("provider should be configured");
        assert_eq!(next_scheduled_config(&mut events).await, "redis");
        assert_eq!(next_scheduled_config(&mut events).await, "nginx");
        assert_eq!(agent.opens(), 1);

        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_while_waiting_for_first_subscriber_is_clean() {
        let agent = FakeAgent::new(&["redis"]);
        let (_provider, supervisor) = start(&agent).await;

        sleep(Duration::from_secs(1)).await;
        assert_eq!(agent.opens(), 0, "stream opened before anything subscribed");

        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn subscriber_after_stream_opens_misses_snapshot() {
        let agent = FakeAgent::new(&["redis"]);
        let (provider, supervisor) = start(&agent).await;

        let mut first = provider.subscribe().await.expect("provider should be configured");
        assert_eq!(next_scheduled_config(&mut first).await, "redis");

        let mut late = provider.subscribe().await.expect("provider should be configured");
        agent.push_update("nginx");
        assert_eq!(next_scheduled_config(&mut first).await, "nginx");
        assert_eq!(next_scheduled_config(&mut late).await, "nginx");
        assert_eq!(agent.opens(), 1);

        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_delivers_snapshot_again() {
        let agent = FakeAgent::new(&["redis"]);
        let (provider, supervisor) = start(&agent).await;

        let mut events = provider.subscribe().await.expect("provider should be configured");
        assert_eq!(next_scheduled_config(&mut events).await, "redis");

        agent.end_stream();
        assert_eq!(next_scheduled_config(&mut events).await, "redis");
        assert_eq!(agent.opens(), 2);

        supervisor.shutdown().await.unwrap();
    }
}
