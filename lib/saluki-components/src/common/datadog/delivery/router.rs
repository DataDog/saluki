//! A delivery host's routing loop: its recipient table, and the one task that serves the control mailbox, routes
//! payloads, and follows the recipients' exits, so that control messages and payloads are ordered in one place.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::{pending, Future},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime},
};

use futures::{stream::FuturesUnordered, StreamExt as _};
use http::Uri;
use saluki_core::{
    data_model::payload::{Payload, PayloadMetadata},
    execution::control::{ControlReceiver, ControlReply},
    health::Health,
    topology::PayloadsConsumer,
};
use saluki_error::{generic_error, GenericError};
use saluki_io::net::client::http::HttpClient;
use saluki_metrics::{Counter, Gauge};
use stringtheory::MetaString;
use tokio::{
    select,
    sync::{
        mpsc::{self, error::SendError, error::TrySendError, OwnedPermit},
        oneshot, watch,
    },
    task::{JoinError, JoinHandle},
    time::Instant,
};
use tracing::{debug, error, info, warn};

use super::{
    client::{build_client, ClientProfile},
    credentials::{CredentialChange, Credentials},
    host::{
        route_of, Body, BootPublication, DeliveryHostStatus, HostShared, RecipientConflict, RecipientState,
        RecipientStatus, MAX_DRAINING_RECIPIENTS,
    },
    Creator, DeliveryControl, DeliveryEvent, DeliverySettings, DrainReport, PayloadClasses, PayloadRecipients,
    PreparedRecipient, RecipientCloser, RecipientExit, RecipientHandle, RecipientLoop, RecipientOrigin,
    RecipientPieces, RecipientRef, RecipientSettings, RecipientSettingsSender, RecipientSlot, RecipientSpec,
    ReopenReply,
};
use crate::common::datadog::{
    api_key::ApiKeyChanges,
    endpoints::{EndpointRoute, MetricsRecipientRoute, ResolvedEndpoint},
    io::should_route_to_endpoint,
    protocol::{MetricsEndpointRouting, MetricsPayloadInfo},
    transaction::{Metadata, Transaction},
    validation::{
        ApiKeyValidationHandle, ApiKeyValidator, RecipientValidationTargets, ValidationReadiness, ValidationTargets,
    },
    METRIC_INTAKE_PATHS,
};

/// Payloads dropped because they were sent to a recipient the current lowering no longer declares.
const PLAN_RECIPIENT_WITHDRAWN_PAYLOADS_TOTAL: &str = "plan_recipient_withdrawn_payloads_total";

/// Recipients of a host, by state.
const PLAN_RECIPIENTS: &str = "plan_recipients";

/// Recipient retirements, by outcome.
const PLAN_RECIPIENT_RETIREMENTS_TOTAL: &str = "plan_recipient_retirements_total";

/// Recipients that send to the same endpoint with the same key as another.
const PLAN_RESOURCE_CONFLICTS: &str = "plan_resource_conflicts";

/// How many identities a host remembers the newest created incarnation of once it no longer runs or holds any
/// incarnation of them, so that what a long-running host keeps stays bounded however many distinct recipients it ever
/// ran. A controller repeats a creation only until it handled the answer of the part whose abort dropped the
/// incarnation, a few messages later, and a host runs at most 16 declared and 16 draining recipients beside its
/// native ones: this covers every incarnation such a window can drop many times over.
pub(super) const REMEMBERED_IDENTITIES: usize = 128;

/// Spawns recipients' I/O loops.
pub(super) trait LoopSpawner: Send + Sync {
    /// Spawns the I/O loop of the recipient `spec` describes, with its pieces, and returns its handle.
    fn spawn(&self, spec: &RecipientSpec, pieces: RecipientPieces<Body>) -> RecipientHandle<Body>;
}

/// Spawns recipients' native I/O loops, on the ambient supervisor.
pub(super) struct NativeLoops;

impl LoopSpawner for NativeLoops {
    fn spawn(&self, spec: &RecipientSpec, pieces: RecipientPieces<Body>) -> RecipientHandle<Body> {
        RecipientLoop::spawn(spec, pieces)
    }
}

/// Runs recipients' loops that send nothing, for tests: each takes what its host routes to it, counts the events, and
/// exits when it is asked to retire or close, or its input closes, reporting a retirement that delivered nothing.
#[cfg(any(test, feature = "test-util"))]
pub(super) struct DiscardingLoops {
    delivered: Arc<std::sync::atomic::AtomicU64>,
}

#[cfg(any(test, feature = "test-util"))]
impl DiscardingLoops {
    pub(super) fn new(delivered: Arc<std::sync::atomic::AtomicU64>) -> Self {
        Self { delivered }
    }
}

#[cfg(any(test, feature = "test-util"))]
impl LoopSpawner for DiscardingLoops {
    fn spawn(&self, _: &RecipientSpec, _: RecipientPieces<Body>) -> RecipientHandle<Body> {
        let (handle, detached) = RecipientHandle::detached(8);
        let super::DetachedLoop {
            mut transactions,
            mut close,
            exit,
        } = detached;
        let delivered = Arc::clone(&self.delivered);
        tokio::spawn(async move {
            loop {
                select! {
                    transaction = transactions.recv() => match transaction {
                        Some(transaction) => {
                            delivered.fetch_add(
                                transaction.metadata().event_count as u64,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        }
                        None => break,
                    },
                    _ = close.wait_for(|request| request != super::CloseRequest::None) => break,
                }
            }
            let _ = exit.send(RecipientExit::Retired(DrainReport::default()));
        });
        handle
    }
}

/// Returns the transaction metadata of a payload: its counts, its metrics protocol variant, and any legacy endpoint
/// routing.
pub(crate) fn transaction_metadata(payload_meta: &PayloadMetadata) -> Metadata {
    let mut transaction_meta =
        Metadata::from_event_and_data_point_count(payload_meta.event_count(), payload_meta.data_point_count());
    transaction_meta.payload_info = payload_meta.get::<MetricsPayloadInfo>().copied();
    transaction_meta.metrics_endpoint_routing = payload_meta.get::<Arc<MetricsEndpointRouting>>().cloned();
    transaction_meta
}

/// Where a recipient's lifecycle is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    /// Prepared, or reopened.
    Live,

    /// Released: routed to until the payloads received before the release, `target` of them in all, are routed.
    Releasing { target: u64 },

    /// Removed from routing, and dispatching its queued work until its deadline.
    Draining { drain: Drain },

    /// Its loop ended on an error, which was reported.
    Failed,
}

/// One drain of a recipient: when it gives up on queued work, and when it started.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Drain {
    deadline: Instant,
    since: Instant,
    since_wall: SystemTime,
}

/// One recipient of the table.
struct Entry {
    recipient: RecipientRef,
    spec: RecipientSpec,

    /// The endpoint the loop sends to, sharing the loop's credential and protocol cells.
    endpoint: ResolvedEndpoint,
    route: EndpointRoute,
    domain: String,

    /// Sends to the loop; `None` once the loop exited. A draining recipient keeps it, so that it can be reopened.
    sender: Option<mpsc::Sender<Transaction<Body>>>,
    close: RecipientCloser<Body>,

    /// Publishes the host's settings and the recipient's declaration to its loop.
    settings: RecipientSettingsSender,
    state: State,

    /// The failed incarnation of the same identity whose slot this one took, which an abort that drops this one puts
    /// back, so that the slot never empties under the leases that name it. Dropped once this one is committed.
    replaced: Option<Box<Entry>>,

    /// The preparations that created the recipient and have not been aborted.
    created_by: BTreeSet<Creator>,

    /// Whether no abort can drop the recipient: it ran from boot, or a publication committed it (the publication of
    /// the revision whose transaction created it, or one whose native set names it).
    committed: bool,

    /// The preparations that reopened the recipient since its release, and have not been aborted.
    reopened_by: BTreeSet<Creator>,

    /// The release a reopen cancelled, which an abort of every reopen returns the recipient to: `Some(drain)` once its
    /// drain had started, `Some(None)` while it waited at the input barrier.
    released: Option<Option<Drain>>,
}

impl Entry {
    fn routable(&self) -> bool {
        matches!(self.state, State::Live | State::Releasing { .. })
    }

    fn reported_state(&self) -> RecipientState {
        match self.state {
            State::Live => RecipientState::Live,
            State::Releasing { .. } => RecipientState::Releasing,
            State::Draining { .. } => RecipientState::Draining,
            State::Failed => RecipientState::Failed,
        }
    }

    fn is_declared(&self) -> bool {
        matches!(self.spec.origin, RecipientOrigin::Declared { .. })
    }

    /// Returns whether the entry, or a failed incarnation it keeps to put back, is an incarnation of `id`.
    fn names(&self, id: &MetaString) -> bool {
        self.recipient.id == *id || self.replaced.as_ref().is_some_and(|replaced| replaced.names(id))
    }

    /// Forgets `creator` as a creator of every failed incarnation the entry keeps to put back: the one it replaced,
    /// and, when that one failed before it was committed, the one that one replaced in turn.
    fn forget_creator_of_replaced(&mut self, creator: Creator) {
        let mut replaced = self.replaced.as_deref_mut();
        while let Some(failed) = replaced {
            failed.created_by.remove(&creator);
            replaced = failed.replaced.as_deref_mut();
        }
    }

    /// Takes the failed incarnation to put back in the slot once the entry is dropped: the newest of those it keeps
    /// that a preparation still holds or that was committed, if any, so that a committed failed marker returns even
    /// when the incarnation that replaced it failed too and was replaced again.
    fn take_restorable(&mut self) -> Option<Box<Entry>> {
        let mut replaced = self.replaced.take();
        while let Some(mut failed) = replaced {
            if failed.committed || !failed.created_by.is_empty() {
                return Some(failed);
            }
            replaced = failed.replaced.take();
        }
        None
    }

    /// Applies a declaration's settings, and the host's: the protocol at the recipient's next dispatch, the loop's
    /// limits and policy between its requests.
    fn apply(&mut self, spec: RecipientSpec, host: &DeliverySettings) {
        if let Some(protocol) = self.endpoint.protocol_cell() {
            protocol.store(&spec.protocol);
        }
        self.route = route_of(&spec);
        self.settings.publish(host, &spec);
        self.spec = spec;
    }

    fn start_drain(&mut self, drain: Drain) {
        self.state = State::Draining { drain };
        self.released = Some(Some(drain));
        self.close.retire(drain.deadline);
    }

    /// Returns what stays of the recipient once it leaves the table while payloads received up to `until` may still
    /// name it.
    fn tombstone(&self, until: u64) -> Tombstone {
        Tombstone {
            recipient: self.recipient.clone(),
            domain: self.domain.clone(),
            until,
        }
    }
}

/// A recipient that left the table before every payload that may name it was routed: a failed recipient that was
/// released, a loop that ended while its release waited at the input barrier, or a recipient an abort dropped.
///
/// Its slot is free for another recipient at once. The payloads received up to `until` that name the slot were stamped
/// for this recipient, so they are dropped for it and counted, and never reach a recipient prepared into the slot
/// since: that one's payloads are received after the message that freed the slot. The tombstone goes once they are
/// routed.
struct Tombstone {
    recipient: RecipientRef,
    domain: String,

    /// The count of received payloads up to which a payload may name the recipient.
    until: u64,
}

/// A preparation that waits for the exit of a loop an abort dropped, which still owns the queue directory of one of
/// its new recipients: the abort closed the loop as at shutdown, so it exits once its requests in flight end, and the
/// preparation proceeds then, as if it had arrived after that exit.
struct WaitingPrepare {
    creator: Creator,
    recipients: Vec<PreparedRecipient<Body>>,
    requires: Vec<RecipientRef>,
    reply: ControlReply,

    /// The dropped loop it waits for.
    owner: RecipientRef,
}

/// What a preparation did.
enum Prepared {
    /// Every recipient it lists is in the table.
    Done,

    /// A new recipient's queue directory is still owned by a loop an abort dropped: the preparation waits for that
    /// loop's exit, with its recipients unchanged.
    Waits {
        recipients: Vec<PreparedRecipient<Body>>,
        owner: RecipientRef,
    },
}

type ExitFuture = Pin<Box<dyn Future<Output = (RecipientRef, RecipientExit)> + Send>>;
type ReserveFuture = Pin<Box<dyn Future<Output = Result<OwnedPermit<Transaction<Body>>, SendError<()>>> + Send>>;

/// A client being built off the routing loop, and the profile it is built from.
struct ClientBuild {
    profile: ClientProfile,
    handle: JoinHandle<Result<HttpClient, GenericError>>,
}

/// The payload being routed: its transaction, the recipients it still goes to, in order, and the one it waits for room
/// in.
struct Routing {
    transaction: Transaction<Body>,
    targets: VecDeque<RecipientRef>,
    waiting: Option<(RecipientRef, ReserveFuture)>,
}

/// What a send to one recipient does.
enum Target {
    Send(mpsc::Sender<Transaction<Body>>),
    Withdrawn,
    Failed(String),
    Gone,
}

/// The API key validation of the host's native Datadog intake recipients, which follows the table.
struct Validation {
    /// Publishes the table's recipients to the validator.
    publisher: RecipientValidationTargets,

    /// What the validator is started with, until the routing loop starts it.
    start: Option<(ValidationTargets, ApiKeyChanges)>,
    handle: Option<ApiKeyValidationHandle>,

    /// Tells the validator that a recipient's key changed.
    changes: watch::Sender<()>,

    /// The recipients whose keys are validated, in the order the validator asks about them.
    targets: Vec<RecipientRef>,
}

impl Validation {
    fn new() -> Self {
        let (publisher, targets) = RecipientValidationTargets::new();
        let (changes, changed) = watch::channel(());
        Self {
            publisher,
            start: Some((targets, ApiKeyChanges::new(changed))),
            handle: None,
            changes,
            targets: Vec::new(),
        }
    }

    async fn wait(&mut self) -> ValidationReadiness {
        match &mut self.handle {
            Some(handle) => handle.wait_for_change().await,
            None => pending().await,
        }
    }
}

/// The telemetry the host records itself.
struct HostTelemetry {
    withdrawn: Counter,
    live: Gauge,
    draining: Gauge,
    failed: Gauge,
    drained: Counter,
    persisted: Counter,
    deadline: Counter,
    discarded: Counter,
    conflicts: Gauge,
}

impl HostTelemetry {
    fn new(shared: &HostShared) -> Self {
        let builder = &shared.metrics_builder;
        let host = shared.context.component_id().to_string();
        let tagged = |tag: (&'static str, &'static str)| [("host", host.clone()), (tag.0, tag.1.to_string())];
        Self {
            withdrawn: builder
                .register_counter_with_tags(PLAN_RECIPIENT_WITHDRAWN_PAYLOADS_TOTAL, [("host", host.clone())]),
            live: builder.register_gauge_with_tags(PLAN_RECIPIENTS, tagged(("state", "active"))),
            draining: builder.register_gauge_with_tags(PLAN_RECIPIENTS, tagged(("state", "draining"))),
            failed: builder.register_gauge_with_tags(PLAN_RECIPIENTS, tagged(("state", "failed"))),
            drained: builder
                .register_counter_with_tags(PLAN_RECIPIENT_RETIREMENTS_TOTAL, tagged(("outcome", "drained"))),
            persisted: builder
                .register_counter_with_tags(PLAN_RECIPIENT_RETIREMENTS_TOTAL, tagged(("outcome", "persisted"))),
            deadline: builder
                .register_counter_with_tags(PLAN_RECIPIENT_RETIREMENTS_TOTAL, tagged(("outcome", "deadline"))),
            discarded: builder
                .register_counter_with_tags(PLAN_RECIPIENT_RETIREMENTS_TOTAL, tagged(("outcome", "discarded"))),
            conflicts: builder.register_gauge_with_tags(PLAN_RESOURCE_CONFLICTS, [("host", host)]),
        }
    }

    /// Counts a retirement by what became of the work the recipient held: `deadline` if its drain reached its deadline,
    /// `discarded` if its close dropped work, `persisted` if its close wrote work to disk (a `persist` closure, which
    /// a recipient of the same identity adopts), and `drained` once its queue emptied by delivery.
    fn retired(&self, report: &DrainReport) {
        if report.deadline_reached {
            self.deadline.increment(1);
        } else if report.dropped > 0 {
            self.discarded.increment(1);
        } else if report.persisted > 0 {
            self.persisted.increment(1);
        } else {
            self.drained.increment(1);
        }
    }
}

/// What woke the routing loop.
enum Step {
    Exited(RecipientRef, RecipientExit),
    Live,
    Readiness(ValidationReadiness),
    Control(Option<(DeliveryControl<Body>, ControlReply)>),
    EventRoom(Option<mpsc::OwnedPermit<DeliveryEvent>>),
    Credential(Option<CredentialChange>),
    ClientBuilt(Result<Result<HttpClient, GenericError>, JoinError>),
    Reserved(Result<OwnedPermit<Transaction<Body>>, SendError<()>>),
    Payload(Option<Payload>),
}

/// The state of a delivery host's routing loop.
pub(super) struct Router {
    shared: Arc<HostShared>,
    drain_deadline: Duration,
    table: BTreeMap<RecipientSlot, Entry>,

    /// The recipients that left the table while payloads in the host's input may still name them.
    tombstones: Vec<Tombstone>,

    /// The exits of every loop that has not exited yet, dropped ones included.
    exits: FuturesUnordered<ExitFuture>,

    /// The queue directory of every loop that has not exited yet, and the recipient that owns it.
    directories: BTreeMap<String, RecipientRef>,

    /// The preparations that wait for a dropped loop's exit, in the order they arrived.
    waiting_prepares: VecDeque<WaitingPrepare>,

    /// The newest incarnation the host created of each identity it runs a loop of or holds in its table: an
    /// incarnation is created at most once, so a create of one the host dropped, or of an older one, fails.
    created: BTreeMap<MetaString, u64>,

    /// The newest incarnation created of each identity the host no longer runs or holds, for the latest
    /// [`REMEMBERED_IDENTITIES`] of them, so that a creation a controller repeats after an abort dropped the
    /// incarnation still fails.
    forgotten: VecDeque<(MetaString, u64)>,
    revision: u64,
    settings: Arc<DeliverySettings>,
    native_set: Vec<RecipientRef>,
    withdrawn: BTreeSet<MetaString>,

    /// How many payloads the host received.
    received: u64,

    /// The revision under which the host last logged dropping a payload that no recipient of the native set takes.
    unroutable_logged: Option<u64>,
    routing: Option<Routing>,
    events: Option<mpsc::Sender<DeliveryEvent>>,
    unsent: VecDeque<DeliveryEvent>,
    credentials: Credentials,

    /// What the client the host sends through was built from.
    client_profile: ClientProfile,

    /// The client the host builds for its current settings, while the one it sends through does not fit them.
    client_build: Option<ClientBuild>,

    /// Why the last client built for the host's current settings could not be built, while it still sends through an
    /// older one.
    client_failure: Option<String>,

    /// How many clients the host started building.
    client_builds: u64,
    validation: Validation,
    readiness: Option<ValidationReadiness>,
    status: watch::Sender<DeliveryHostStatus>,
    telemetry: HostTelemetry,
}

impl Router {
    pub(super) fn new(
        shared: Arc<HostShared>, events: Option<mpsc::Sender<DeliveryEvent>>, drain_deadline: Duration,
        status: watch::Sender<DeliveryHostStatus>, settings: Arc<DeliverySettings>,
    ) -> Self {
        Self {
            telemetry: HostTelemetry::new(&shared),
            credentials: Credentials::new(Arc::clone(&shared.credentials)),
            client_profile: shared.client_profile.clone(),
            shared,
            drain_deadline,
            table: BTreeMap::new(),
            tombstones: Vec::new(),
            exits: FuturesUnordered::new(),
            directories: BTreeMap::new(),
            waiting_prepares: VecDeque::new(),
            created: BTreeMap::new(),
            forgotten: VecDeque::new(),
            revision: 0,
            settings,
            native_set: Vec::new(),
            withdrawn: BTreeSet::new(),
            received: 0,
            unroutable_logged: None,
            routing: None,
            events,
            unsent: VecDeque::new(),
            client_build: None,
            client_failure: None,
            client_builds: 0,
            validation: Validation::new(),
            readiness: None,
            status,
        }
    }

    /// Runs the boot publication: spawns its recipients' loops, and routes with its native set from the first payload.
    pub(super) fn start(
        &mut self, boot: Vec<(RecipientSettings, RecipientPieces<Body>)>, publication: BootPublication,
    ) {
        self.revision = publication.revision;
        self.settings = publication.settings;
        self.native_set = publication.native_set;
        self.withdrawn = publication.withdrawn;
        for (settings, pieces) in boot {
            let directory = pieces.queue_directory(&settings.spec);
            self.insert(
                PreparedRecipient {
                    recipient: settings.recipient,
                    spec: settings.spec,
                    pieces,
                },
                directory,
                None,
            );
        }
        self.update_client();
        self.changed();
    }

    /// Runs the routing loop until the host's input ends, then closes every recipient as at process shutdown.
    pub(super) async fn run(
        &mut self, input: &mut PayloadsConsumer, control: ControlReceiver<DeliveryControl<Body>>,
        mut health: Option<&mut Health>,
    ) {
        // The mailbox is read through an `Option`, which is `None` once every sender is gone.
        let mut control = Some(control);
        if let Some(control) = &control {
            control.mark_running();
        }
        let mut input_open = true;
        debug!(recipients = self.table.len(), "Delivery host started.");

        loop {
            if let Some(readiness) = self.readiness.take() {
                if let Some(health) = health.as_deref_mut() {
                    match readiness {
                        ValidationReadiness::Ready => health.mark_ready(),
                        ValidationReadiness::NotReady => health.mark_not_ready(),
                    }
                }
            }
            if !input_open && self.routing.is_none() {
                break;
            }

            let step = select! {
                biased;

                // Exits first, so that a reopen of a recipient whose loop already exited is answered as such.
                Some((recipient, outcome)) = self.exits.next(), if !self.exits.is_empty() => {
                    Step::Exited(recipient, outcome)
                },
                _ = live(&mut health) => Step::Live,
                readiness = self.validation.wait() => Step::Readiness(readiness),
                message = recv(&mut control) => Step::Control(message),
                permit = reserve_event(&self.events), if !self.unsent.is_empty() => Step::EventRoom(permit),
                change = self.credentials.next_change(), if !self.credentials.is_idle() => Step::Credential(change),
                built = client_built(&mut self.client_build) => Step::ClientBuilt(built),
                reserved = waiting(&mut self.routing) => Step::Reserved(reserved),
                payload = input.next(), if input_open && self.routing.is_none() => Step::Payload(payload),
            };

            match step {
                Step::Exited(recipient, outcome) => {
                    self.exited(recipient, outcome);
                    self.resume_prepares().await;
                }
                Step::Live => continue,
                Step::Readiness(readiness) => self.readiness = Some(readiness),
                Step::Control(Some((message, reply))) => self.control(message, reply, input.queued()).await,
                Step::Control(None) => {
                    debug!("Delivery host's control mailbox closed: the recipient table no longer changes.");
                    control = None;
                }
                Step::EventRoom(Some(permit)) => {
                    if let Some(event) = self.unsent.pop_front() {
                        permit.send(event);
                    }
                    self.flush_events();
                }
                Step::EventRoom(None) => {
                    warn!(
                        unsent = self.unsent.len(),
                        "Delivery host's event channel closed: recipient exits are no longer reported."
                    );
                    self.unsent.clear();
                    self.events = None;
                }
                Step::Credential(Some(change)) => self.credential_changed(change),
                Step::Credential(None) => {}
                Step::ClientBuilt(built) => self.client_built(built),
                Step::Reserved(reserved) => self.reserved(reserved),
                Step::Payload(Some(payload)) => {
                    self.received += 1;
                    self.route(payload);
                }
                Step::Payload(None) => {
                    debug!("Delivery host's input closed. Closing every recipient as at shutdown...");
                    input_open = false;
                }
            }
            self.pass_barrier();
        }

        self.shutdown();
        debug!("Delivery host stopped.");
    }

    /// Closes every recipient as at process shutdown: loops stop popping queued work, finish their requests in flight,
    /// and persist or drop what is left, a draining loop included.
    fn shutdown(&mut self) {
        for waiting in self.waiting_prepares.drain(..) {
            waiting.reply.reject(generic_error!(
                "The delivery host stopped while the preparation of {} waited for the exit of recipient {}.",
                waiting.creator,
                waiting.owner
            ));
        }
        for entry in self.table.values() {
            entry.close.shutdown();
        }
        // Dropping the senders closes each loop's input.
        self.table.clear();
        self.flush_events();
    }

    // Control.

    /// Handles one control message, and answers it once what it changed is in effect: a send it withdrew is
    /// cancelled, a release that passed the barrier drains, and the status shows the new table.
    ///
    /// A preparation that waits for a dropped loop's exit is answered once it proceeds, after that exit.
    async fn control(&mut self, message: DeliveryControl<Body>, reply: ControlReply, queued: usize) {
        let mut reply = Some(reply);
        let outcome = match message {
            DeliveryControl::Prepare {
                creator,
                recipients,
                requires,
            } => match self.prepare(creator, recipients, &requires).await {
                Ok(Prepared::Done) => Ok(()),
                Ok(Prepared::Waits { recipients, owner }) => {
                    if let Some(reply) = reply.take() {
                        self.wait_for_exit(WaitingPrepare {
                            creator,
                            recipients,
                            requires,
                            reply,
                            owner,
                        });
                    }
                    Ok(())
                }
                Err(error) => Err(error),
            },
            DeliveryControl::Publish {
                revision,
                native_set,
                settings,
                recipients,
                withdrawn,
            } => self.publish(revision, native_set, settings, recipients, withdrawn),
            DeliveryControl::Release { recipients } => {
                self.release(recipients, queued);
                Ok(())
            }
            DeliveryControl::Reopen {
                creator,
                recipients,
                reply: answers,
            } => {
                let answered = self.reopen(creator, recipients);
                if answers.send(answered).is_err() {
                    debug!(%creator, "Reopen answers were not awaited.");
                }
                Ok(())
            }
            DeliveryControl::Abort { generation } => {
                self.abort(Creator::Generation(generation), queued);
                Ok(())
            }
            DeliveryControl::AbortResources { revision } => {
                self.abort(Creator::Transaction(revision), queued);
                Ok(())
            }
        };
        self.revalidate_waiting();
        self.pass_barrier();
        self.changed();
        if let Some(reply) = reply {
            match outcome {
                Ok(()) => reply.ack(),
                Err(error) => reply.reject(error),
            }
        }
    }

    /// Holds a preparation until the dropped loop that owns a queue directory it needs exits.
    fn wait_for_exit(&mut self, waiting: WaitingPrepare) {
        debug!(
            creator = %waiting.creator,
            owner = %waiting.owner,
            "Preparation waits for the exit of a loop an abort dropped, which still owns a queue directory it needs."
        );
        self.waiting_prepares.push_back(waiting);
    }

    /// Runs again, in the order they arrived, the preparations whose dropped loop exited, and answers each that no
    /// longer waits.
    async fn resume_prepares(&mut self) {
        let owned = |router: &Self, owner: &RecipientRef| router.directories.values().any(|held| held == owner);
        if self.waiting_prepares.iter().all(|waiting| owned(self, &waiting.owner)) {
            return;
        }
        for waiting in std::mem::take(&mut self.waiting_prepares) {
            if owned(self, &waiting.owner) {
                self.waiting_prepares.push_back(waiting);
                continue;
            }
            let WaitingPrepare {
                creator,
                recipients,
                requires,
                reply,
                ..
            } = waiting;
            match self.prepare(creator, recipients, &requires).await {
                Ok(Prepared::Done) => reply.ack(),
                Ok(Prepared::Waits { recipients, owner }) => self.wait_for_exit(WaitingPrepare {
                    creator,
                    recipients,
                    requires,
                    reply,
                    owner,
                }),
                Err(error) => reply.reject(error),
            }
        }
        self.revalidate_waiting();
        self.pass_barrier();
        self.changed();
    }

    fn held(&self, recipient: &RecipientRef) -> Option<&Entry> {
        self.table
            .get(&recipient.slot)
            .filter(|entry| entry.recipient == *recipient)
    }

    /// Returns the recipient that stands for one the published native set names: that recipient, or, once it failed,
    /// its next incarnation in its slot, which a publication names only later, as payloads that name the slot reach it
    /// at once.
    fn native_member(&self, recipient: &RecipientRef) -> Option<&Entry> {
        self.table
            .get(&recipient.slot)
            .filter(|entry| entry.recipient.id == recipient.id && entry.recipient.incarnation >= recipient.incarnation)
    }

    fn held_mut(&mut self, recipient: &RecipientRef) -> Option<&mut Entry> {
        self.table
            .get_mut(&recipient.slot)
            .filter(|entry| entry.recipient == *recipient)
    }

    /// Prepares recipients: checks every one, opens the queue of each new one on its directory, which no live loop owns
    /// then, and spawns its loop. A preparation adds all its recipients, or none.
    ///
    /// A new recipient whose directory a loop an abort dropped still owns makes the preparation wait for that loop's
    /// exit ([`Prepared::Waits`]): the dropped loop closes as at shutdown, and a controller that prepares an identity's
    /// next incarnation right after an abort, as it does when a newer candidate cancels a preparation, cannot know
    /// when it exits. A directory a loop of the table owns fails the preparation.
    async fn prepare(
        &mut self, creator: Creator, recipients: Vec<PreparedRecipient<Body>>, requires: &[RecipientRef],
    ) -> Result<Prepared, GenericError> {
        for required in requires {
            match self.held(required).map(|entry| entry.state) {
                Some(State::Live) => {}
                state => {
                    return Err(generic_error!(
                        "Recipient {} is required by {}, but {}. Prepare it again first.",
                        required,
                        creator,
                        describe(state)
                    ))
                }
            }
        }

        // Every new recipient is checked before any is added, so a preparation adds all or none.
        let mut repeated = Vec::new();
        let mut added: Vec<(usize, String)> = Vec::new();
        let mut waits_for = None;
        for (index, prepared) in recipients.iter().enumerate() {
            let recipient = &prepared.recipient;
            if prepared.spec.id != recipient.id {
                return Err(generic_error!(
                    "Recipient {} is declared as '{}'.",
                    recipient,
                    prepared.spec.id
                ));
            }
            if self.held(recipient).is_some() {
                repeated.push(recipient.clone());
                continue;
            }
            if let Some(held) = self.table.get(&recipient.slot) {
                let replaces_failed = held.state == State::Failed
                    && held.recipient.id == recipient.id
                    && held.recipient.incarnation < recipient.incarnation;
                if !replaces_failed {
                    return Err(generic_error!(
                        "Slot {} holds recipient {}, which {}, and cannot be prepared for recipient {}.",
                        recipient.slot.get(),
                        held.recipient,
                        describe(Some(held.state)),
                        recipient
                    ));
                }
            }
            // An incarnation is created at most once: one the host dropped (an abort's) is not created again under
            // the same reference, which would start a second loop whose exit the installer could not tell apart.
            if let Some(newest) = self
                .newest_created(&recipient.id)
                .filter(|newest| recipient.incarnation <= *newest)
            {
                return Err(generic_error!(
                    "Recipient {} cannot be created: the host created incarnation {} of it already, and creates an \
                     incarnation once. Prepare its next incarnation.",
                    recipient,
                    newest
                ));
            }
            if let Some((other, _)) = added
                .iter()
                .find(|(other, _)| recipients[*other].recipient.slot == recipient.slot)
            {
                return Err(generic_error!(
                    "Recipients {} and {} are prepared into the same slot.",
                    recipients[*other].recipient,
                    recipient
                ));
            }
            let directory = prepared.pieces.queue_directory(&prepared.spec);
            if let Some((other, _)) = added.iter().find(|(_, other)| *other == directory) {
                return Err(generic_error!(
                    "Recipients {} and {} are prepared on the same queue directory '{}'.",
                    recipients[*other].recipient,
                    recipient,
                    directory
                ));
            }
            if let Some(owner) = self.directories.get(&directory) {
                if self.table.values().any(|entry| entry.recipient == *owner) {
                    return Err(generic_error!(
                        "The queue directory '{}' of recipient {} is still owned by recipient {}, whose loop has not \
                         exited. Prepare it again once that loop's exit is reported.",
                        directory,
                        recipient,
                        owner
                    ));
                }
                // An abort dropped the owner, whose loop closes as at shutdown: the preparation waits for its exit.
                waits_for.get_or_insert_with(|| owner.clone());
            }
            if matches!(prepared.spec.origin, RecipientOrigin::Declared { .. }) {
                // A recipient that receives nothing new never counts: neither a draining one nor a withdrawn one,
                // which a renamed credential's old recipient is until its generation retires.
                let key = self.key_of(&prepared.pieces.endpoint);
                let duplicate = self
                    .table
                    .values()
                    .filter(|entry| self.receives(entry))
                    .map(|entry| (&entry.recipient, &entry.endpoint))
                    .chain(
                        added
                            .iter()
                            .map(|(other, _)| (&recipients[*other].recipient, &recipients[*other].pieces.endpoint)),
                    )
                    .find(|(_, endpoint)| self.key_of(endpoint) == key);
                if let Some((duplicate, endpoint)) = duplicate {
                    return Err(generic_error!(
                        "Recipient {} duplicates recipient {}: both send to '{}' with the same key.",
                        recipient,
                        duplicate,
                        endpoint.endpoint()
                    ));
                }
            }
            added.push((index, directory));
        }
        if let Some(owner) = waits_for {
            return Ok(Prepared::Waits { recipients, owner });
        }

        // Opening a queue creates its directory, lists it, and removes its outdated files, so it happens only now that
        // no loop owns the directory.
        let mut recipients = recipients.into_iter().map(Some).collect::<Vec<_>>();
        let mut opened = Vec::with_capacity(added.len());
        for (index, directory) in added {
            if let Some(mut prepared) = recipients.get_mut(index).and_then(Option::take) {
                prepared.pieces.open_queue(&prepared.spec).await?;
                opened.push((prepared, directory));
            }
        }

        for recipient in repeated {
            if let Some(entry) = self.held_mut(&recipient) {
                entry.created_by.insert(creator);
            }
            debug!(%recipient, %creator, "Recipient is prepared already.");
        }
        for (prepared, directory) in opened {
            self.insert(prepared, directory, Some(creator));
        }
        Ok(Prepared::Done)
    }

    /// Returns the newest incarnation of `id` the host created, if it runs or holds one, or remembers the identity.
    fn newest_created(&self, id: &MetaString) -> Option<u64> {
        self.created.get(id).copied().or_else(|| {
            self.forgotten
                .iter()
                .rev()
                .find(|(forgotten, _)| forgotten == id)
                .map(|(_, newest)| *newest)
        })
    }

    /// Forgets an identity's newest created incarnation into the bounded memory of identities the host no longer runs
    /// or holds, once no loop of it runs and no entry of the table names it.
    fn forget_if_gone(&mut self, id: &MetaString) {
        let runs = self.directories.values().any(|owner| owner.id == *id);
        if runs || self.table.values().any(|entry| entry.names(id)) {
            return;
        }
        if let Some(newest) = self.created.remove(id) {
            self.forgotten.push_back((id.clone(), newest));
            while self.forgotten.len() > REMEMBERED_IDENTITIES {
                self.forgotten.pop_front();
            }
        }
    }

    /// Returns whether payloads still reach a recipient: it is routable, and the current lowering declares it.
    fn receives(&self, entry: &Entry) -> bool {
        entry.routable() && !self.withdrawn.contains(&entry.recipient.id)
    }

    /// Returns what tells two recipients' sends apart: their endpoint, and the keyed fingerprint of their key.
    fn key_of(&self, endpoint: &ResolvedEndpoint) -> (String, u64) {
        (
            endpoint.endpoint().to_string(),
            self.credentials.fingerprint(&endpoint.api_key()),
        )
    }

    /// Adds a recipient to the table and spawns its loop. `creator` is `None` for a recipient the host runs from boot.
    fn insert(&mut self, prepared: PreparedRecipient<Body>, directory: String, creator: Option<Creator>) {
        let PreparedRecipient {
            recipient,
            spec,
            pieces,
        } = prepared;
        let replaced = self.table.remove(&recipient.slot).map(|failed| {
            debug!(%recipient, failed = %failed.recipient, "Recipient's next incarnation takes over its slot.");
            Box::new(failed)
        });
        let endpoint = pieces.endpoint.clone();
        let domain = endpoint.endpoint().origin().ascii_serialization();
        let route = route_of(&spec);
        let RecipientHandle {
            sender,
            close,
            settings,
            exited,
        } = self.shared.loops.spawn(&spec, pieces);
        self.exits.push(Box::pin(exit_of(recipient.clone(), exited)));
        self.directories.insert(directory, recipient.clone());
        let remembered = self
            .forgotten
            .iter()
            .position(|(forgotten, _)| *forgotten == recipient.id)
            .and_then(|position| self.forgotten.remove(position))
            .map_or(0, |(_, newest)| newest);
        let newest = self.created.entry(recipient.id.clone()).or_insert(remembered);
        *newest = (*newest).max(recipient.incarnation);
        if !self
            .credentials
            .register(&recipient, &spec.credential, endpoint.api_key_cell().clone())
        {
            warn!(
                %recipient,
                credential = %spec.credential,
                "Recipient's credential cannot be followed: it keeps its key."
            );
        }
        debug!(%recipient, endpoint = %endpoint.endpoint(), "Recipient prepared.");
        self.table.insert(
            recipient.slot,
            Entry {
                recipient,
                spec,
                endpoint,
                route,
                domain,
                sender: Some(sender),
                close,
                settings,
                state: State::Live,
                replaced,
                created_by: creator.into_iter().collect(),
                committed: creator.is_none(),
                reopened_by: BTreeSet::new(),
                released: None,
            },
        );
    }

    fn publish(
        &mut self, revision: u64, native_set: Vec<RecipientRef>, settings: Arc<DeliverySettings>,
        recipients: Vec<RecipientSettings>, withdrawn: BTreeSet<MetaString>,
    ) -> Result<(), GenericError> {
        if revision < self.revision {
            debug!(
                revision,
                published = self.revision,
                "Ignoring a publication older than the one the host runs."
            );
            return Ok(());
        }
        if revision == self.revision {
            // A publication repeated after an installer restart, which may follow a repeated preparation.
            commit(&mut self.table, revision, &self.native_set);
            debug!(revision, "Publication applied already.");
            return Ok(());
        }
        for recipient in &native_set {
            match self.held(recipient).map(|entry| entry.state) {
                // A failed recipient is still the native set's: its payloads are dropped and counted until its next
                // incarnation is live, and its failure may have raced the publication.
                Some(State::Live | State::Failed) => {}
                state => {
                    return Err(generic_error!(
                        "The publication of revision {} names recipient {} in its native set, but {}. Prepare or \
                         reopen it first.",
                        revision,
                        recipient,
                        describe(state)
                    ))
                }
            }
        }

        self.revision = revision;
        commit(&mut self.table, revision, &native_set);
        self.settings = settings;
        let mut declared = BTreeSet::new();
        for declaration in recipients {
            let host = Arc::clone(&self.settings);
            match self.held_mut(&declaration.recipient) {
                Some(entry) if declaration.spec.id == entry.recipient.id => {
                    declared.insert(declaration.recipient.clone());
                    entry.apply(declaration.spec, &host);
                }
                _ => {
                    debug!(
                        recipient = %declaration.recipient,
                        "Ignoring settings of a recipient the host does not hold."
                    )
                }
            }
        }
        // The host's settings reach every other recipient's loop too; a loop whose settings do not change is not woken.
        for entry in self.table.values() {
            if !declared.contains(&entry.recipient) {
                entry.settings.publish(&self.settings, &entry.spec);
            }
        }
        self.native_set = native_set;
        self.withdrawn = withdrawn;
        // Unchanged settings build no client, unless the last build for them failed.
        self.update_client();
        debug!(
            revision,
            native_set = self.native_set.len(),
            withdrawn = self.withdrawn.len(),
            "Publication applied."
        );
        Ok(())
    }

    fn release(&mut self, recipients: Vec<RecipientRef>, queued: usize) {
        let target = self.received + queued as u64;
        for recipient in recipients {
            let Some(entry) = self.held_mut(&recipient) else {
                debug!(%recipient, "Ignoring a release of a recipient the host does not hold.");
                continue;
            };
            match entry.state {
                State::Live => {
                    // A release starts a new drain, which no earlier reopen holds open.
                    entry.state = State::Releasing { target };
                    entry.released = Some(None);
                    entry.reopened_by.clear();
                    debug!(
                        %recipient,
                        target, "Recipient released: it drains once the payloads before the release are routed."
                    );
                }
                // A repeated release keeps the drain it started.
                State::Releasing { .. } | State::Draining { .. } => {}
                // Its exit was reported when it failed: the release frees its slot, and the payloads before the
                // release that name it are dropped for it, even if another recipient takes the slot meanwhile.
                State::Failed => {
                    if let Some(entry) = self.table.remove(&recipient.slot) {
                        self.tombstones.push(entry.tombstone(target));
                    }
                    self.forget_if_gone(&recipient.id);
                    debug!(%recipient, target, "Failed recipient released.");
                }
            }
        }
    }

    fn reopen(&mut self, creator: Creator, recipients: Vec<RecipientSettings>) -> Vec<(RecipientRef, ReopenReply)> {
        let host = Arc::clone(&self.settings);
        recipients
            .into_iter()
            .map(|declaration| {
                let recipient = declaration.recipient;
                let reply = match self.held_mut(&recipient) {
                    // The loop decides, atomically with its own decision to close for good: a loop whose close became
                    // final is answered `Exited`, and its exit follows on the event channel.
                    Some(entry) if entry.state != State::Failed => match entry.close.reopen() {
                        ReopenReply::Reopened => {
                            entry.state = State::Live;
                            entry.reopened_by.insert(creator);
                            if declaration.spec.id == entry.recipient.id {
                                entry.apply(declaration.spec, &host);
                            }
                            ReopenReply::Reopened
                        }
                        ReopenReply::Exited => ReopenReply::Exited,
                    },
                    // Its loop exited: its exit is, or was, reported on the event channel.
                    _ => ReopenReply::Exited,
                };
                debug!(%recipient, %creator, ?reply, "Recipient reopen answered.");
                (recipient, reply)
            })
            .collect()
    }

    /// Undoes what `creator` did: drops the recipients only it created, whose exits are then reported as any loop's,
    /// returns those only it held reopened to their release, and rejects its preparations that wait for a dropped
    /// loop's exit.
    ///
    /// A dropped recipient that took the slot of a failed incarnation of its identity gives the slot back to it, while
    /// a preparation still holds it or it was committed, so that the leases that name the slot keep finding the failed
    /// marker, whose payloads are dropped and counted.
    fn abort(&mut self, creator: Creator, queued: usize) {
        let target = self.received + queued as u64;
        // A preparation that waits for a dropped loop's exit added nothing, and never will.
        let (aborted, waiting) = std::mem::take(&mut self.waiting_prepares)
            .into_iter()
            .partition::<VecDeque<_>, _>(|waiting| waiting.creator == creator);
        self.waiting_prepares = waiting;
        for waiting in aborted {
            debug!(%creator, owner = %waiting.owner, "Preparation aborted while it waited for a dropped loop's exit.");
            waiting.reply.reject(generic_error!(
                "The preparation of {} was aborted while it waited for the exit of recipient {}, which owns the \
                 queue directory of one of its recipients.",
                creator,
                waiting.owner
            ));
        }
        let mut dropped = Vec::new();
        for (slot, entry) in &mut self.table {
            entry.forget_creator_of_replaced(creator);
            let created = entry.created_by.remove(&creator);
            if created && entry.created_by.is_empty() && !entry.committed {
                dropped.push(*slot);
                continue;
            }
            let reopened = entry.reopened_by.remove(&creator);
            if reopened && entry.reopened_by.is_empty() && entry.state == State::Live {
                match entry.released {
                    Some(Some(drain)) => entry.start_drain(drain),
                    Some(None) => entry.state = State::Releasing { target },
                    None => {}
                }
            }
        }
        for slot in dropped {
            if let Some(mut entry) = self.table.remove(&slot) {
                entry.close.shutdown();
                self.credentials.unregister(&entry.recipient, &entry.spec.credential);
                debug!(recipient = %entry.recipient, %creator, "Recipient dropped by an abort.");
                match entry.take_restorable() {
                    Some(failed) => {
                        debug!(recipient = %failed.recipient, "Failed recipient takes its slot back.");
                        self.table.insert(slot, *failed);
                    }
                    // Nothing that committed named it, but a payload in the input that names it must not reach a
                    // recipient prepared into its slot next.
                    None => self.tombstones.push(entry.tombstone(target)),
                }
            }
        }
    }

    // Exits and events.

    fn exited(&mut self, recipient: RecipientRef, outcome: RecipientExit) {
        self.directories.retain(|_, owner| *owner != recipient);
        let mut credential = None;
        if let Some(entry) = self
            .table
            .get_mut(&recipient.slot)
            .filter(|entry| entry.recipient == recipient)
        {
            credential = Some(entry.spec.credential.clone());
            match entry.state {
                // Its drain started once every payload that may name it was routed.
                State::Draining { .. } => {
                    self.table.remove(&recipient.slot);
                    if let RecipientExit::Retired(report) = &outcome {
                        self.telemetry.retired(report);
                    }
                }
                // It ended while its release waited at the input barrier: its slot is free, but the payloads before
                // the release that name it are dropped for it, even if another recipient takes the slot meanwhile.
                State::Releasing { target } => {
                    let tombstone = entry.tombstone(target);
                    self.table.remove(&recipient.slot);
                    self.tombstones.push(tombstone);
                    if let RecipientExit::Retired(report) = &outcome {
                        self.telemetry.retired(report);
                    }
                    warn!(
                        %recipient,
                        ?outcome,
                        "Released recipient's loop ended before the payloads sent to it were routed: they are dropped."
                    );
                }
                State::Live => {
                    entry.state = State::Failed;
                    entry.sender = None;
                    warn!(
                        %recipient,
                        ?outcome,
                        "Recipient's loop ended: payloads to it are dropped until its next incarnation is live."
                    );
                }
                State::Failed => {}
            }
        }
        if let Some(credential) = credential {
            self.credentials.unregister(&recipient, &credential);
        }
        self.forget_if_gone(&recipient.id);
        debug!(%recipient, ?outcome, "Recipient exited.");
        self.report(DeliveryEvent::RecipientExited { recipient, outcome });
        // A send waiting for room in the loop that ended stops waiting.
        self.revalidate_waiting();
        self.changed();
    }

    fn report(&mut self, event: DeliveryEvent) {
        if self.events.is_some() {
            self.unsent.push_back(event);
            self.flush_events();
        }
    }

    fn flush_events(&mut self) {
        let Some(events) = &self.events else {
            return;
        };
        while let Some(event) = self.unsent.pop_front() {
            match events.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    // Held until there is room, which the routing loop waits for without blocking anything else.
                    self.unsent.push_front(event);
                    return;
                }
                Err(TrySendError::Closed(_)) => {
                    self.unsent.clear();
                    return;
                }
            }
        }
    }

    // Routing.

    fn route(&mut self, payload: Payload) {
        let Some(payload) = payload.try_into_http_payload() else {
            return;
        };
        let (payload_meta, request) = payload.into_parts();
        let recipients = payload_meta.get::<PayloadRecipients>().cloned();
        // The host decides which recipients take a payload, so no recipient's loop filters it again (by the protocol
        // it was spawned with, which a publication may have changed since).
        let transaction = Transaction::from_original(transaction_metadata(&payload_meta), request);
        let targets = match recipients {
            Some(recipients) => self.resolved_targets(&recipients, transaction.metadata()),
            None => {
                let (targets, taken) = self.native_targets(transaction.request_uri(), transaction.metadata());
                if !taken {
                    self.count_unroutable(transaction.metadata());
                }
                targets
            }
        };
        self.routing = Some(Routing {
            transaction,
            targets,
            waiting: None,
        });
        self.advance();
    }

    /// Returns the recipients a payload names, in slot order: every one, unfiltered, since its lease resolved them for
    /// the payload's own protocol variant.
    ///
    /// A slot whose recipient left the table after this payload was received names that recipient, whichever one the
    /// slot holds now: the payload is dropped for it, and counted.
    fn resolved_targets(&self, recipients: &PayloadRecipients, metadata: &Metadata) -> VecDeque<RecipientRef> {
        // The payload being routed is the last one received.
        let sequence = self.received;
        recipients
            .slots()
            .iter()
            .filter_map(|slot| {
                if let Some(tombstone) = self
                    .tombstones
                    .iter()
                    .find(|tombstone| tombstone.recipient.slot == *slot)
                {
                    // A tombstone goes as soon as the last payload it covers was routed.
                    debug_assert!(sequence <= tombstone.until, "a tombstone outlived its payloads");
                    self.shared
                        .telemetry
                        .track_permanently_failed_transaction(metadata, None, &tombstone.domain);
                    debug!(
                        recipient = %tombstone.recipient,
                        "Dropping a payload sent to a recipient that left the host's table before it was routed."
                    );
                    return None;
                }
                match self.table.get(slot) {
                    Some(entry) => Some(entry.recipient.clone()),
                    None => {
                        // The installer releases a recipient only after the last payload naming it was received (the
                        // encode host's drain acknowledgment), and a slot whose recipient left the table before those
                        // payloads were routed keeps a tombstone until they are.
                        debug_assert!(false, "a payload names slot {}, which holds no recipient", slot.get());
                        error!(
                            slot = slot.get(),
                            "Dropping a payload sent to a slot that holds no recipient."
                        );
                        self.shared.telemetry.track_dropped_items(1);
                        self.shared.telemetry.track_dropped_events(metadata.event_count as u64);
                        None
                    }
                }
            })
            .collect()
    }

    /// Returns the recipients of the native set a payload that names none goes to, with the native rule: metrics go to
    /// the alternate metrics intake in place of the primary endpoint when there is one, other payloads never go to it,
    /// only recipients of every payload class receive payloads other than metrics, a legacy endpoint routing selects
    /// by policy identity, and a recipient takes only the metrics variants it accepts.
    ///
    /// Also returns whether any recipient of the native set takes the payload's class: a payload no endpoint takes is
    /// dropped, and counted, where a native forwarder, whose primary endpoint always exists, would have sent it.
    fn native_targets(&self, uri: &Uri, metadata: &Metadata) -> (VecDeque<RecipientRef>, bool) {
        let is_metrics = METRIC_INTAKE_PATHS.contains(&uri.path());
        let entries = self
            .native_set
            .iter()
            .filter_map(|recipient| self.native_member(recipient))
            .collect::<Vec<_>>();
        let has_metrics_primary = entries.iter().any(|entry| entry.route == EndpointRoute::MetricsPrimary);
        let primary = entries
            .iter()
            .find(|entry| entry.route == EndpointRoute::Primary)
            .map(|entry| entry.spec.endpoint.as_str());
        let classed = entries
            .into_iter()
            .filter(|entry| {
                // A declared recipient receives only the metrics payloads that name it.
                !entry.is_declared()
                    && (is_metrics || entry.spec.classes != PayloadClasses::MetricsOnly)
                    && should_route_to_endpoint(is_metrics, has_metrics_primary, entry.route)
            })
            .collect::<Vec<_>>();
        let taken = !classed.is_empty();
        let targets = classed
            .into_iter()
            .filter(|entry| {
                let policy_endpoint = match entry.route {
                    EndpointRoute::MetricsPrimary => primary.unwrap_or(entry.spec.endpoint.as_str()),
                    EndpointRoute::Primary | EndpointRoute::Additional => entry.spec.endpoint.as_str(),
                };
                let routed = metadata
                    .metrics_endpoint_routing
                    .as_ref()
                    .is_none_or(|routing| routing.should_route_to(policy_endpoint));
                if !routed {
                    return false;
                }
                let accepted = entry.spec.protocol.v3().accepts(metadata.payload_info);
                if !accepted {
                    debug!(
                        recipient = %entry.recipient,
                        payload_info = ?metadata.payload_info,
                        "Filtering out a payload of a variant the recipient does not take."
                    );
                }
                accepted
            })
            .map(|entry| entry.recipient.clone())
            .collect();
        (targets, taken)
    }

    /// Counts a payload no recipient of the native set takes as dropped, logging it once per publication.
    fn count_unroutable(&mut self, metadata: &Metadata) {
        self.shared.telemetry.track_failed_transaction(metadata, None);
        if self.unroutable_logged != Some(self.revision) {
            self.unroutable_logged = Some(self.revision);
            warn!(
                revision = self.revision,
                native_set = self.native_set.len(),
                "Dropping a payload that no recipient of the delivery host's native set takes, such as one other than \
                 metrics while it has no primary endpoint. Later such payloads are counted as dropped without being \
                 logged until the native set changes."
            );
        }
    }

    /// Sends the payload being routed to its next recipients, one at a time, until one has no room.
    fn advance(&mut self) {
        let Some(routing) = &mut self.routing else {
            return;
        };
        if routing.waiting.is_some() {
            return;
        }
        while let Some(target) = routing.targets.pop_front() {
            match target_of(&self.table, &self.tombstones, &self.withdrawn, &target) {
                Target::Send(sender) => match sender.try_send(routing.transaction.clone()) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        // The recipient's loop has no room: wait for it in the routing loop, which keeps serving
                        // control and health meanwhile.
                        routing.waiting = Some((target, Box::pin(sender.reserve_owned())));
                        return;
                    }
                    Err(TrySendError::Closed(_)) => count_undelivered(
                        &self.shared,
                        &self.table,
                        &self.telemetry,
                        &target,
                        Target::Gone,
                        routing,
                    ),
                },
                undelivered => count_undelivered(
                    &self.shared,
                    &self.table,
                    &self.telemetry,
                    &target,
                    undelivered,
                    routing,
                ),
            }
        }
        self.routing = None;
    }

    /// Finishes the send the payload being routed waited for room for, and continues with its next recipients.
    fn reserved(&mut self, reserved: Result<OwnedPermit<Transaction<Body>>, SendError<()>>) {
        let Some(routing) = &mut self.routing else {
            return;
        };
        let Some((target, _)) = routing.waiting.take() else {
            return;
        };
        match (
            reserved,
            target_of(&self.table, &self.tombstones, &self.withdrawn, &target),
        ) {
            (Ok(permit), Target::Send(_)) => {
                permit.send(routing.transaction.clone());
            }
            (Ok(_), undelivered) => count_undelivered(
                &self.shared,
                &self.table,
                &self.telemetry,
                &target,
                undelivered,
                routing,
            ),
            (Err(_), _) => count_undelivered(
                &self.shared,
                &self.table,
                &self.telemetry,
                &target,
                Target::Gone,
                routing,
            ),
        }
        self.advance();
    }

    /// Stops waiting for room in a recipient that a control message withdrew, dropped, or failed meanwhile.
    fn revalidate_waiting(&mut self) {
        let Some(routing) = &mut self.routing else {
            return;
        };
        let Some((target, _)) = &routing.waiting else {
            return;
        };
        let target = target.clone();
        match target_of(&self.table, &self.tombstones, &self.withdrawn, &target) {
            Target::Send(_) => {}
            undelivered => {
                routing.waiting = None;
                count_undelivered(
                    &self.shared,
                    &self.table,
                    &self.telemetry,
                    &target,
                    undelivered,
                    routing,
                );
                self.advance();
            }
        }
    }

    /// Starts the drain of every released recipient once every payload received before its release was routed to it:
    /// the input barrier.
    fn pass_barrier(&mut self) {
        if self.routing.is_some() {
            return;
        }
        // Every payload that may name a recipient that left the table was routed.
        let received = self.received;
        self.tombstones.retain(|tombstone| tombstone.until > received);
        let now = Instant::now();
        let mut started = false;
        for entry in self.table.values_mut() {
            if let State::Releasing { target } = entry.state {
                if self.received >= target {
                    entry.start_drain(Drain {
                        deadline: now + self.drain_deadline,
                        since: now,
                        since_wall: SystemTime::now(),
                    });
                    debug!(recipient = %entry.recipient, "Recipient passed the input barrier and drains.");
                    started = true;
                }
            }
        }
        if started {
            self.bound_drains(now);
            self.changed();
        }
    }

    /// Closes the oldest draining recipients early, with their closures, while more than the most that drain at once
    /// do.
    fn bound_drains(&mut self, now: Instant) {
        loop {
            let draining = self
                .table
                .values()
                .filter(|entry| matches!(entry.state, State::Draining { drain } if drain.deadline > now))
                .count();
            if draining <= MAX_DRAINING_RECIPIENTS {
                return;
            }
            let oldest = self
                .table
                .values_mut()
                .filter_map(|entry| match entry.state {
                    State::Draining { drain } if drain.deadline > now => Some((drain.since, entry)),
                    _ => None,
                })
                .min_by_key(|(since, _)| *since)
                .map(|(_, entry)| entry);
            let Some(entry) = oldest else {
                return;
            };
            if let State::Draining { mut drain } = entry.state {
                drain.deadline = now;
                entry.start_drain(drain);
                info!(recipient = %entry.recipient, "Too many recipients drain at once: closing the oldest early.");
            }
        }
    }

    // Credentials, the client, and validation.

    fn credential_changed(&mut self, change: CredentialChange) {
        let outcome = self.credentials.apply(change);
        if outcome.replaced {
            self.validation.changes.send_replace(());
        }
        if outcome.proxy {
            self.update_client();
        }
        self.changed();
    }

    /// Follows the proxy credentials of the current settings, and builds a new client, off the routing loop, unless the
    /// client the host sends through, or the one it builds, was built from what the settings and credentials ask for.
    ///
    /// A client that failed to build is built again here, at the next publication or proxy credential change.
    fn update_client(&mut self) {
        let proxy = &self.settings.proxy;
        let references = [proxy.http.as_ref(), proxy.https.as_ref()].map(|proxy| proxy.map(|proxy| &proxy.credential));
        self.credentials
            .unfollow_proxies(&references.iter().flatten().copied().collect::<Vec<_>>());
        let references = references.map(|reference| reference.cloned());
        let userinfo =
            references.map(|reference| reference.and_then(|reference| self.credentials.follow_proxy(&reference)));
        let profile = ClientProfile::new(&self.settings, userinfo);
        if profile == self.client_profile {
            // The client the host has fits: a build for other settings is no longer needed, and its result is dropped.
            self.client_build = None;
            self.client_failure = None;
            return;
        }
        if self.client_build.as_ref().is_some_and(|build| build.profile == profile) {
            return;
        }
        self.client_builds += 1;
        let base = self.shared.base.clone();
        let metrics_builder = self.shared.metrics_builder.clone();
        let endpoint_name = Arc::clone(&self.shared.endpoint_name);
        let built_from = profile.clone();
        // Building a client with proxies loads the platform's root certificates, which can take a while: it happens off
        // the routing loop, and a newer build supersedes it.
        let handle =
            tokio::task::spawn_blocking(move || build_client(&built_from, &base, &metrics_builder, &endpoint_name));
        self.client_build = Some(ClientBuild { profile, handle });
    }

    fn client_built(&mut self, built: Result<Result<HttpClient, GenericError>, JoinError>) {
        let Some(build) = self.client_build.take() else {
            return;
        };
        let error = match built {
            Ok(Ok(client)) => {
                self.shared.switch.replace(client);
                self.client_profile = build.profile;
                self.client_failure = None;
                info!("Delivery host switched to a client built for its new settings.");
                self.changed();
                return;
            }
            Ok(Err(error)) => format!("{error:#}"),
            Err(error) => format!("the build stopped: {error}"),
        };
        error!(
            %error,
            "Failed to build a client for the delivery host's new settings: keeping the current one. It is built again \
             at the next publication or proxy credential change."
        );
        self.client_failure = Some(error);
        self.changed();
    }

    /// Validates the API keys of the native Datadog intake recipients the table holds, again whenever that set changes.
    ///
    /// A host whose table holds none is ready, and validates nothing.
    ///
    /// The validator starts with the routing loop and follows the table from then on: it validates again when the set
    /// of native intake recipients, or one of their keys, changes. It asks about them in the order a native forwarder
    /// validates its endpoints, by route: the primary endpoint, the alternate metrics intake, then the additional
    /// endpoints (in the order of the native set, then by slot); it stops at the first key the intake accepts, as the
    /// native one does.
    fn update_validation(&mut self) {
        let mut routable = self.table.values().filter(|entry| entry.routable()).collect::<Vec<_>>();
        let native_set = &self.native_set;
        routable.sort_by_key(|entry| {
            let route = match &entry.spec.origin {
                RecipientOrigin::Native(recipient) => match recipient.route {
                    MetricsRecipientRoute::Primary => 0,
                    MetricsRecipientRoute::MetricsOverride => 1,
                    MetricsRecipientRoute::Additional => 2,
                },
                RecipientOrigin::Declared { .. } => 3,
            };
            let position = native_set
                .iter()
                .position(|member| member.id == entry.recipient.id)
                .unwrap_or(usize::MAX);
            (route, position, entry.recipient.slot)
        });
        self.validation.targets = routable
            .iter()
            .filter(|entry| RecipientValidationTargets::includes(&entry.spec))
            .map(|entry| entry.recipient.clone())
            .collect();
        self.validation
            .publisher
            .publish(routable.iter().map(|entry| (&entry.spec, &entry.endpoint)));
        if let Some((targets, changes)) = self.validation.start.take() {
            let validator = ApiKeyValidator::new(
                targets,
                self.shared.client(),
                Some(changes),
                self.settings.api_key_validation_interval,
                self.shared.emitter.clone(),
            );
            self.validation.handle = Some(validator.spawn());
        }
    }

    // Status.

    /// Updates what follows the table: validation, conflicts, telemetry, and status.
    fn changed(&mut self) {
        self.update_validation();
        let conflicts = self.conflicts();
        let mut status = DeliveryHostStatus {
            revision: self.revision,
            recipients: Vec::with_capacity(self.table.len()),
            conflicts,
            withdrawn: self.withdrawn.iter().cloned().collect(),
            client_failure: self.client_failure.clone(),
        };
        let (mut live, mut draining, mut failed) = (0, 0, 0);
        for entry in self.table.values() {
            let state = entry.reported_state();
            match state {
                RecipientState::Live | RecipientState::Releasing => live += 1,
                RecipientState::Draining => draining += 1,
                RecipientState::Failed => failed += 1,
            }
            status.recipients.push(RecipientStatus {
                recipient: entry.recipient.clone(),
                state,
                draining_since: match entry.state {
                    State::Draining { drain } => Some(drain.since_wall),
                    _ => None,
                },
            });
        }
        self.telemetry.live.set(live as f64);
        self.telemetry.draining.set(draining as f64);
        self.telemetry.failed.set(failed as f64);
        self.telemetry.conflicts.set(status.conflicts.len() as f64);
        self.status.send_if_modified(|current| {
            if *current == status {
                return false;
            }
            for conflict in status
                .conflicts
                .iter()
                .filter(|conflict| !current.conflicts.contains(conflict))
            {
                warn!(
                    declared = %conflict.declared,
                    duplicates = %conflict.duplicates,
                    "A declared recipient sends to the same endpoint with the same key as another recipient."
                );
            }
            *current = status;
            true
        });
    }

    /// Returns every declared recipient that sends to the same endpoint with the same key as another recipient, of
    /// those that payloads still reach.
    fn conflicts(&self) -> Vec<RecipientConflict> {
        let keys = self
            .table
            .values()
            .filter(|entry| self.receives(entry))
            .map(|entry| (entry, self.key_of(&entry.endpoint)))
            .collect::<Vec<_>>();
        let mut conflicts = Vec::new();
        for (declared, key) in keys.iter().filter(|(entry, _)| entry.is_declared()) {
            for (other, other_key) in &keys {
                let counted_once = !other.is_declared() || other.recipient.slot > declared.recipient.slot;
                if other.recipient.slot != declared.recipient.slot && counted_once && other_key == key {
                    conflicts.push(RecipientConflict {
                        declared: declared.recipient.clone(),
                        duplicates: other.recipient.clone(),
                    });
                }
            }
        }
        conflicts.sort();
        conflicts
    }

    // Test access.

    /// Returns the number of credentials the host follows.
    #[cfg(test)]
    pub(super) fn credentials_followed(&self) -> usize {
        self.credentials.followed()
    }

    /// Returns how many clients the host started building.
    #[cfg(test)]
    pub(super) fn client_builds(&self) -> u64 {
        self.client_builds
    }

    /// Returns the recipients whose API keys the host validates.
    #[cfg(test)]
    pub(super) fn validated(&self) -> &[RecipientRef] {
        &self.validation.targets
    }

    /// Returns how many identities the host remembers the newest created incarnation of: those it runs or holds, and
    /// those it no longer does.
    #[cfg(test)]
    pub(super) fn remembered_identities(&self) -> (usize, usize) {
        (self.created.len(), self.forgotten.len())
    }
}

/// Returns what a send to `target` does now.
fn target_of(
    table: &BTreeMap<RecipientSlot, Entry>, tombstones: &[Tombstone], withdrawn: &BTreeSet<MetaString>,
    target: &RecipientRef,
) -> Target {
    let Some(entry) = table.get(&target.slot).filter(|entry| entry.recipient == *target) else {
        // A recipient whose loop ended, or that an abort dropped, while the payload was being routed.
        return match tombstones.iter().find(|tombstone| tombstone.recipient == *target) {
            Some(tombstone) => Target::Failed(tombstone.domain.clone()),
            None => Target::Gone,
        };
    };
    if withdrawn.contains(&entry.recipient.id) {
        return Target::Withdrawn;
    }
    match (&entry.state, &entry.sender) {
        (State::Live | State::Releasing { .. }, Some(sender)) => Target::Send(sender.clone()),
        (State::Draining { .. }, _) => Target::Gone,
        (State::Failed, _) | (_, None) => Target::Failed(entry.domain.clone()),
    }
}

/// Counts a send that did not reach `target`.
fn count_undelivered(
    shared: &HostShared, table: &BTreeMap<RecipientSlot, Entry>, telemetry: &HostTelemetry, target: &RecipientRef,
    undelivered: Target, routing: &Routing,
) {
    let metadata = routing.transaction.metadata();
    match undelivered {
        Target::Send(_) => {}
        Target::Withdrawn => {
            telemetry.withdrawn.increment(1);
            debug!(recipient = %target, "Dropping a payload sent to a withdrawn recipient.");
        }
        Target::Failed(domain) => {
            shared
                .telemetry
                .track_permanently_failed_transaction(metadata, None, &domain);
            error!(recipient = %target, "Failed to send request to endpoint I/O task: receiver dropped.");
        }
        Target::Gone => {
            let domain = table
                .get(&target.slot)
                .map(|entry| entry.domain.as_str())
                .unwrap_or_default();
            shared
                .telemetry
                .track_permanently_failed_transaction(metadata, None, domain);
            debug!(recipient = %target, "Dropping a payload sent to a recipient that left the table meanwhile.");
        }
    }
}

/// Commits what the publication of `revision` made part of the state the host runs, so that no abort undoes it: the
/// recipients its resource transaction created or reopened, and those its native set names, which the native set
/// holds as a user of its own.
///
/// A transaction of an older revision that never published can still be aborted: a newer lowering superseded it, and
/// its abort may arrive after the newer publication.
fn commit(table: &mut BTreeMap<RecipientSlot, Entry>, revision: u64, native_set: &[RecipientRef]) {
    let published = Creator::Transaction(revision);
    for entry in table.values_mut() {
        let named = native_set.contains(&entry.recipient);
        if named || entry.created_by.contains(&published) {
            entry.committed = true;
        }
        if entry.committed {
            // No abort can drop it now, so the failed incarnation whose slot it took is never put back.
            entry.replaced = None;
        }
        if named || entry.reopened_by.contains(&published) {
            // The release a reopen cancelled is over: no abort returns the recipient to it.
            entry.reopened_by.clear();
            entry.released = None;
        }
    }
}

/// Describes a recipient's state, for an error.
fn describe(state: Option<State>) -> &'static str {
    match state {
        None => "is not in the host's table under that reference",
        Some(State::Live) => "is live",
        Some(State::Releasing { .. }) => "is released",
        Some(State::Draining { .. }) => "is draining",
        Some(State::Failed) => "has failed",
    }
}

async fn exit_of(recipient: RecipientRef, exited: oneshot::Receiver<RecipientExit>) -> (RecipientRef, RecipientExit) {
    let outcome = exited.await.unwrap_or_else(|_| {
        RecipientExit::Failed("the recipient's I/O loop stopped without reporting how".to_string())
    });
    (recipient, outcome)
}

async fn live(health: &mut Option<&mut Health>) {
    match health {
        Some(health) => health.live().await,
        None => pending().await,
    }
}

async fn recv(
    control: &mut Option<ControlReceiver<DeliveryControl<Body>>>,
) -> Option<(DeliveryControl<Body>, ControlReply)> {
    match control {
        Some(control) => control.recv().await,
        None => pending().await,
    }
}

async fn reserve_event(events: &Option<mpsc::Sender<DeliveryEvent>>) -> Option<mpsc::OwnedPermit<DeliveryEvent>> {
    match events {
        Some(events) => events.clone().reserve_owned().await.ok(),
        None => pending().await,
    }
}

async fn client_built(build: &mut Option<ClientBuild>) -> Result<Result<HttpClient, GenericError>, JoinError> {
    match build {
        Some(build) => (&mut build.handle).await,
        None => pending().await,
    }
}

async fn waiting(routing: &mut Option<Routing>) -> Result<OwnedPermit<Transaction<Body>>, SendError<()>> {
    match routing.as_mut().and_then(|routing| routing.waiting.as_mut()) {
        Some((_, reserve)) => reserve.await,
        None => pending().await,
    }
}
