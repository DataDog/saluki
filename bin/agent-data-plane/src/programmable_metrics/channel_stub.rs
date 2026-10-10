//! The local stub of the artifact channel, for development, tests, and demos.
//!
//! When `experimental.programmable_metrics.artifact_channel.stub_url` is set, ADP polls `POST
//! {stub_url}/v1/artifacts/poll` every `poll_interval` (1 second by default) with its acknowledgments and status
//! summary, and receives a complete snapshot of the `policy` product, which it publishes to the installer through the
//! [artifact channel](saluki_metric_plan::channel). The protocol is specified in
//! `docs/design/programmable-metrics-stub-protocol.md`; the demo server (`bin/programmable-metrics-demo`) is the other
//! side.
//!
//! - The stub URL must be an `http://` URL of a loopback address (`127.0.0.1`, `[::1]`, or `localhost`), without a
//!   query: the stub has no authentication, and is never meant for production. A path in it is kept, and the poll
//!   path is appended to it. Unset means no channel: the default program runs.
//! - Each config's `payload` is the envelope as a JSON string, so that its `sha256` covers exact bytes; the installer
//!   checks it.
//! - A transport failure (a connection error, no complete answer within [`POLL_TIMEOUT`], a status other than `200
//!   OK`, or a body that cannot be read, including one that states another protocol version or has no `products`)
//!   publishes nothing, so the current snapshot stays in effect: it never revokes. The next poll retries.
//! - Only the products ADP asks for are read from an answer: another product, whatever the shape of its entries, is
//!   ignored, so that the stub can serve products this version does not know.

use std::{collections::BTreeMap, pin::pin, sync::Arc, time::Duration};

use agent_data_plane_config::control::ArtifactChannel;
use async_trait::async_trait;
use http::{header::CONTENT_TYPE, Request, StatusCode, Uri};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::Bytes;
use rustls::RootCertStore;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{InitializationError, Supervisable, SupervisorFuture};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::client::http::{HttpClient, HttpProtocol};
use saluki_metric_plan::{
    channel::{
        ApplyStates, ArtifactConfig, ArtifactProduct, ArtifactSnapshot, ConfigAcknowledgment, StatusSummary,
        TransportEnd,
    },
    limits,
};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use tokio::{
    select,
    time::{interval, MissedTickBehavior},
};
use tracing::{debug, info, warn};

/// The version of the stub protocol ADP speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The path ADP polls, under the stub URL.
pub const POLL_PATH: &str = "/v1/artifacts/poll";

/// How long one poll may take, from connecting to the stub to reading its whole answer.
///
/// The stub is a local process, so a poll that takes longer is stuck: it is abandoned as a transport failure, and the
/// next poll retries.
pub const POLL_TIMEOUT: Duration = Duration::from_secs(5);

/// The largest answer read from the stub, in bytes.
///
/// A policy snapshot holds at most one config to be installed, whose artifact is within the artifact size limit in its
/// canonical form; the payload is that artifact as written, inside an envelope, escaped as a JSON string. An answer
/// larger than this is unreadable, which is a transport failure.
const MAX_ANSWER_SIZE: usize = 32 * limits::values::ARTIFACT_SIZE as usize;

/// The configuration key of the stub URL, as errors name it.
const STUB_URL_KEY: &str = "experimental.programmable_metrics.artifact_channel.stub_url";

/// The configuration key of the poll interval, as errors name it.
const POLL_INTERVAL_KEY: &str = "experimental.programmable_metrics.artifact_channel.poll_interval";

/// Where and how often the stub is polled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StubChannelConfig {
    stub_url: Uri,
    poll_url: Uri,
    poll_interval: Duration,
    poll_timeout: Duration,
}

impl StubChannelConfig {
    /// Creates the stub's configuration from its URL and poll interval.
    ///
    /// # Errors
    ///
    /// Returns an error naming the configuration key if the URL cannot be parsed, is not `http`, does not name a
    /// loopback address, or has a query, or if the poll interval is zero.
    pub fn new(stub_url: &str, poll_interval: Duration) -> Result<Self, GenericError> {
        let uri: Uri = stub_url
            .parse()
            .map_err(|error| generic_error!("`{}` is not a valid URL ({}): {}", STUB_URL_KEY, stub_url, error))?;
        if uri.scheme_str() != Some("http") {
            return Err(generic_error!(
                "`{}` must be an `http://` URL of a loopback address, such as `http://127.0.0.1:17100`, but is `{}`.",
                STUB_URL_KEY,
                stub_url
            ));
        }
        let loopback = match uri.host() {
            Some("localhost") => true,
            Some(host) => host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback()),
            None => false,
        };
        if !loopback {
            return Err(generic_error!(
                "`{}` must name a loopback address (`127.0.0.1`, `[::1]`, or `localhost`), but is `{}`. The artifact \
                 channel stub is for development only and has no authentication.",
                STUB_URL_KEY,
                stub_url
            ));
        }
        // ADP polls `{stub_url}/v1/artifacts/poll`: a query could only end up in the middle of that path.
        if uri.query().is_some() {
            return Err(generic_error!(
                "`{}` must not have a query, but is `{}`. ADP polls `<stub_url>{}`: set the stub's URL without one, \
                 such as `http://127.0.0.1:17100`.",
                STUB_URL_KEY,
                stub_url,
                POLL_PATH
            ));
        }
        if poll_interval.is_zero() {
            return Err(generic_error!(
                "`{}` must be greater than zero, such as `1s`.",
                POLL_INTERVAL_KEY
            ));
        }
        let poll_url = append_poll_path(&uri)
            .map_err(|error| generic_error!("`{}` is not a valid URL ({}): {}", STUB_URL_KEY, stub_url, error))?;
        Ok(Self {
            stub_url: uri,
            poll_url,
            poll_interval,
            poll_timeout: POLL_TIMEOUT,
        })
    }

    /// Returns the configuration with another poll timeout than [`POLL_TIMEOUT`], so that tests of stuck polls run
    /// quickly.
    #[cfg(test)]
    fn with_poll_timeout(mut self, poll_timeout: Duration) -> Self {
        self.poll_timeout = poll_timeout;
        self
    }

    /// Returns the stub's configuration from the artifact channel settings, or `None` when no stub URL is set, which
    /// means there is no channel.
    ///
    /// # Errors
    ///
    /// Returns an error naming the configuration key if the stub URL or the poll interval is not valid; see
    /// [`new`][Self::new].
    pub fn from_configuration(settings: &ArtifactChannel) -> Result<Option<Self>, GenericError> {
        settings
            .stub_url
            .as_deref()
            .map(|stub_url| Self::new(stub_url, settings.poll_interval))
            .transpose()
    }

    /// Returns the stub's URL.
    pub fn stub_url(&self) -> &Uri {
        &self.stub_url
    }

    /// Returns how often the stub is polled.
    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Returns the URL ADP polls: the stub URL with [`POLL_PATH`] appended to its path.
    pub fn poll_url(&self) -> Uri {
        self.poll_url.clone()
    }
}

/// Returns `stub_url` with [`POLL_PATH`] appended to its path, less the trailing `/` the path may end with.
fn append_poll_path(stub_url: &Uri) -> Result<Uri, http::Error> {
    let path = format!("{}{}", stub_url.path().trim_end_matches('/'), POLL_PATH);
    let mut parts = stub_url.clone().into_parts();
    parts.path_and_query = Some(http::uri::PathAndQuery::try_from(path)?);
    Ok(Uri::from_parts(parts)?)
}

/// The polling process, as a poll names it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientInfo {
    /// A random identifier per ADP process.
    pub id: String,

    /// ADP's version.
    pub version: String,
}

impl ClientInfo {
    /// Returns a new identity for this process: a random identifier, and ADP's version.
    ///
    /// Each call returns another identifier, so call it once per process.
    pub fn for_process() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            version: saluki_metadata::get_app_details().version().raw().to_string(),
        }
    }
}

/// The body of a poll.
#[derive(Debug, Serialize)]
pub struct PollRequest<'a> {
    /// The protocol version, [`PROTOCOL_VERSION`].
    pub protocol_version: u32,

    /// Who is polling.
    pub client: &'a ClientInfo,

    /// The products asked for: `policy`.
    pub products: &'a [ArtifactProduct],

    /// The latest acknowledgment of each config ADP has seen.
    pub config_states: &'a [ConfigAcknowledgment],

    /// ADP's status summary.
    pub status: &'a StatusSummary,
}

impl<'a> PollRequest<'a> {
    /// Creates the body of a poll that reports the installer's latest acknowledgments and status.
    pub fn new(client: &'a ClientInfo, states: &'a ApplyStates) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            client,
            products: &[ArtifactProduct::Policy],
            config_states: &states.config_states,
            status: &states.status,
        }
    }
}

/// The stub's answer to a poll: a complete snapshot per product.
#[derive(Debug, Deserialize)]
pub struct PollResponse {
    /// The protocol version the stub speaks.
    pub protocol_version: u32,

    /// The configs of each product ADP knows, by product name. An empty list, or a missing product, revokes.
    ///
    /// The member itself is required: an answer without it, such as an error object from a misbehaving stub, is
    /// unreadable, which is a transport failure that never revokes. Products ADP does not know are left out when the
    /// answer is read, whatever their entries look like. A known product whose member is not a list of configs makes
    /// the whole answer unreadable.
    #[serde(deserialize_with = "known_products")]
    pub products: BTreeMap<String, Vec<ArtifactConfig>>,
}

impl PollResponse {
    /// Returns the snapshot of the `policy` product the answer holds: empty, which revokes, when the product is
    /// missing. Other products are ignored.
    pub fn into_policy_snapshot(mut self) -> ArtifactSnapshot {
        let configs = self
            .products
            .remove(ArtifactProduct::Policy.as_str())
            .unwrap_or_default();
        ArtifactSnapshot::new(ArtifactProduct::Policy, configs)
    }
}

/// Reads the products of an answer that ADP knows, leaving out every other product unread.
fn known_products<'de, D>(deserializer: D) -> Result<BTreeMap<String, Vec<ArtifactConfig>>, D::Error>
where
    D: Deserializer<'de>,
{
    let products = BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?;
    products
        .into_iter()
        .filter(|(name, _)| name.parse::<ArtifactProduct>().is_ok())
        .map(|(name, configs)| {
            let configs = Vec::<ArtifactConfig>::deserialize(configs)
                .map_err(|error| D::Error::custom(format!("product `{}`: {}", name, error)))?;
            Ok((name, configs))
        })
        .collect()
}

/// The supervised worker that polls the stub and connects it to the installer.
pub struct StubChannelWorker {
    config: StubChannelConfig,
    client: ClientInfo,
    transport: Arc<TransportEnd>,
}

impl StubChannelWorker {
    /// Creates the worker, which polls as `client` and publishes to the installer through `transport`.
    pub fn new(config: StubChannelConfig, client: ClientInfo, transport: TransportEnd) -> Self {
        Self {
            config,
            client,
            transport: Arc::new(transport),
        }
    }
}

#[async_trait]
impl Supervisable for StubChannelWorker {
    fn name(&self) -> &str {
        "programmable-metrics-artifact-channel-stub"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let poll_timeout = self.config.poll_timeout;
        let http = stub_client(poll_timeout)?;
        let poller = Poller {
            http,
            poll_url: self.config.poll_url(),
            poll_timeout,
            client: self.client.clone(),
            transport: Arc::clone(&self.transport),
        };
        let poll_interval = self.config.poll_interval();
        warn!(
            stub_url = %self.config.stub_url(),
            poll_interval = ?poll_interval,
            "Polling the local stub of the programmable metrics artifact channel for policy artifacts. The stub is for \
             development, tests, and demos only: it has no authentication. Do not use it in production."
        );
        Ok(Box::pin(async move {
            poller.run(poll_interval, process_shutdown).await;
            Ok(())
        }))
    }
}

/// Builds the HTTP client the stub is polled with, which gives up connecting, or waiting for an answer's headers, after
/// `timeout`.
///
/// The stub is only ever reached over plain HTTP, so the client's own TLS configuration trusts no certificate
/// authority at all, and, without proxies, the client needs none of the platform's root certificates. The client's
/// request timeout ends when the headers arrive; the poller bounds the whole exchange, body included.
fn stub_client(timeout: Duration) -> Result<HttpClient, GenericError> {
    HttpClient::builder()
        .with_request_timeout(timeout)
        .with_connect_timeout(timeout)
        .with_http_protocol(HttpProtocol::Http1)
        .with_tls_config(|tls| tls.with_root_cert_store(RootCertStore::empty()))
        .build()
        .error_context("Failed to build the HTTP client for the artifact channel stub.")
}

/// What one run of the worker polls with.
struct Poller {
    http: HttpClient,
    poll_url: Uri,
    /// How long one poll may take, from connecting to the stub to reading its whole answer.
    poll_timeout: Duration,
    client: ClientInfo,
    transport: Arc<TransportEnd>,
}

impl Poller {
    /// Polls every `poll_interval` until `shutdown` triggers, publishing each snapshot received.
    ///
    /// The first poll is sent at once. A poll that is late, because the one before it took long, delays the ones after
    /// it rather than bunching them up. A failure is logged when it starts or changes, and once more when polls succeed
    /// again, rather than once per poll.
    async fn run(mut self, poll_interval: Duration, shutdown: ShutdownHandle) {
        let mut shutdown = pin!(shutdown);
        let mut ticks = interval(poll_interval);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut failing: Option<String> = None;
        loop {
            select! {
                _ = &mut shutdown => return,
                _ = ticks.tick() => {}
            }
            let answer = select! {
                _ = &mut shutdown => return,
                answer = self.poll() => answer,
            };
            match answer {
                Ok(answer) => {
                    if let Some(error) = failing.take() {
                        info!(previous_error = %error, "The artifact channel stub answers polls again.");
                    }
                    let snapshot = answer.into_policy_snapshot();
                    let configs = snapshot.configs.len();
                    if self.transport.publisher.publish(snapshot) {
                        debug!(configs, "The artifact channel stub delivered a new policy snapshot.");
                    }
                }
                Err(error) => {
                    let error = format!("{:#}", error);
                    if failing.as_ref() != Some(&error) {
                        warn!(
                            poll_url = %self.poll_url,
                            error = %error,
                            "Failed to poll the artifact channel stub. The current policy snapshot stays in effect, \
                             and the next poll retries."
                        );
                    }
                    failing = Some(error);
                }
            }
        }
    }

    /// Sends one poll, with the installer's latest acknowledgments and status, and reads the answer.
    ///
    /// # Errors
    ///
    /// Returns an error for a transport failure: the stub cannot be reached, does not answer in full within the poll
    /// timeout, answers another status than `200 OK` or with a body that cannot be read, or speaks another protocol
    /// version.
    async fn poll(&mut self) -> Result<PollResponse, GenericError> {
        let states = self.transport.apply_states.latest();
        let body = serde_json::to_vec(&PollRequest::new(&self.client, &states))
            .error_context("Failed to write the poll as JSON.")?;
        let request = Request::post(self.poll_url.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .error_context("Failed to build the poll request.")?;
        // The deadline covers the whole exchange: a stub that sends its headers and then stalls its body is as stuck as
        // one that never answers.
        let poll_timeout = self.poll_timeout;
        let (status, body) = tokio::time::timeout(poll_timeout, self.exchange(request))
            .await
            .map_err(|_| {
                generic_error!(
                    "The artifact channel stub did not answer in full within {:?}.",
                    poll_timeout
                )
            })??;
        if status != StatusCode::OK {
            return Err(generic_error!(
                "The stub answered {}: {}",
                status,
                String::from_utf8_lossy(&body[..body.len().min(512)])
            ));
        }
        let answer: PollResponse =
            serde_json::from_slice(&body).error_context("The stub's answer is not a poll answer.")?;
        if answer.protocol_version != PROTOCOL_VERSION {
            return Err(generic_error!(
                "The stub speaks protocol version {}, but this ADP speaks version {}.",
                answer.protocol_version,
                PROTOCOL_VERSION
            ));
        }
        Ok(answer)
    }

    /// Sends a poll request, and reads the answer's status and its whole body, of at most [`MAX_ANSWER_SIZE`] bytes.
    async fn exchange(&mut self, request: Request<Full<Bytes>>) -> Result<(StatusCode, Bytes), GenericError> {
        let response = self
            .http
            .send(request)
            .await
            .error_context("The artifact channel stub could not be reached.")?;
        let status = response.status();
        let body = Limited::new(response.into_body(), MAX_ANSWER_SIZE)
            .collect()
            .await
            .map_err(|error| generic_error!("Failed to read the stub's answer: {}", error))?
            .to_bytes();
        Ok((status, body))
    }
}

#[cfg(test)]
mod tests;
