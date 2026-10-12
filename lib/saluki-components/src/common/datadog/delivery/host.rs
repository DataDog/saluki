//! The delivery host: its configuration, what it shares with the factory that builds its recipients, and the
//! forwarder component that runs its routing loop.

use std::{
    collections::BTreeSet,
    fmt,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, SystemTime},
};

use agent_data_plane_config::shared::Secrets;
use agent_data_plane_config::Live;
use async_trait::async_trait;
use http::Uri;
use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_core::{
    components::{forwarders::*, BuildContext, ComponentContext},
    diagnostic::DiagnosticsEmitter,
    execution::control::{ControlMailbox, ControlReceiver},
    observability::ComponentMetricsExt as _,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::client::http::{HttpClient, HttpClientSwitch};
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::sync::{mpsc, watch};

use super::{
    client::{build_client, ClientProfile},
    router::{LoopSpawner, NativeLoops, Router},
    CredentialSource, DeliveryControl, DeliveryEvent, DeliverySettings, RecipientFactory, RecipientOrigin,
    RecipientPieces, RecipientProtocol, RecipientRef, RecipientSettings, RecipientSpec,
};
use crate::common::datadog::{
    config::ForwarderConfiguration,
    endpoints::{EndpointRoute, MetricsRecipientRoute, ResolvedEndpoint},
    io::EndpointRequestMapperFactory,
    middleware::for_hosted_recipient,
    retry::{SecretsGate, SecretsGateRefresher},
    telemetry::{
        ComponentTelemetry, SharedTransactionQueueTelemetry, TransactionQueueTelemetry, TransactionRetryTelemetry,
    },
};

/// The body of the payloads a delivery host carries.
pub(super) type Body = FrozenChunkedBytesBuffer;

/// Names the endpoint of a request for a client's and a recipient's telemetry.
pub(super) type EndpointName = Arc<dyn Fn(&Uri) -> Option<MetaString> + Send + Sync>;

/// How long a released recipient keeps dispatching its queued work before it closes as at shutdown.
pub const DRAIN_DEADLINE: Duration = Duration::from_secs(30);

/// Returns the memory one recipient's queues hold at most with `settings` and its delivery `policy`: its in-memory
/// retry queue's bound (the policy's, at most the host's), and its high-priority queue of the largest compressed
/// payloads.
///
/// This is the bound a native forwarder declares for its queues, which a delivery host with recipients at boot
/// declares too; a controller admits it for what it adds later, such as a declared recipient, or an idle host's first
/// recipient.
pub fn recipient_queue_bound(settings: &DeliverySettings, policy: &super::RecipientPolicy) -> u64 {
    let retry_queue = policy.queue_bytes.map_or(settings.retry.queue_max_size_bytes, |bytes| {
        bytes.min(settings.retry.queue_max_size_bytes)
    });
    let high_priority = (settings.endpoint_buffer_size as u64)
        .saturating_mul(crate::common::datadog::DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT as u64);
    retry_queue.saturating_add(high_priority)
}

/// The most recipients one delivery host drains at once: releasing one more closes the oldest draining recipient
/// early, with its closure.
///
/// The bound of a host's event channel is the most recipients its table holds plus this, so that a send never waits.
pub const MAX_DRAINING_RECIPIENTS: usize = 16;

/// What a delivery host runs from its first payload: the recipients, settings, native set, and withdrawn set of the
/// publication its controller made before the host was built.
#[derive(Clone, Debug)]
pub struct BootPublication {
    /// The revision published.
    pub revision: u64,

    /// The host's settings.
    pub settings: Arc<DeliverySettings>,

    /// The recipients the host runs from the start, each with its slot, identity, and incarnation.
    pub recipients: Vec<RecipientSettings>,

    /// The recipients payloads that do not name their own are routed over, filtered by payload class.
    pub native_set: Vec<RecipientRef>,

    /// The recipients the current lowering no longer declares, by identity.
    pub withdrawn: BTreeSet<MetaString>,
}

impl BootPublication {
    /// Returns the publication of a host with no recipients yet, such as an idle failover host, at revision 0.
    pub fn empty(settings: Arc<DeliverySettings>) -> Self {
        Self {
            revision: 0,
            settings,
            recipients: Vec::new(),
            native_set: Vec::new(),
            withdrawn: BTreeSet::new(),
        }
    }
}

/// The state of one recipient in a delivery host's table, as the host reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecipientState {
    /// Prepared, or reopened: payloads that name it reach it.
    Live,

    /// Released, and waiting for every payload received before the release to be routed.
    Releasing,

    /// Released, and dispatching its queued work until its queue is empty or its drain deadline passes.
    Draining,

    /// Its loop ended on an error: payloads that name it are dropped and counted until its next incarnation is live.
    Failed,
}

/// One recipient of a delivery host's table, as the host reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecipientStatus {
    /// The recipient.
    pub recipient: RecipientRef,

    /// Its state.
    pub state: RecipientState,

    /// When its drain started, while it drains.
    pub draining_since: Option<SystemTime>,
}

/// Two recipients of one delivery host that send to the same endpoint with the same key.
///
/// A declared recipient that duplicates a live one fails to prepare. A conflict is what remains: a native recipient
/// that duplicates a declared one, which native configuration wins and which both keep running, or a credential
/// rotation that made two recipients' keys equal.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RecipientConflict {
    /// The declared recipient.
    pub declared: RecipientRef,

    /// The recipient it duplicates.
    pub duplicates: RecipientRef,
}

/// What a delivery host reports about its table, for status.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeliveryHostStatus {
    /// The revision of the last publication the host applied.
    pub revision: u64,

    /// Every recipient of the table, by slot.
    pub recipients: Vec<RecipientStatus>,

    /// The recipients that send to the same endpoint with the same key as another.
    pub conflicts: Vec<RecipientConflict>,

    /// The recipients the host drops every payload to, by identity.
    pub withdrawn: Vec<MetaString>,

    /// Why the host could not build a client for its current settings, such as an invalid proxy, while it keeps
    /// sending through the client it had. The host builds one again at its next publication or proxy credential
    /// change.
    pub client_failure: Option<String>,
}

/// The configuration of a delivery host: a forwarder whose recipients a controller prepares, publishes, drains,
/// retires, and reopens while it runs, through its control mailbox.
///
/// A forwarder configuration becomes a delivery host's when it is given one (for example,
/// [`DatadogForwarderConfiguration::controlled`][crate::forwarders::DatadogForwarderConfiguration::controlled]). The
/// controller keeps the mailbox's sender, the receiving end of the event channel, and the host's
/// [factory][Self::factory], which builds recipients' pieces off the data path once the host is built.
pub struct DeliveryHostConfiguration {
    mailbox: Mutex<Option<ControlMailbox<DeliveryControl<Body>>>>,
    credentials: Arc<dyn CredentialSource>,
    boot: BootPublication,
    events: Option<mpsc::Sender<DeliveryEvent>>,
    drain_deadline: Duration,
    factory: Arc<HostFactory>,
    status: watch::Sender<DeliveryHostStatus>,

    /// What runs the recipients' loops in place of their I/O loops, for tests.
    #[cfg(any(test, feature = "test-util"))]
    loops: Option<Arc<dyn LoopSpawner>>,
}

impl DeliveryHostConfiguration {
    /// Creates the configuration of a delivery host that is controlled through `mailbox`, resolves its recipients'
    /// credentials and its proxies' user information through `credentials`, and runs `boot` from its first payload.
    pub fn new(
        mailbox: ControlMailbox<DeliveryControl<FrozenChunkedBytesBuffer>>, credentials: Arc<dyn CredentialSource>,
        boot: BootPublication,
    ) -> Self {
        Self {
            mailbox: Mutex::new(Some(mailbox)),
            credentials,
            boot,
            events: None,
            drain_deadline: DRAIN_DEADLINE,
            factory: Arc::new(HostFactory::default()),
            status: watch::channel(DeliveryHostStatus::default()).0,
            #[cfg(any(test, feature = "test-util"))]
            loops: None,
        }
    }

    /// Runs every recipient as a loop that sends nothing, for tests (`test-util`): it takes each transaction its host
    /// routes to it, adds the events the transaction carries to `delivered`, and exits when it is asked to retire or
    /// close, or its input closes.
    ///
    /// Everything else is the host's own: its table, its routing, its control, and its events.
    #[cfg(any(test, feature = "test-util"))]
    pub fn with_discarding_recipients(mut self, delivered: Arc<std::sync::atomic::AtomicU64>) -> Self {
        self.loops = Some(Arc::new(super::router::DiscardingLoops::new(delivered)));
        self
    }

    /// Reports each recipient incarnation's exit on `events`, exactly once.
    ///
    /// The channel should be bounded at the most recipients the host's table holds plus [`MAX_DRAINING_RECIPIENTS`]:
    /// the host never waits for room in it, and holds what it cannot send yet until there is room.
    pub fn with_events(mut self, events: mpsc::Sender<DeliveryEvent>) -> Self {
        self.events = Some(events);
        self
    }

    /// Sets how long a released recipient drains its queued work, [`DRAIN_DEADLINE`] by default.
    pub fn with_drain_deadline(mut self, deadline: Duration) -> Self {
        self.drain_deadline = deadline;
        self
    }

    /// Returns the factory that builds the pieces of this host's recipients, which a controller sends in its prepares.
    ///
    /// The factory builds nothing until the host is built: the pieces are the host's own, named after it.
    pub fn factory(&self) -> Arc<dyn RecipientFactory<FrozenChunkedBytesBuffer>> {
        Arc::clone(&self.factory) as Arc<dyn RecipientFactory<FrozenChunkedBytesBuffer>>
    }

    /// Returns what the host reports about its table, updated whenever it changes.
    pub fn status(&self) -> watch::Receiver<DeliveryHostStatus> {
        self.status.subscribe()
    }

    /// Returns whether the host runs recipients from its first payload, which decides whether it counts a queue in its
    /// boot memory bound.
    pub(crate) fn has_boot_recipients(&self) -> bool {
        !self.boot.recipients.is_empty()
    }

    /// Builds the host's forwarder component.
    ///
    /// `base` holds what stays fixed while the process runs (the retry queue's storage path, the TLS key log);
    /// `kind` the host's telemetry names and whether a rejected key is retried while secrets are in use.
    ///
    /// # Errors
    ///
    /// Returns an error if the host was built already, if its client or diagnostics cannot be created, or if a boot
    /// recipient cannot be built.
    pub(crate) fn build(
        &self, context: &BuildContext, base: ForwarderConfiguration, kind: HostKind,
    ) -> Result<Box<dyn Forwarder + Send>, GenericError> {
        let component = context.component_context().clone();
        let id = component.component_id().to_string();
        let (shared, secrets) = HostShared::build(component, base, kind, &self.credentials, &self.boot.settings)?;
        #[cfg(any(test, feature = "test-util"))]
        let shared = match &self.loops {
            Some(loops) => shared.with_loops(Arc::clone(loops)),
            None => shared,
        };
        let shared = Arc::new(shared);
        let mut boot = Vec::with_capacity(self.boot.recipients.len());
        for recipient in &self.boot.recipients {
            let pieces = shared.build_pieces(&recipient.spec, &self.boot.settings)?;
            boot.push((recipient.clone(), pieces));
        }
        // Taken last, so that a build that fails leaves the host buildable.
        let mailbox = self
            .mailbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .ok_or_else(|| {
                generic_error!(
                    "Delivery host '{}' was already built: its control mailbox can be taken only once.",
                    id
                )
            })?;
        if self.factory.shared.set(Arc::clone(&shared)).is_err() {
            return Err(generic_error!(
                "Delivery host '{}' was already built.",
                shared.context.component_id()
            ));
        }

        Ok(Box::new(DeliveryHostForwarder {
            shared,
            control: mailbox.into_receiver_for("delivery host"),
            events: self.events.clone(),
            boot,
            publication: self.boot.clone(),
            drain_deadline: self.drain_deadline,
            status: self.status.clone(),
            secrets,
        }))
    }
}

impl fmt::Debug for DeliveryHostConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeliveryHostConfiguration")
            .field("boot", &self.boot)
            .field("drain_deadline", &self.drain_deadline)
            .finish_non_exhaustive()
    }
}

/// What differs between the kinds of delivery hosts.
pub(crate) struct HostKind {
    /// Names the endpoint of a request for telemetry.
    pub(crate) endpoint_name: EndpointName,

    /// The live view of the secrets settings, which decides whether a rejected API key is retried; `None` never
    /// retries one, as for a host that presents bearer tokens.
    pub(crate) secrets: Option<Live<Secrets>>,
}

/// What a delivery host shares with the factory that builds its recipients' pieces: everything a recipient's I/O loop
/// is built with besides its specification and the host's settings.
pub(super) struct HostShared {
    pub(super) context: ComponentContext,
    pub(super) base: ForwarderConfiguration,
    pub(super) metrics_builder: MetricsBuilder,
    pub(super) telemetry: ComponentTelemetry,
    queue_telemetry: SharedTransactionQueueTelemetry,

    /// The host's client, which every recipient sends through, and which a client built for new settings replaces
    /// through `switch`. It is shared only to be cloned.
    client: Mutex<HttpClient>,
    pub(super) switch: HttpClientSwitch,

    /// What the client the host was built with was built from.
    pub(super) client_profile: ClientProfile,
    secrets: SecretsGate,
    pub(super) endpoint_name: EndpointName,
    pub(super) emitter: DiagnosticsEmitter,
    pub(super) credentials: Arc<dyn CredentialSource>,
    pub(super) loops: Arc<dyn LoopSpawner>,
}

impl HostShared {
    /// Creates what a host shares with its factory, with a client built for `settings`, and the refresher of the
    /// secrets gate to spawn when the host starts, if the host follows secrets.
    pub(super) fn build(
        context: ComponentContext, base: ForwarderConfiguration, kind: HostKind,
        credentials: &Arc<dyn CredentialSource>, settings: &DeliverySettings,
    ) -> Result<(Self, Option<SecretsGateRefresher>), GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let emitter = DiagnosticsEmitter::from_current(context.identity())
            .error_context("Failed to create diagnostics emitter for delivery host.")?;
        let (secrets, refresher) = match kind.secrets {
            Some(secrets) => {
                let refresher = SecretsGateRefresher::new(secrets);
                (refresher.gate.clone(), Some(refresher))
            }
            None => (SecretsGate::new_fixed(&Secrets::default()), None),
        };
        let profile = ClientProfile::new(settings, resolve_proxy_userinfo(credentials, settings));
        let client = build_client(&profile, &base, &metrics_builder, &kind.endpoint_name)
            .error_context("Failed to build the delivery host's HTTP client.")?;
        let (client, switch) = HttpClient::switchable(client);
        let shared = Self {
            telemetry: ComponentTelemetry::from_builder(&metrics_builder),
            queue_telemetry: SharedTransactionQueueTelemetry::from_builder(&metrics_builder),
            context,
            base,
            metrics_builder,
            client: Mutex::new(client),
            switch,
            client_profile: profile,
            secrets,
            endpoint_name: kind.endpoint_name,
            emitter,
            credentials: Arc::clone(credentials),
            loops: Arc::new(NativeLoops),
        };
        Ok((shared, refresher))
    }

    /// Makes the host spawn its recipients' loops through `loops`.
    #[cfg(any(test, feature = "test-util"))]
    pub(super) fn with_loops(mut self, loops: Arc<dyn LoopSpawner>) -> Self {
        self.loops = loops;
        self
    }

    /// Returns the host's client, which every clone sends through whatever client the host switched to last.
    pub(super) fn client(&self) -> HttpClient {
        self.client.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Builds the pieces of one recipient's I/O loop, with the host's settings.
    pub(super) fn build_pieces(
        &self, spec: &RecipientSpec, settings: &DeliverySettings,
    ) -> Result<RecipientPieces<Body>, GenericError> {
        let view = self.credentials.resolve(&spec.credential).ok_or_else(|| {
            generic_error!(
                "Recipient '{}' names credential '{}', which the delivery host '{}' cannot resolve.",
                spec.id,
                spec.credential,
                self.context.component_id()
            )
        })?;
        let api_key = (*view).clone().ok_or_else(|| {
            generic_error!(
                "Credential '{}' of recipient '{}' is not configured. Configure it, or remove the recipient.",
                spec.credential,
                spec.id
            )
        })?;
        let endpoint = ResolvedEndpoint::for_recipient(spec, &api_key)
            .map_err(|e| generic_error!("Recipient '{}' cannot be built: {}", spec.id, e))?;
        let endpoint_url = endpoint.endpoint().to_string();
        let domain = endpoint.endpoint().origin().ascii_serialization();
        let secrets = match spec.protocol {
            RecipientProtocol::DatadogIntake { .. } => self.secrets.clone(),
            // A rejected bearer token is not worth retrying on the chance that a secret is resolved again.
            RecipientProtocol::ClusterAgent => SecretsGate::new_fixed(&Secrets::default()),
        };
        let request_mapper: EndpointRequestMapperFactory<Body> =
            Arc::new(|endpoint| Box::new(for_hosted_recipient(endpoint)));

        Ok(RecipientPieces {
            context: self.context.clone(),
            config: self.base.for_recipient(settings, spec),
            secrets,
            client: self.client(),
            telemetry: self.telemetry.clone(),
            queue_telemetry: TransactionQueueTelemetry::from_builder(
                &self.metrics_builder,
                &endpoint_url,
                &spec.id,
                self.queue_telemetry.clone(),
            ),
            retry_telemetry: TransactionRetryTelemetry::from_builder(&self.metrics_builder, &domain),
            retry_cause_telemetry: saluki_io::net::util::retry::RetryCauseTelemetry::from_builder(
                &self.metrics_builder,
                &domain,
            ),
            endpoint_name: Arc::clone(&self.endpoint_name),
            route: route_of(spec),
            endpoint,
            request_mapper,
            emitter: self.emitter.clone(),
            // The routing loop opens the queue once it checked that no live loop owns its directory.
            queue: None,
        })
    }
}

/// Returns how a native forwarder came to send to the recipient's endpoint, which its routing and its I/O loop follow;
/// a declared recipient is routed as an additional endpoint.
pub(super) fn route_of(spec: &RecipientSpec) -> EndpointRoute {
    match &spec.origin {
        RecipientOrigin::Native(recipient) => match recipient.route {
            MetricsRecipientRoute::Primary => EndpointRoute::Primary,
            MetricsRecipientRoute::MetricsOverride => EndpointRoute::MetricsPrimary,
            MetricsRecipientRoute::Additional => EndpointRoute::Additional,
        },
        RecipientOrigin::Declared { .. } => EndpointRoute::Additional,
    }
}

/// Returns the current user information of the `http` and `https` proxies of `settings`, through their credentials.
pub(super) fn resolve_proxy_userinfo(
    credentials: &Arc<dyn CredentialSource>, settings: &DeliverySettings,
) -> [Option<MetaString>; 2] {
    let userinfo = |proxy: &Option<super::ProxyEndpoint>| {
        proxy
            .as_ref()
            .and_then(|proxy| credentials.resolve(&proxy.credential))
            .and_then(|view| (*view).clone())
    };
    [userinfo(&settings.proxy.http), userinfo(&settings.proxy.https)]
}

/// The factory a delivery host exposes: it builds recipients' pieces once the host is built.
#[derive(Default)]
pub(super) struct HostFactory {
    shared: OnceLock<Arc<HostShared>>,
}

impl RecipientFactory<Body> for HostFactory {
    fn build(&self, spec: &RecipientSpec, settings: &DeliverySettings) -> Result<RecipientPieces<Body>, GenericError> {
        let shared = self.shared.get().ok_or_else(|| {
            generic_error!(
                "Recipient '{}' cannot be built before its delivery host is built.",
                spec.id
            )
        })?;
        shared.build_pieces(spec, settings)
    }
}

/// The forwarder component of a delivery host.
pub(super) struct DeliveryHostForwarder {
    pub(super) shared: Arc<HostShared>,
    pub(super) control: ControlReceiver<DeliveryControl<Body>>,
    pub(super) events: Option<mpsc::Sender<DeliveryEvent>>,
    pub(super) boot: Vec<(RecipientSettings, RecipientPieces<Body>)>,
    pub(super) publication: BootPublication,
    pub(super) drain_deadline: Duration,
    pub(super) status: watch::Sender<DeliveryHostStatus>,
    pub(super) secrets: Option<SecretsGateRefresher>,
}

#[async_trait]
impl Forwarder for DeliveryHostForwarder {
    async fn run(mut self: Box<Self>, mut context: ForwarderContext) -> Result<(), GenericError> {
        let Self {
            shared,
            control,
            events,
            boot,
            publication,
            drain_deadline,
            status,
            secrets,
        } = *self;
        let mut health = context.take_health_handle();

        // The refresher is a supervised child of this component's supervisor, which stops it once this run-future
        // returns.
        if let Some(secrets) = secrets {
            secrets.spawn();
        }

        let mut router = Router::new(
            shared,
            events,
            drain_deadline,
            status,
            Arc::clone(&publication.settings),
        );
        router.start(boot, publication);
        router.run(context.payloads(), control, Some(&mut health)).await;
        Ok(())
    }
}
