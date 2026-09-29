//! Provides a client for remote configuration.
//!
//! Configuration assigned to this client is delivered by polling the Datadog Agent, which fetches it from the
//! Datadog backend. This crate hides that protocol: a subscriber supplies a [`ProductDecoder`], which names its product
//! and decodes that product's payloads, and receives typed snapshots through a [`Subscription`]. The client's identity,
//! its protocol cursor, its cache advertisement, the paths configurations arrive under, and the numeric apply states it
//! reports are all private.
//!
//! # Testing
//!
//! With the `test-util` feature enabled, [`TestPublisher`] creates a [`Subscription`] that a test publishes into by
//! hand, either with finished snapshots and rejections or by running a decoder over payloads exactly as the client
//! does. A component that takes a `Subscription` can therefore be tested without an Agent.
//!
//! # Trust
//!
//! The client performs no TUF signature verification. It trusts the Agent, reached over an authenticated local IPC
//! channel, to have verified already. It does validate that each payload matches the length and SHA-256 hash published
//! in the accompanying targets metadata, which guards against a bug in delivery.
//!
//! # Examples
//!
//! A decoder for a product whose payload is a message, a consumer of its subscription, and a test of both through
//! [`TestPublisher`]:
//!
//! ```
//! use datadog_agent_remote_config::{ConfigId, ProductDecoder, RemoteConfigurationClient, Subscription, TestPublisher};
//!
//! #[derive(Debug)]
//! struct Example {
//!     message: String,
//! }
//!
//! /// Keeps the last valid message in ascending configuration ID order.
//! #[derive(Default)]
//! struct ExampleDecoder {
//!     message: Option<String>,
//! }
//!
//! impl ProductDecoder for ExampleDecoder {
//!     const PRODUCT: &'static str = "EXAMPLE_PRODUCT";
//!
//!     type Snapshot = Example;
//!     type Error = String;
//!
//!     fn decode(&mut self, _id: &ConfigId, payload: &[u8]) -> Result<(), Self::Error> {
//!         let message = std::str::from_utf8(payload).map_err(|_| "Message is not UTF-8.".to_owned())?;
//!         self.message = Some(message.to_owned());
//!         Ok(())
//!     }
//!
//!     fn build(self) -> Result<Self::Snapshot, Self::Error> {
//!         let message = self.message.ok_or_else(|| "No message was assigned.".to_owned())?;
//!         Ok(Example { message })
//!     }
//! }
//!
//! /// Subscribes once where the application is wired together, and hands the subscription to its consumer.
//! fn wire(rc_client: &RemoteConfigurationClient) -> datadog_agent_remote_config::Result<()> {
//!     let subscription = rc_client.subscribe::<ExampleDecoder>()?;
//!     tokio::spawn(consume(subscription));
//!     Ok(())
//! }
//!
//! async fn consume(mut subscription: Subscription<Example>) {
//!     // A snapshot accepted before the subscription was created is not announced by `changed`, so read it first.
//!     if let Some(example) = subscription.current() {
//!         println!("{}", example.message);
//!     }
//!     loop {
//!         match subscription.changed().await {
//!             Ok(example) => println!("{}", example.message),
//!             // The client reports the rejection to the Agent; `current` still returns the last accepted snapshot.
//!             Err(error) => eprintln!("{error}"),
//!         }
//!     }
//! }
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! // In a test, `TestPublisher` runs the decoder over payloads exactly as the client does.
//! let (publisher, mut subscription) = TestPublisher::<Example>::new();
//!
//! publisher.assign::<ExampleDecoder>([("greeting.v1", "hello"), ("greeting.v2", "hi")]);
//! assert_eq!(subscription.changed().await.unwrap().message, "hi");
//!
//! // An empty assignment fails to build, and the last accepted snapshot is kept.
//! publisher.assign::<ExampleDecoder>(Vec::<(&str, &str)>::new());
//! assert_eq!(*subscription.changed().await.unwrap_err(), "No message was assigned.");
//! assert_eq!(subscription.current().unwrap().message, "hi");
//! # }
//! ```

#![deny(missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use datadog_agent_commons::ipc::client::RemoteAgentClient;

mod decoder;
mod error;
mod identity;
mod json;
mod metrics;
mod product;
mod protocol;
mod registry;
mod repository;
mod source;
mod subscription;
#[cfg(test)]
mod test;
#[cfg(any(test, feature = "test-util"))]
mod test_util;
mod worker;

pub use decoder::ProductDecoder;
pub use error::{ApplyError, Error, Result};
pub use identity::{AgentIdentity, ClientKind};
pub use json::{decode_json, JsonError};
pub use product::ConfigId;
pub use subscription::Subscription;
#[cfg(any(test, feature = "test-util"))]
pub use test_util::TestPublisher;
pub use worker::RemoteConfigurationWorker;

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(90);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Settings for a [`RemoteConfigurationClient`].
///
/// `Rc` abbreviates Remote Configuration, which avoids the repetition in `RemoteConfigurationClientConfiguration`.
///
/// Start from [`new`](Self::new), which takes the kind of client to report and uses the standard schedule, and change
/// fields as needed. [`validate`](Self::validate) checks the values, and [`RemoteConfigurationClient::new`]
/// rejects invalid settings.
///
/// Whatever the settings, the worker polls once immediately when it starts, and retries every second until its first
/// successful poll.
#[derive(Clone, Debug)]
pub struct RcClientConfiguration {
    /// The kind of client the Agent sees, and the details it reports in every poll.
    ///
    /// Has no default.
    pub kind: ClientKind,

    /// How long the worker waits between successful polls.
    ///
    /// Shorter intervals deliver changes sooner at the cost of more requests to the Agent. The Agent refreshes from the
    /// backend far less often than this, so lowering it rarely helps. Must be at least one second.
    ///
    /// Defaults to 5 seconds.
    pub poll_interval: Duration,

    /// The longest the worker waits between polls while polls are failing.
    ///
    /// After consecutive failures the wait doubles, with jitter, from `poll_interval` up to this ceiling, and resets
    /// on the next success. The worker also waits this long between attempts when the Agent has remote configuration
    /// disabled. Must be at least `poll_interval`.
    ///
    /// Defaults to 90 seconds.
    pub max_backoff: Duration,

    /// How long the worker waits for the Agent to answer one poll.
    ///
    /// A poll that runs out of time is abandoned and handled like any other failed poll, so a stuck connection cannot
    /// stall delivery indefinitely. The Agent may hold the first poll from a new client for up to about two seconds
    /// while it fetches from the backend, so this must stay well above that. Must be at least one second.
    ///
    /// Defaults to 30 seconds.
    pub request_timeout: Duration,
}

impl RcClientConfiguration {
    /// Creates settings for a client that reports itself as `kind`, with the standard schedule.
    pub fn new(kind: ClientKind) -> Self {
        Self {
            kind,
            poll_interval: POLL_INTERVAL,
            max_backoff: MAX_BACKOFF,
            request_timeout: REQUEST_TIMEOUT,
        }
    }

    /// Checks that the settings are valid.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidSettings`] if an agent's [`name`](AgentIdentity::name) or
    /// [`version`](AgentIdentity::version) is empty, `poll_interval` is shorter than one second, `max_backoff` is
    /// shorter than `poll_interval`, or `request_timeout` is shorter than one second. The fields are checked in that
    /// order, and the error describes the first that fails.
    pub fn validate(&self) -> Result<()> {
        let ClientKind::Agent(agent) = &self.kind;
        if agent.name.is_empty() {
            return Err(Error::InvalidSettings {
                message: "The agent name must not be empty.".to_owned(),
            });
        }
        if agent.version.is_empty() {
            return Err(Error::InvalidSettings {
                message: "The agent version must not be empty.".to_owned(),
            });
        }
        if self.poll_interval < Duration::from_secs(1) {
            return Err(Error::InvalidSettings {
                message: format!(
                    "poll_interval must be at least one second; got {:?}.",
                    self.poll_interval
                ),
            });
        }
        if self.max_backoff < self.poll_interval {
            return Err(Error::InvalidSettings {
                message: format!(
                    "max_backoff must be at least poll_interval ({:?}); got {:?}.",
                    self.poll_interval, self.max_backoff
                ),
            });
        }
        if self.request_timeout < Duration::from_secs(1) {
            return Err(Error::InvalidSettings {
                message: format!(
                    "request_timeout must be at least one second; got {:?}.",
                    self.request_timeout
                ),
            });
        }
        // Settings are valid.
        Ok(())
    }
}

/// A cloneable handle for subscribing to Remote Configuration products.
///
/// Every clone shares one set of subscriptions and one client identity with the worker.
#[derive(Clone)]
pub struct RemoteConfigurationClient {
    shared: Arc<registry::Shared>,
}

// TODO: consider opt-in health notifications when a subscriber needs them (e.g. CWS enforcement).
// TODO: consider per-product option to keep last good when the Agent reports expired (e.g. Cluster Agent autoscaling).
impl RemoteConfigurationClient {
    /// Creates a client and its worker from a connected Datadog Agent client.
    ///
    /// The connection must be dedicated to Remote Configuration. Construction does not spawn the worker or probe
    /// Remote Configuration availability; the caller schedules the worker through a supervisor or its `run` method.
    ///
    /// The client identifies itself to the Agent with a random ID, generated here and kept for the life of the client,
    /// and with the kind and details in `config`.
    ///
    /// # Errors
    ///
    /// Returns an error if `config` fails [`RcClientConfiguration::validate`].
    pub fn new(ra: RemoteAgentClient, config: RcClientConfiguration) -> Result<(Self, RemoteConfigurationWorker)> {
        config.validate()?;
        Ok(Self::with_agent(Box::new(ra), config))
    }

    /// Creates a client and its worker around any [`RcAgent`](source::RcAgent), so tests can replace the Agent.
    pub(crate) fn with_agent(
        agent: Box<dyn source::RcAgent>, config: RcClientConfiguration,
    ) -> (Self, RemoteConfigurationWorker) {
        let shared = Arc::new(registry::Shared::new());
        let worker = RemoteConfigurationWorker::new(Arc::clone(&shared), agent, config);
        (Self { shared }, worker)
    }

    /// Subscribes to the product `P` decodes, [`P::PRODUCT`](ProductDecoder::PRODUCT).
    ///
    /// The decoder is named here, where how a product is read is the subject; the returned subscription is typed by the
    /// snapshot that decoder builds. Subscriptions can be added while the worker runs.
    ///
    /// A product may have only one live subscription per client, even when two decoders name it. Several consumers of
    /// one product therefore share a single [`Subscription`] by cloning it, rather than each subscribing for
    /// themselves. Dropping the last clone unsubscribes, after which the product may be subscribed again.
    /// The new subscription starts with no accepted snapshot.
    ///
    /// Subscribing wakes the worker to poll without waiting for its next scheduled poll. The Agent answers polls from a
    /// cache that it refreshes from the Datadog backend on its own schedule, every minute by default. It refreshes
    /// early only for a client it has not seen before, so a product that no other client of the Agent requests,
    /// subscribed after this client's first poll, may take up to the Agent's refresh interval to arrive.
    ///
    /// # Errors
    ///
    /// Returns [`Error::AlreadySubscribed`] when the product still has a live subscription on this client, which
    /// indicates that the caller should be receiving a clone of the existing subscription instead.
    pub fn subscribe<P>(&self) -> Result<Subscription<P::Snapshot, P::Error>>
    where
        P: ProductDecoder,
    {
        self.shared.subscribe::<P>()
    }
}
