//! The installer's links to the encode and delivery hosts, and the resource actions it performs through them.
//!
//! # Wiring
//!
//! Every fixed delivery host of the frame ([`FixedHosts`]) has, with its encode host, two control mailboxes and an
//! event channel, created at boot ([`wire_hosts`]). The installer keeps the sending sides of the mailboxes and the
//! receiving side of the event channel in its state, outside the worker, so a host's exit reported during a worker
//! restart is not lost. Whatever builds a host takes its [`HostWiring`] from the boot topology: the receiving sides of
//! its mailboxes, the sending side of its event channel, its boot publication and preparation, and the slots its
//! factories go in, which the installer builds leases and recipients with, off the data path.
//!
//! # Actions
//!
//! Every message is sent when its action is performed, in order, so a host handles its mailbox in the order the core
//! decided; only acknowledgments are awaited, on the worker's acknowledgment set:
//!
//! - a preparation's part for a host (a generation's, or a resource transaction's) builds every new lease's builders
//!   and every new recipient's pieces first, and fails the part, sending nothing, if one cannot be built; it then sends
//!   the delivery host its recipients (with those the part requires) and its reopens, and the encode host each lease,
//!   and reports one outcome for the host: prepared, prepared with reopens that found their loops exited, or failed;
//! - an abort tells each listed delivery host to undo what the generation or transaction alone prepared; an encode
//!   host has no abort, since the core drains an aborted generation's new leases like any other;
//! - a publication sends each host its native set, settings, recipients' settings, and withdrawn set;
//! - a lease release drains each lease, and reports it released once the encode host acknowledges, whatever the
//!   acknowledgment says: a host that does not hold the lease, or is gone, holds nothing of it;
//! - a recipient release is acknowledged once accepted; the recipient's exit arrives later on the event channel.

use std::{
    collections::BTreeMap,
    future::{poll_fn, Future},
    sync::{Arc, OnceLock},
    task::Poll,
    time::{Duration, SystemTime},
};

use futures::future::BoxFuture;
use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_components::{
    encoders::encode_host::{self, EncodeControl, EncodeLeaseFactory},
    forwarders::delivery::{
        Creator, CredentialSource, DeliveryControl, DeliveryEvent, DeliveryHostStatus, PreparedRecipient,
        RecipientExit, RecipientFactory, RecipientSettings, ReopenReply,
    },
};
use saluki_core::execution::{
    control::{ControlError, ControlMailbox, ControlSender},
    control_mailbox, GenerationId,
};
use saluki_metric_plan::{
    install::{Action, Boot, CreditKey, HostPrepare, HostPublish, LeaseRef, PrepareOutcome, RecipientRef},
    ir::{HostId, Resources},
    limits,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::Instant,
};
use tracing::{debug, warn};

use super::{status::Provenance, Acknowledged, Installer};
use crate::programmable_metrics::resources::{
    specs::{self, core_ref, delivery_ref, lease_spec, recipient_settings, recipient_spec, retirement_report},
    FixedHost, FixedHosts,
};

/// The body of the payloads the Datadog delivery hosts carry.
pub type Body = FrozenChunkedBytesBuffer;

/// How many exits a delivery host's event channel holds before the host keeps further exits waiting: more than the
/// recipients a host runs at once, the declared ones and those draining, ever need.
pub const HOST_EVENTS: usize =
    4 * (limits::values::MAX_DECLARED_RECIPIENTS as usize + limits::values::MAX_DRAINING_RECIPIENTS as usize);

/// The factories a built host exposes, through which the installer builds leases and recipients off the data path,
/// and what the delivery host reports about its table.
///
/// Whatever builds the host sets them; until then, a preparation that needs one fails, naming the host.
#[derive(Clone, Default)]
pub struct HostFactories {
    /// The delivery host's recipient factory.
    pub recipients: Arc<OnceLock<Arc<dyn RecipientFactory<Body>>>>,

    /// The encode host's lease factory.
    pub leases: Arc<OnceLock<Arc<dyn EncodeLeaseFactory>>>,

    /// What the delivery host reports about its table: each recipient's state, its conflicts, its withdrawn set, and
    /// a client it could not build.
    pub status: Arc<OnceLock<watch::Receiver<DeliveryHostStatus>>>,
}

impl HostFactories {
    fn recipients(&self, host: &FixedHost) -> Result<Arc<dyn RecipientFactory<Body>>, String> {
        self.recipients.get().cloned().ok_or_else(|| {
            format!(
                "delivery host `{}` was not built, so its recipients cannot be prepared",
                host.delivery
            )
        })
    }

    fn leases(&self, host: &FixedHost) -> Result<Arc<dyn EncodeLeaseFactory>, String> {
        self.leases.get().cloned().ok_or_else(|| {
            format!(
                "encode host `{}` was not built, so its leases cannot be prepared",
                host.encoder
            )
        })
    }
}

/// What builds a fixed delivery host and its encode host takes from the boot topology.
pub struct HostWiring {
    /// The host.
    pub host: FixedHost,

    /// The receiving side of the delivery host's control mailbox.
    pub delivery: ControlMailbox<DeliveryControl<Body>>,

    /// The receiving side of the encode host's control mailbox.
    pub encode: ControlMailbox<EncodeControl>,

    /// The sending side of the delivery host's event channel, which holds [`HOST_EVENTS`] exits.
    pub events: mpsc::Sender<DeliveryEvent>,

    /// The leases and recipients the boot generation needs in the host, if any.
    pub boot_prepare: Option<HostPrepare>,

    /// What the delivery host publishes from its start, if anything, at the boot revision.
    pub boot_publish: Option<HostPublish>,

    /// The boot revision: the delivery host ignores any later publication older than it.
    pub boot_revision: u64,

    /// The slots the host's factories go in.
    pub factories: HostFactories,

    /// The credentials the delivery host's recipients send with, resolved from their references.
    pub credentials: Arc<dyn CredentialSource>,
}

/// The wiring of every fixed host, which whatever builds each host takes at boot.
pub struct BootHosts {
    wiring: BTreeMap<HostId, HostWiring>,
}

impl BootHosts {
    /// Takes the wiring of the delivery host `host`, once: `None` if the process has no such host, or it was taken.
    ///
    /// The topology takes it when it builds the host and its encode host; tests take it to serve a host with a fake.
    pub fn take(&mut self, host: &str) -> Option<HostWiring> {
        let host = HostId::new(host).ok()?;
        self.wiring.remove(&host)
    }

    /// Closes the wiring of every host nothing took: the hosts a topology did not build, such as one a test serves with
    /// nothing.
    ///
    /// The mailboxes of each host not built close: a message the installer sends one fails as stopped rather than
    /// waiting, and its boot preparation and publication are not run.
    pub(crate) fn close_unbuilt(&mut self) {
        for (_, wiring) in std::mem::take(&mut self.wiring) {
            let HostWiring {
                host,
                delivery,
                encode,
                events,
                boot_prepare,
                boot_publish,
                boot_revision,
                factories,
                credentials,
            } = wiring;
            debug!(
                host = host.delivery,
                encoder = host.encoder,
                boot_leases = boot_prepare.as_ref().map_or(0, |prepare| prepare.leases.len()),
                boot_recipients = boot_publish.as_ref().map_or(0, |publish| publish.native_set.len()),
                boot_revision,
                "The topology does not build this delivery host and its encode host: their control mailboxes close."
            );
            drop((delivery, encode, events, factories, credentials));
        }
    }
}

/// The installer's link to one fixed delivery host and its encode host.
pub(super) struct HostLink {
    host: FixedHost,
    delivery: ControlSender<DeliveryControl<Body>>,
    encode: ControlSender<EncodeControl>,
    events: mpsc::Receiver<DeliveryEvent>,
    factories: HostFactories,
}

impl HostLink {
    /// Returns what the delivery host reports about its table, once it is built.
    pub(super) fn status(&self) -> Option<DeliveryHostStatus> {
        self.factories.status.get().map(|status| status.borrow().clone())
    }

    /// Returns the senders of the host's two mailboxes, which the installer waits on to run before it starts.
    pub(super) fn senders(&self) -> (ControlSender<DeliveryControl<Body>>, ControlSender<EncodeControl>) {
        (self.delivery.clone(), self.encode.clone())
    }
}

/// Creates the mailboxes and event channel of every fixed host, with what the boot generation `boot` builds in each
/// and the credentials its recipients send with: the installer's links, and the boot wiring whatever builds each host
/// takes.
pub(super) fn wire_hosts(
    hosts: &FixedHosts, boot: &Boot, credentials: Arc<dyn CredentialSource>,
) -> (BTreeMap<HostId, HostLink>, BootHosts) {
    let mut links = BTreeMap::new();
    let mut wiring = BTreeMap::new();
    for host in hosts.iter() {
        let id = host.id();
        let (delivery, delivery_mailbox) = control_mailbox();
        let (encode, encode_mailbox) = control_mailbox();
        let (events_tx, events) = mpsc::channel(HOST_EVENTS);
        let factories = HostFactories::default();
        links.insert(
            id.clone(),
            HostLink {
                host,
                delivery,
                encode,
                events,
                factories: factories.clone(),
            },
        );
        wiring.insert(
            id.clone(),
            HostWiring {
                host,
                delivery: delivery_mailbox,
                encode: encode_mailbox,
                events: events_tx,
                boot_prepare: boot.hosts.iter().find(|prepare| prepare.host == id).cloned(),
                boot_publish: boot.publish.iter().find(|publish| publish.host == id).cloned(),
                boot_revision: boot.revision.get(),
                factories,
                credentials: Arc::clone(&credentials),
            },
        );
    }
    (links, BootHosts { wiring })
}

/// Waits for the next event a delivery host reports, or never, once every host's event channel closed.
pub(super) async fn next_host_event(links: &mut BTreeMap<HostId, HostLink>) -> (HostId, DeliveryEvent) {
    poll_fn(|cx| {
        for (host, link) in links.iter_mut() {
            if let Poll::Ready(Some(event)) = link.events.poll_recv(cx) {
                return Poll::Ready((host.clone(), event));
            }
        }
        Poll::Pending
    })
    .await
}

/// What one host's part of a preparation was built into, ready to send.
struct BuiltPart {
    leases: Vec<encode_host::LeasePrepare>,
    recipients: Vec<PreparedRecipient<Body>>,
    requires: Vec<saluki_components::forwarders::delivery::RecipientRef>,
    reopen: Vec<RecipientSettings>,
}

/// The preparation a host's part belongs to: a generation's, or a resource transaction's.
#[derive(Clone, Copy)]
enum Part {
    Generation(GenerationId),
    Transaction(saluki_metric_plan::install::ResourcesRevision),
}

impl Part {
    fn creator(self) -> Creator {
        match self {
            Self::Generation(generation) => Creator::Generation(generation),
            Self::Transaction(revision) => Creator::Transaction(revision.get()),
        }
    }
}

impl Installer {
    /// Returns the resources of the plan a preparation of `generation` prepares: the preparing plan's, or, for a
    /// preparation the core replaced before it was sent, those of the plan last lowered.
    fn preparing_resources(&self, generation: GenerationId) -> Resources {
        let preparing = self
            .core
            .status()
            .preparing
            .filter(|preparing| preparing.id == generation)
            .and(self.core.preparing_plan());
        match preparing {
            Some(plan) => plan.resources.clone(),
            None => self.last_lowered.resources.clone(),
        }
    }

    /// Sends each host its part of the preparation of `generation`, and awaits each host's acknowledgment.
    ///
    /// A part that cannot be built fails at once, naming its lease or recipient, and nothing is sent to its host.
    pub(super) fn prepare_hosts(&mut self, generation: GenerationId, hosts: Vec<HostPrepare>) {
        let resources = self.preparing_resources(generation);
        for part in hosts {
            let host = part.host.clone();
            match self.send_part(Part::Generation(generation), &part, &resources) {
                Ok(outcome) => self.acks.push(Box::pin(async move {
                    Acknowledged::Host {
                        generation,
                        host,
                        outcome: outcome.await,
                    }
                })),
                Err(reason) => {
                    let actions = self.resource_paths.host_prepared(
                        &mut self.core,
                        generation,
                        host,
                        PrepareOutcome::Failed(reason),
                    );
                    self.actions.extend(actions);
                }
            }
        }
    }

    /// Sends each host its part of the resource transaction of `revision`, awaits each host's acknowledgment, and
    /// times the transaction out after `timeout`.
    ///
    /// A host whose credits the budget refuses fails its part without being sent anything.
    pub(super) fn prepare_resources(
        &mut self, revision: saluki_metric_plan::install::ResourcesRevision, hosts: Vec<HostPrepare>,
        timeout: Duration, admitted: &[(CreditKey, u64)],
    ) {
        self.resources_deadline = Some((revision, Instant::now() + timeout));
        let resources = self.last_lowered.resources.clone();
        for part in hosts {
            let host = part.host.clone();
            let credits = admitted
                .iter()
                .filter(|(key, _)| credit_host(key) == Some(&host))
                .cloned()
                .collect::<Vec<_>>();
            if let Err(refused) = self.budget.acquire(&credits) {
                let outcome =
                    PrepareOutcome::Failed(format!("the memory limit's headroom has no room for {}", refused));
                let actions = self
                    .resource_paths
                    .resources_prepared(&mut self.core, revision, host, outcome);
                self.actions.extend(actions);
                continue;
            }
            match self.send_part(Part::Transaction(revision), &part, &resources) {
                Ok(outcome) => self.acks.push(Box::pin(async move {
                    Acknowledged::Resources {
                        revision,
                        host,
                        outcome: outcome.await,
                    }
                })),
                Err(reason) => {
                    let actions = self.resource_paths.resources_prepared(
                        &mut self.core,
                        revision,
                        host,
                        PrepareOutcome::Failed(reason),
                    );
                    self.actions.extend(actions);
                }
            }
        }
    }

    /// Builds one host's part of a preparation and sends it, returning the host's outcome to await.
    ///
    /// # Errors
    ///
    /// Returns why the part cannot be prepared, having sent nothing: the process has no such host, a lease or recipient
    /// cannot be read or built, or the host was not built.
    fn send_part(
        &self, preparation: Part, part: &HostPrepare, resources: &Resources,
    ) -> Result<BoxFuture<'static, PrepareOutcome>, String> {
        let link = self
            .hosts
            .get(&part.host)
            .ok_or_else(|| format!("the topology has no delivery host `{}`", part.host))?;
        let built = self.build_part(link, part, resources)?;
        let creator = preparation.creator();
        let BuiltPart {
            leases,
            recipients,
            requires,
            reopen,
        } = built;

        let prepared = (!recipients.is_empty() || !requires.is_empty()).then(|| {
            link.delivery.send(DeliveryControl::Prepare {
                creator,
                recipients,
                requires,
            })
        });
        let reopened = (!reopen.is_empty()).then(|| {
            let (reply, answers) = oneshot::channel();
            let ack = link.delivery.send(DeliveryControl::Reopen {
                creator,
                recipients: reopen,
                reply,
            });
            (ack, answers)
        });
        let leases = leases
            .into_iter()
            .map(|lease| {
                let output = lease.spec.key.output.clone();
                (output, link.encode.send(EncodeControl::Prepare(lease)))
            })
            .collect::<Vec<_>>();

        Ok(Box::pin(async move {
            if let Some(ack) = prepared {
                if let Err(error) = ack.await {
                    return PrepareOutcome::Failed(error.to_string());
                }
            }
            let mut exited = Vec::new();
            if let Some((ack, answers)) = reopened {
                if let Err(error) = ack.await {
                    return PrepareOutcome::Failed(error.to_string());
                }
                let Ok(answers) = answers.await else {
                    return PrepareOutcome::Failed("the delivery host did not answer a reopen".to_string());
                };
                for (recipient, answer) in answers {
                    if answer == ReopenReply::Exited {
                        match core_ref(&recipient) {
                            Ok(recipient) => exited.push(recipient),
                            Err(error) => return PrepareOutcome::Failed(error.to_string()),
                        }
                    }
                }
            }
            for (output, ack) in leases {
                if let Err(error) = ack.await {
                    return PrepareOutcome::Failed(format!("the lease of output `{}`: {}", output, error));
                }
            }
            if exited.is_empty() {
                PrepareOutcome::Prepared
            } else {
                PrepareOutcome::ReopenExited { recipients: exited }
            }
        }))
    }

    /// Builds every new lease's builders and every new recipient's pieces of one host's part, through the host's
    /// factories, with the host's settings as `resources` declares them.
    fn build_part(&self, link: &HostLink, part: &HostPrepare, resources: &Resources) -> Result<BuiltPart, String> {
        let held = self.resource_paths.held_resources(&self.core);
        let declared_name =
            |prepare: &saluki_metric_plan::install::RecipientPrepare| held.declared_name(&prepare.decl.id);
        let mut leases = Vec::with_capacity(part.leases.len());
        if !part.leases.is_empty() {
            let factory = link.factories.leases(&link.host)?;
            for lease in &part.leases {
                let spec = lease_spec(lease, resources).map_err(|error| format!("{:#}", error))?;
                let built = factory.build(&spec).map_err(|error| {
                    format!("the lease of output `{}` cannot be built: {:#}", lease.output.id, error)
                })?;
                leases.push(encode_host::LeasePrepare { spec, built });
            }
        }
        let mut recipients = Vec::with_capacity(part.create.len());
        if !part.create.is_empty() {
            let factory = link.factories.recipients(&link.host)?;
            let binding = resources
                .host(&part.host)
                .ok_or_else(|| format!("the plan declares no settings for delivery host `{}`", part.host))?;
            let settings = specs::host_settings(binding).map_err(|error| format!("{:#}", error))?;
            for create in &part.create {
                let spec = recipient_spec(&create.decl, declared_name(create).as_deref())
                    .map_err(|error| format!("{:#}", error))?;
                let pieces = factory
                    .build(&spec, &settings)
                    .map_err(|error| format!("{} cannot be built: {:#}", create.recipient, error))?;
                recipients.push(PreparedRecipient {
                    recipient: delivery_ref(&create.recipient),
                    spec,
                    pieces,
                });
            }
        }
        let reopen = part
            .reopen
            .iter()
            .map(|prepare| recipient_settings(prepare, declared_name(prepare).as_deref()))
            .collect::<Result<_, _>>()
            .map_err(|error| format!("{:#}", error))?;
        Ok(BuiltPart {
            leases,
            recipients,
            requires: part.requires.iter().map(delivery_ref).collect(),
            reopen,
        })
    }

    /// Tells each listed delivery host to undo what the generation's preparation alone prepared.
    pub(super) fn abort_hosts(&mut self, generation: GenerationId, hosts: Vec<HostId>) {
        for host in hosts {
            if let Some(link) = self.hosts.get(&host) {
                let ack = link.delivery.send(DeliveryControl::Abort { generation });
                self.sent(host, "abort", ack);
            }
        }
    }

    /// Tells each listed delivery host to undo what the resource transaction of `revision` prepared, and stops timing
    /// the transaction out.
    pub(super) fn abort_resources(
        &mut self, revision: saluki_metric_plan::install::ResourcesRevision, hosts: Vec<HostId>,
    ) {
        if self.resources_deadline.is_some_and(|(timed, _)| timed == revision) {
            self.resources_deadline = None;
        }
        for host in hosts {
            if let Some(link) = self.hosts.get(&host) {
                let ack = link.delivery.send(DeliveryControl::AbortResources {
                    revision: revision.get(),
                });
                self.sent(host, "resource transaction abort", ack);
            }
        }
    }

    /// Publishes, in each listed host, what the resource transaction of `revision` publishes, and stops timing the
    /// transaction out.
    pub(super) fn publish_resources(
        &mut self, revision: saluki_metric_plan::install::ResourcesRevision, hosts: Vec<HostPublish>,
    ) {
        if self.resources_deadline.is_some_and(|(timed, _)| timed == revision) {
            self.resources_deadline = None;
        }
        let held = self.resource_paths.held_resources(&self.core);
        for publish in hosts {
            let host = publish.host.clone();
            let Some(link) = self.hosts.get(&host) else {
                warn!(%host, "The installer core published resources in a delivery host the topology does not have.");
                continue;
            };
            let publication = match specs::publication(revision.get(), &publish, |id| held.declared_name(id)) {
                Ok(publication) => publication,
                Err(error) => {
                    // Only a bug writes parameters the codec cannot read: the host keeps what it published before.
                    warn!(
                        %host,
                        revision = revision.get(),
                        error = format!("{:#}", error),
                        "A resource publication cannot be read; the delivery host keeps the one before it."
                    );
                    continue;
                }
            };
            let ack = link.delivery.send(DeliveryControl::Publish {
                revision: publication.revision,
                native_set: publication.native_set,
                settings: publication.settings,
                recipients: publication.recipients,
                withdrawn: publication.withdrawn,
            });
            self.sent(host, "publication", ack);
        }
    }

    /// Drains each listed lease of the encode host of `host`, and reports each released once the host acknowledges.
    pub(super) fn release_leases(&mut self, host: HostId, leases: Vec<LeaseRef>) {
        let Some(link) = self.hosts.get(&host) else {
            // Nothing runs a lease in a host the topology does not have.
            for lease in leases {
                let actions = self.resource_paths.lease_released(&mut self.core, host.clone(), lease);
                self.actions.extend(actions);
            }
            return;
        };
        for lease in leases {
            let ack = link.encode.send(EncodeControl::Drain {
                lease: lease.lease,
                key: saluki_components::forwarders::delivery::LeaseKeyDigest {
                    output: lease.key.output.as_str().into(),
                    binding: *lease.key.binding.digest(),
                },
            });
            let host = host.clone();
            self.acks.push(Box::pin(async move {
                if let Err(error) = ack.await {
                    debug!(
                        %host,
                        lease = lease.lease.get(),
                        %error,
                        "An encode host answered a lease's drain with an error."
                    );
                }
                Acknowledged::LeaseReleased { host, lease }
            }));
        }
    }

    /// Releases each listed recipient of `host`: each drains, then retires, and its exit arrives on the host's event
    /// channel.
    pub(super) fn release_recipients(&mut self, host: HostId, recipients: Vec<RecipientRef>) {
        if let Some(link) = self.hosts.get(&host) {
            let ack = link.delivery.send(DeliveryControl::Release {
                recipients: recipients.iter().map(delivery_ref).collect(),
            });
            self.sent(host, "release", ack);
        }
    }

    /// Awaits the acknowledgment of a message whose outcome the core does not take, to log a failure.
    fn sent(
        &mut self, host: HostId, what: &'static str,
        ack: impl Future<Output = Result<(), ControlError>> + Send + 'static,
    ) {
        self.acks.push(Box::pin(async move {
            Acknowledged::Sent {
                host,
                what,
                outcome: ack.await.map_err(|error| error.to_string()),
            }
        }));
    }

    /// Hands an event a delivery host reported to the core.
    pub(super) fn host_event(&mut self, host: HostId, event: DeliveryEvent) -> Vec<Action> {
        let DeliveryEvent::RecipientExited { recipient, outcome } = event;
        let recipient = match core_ref(&recipient) {
            Ok(recipient) => recipient,
            Err(error) => {
                warn!(
                    %host,
                    error = %error,
                    "A delivery host reported the exit of a recipient the installer cannot name."
                );
                return Vec::new();
            }
        };
        match outcome {
            RecipientExit::Retired(report) => {
                let report = retirement_report(&report);
                let (actions, released) =
                    self.resource_paths
                        .recipient_retired(&mut self.core, recipient.clone(), report);
                match released {
                    // Its delivery host counts the retirement (`plan_recipient_retirements_total`); the status route
                    // lists it, declared as the core held it, since the latest lowering no longer declares it.
                    Some(provenance) => {
                        debug!(%host, %recipient, ?report, "A recipient retired.");
                        self.retired_recipients.record(
                            &host,
                            &recipient,
                            &report,
                            Provenance::from(&provenance),
                            SystemTime::now(),
                        );
                    }
                    None => debug!(
                        %host,
                        %recipient,
                        "A recipient the installer was not releasing exited, such as one an abort dropped."
                    ),
                }
                actions
            }
            RecipientExit::Failed(reason) => {
                warn!(
                    %host,
                    %recipient,
                    reason = %reason,
                    "A recipient's I/O loop failed; its next incarnation replaces it."
                );
                self.resource_paths.recipient_failed(&mut self.core, recipient, &reason)
            }
        }
    }
}

/// Returns the delivery host a credit's resource belongs to, if it belongs to one.
fn credit_host(credit: &CreditKey) -> Option<&HostId> {
    match credit {
        CreditKey::Lease(host, ..) | CreditKey::Recipient(host, _) | CreditKey::HostQueue(host) => Some(host),
        CreditKey::Instances(..) => None,
    }
}
