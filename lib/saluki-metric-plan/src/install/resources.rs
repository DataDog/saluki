//! The resource table: the leases and recipients that the fixed encode and delivery hosts run for live generations.
//!
//! Outputs and recipients are logical resources multiplexed over fixed hosts. An output's resolved binding runs as a
//! **lease** in the encode host of its delivery host, keyed by its [`LeaseKey`]; a **recipient** is one endpoint of a
//! delivery host, addressed by its [`RecipientRef`]. Generations reference both, and the table keeps, per host, who
//! uses what, so that:
//!
//! - a generation that binds an output as a live lease does reuses the lease, keeping its builders and open payloads;
//!   a draining lease is never reused;
//! - a lease drains after the last generation using it retires, and a recipient after every lease naming it is
//!   released and the published native set no longer names it, so the schedule store stays the only commit;
//! - state that outlives generations, the native recipient set for payloads that do not name their recipients, every
//!   recipient's settings, each host's settings, and the recipients a removal withdraws, follows the current
//!   configuration through **resource transactions**: prepare what a native set adds, then publish.
//!
//! The table lives inside the installer core, so it survives worker restarts with the rest of the installer's state,
//! and it does no I/O: the core returns actions ([`HostPrepare`], and the core's resource actions) that the caller
//! sends to each host, and the caller reports each host's acknowledgment and events.
//!
//! # Preparations
//!
//! A generation's preparation and a resource transaction prepare recipients by the same rule. For each recipient it
//! needs, a preparation:
//!
//! | The table holds the recipient | The preparation |
//! |---|---|
//! | not at all | creates it: a new incarnation in a free slot |
//! | live, committed | requires it (a generation, when one of its leases names it), or names it (a transaction) |
//! | being prepared by another preparation that has not committed | creates it again under the same reference, so that the host holds it until every preparation holding it aborted, and lists its credits again |
//! | released and draining | reopens it, so that its loop keeps its queue and directory |
//! | failed, committed | names its slot; its next incarnation, prepared into the same slot by a transaction after its backoff, takes over, and a publication leaves it out meanwhile |
//! | failed before a preparation that created it committed, or released and failed while reopens were in flight | waits until those preparations' answers settle its fate |
//! | exiting: a reopen found its loop already exited | waits for its exit, then creates its next incarnation |
//!
//! A recipient is **committed** once a preparation that created or reopened it commits: a generation's commit, or a
//! transaction's publication. An abort drops what only uncommitted preparations hold, gives a failed recipient's slot
//! back to it if the abort drops its next incarnation, and returns a recipient it reopened to the drain it was in. A
//! part a host failed, or was never sent after a refusal of the budget, holds nothing, at once.
//!
//! Hosts keep three rules the table counts on. A host creates an incarnation at most once, so that every incarnation
//! names one loop: a part that lists to create one the host dropped fails. A failed loop keeps its slot, as a marker
//! payloads naming it are dropped at, until its next incarnation is prepared into the slot or it is released. And the
//! host reports the exit of every incarnation, those an abort dropped included; the table ignores the exits of
//! incarnations it no longer holds, so a dropped incarnation's exit changes nothing.
//!
//! # Credits
//!
//! Everything a preparation adds after boot holds credits of the resource budget, which the caller acquires before it
//! sends the preparation and returns when the core says the creation ended ([`CreditKey`]). The table keeps the
//! credits it listed and has not released yet, so every credit is released exactly once, however its creation ends.
//! The bytes of each come from the host's [`ResourceBounds`].

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    sync::Arc,
};

use saluki_core::execution::{EntryId, GenerationId};
use serde_json::{json, Value};

use crate::{
    canonical::PlanHash,
    channel::ArtifactConfigId,
    compile::{KernelKey, LeaseTable, SlotId},
    ir::{
        HostBinding, HostId, OutputDecl, OutputId, Params, Plan, RecipientDecl, RecipientId, ResourceProvenance,
        Resources,
    },
    resources::ResourcesArtifact,
};

/// The revision of what resource transactions published: it grows with every transaction, and a host ignores a
/// publication older than the one it runs.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ResourcesRevision(u64);

impl ResourcesRevision {
    /// Creates a revision.
    pub const fn new(revision: u64) -> Self {
        Self(revision)
    }

    /// Returns the numeric value of the revision.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the revision after this one.
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for ResourcesRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The key of a lease: an output, and a digest of its binding.
///
/// Two generations that bind an output equally share one lease; a binding change is another key, and so another
/// lease.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LeaseKey {
    /// The output.
    pub output: OutputId,

    /// The digest of the output's binding.
    pub binding: PlanHash,
}

impl LeaseKey {
    /// Returns the key of the lease an output runs as, given the resources of the plan that declares it, or `None` for
    /// an output without a binding, which runs in a component of its own.
    ///
    /// The digest covers the binding without its provenance, the type of records the output accepts, and each of the
    /// output's resolved recipients with its protocol, which decides the protocol variants the lease produces and the
    /// recipients each variant names. A resources revision that declares the same output again therefore keeps its
    /// lease, and a recipient added to the output, or one whose protocol changed, gives it a new one.
    pub fn of(output: &OutputDecl, resources: &Resources) -> Option<Self> {
        let binding = output.binding.as_ref()?;
        let mut binding = serde_json::to_value(binding).unwrap_or(Value::Null);
        if let Some(members) = binding.as_object_mut() {
            members.remove("provenance");
        }
        let recipients = output
            .recipients
            .iter()
            .map(|id| {
                let protocol = resources.recipient(id).map(|recipient| &recipient.protocol);
                json!({"id": id, "protocol": protocol})
            })
            .collect::<Vec<_>>();
        let value = json!({"binding": binding, "accepts": output.accepts, "recipients": recipients});
        let digest = PlanHash::of_value(&value).unwrap_or_else(|_| PlanHash::of_bytes(value.to_string().as_bytes()));
        Some(Self {
            output: output.id.clone(),
            binding: digest,
        })
    }
}

/// How control messages address one lease of an encode host: its identifier, with its key.
///
/// A host ignores, and logs, a message whose pair does not match its table, so a release repeated after an installer
/// worker restart never reaches a reused identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LeaseRef {
    /// The lease's identifier, which every batch addressed to it carries as its header's entry.
    pub lease: EntryId,

    /// The lease's key.
    pub key: LeaseKey,
}

/// The slot of a recipient in its delivery host's recipient table.
///
/// The core allocates slots per host and reuses one only after the recipient that held it retired, so payloads name
/// their recipients by slot rather than by identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RecipientSlot(u16);

impl RecipientSlot {
    /// Creates a slot.
    pub const fn new(slot: u16) -> Self {
        Self(slot)
    }

    /// Returns the numeric value of the slot.
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// How control messages, acknowledgments, and host events address one recipient of a delivery host: its slot, its
/// identity, and its incarnation, which together name one I/O loop on one queue.
///
/// The core allocates an incarnation per recipient prepare, so a recipient that retired, or failed, and is declared
/// again is another incarnation in the same slot. A host ignores, and logs, a message whose reference does not match
/// its table.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RecipientRef {
    /// The recipient's slot.
    pub slot: RecipientSlot,

    /// The recipient's identity.
    pub id: RecipientId,

    /// The recipient's incarnation.
    pub incarnation: u64,
}

impl fmt::Display for RecipientRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "recipient `{}` (slot {}, incarnation {})",
            self.id,
            self.slot.get(),
            self.incarnation
        )
    }
}

/// Who prepared a lease or recipient in a host: a generation's preparation, or a resource transaction.
///
/// A host keeps, per recipient, every preparation that created or reopened it and has not committed, so that an abort
/// of one drops only what no other holds.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Creator {
    /// The preparation of a generation.
    Generation(GenerationId),

    /// A resource transaction, by the revision it publishes.
    Transaction(ResourcesRevision),
}

impl fmt::Display for Creator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Generation(generation) => write!(f, "generation {}", generation.get()),
            Self::Transaction(revision) => write!(f, "resource transaction {}", revision),
        }
    }
}

/// One entry of the resource budget's credit ledger: what a credit was acquired for.
///
/// The caller holds the resource budget and a ledger of the credits it holds. It acquires every credit an action lists
/// that it does not hold yet before it performs the action, so an action repeated after a worker restart acquires
/// nothing twice, and releases a credit when the core says the creation it was acquired for ended
/// ([`Action::ReleaseCredits`][super::Action::ReleaseCredits]), so every credit returns exactly once.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CreditKey {
    /// A lease of the given delivery host's encode host, by its identifier and the sequence number that tells it apart
    /// from other leases that used the identifier.
    Lease(HostId, EntryId, u64),

    /// A recipient incarnation of the given delivery host.
    Recipient(HostId, RecipientRef),

    /// An idle delivery host's queue bound, held while it has any live or draining recipient.
    HostQueue(HostId),

    /// A generation's new kernel instances in one slot, on every lane of the slot.
    Instances(GenerationId, SlotId),
}

impl fmt::Display for CreditKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lease(host, lease, sequence) => write!(
                f,
                "lease {} (sequence {}) of the encode host of `{}`",
                lease.get(),
                sequence,
                host
            ),
            Self::Recipient(host, recipient) => write!(f, "{} of delivery host `{}`", recipient, host),
            Self::HostQueue(host) => write!(f, "the queue of delivery host `{}`", host),
            Self::Instances(generation, slot) => write!(
                f,
                "the new kernel instances of generation {} in slot {}",
                generation.get(),
                slot.get()
            ),
        }
    }
}

/// How many bytes each resource a preparation adds holds, for its credit of the resource budget.
///
/// The plan crate cannot read host parameters, so the host that knows them answers. A resource of zero bytes holds no
/// credit. Every method defaults to zero.
pub trait ResourceBounds: fmt::Debug + Send + Sync {
    /// Returns the bytes a lease of `output` holds in the encode host of `host`: its builders' buffers and its shard
    /// channels, counted at full-buffer size.
    fn lease(&self, host: &HostId, output: &OutputDecl) -> u64 {
        let _ = (host, output);
        0
    }

    /// Returns the bytes a recipient holds beyond its host's own bound, given its host's settings if the lowering
    /// declares them: a declared recipient's own queue, for example.
    fn recipient(&self, recipient: &RecipientDecl, settings: Option<&HostBinding>) -> u64 {
        let _ = (recipient, settings);
        0
    }

    /// Returns the bytes the queue of an idle delivery host holds, one that had no recipient at boot, while it has any
    /// recipient, given the host's settings if the lowering declares them.
    fn host_queue(&self, host: &HostId, settings: Option<&HostBinding>) -> u64 {
        let _ = (host, settings);
        0
    }
}

/// Bounds that give every resource zero bytes: no preparation holds a credit for a lease or a recipient.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoResourceBounds;

impl ResourceBounds for NoResourceBounds {}

/// What one encode host prepares for one new lease.
#[derive(Clone, Debug, PartialEq)]
pub struct LeasePrepare {
    /// The lease.
    pub lease: LeaseRef,

    /// The output, with the binding the lease runs.
    pub output: OutputDecl,

    /// The slot of every recipient the output's binding resolved to, in its delivery host.
    pub recipients: Vec<RecipientRef>,
}

/// What one recipient of a delivery host is prepared or reopened with.
///
/// A recipient is created with its I/O loop's pieces, which the caller builds through the host's factory; a reopened
/// one keeps its loop, queue, and directory, and takes only the declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct RecipientPrepare {
    /// The recipient's slot, identity, and incarnation.
    pub recipient: RecipientRef,

    /// Its declaration.
    pub decl: RecipientDecl,
}

/// What one host prepares, for a generation or for a resource transaction.
///
/// Every new lease and recipient is built off the data path, through the factories the host exposes, before the
/// host is sent its part; nothing addresses a lease or recipient before the generation that needs it commits, or
/// before the transaction that adds it publishes. A generation's prepare creates, reopens, or requires recipients and
/// never reconfigures one: only resource transactions change a live recipient's settings.
#[derive(Clone, Debug, PartialEq)]
pub struct HostPrepare {
    /// The delivery host; its encode host prepares the leases.
    pub host: HostId,

    /// The leases its encode host creates.
    pub leases: Vec<LeasePrepare>,

    /// The recipients it creates. A recipient the host already holds under the same reference, which another
    /// preparation that has not committed created, is listed again so that the host holds it for this preparation too.
    ///
    /// A host creates an incarnation at most once, so that every incarnation names one loop and reports one exit: a
    /// recipient listed here that the host does not hold, under an incarnation no newer than one of the same identity
    /// it created before, was dropped since, and the host fails its part, after which the core prepares the next
    /// incarnation.
    pub create: Vec<RecipientPrepare>,

    /// The recipients it reopens: declared again while their release waits at the input barrier or their loop drains.
    pub reopen: Vec<RecipientPrepare>,

    /// Every recipient the leases name that the preparation neither creates nor reopens: the host fails its
    /// acknowledgment when one is not live, so a generation never commits over a recipient whose transaction failed to
    /// prepare it.
    pub requires: Vec<RecipientRef>,
}

impl HostPrepare {
    fn empty(host: HostId) -> Self {
        Self {
            host,
            leases: Vec::new(),
            create: Vec::new(),
            reopen: Vec::new(),
            requires: Vec::new(),
        }
    }

    /// Returns `true` if the host has nothing to prepare, so it is not sent a part.
    pub fn is_empty(&self) -> bool {
        self.leases.is_empty() && self.create.is_empty() && self.reopen.is_empty() && self.requires.is_empty()
    }
}

/// What one delivery host publishes in a resource transaction: the recipients payloads that do not name their own are
/// routed over, the host's settings, and the recipients it withdraws.
#[derive(Clone, Debug, PartialEq)]
pub struct HostPublish {
    /// The delivery host.
    pub host: HostId,

    /// The native recipient set: every native recipient the lowering declares on the host, in reference order. The
    /// host routes each payload that does not name its recipients to those of them whose payload classes take it.
    pub native_set: Vec<RecipientRef>,

    /// The host's settings.
    pub settings: HostBinding,

    /// The settings of every recipient of the host, native and declared, which the publication stores into each
    /// recipient's settings: the only path that changes a live recipient's settings.
    pub recipients: Vec<RecipientPrepare>,

    /// The recipients of the host that the current lowering no longer declares: every send of a payload to one is
    /// dropped and counted, whatever lease or generation stamped it, until a later publication no longer lists it.
    pub withdrawn: BTreeSet<RecipientId>,
}

/// What the core plans for a candidate's resources: per host, the leases and recipients to create, reopen, or require,
/// the lease table of every slot's preparation, and the credits the preparation adds to the resource budget.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResourcePlan {
    /// What each host prepares: only hosts with something to prepare.
    pub hosts: Vec<HostPrepare>,

    /// The lease of every output the candidate declares.
    pub leases: Arc<LeaseTable>,

    /// The credits the preparation adds, per resource, in bytes, which the caller acquires before it sends anything.
    pub admitted: Vec<(CreditKey, u64)>,
}

/// A resource transaction: the native recipients to prepare, then what each host publishes.
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceTransaction {
    /// The revision the transaction publishes.
    pub revision: ResourcesRevision,

    /// What each host prepares before the publication: the native recipients the new native sets add or reopen, and
    /// the next incarnations of failed recipients.
    pub prepare: Vec<HostPrepare>,

    /// What each host publishes once every addition is prepared: only hosts whose publication changes. An addition
    /// that fails is left out of its host's publication, which publishes the rest.
    pub publish: Vec<HostPublish>,

    /// The credits the transaction adds, per resource, in bytes: its new recipients' bounds, and an idle host's queue.
    pub admitted: Vec<(CreditKey, u64)>,
}

/// What a snapshot of the resources product says, once the caller checked its shape, its transport hash, and its
/// envelope, and validated its document.
#[derive(Clone, Debug, PartialEq)]
pub enum ResourcesEvent {
    /// An empty snapshot, or an answer without the product: every declared resource is revoked.
    Revoked,

    /// A config the caller could not read: one of several configs, a transport hash or envelope that does not check
    /// out. Its revision is not seen.
    Refused {
        /// The config.
        config: ArtifactConfigId,

        /// The config's version.
        version: u64,

        /// Why it was refused, in plain text.
        reason: String,
    },

    /// The product's one config, read.
    Delivered {
        /// The config.
        config: ArtifactConfigId,

        /// The config's version.
        version: u64,

        /// The resources artifact's revision, on the product's own high-water mark.
        revision: u64,

        /// The hash of the artifact document as received.
        artifact_hash: PlanHash,

        /// The document, validated, or why it is not valid.
        document: Result<ResourcesArtifact, String>,
    },
}

/// What a recipient's drain did, as its host reports when the recipient retires.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RetirementReport {
    /// Requests delivered while draining.
    pub delivered: u64,

    /// Requests persisted at the close.
    pub persisted: u64,

    /// Requests dropped, and counted, at the close.
    pub dropped: u64,

    /// Whether the drain reached its deadline before its queue was empty.
    pub deadline_reached: bool,
}

/// The state of a recipient in the resource table, for status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecipientState {
    /// Prepared by preparations that have not committed: nothing addresses it yet.
    Preparing,

    /// Live: payloads that name its slot reach it, and the native set when it names it.
    Live,

    /// Released, and draining its queued work until its host reports its exit; a preparation that declares it again
    /// reopens it.
    Draining,

    /// Its loop ended on an error. Payloads naming its slot are dropped until its next incarnation, prepared into the
    /// same slot after a backoff, is live.
    Failed,

    /// A reopen found its loop already exited: its next incarnation waits for the exit.
    Exiting,
}

/// One recipient the resource table holds, for status.
#[derive(Clone, Debug, PartialEq)]
pub struct RecipientStatus {
    /// The delivery host.
    pub host: HostId,

    /// The recipient's slot, identity, and incarnation.
    pub recipient: RecipientRef,

    /// Its state.
    pub state: RecipientState,

    /// Who declared it, as the declaration it runs says.
    pub provenance: ResourceProvenance,
}

/// What the table released, for the core to turn into actions: leases to drain, recipients to release, and credits
/// to return.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Released {
    /// The leases to drain, by delivery host.
    pub(super) leases: BTreeMap<HostId, Vec<LeaseRef>>,

    /// The recipients to release, by delivery host.
    pub(super) recipients: BTreeMap<HostId, Vec<RecipientRef>>,

    /// The credits to return.
    pub(super) credits: Vec<CreditKey>,
}

impl Released {
    fn lease(&mut self, host: &HostId, lease: LeaseRef) {
        self.leases.entry(host.clone()).or_default().push(lease);
    }

    fn recipient(&mut self, host: &HostId, recipient: RecipientRef) {
        self.recipients.entry(host.clone()).or_default().push(recipient);
    }
}

/// How a host answered its part of a resource transaction.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum PartAnswer {
    /// It prepared its part.
    Prepared,

    /// It prepared its part, except reopens of recipients whose loops had exited.
    Exited(Vec<RecipientRef>),

    /// It failed, or the budget refused a credit of its part: it holds nothing of the part.
    Failed(String),
}

/// What completing a resource transaction did, for the core to turn into actions.
#[derive(Clone, Debug, Default)]
pub(super) struct Completed {
    /// What each host publishes.
    pub(super) publish: Vec<HostPublish>,

    /// The hosts that did not answer in time, which drop what they prepared for the transaction.
    pub(super) aborted: Vec<HostId>,

    /// What the publication released.
    pub(super) released: Released,

    /// Whether an addition was left out of a publication, so that a later transaction tries it again.
    pub(super) left_out: bool,

    /// Why each addition that failed was left out, by recipient.
    pub(super) failures: Vec<(RecipientRef, String)>,

    /// Whether the transaction prepared anything, which takes time during which the table may change.
    pub(super) prepared: bool,
}

/// Where a lease's lifecycle is.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LeaseState {
    /// Created by a generation that has not committed.
    Preparing,

    /// Committed: generations with an equal binding reuse it.
    Live,

    /// Being drained: no generation uses it, or the generation that created it was aborted. Never reused.
    Draining,
}

#[derive(Clone, Debug)]
struct Lease {
    key: LeaseKey,
    sequence: u64,
    /// The recipients the lease's payloads name.
    recipients: BTreeSet<RecipientId>,
    /// The live generations whose plans use the lease.
    users: BTreeSet<GenerationId>,
    /// The generation that created it, while it has not committed.
    creator: Option<GenerationId>,
    state: LeaseState,
}

/// Who holds a recipient from being released.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum User {
    /// A live generation whose plan uses it.
    Generation(GenerationId),

    /// The published native set, which names it.
    NativeSet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Live,
    Draining,
    Failed,
    Exiting,
}

#[derive(Clone, Debug)]
struct Recipient {
    reference: RecipientRef,
    /// The failed incarnation that `reference`, being prepared, replaces in the same slot, with what held it.
    replaces: Option<Replaced>,
    /// The declaration the host runs: the one it was prepared with, or last published with.
    decl: RecipientDecl,
    phase: Phase,
    /// Whether a preparation that created or reopened the incarnation committed.
    committed: bool,
    /// The preparations that created the incarnation and have not committed.
    creators: BTreeSet<Creator>,
    /// The preparations that reopened the incarnation since its release and have not committed.
    reopened_by: BTreeSet<Creator>,
    users: BTreeSet<User>,
    /// The failures of the recipient's incarnations so far, for the backoff of the next.
    failures: u32,
    /// Whether a failed incarnation's backoff passed, so that a transaction prepares its next incarnation.
    due: bool,
    /// Whether the loop of a released recipient that preparations reopen reported a failure before their reopens were
    /// answered: if a reopen reached the host first, the loop failed live and keeps its slot; otherwise it failed
    /// during its drain, and the reopens answer that it exited.
    failed_exit: bool,
}

/// A failed incarnation whose next incarnation is being prepared into its slot: it takes the slot back if every
/// preparation of the next one aborts, unless nothing holds it either.
#[derive(Clone, Debug)]
struct Replaced {
    reference: RecipientRef,
    committed: bool,
    creators: BTreeSet<Creator>,
    /// The failed incarnation this one replaced, when this one failed before it was committed: it takes the slot back
    /// in turn if nothing holds this one.
    replaces: Option<Box<Replaced>>,
}

impl Replaced {
    /// Forgets `creator` as a creator of this incarnation and of the ones it replaced.
    fn forget(&mut self, creator: Creator) {
        self.creators.remove(&creator);
        if let Some(older) = self.replaces.as_mut() {
            older.forget(creator);
        }
    }

    /// Commits what `creator` created of this incarnation and of the ones it replaced: once one is committed, the
    /// ones it replaced never take the slot back.
    fn commit(&mut self, creator: Creator) {
        if self.creators.contains(&creator) {
            self.committed = true;
            self.creators.clear();
        }
        if self.committed {
            self.replaces = None;
        } else if let Some(older) = self.replaces.as_mut() {
            older.commit(creator);
        }
    }

    /// Returns the newest of this incarnation and the ones it replaced that a preparation holds or that was committed,
    /// which takes the slot back, if any.
    fn restorable(self) -> Option<Self> {
        if self.committed || !self.creators.is_empty() {
            Some(self)
        } else {
            self.replaces.and_then(|older| older.restorable())
        }
    }
}

impl Recipient {
    fn state(&self) -> RecipientState {
        match self.phase {
            Phase::Live if !self.committed => RecipientState::Preparing,
            Phase::Live => RecipientState::Live,
            Phase::Draining if !self.reopened_by.is_empty() => RecipientState::Preparing,
            Phase::Draining => RecipientState::Draining,
            Phase::Failed => RecipientState::Failed,
            Phase::Exiting => RecipientState::Exiting,
        }
    }

    /// Returns `true` if a publication may name the incarnation: it is committed, and live, or, for a native one,
    /// failed in its slot. A failed native recipient waiting for its backoff stays in its host's native set, so that
    /// what is routed to it is dropped and counted at its slot, as a native forwarder counts what it cannot send: an
    /// abort that drops its next incarnation gives the slot back to it, in its host as in the table, before any later
    /// publication.
    fn publishable(&self) -> bool {
        let native = matches!(self.decl.provenance, ResourceProvenance::Native);
        self.committed && (self.phase == Phase::Live || (self.phase == Phase::Failed && native))
    }
}

/// What a host runs from the last publication it was sent.
#[derive(Clone, Debug, PartialEq)]
struct Published {
    revision: ResourcesRevision,
    publish: HostPublish,
}

#[derive(Clone, Debug, Default)]
struct HostResources {
    leases: BTreeMap<EntryId, Lease>,
    live: BTreeMap<LeaseKey, EntryId>,
    recipients: BTreeMap<RecipientId, Recipient>,
    published: Option<Published>,
    /// Where the search for a free lease identifier starts: identifiers are allocated in rotation, so a released one
    /// is reused as late as possible.
    next_entry: u32,
    /// Where the search for a free slot starts, in rotation as for identifiers.
    next_slot: u32,
    next_sequence: u64,
}

impl HostResources {
    fn slot_taken(&self, slot: RecipientSlot) -> bool {
        self.recipients.values().any(|recipient| {
            recipient.reference.slot == slot
                || recipient
                    .replaces
                    .as_ref()
                    .is_some_and(|old| old.reference.slot == slot)
        })
    }

    fn named_by_lease(&self, id: &RecipientId) -> bool {
        self.leases.values().any(|lease| lease.recipients.contains(id))
    }
}

/// What a generation uses, so that its abort and its retirement release exactly that.
#[derive(Clone, Debug, Default)]
struct Uses {
    /// The delivery hosts sent a part of its preparation.
    hosts: BTreeSet<HostId>,
    leases: BTreeSet<(HostId, EntryId)>,
    recipients: BTreeSet<(HostId, RecipientId)>,
    /// The recipients its preparation creates first: incarnations no other preparation created before it.
    created: BTreeSet<(HostId, RecipientId)>,
    /// Per slot, the kernels its programs hold, with the generation that built each.
    kernels: BTreeMap<SlotId, Vec<(KernelKey, GenerationId)>>,
}

/// The resource transaction in flight.
#[derive(Clone, Debug)]
struct InFlight {
    revision: ResourcesRevision,
    /// The resources of the lowering it follows.
    lowered: Resources,
    /// What each host prepares, by host.
    parts: BTreeMap<HostId, HostPrepare>,
    /// What each host answered, once it answered.
    answers: BTreeMap<HostId, PartAnswer>,
    /// The hosts whose publication changes.
    publish: BTreeSet<HostId>,
}

/// The leases and recipients the hosts run for live generations, by host, and the resource transactions that publish
/// what outlives generations.
#[derive(Clone, Debug)]
pub struct ResourceTable {
    hosts: BTreeMap<HostId, HostResources>,
    /// The lease of every output without a binding a plan has declared: fixed destinations, whose components ignore
    /// it. Never released, so never reused for another output.
    unbound: BTreeMap<OutputId, EntryId>,
    next_unbound: u32,
    revision: ResourcesRevision,
    next_incarnation: u64,
    /// The hosts that had a recipient at boot: every other host is idle, and holds its queue's credit while it has any
    /// recipient.
    booted: BTreeSet<HostId>,
    uses: BTreeMap<GenerationId, Uses>,
    transaction: Option<InFlight>,
    /// The hosts whose parts of recent preparations hold nothing, by creator: a part that failed or was never sent, an
    /// unanswered part that was aborted, and every part of an aborted generation. An acknowledgment of one that
    /// arrives later, such as after a worker restart sent the part again, is answered with another abort.
    dropped_parts: VecDeque<(Creator, HostId)>,
    /// Every credit listed and not released yet, with its bytes.
    credits: BTreeMap<CreditKey, u64>,
    bounds: Arc<dyn ResourceBounds>,
}

impl Default for ResourceTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Allocations a preparation plans before they are recorded: identifiers, slots, incarnations, and credits.
struct Planner<'a> {
    table: &'a ResourceTable,
    /// The resources of the plan or lowering being prepared, whose host settings size an idle host's queue.
    resources: &'a Resources,
    entries: BTreeMap<HostId, (u32, BTreeSet<EntryId>)>,
    slots: BTreeMap<HostId, (u32, BTreeSet<RecipientSlot>)>,
    sequences: BTreeMap<HostId, u64>,
    next_incarnation: u64,
    next_unbound: u32,
    unbound: BTreeMap<OutputId, EntryId>,
    /// The hosts the preparation creates a recipient in.
    creates_in: BTreeSet<HostId>,
    admitted: Vec<(CreditKey, u64)>,
}

impl<'a> Planner<'a> {
    fn new(table: &'a ResourceTable, resources: &'a Resources) -> Self {
        Self {
            table,
            resources,
            entries: BTreeMap::new(),
            slots: BTreeMap::new(),
            sequences: BTreeMap::new(),
            next_incarnation: table.next_incarnation,
            next_unbound: table.next_unbound,
            unbound: BTreeMap::new(),
            creates_in: BTreeSet::new(),
            admitted: Vec::new(),
        }
    }

    /// Allocates a lease identifier and sequence in `host`, or `None` once all 65,536 identifiers are taken.
    fn lease(&mut self, host: &HostId) -> Option<(EntryId, u64)> {
        let resources = self.table.hosts.get(host);
        let (cursor, taken) = self
            .entries
            .entry(host.clone())
            .or_insert_with(|| (resources.map_or(0, |resources| resources.next_entry), BTreeSet::new()));
        let free = rotation(*cursor).map(EntryId::new).find(|entry| {
            !taken.contains(entry) && !resources.is_some_and(|resources| resources.leases.contains_key(entry))
        })?;
        taken.insert(free);
        *cursor = u32::from(free.get()) + 1;
        let sequence = self
            .sequences
            .entry(host.clone())
            .or_insert_with(|| resources.map_or(0, |resources| resources.next_sequence));
        let allocated = *sequence;
        *sequence += 1;
        Some((free, allocated))
    }

    /// Allocates a free slot in `host`, or `None` once all 65,536 slots are taken.
    fn slot(&mut self, host: &HostId) -> Option<RecipientSlot> {
        let resources = self.table.hosts.get(host);
        let (cursor, taken) = self
            .slots
            .entry(host.clone())
            .or_insert_with(|| (resources.map_or(0, |resources| resources.next_slot), BTreeSet::new()));
        let free = rotation(*cursor)
            .map(RecipientSlot::new)
            .find(|slot| !taken.contains(slot) && !resources.is_some_and(|resources| resources.slot_taken(*slot)))?;
        taken.insert(free);
        *cursor = u32::from(free.get()) + 1;
        Some(free)
    }

    fn incarnation(&mut self) -> u64 {
        self.next_incarnation += 1;
        self.next_incarnation
    }

    /// Returns a new incarnation of `id` in a free slot of `host`, with its credits.
    fn new_recipient(&mut self, decl: &RecipientDecl) -> Option<RecipientRef> {
        let slot = self.slot(&decl.host)?;
        let recipient = RecipientRef {
            slot,
            id: decl.id.clone(),
            incarnation: self.incarnation(),
        };
        self.credit_recipient(&recipient, decl);
        Some(recipient)
    }

    /// Lists the credits of a new incarnation, and of its idle host's queue if it is the host's first recipient.
    fn credit_recipient(&mut self, recipient: &RecipientRef, decl: &RecipientDecl) {
        let host = &decl.host;
        let bytes = self.table.bounds.recipient(decl, self.resources.host(host));
        if bytes > 0 {
            self.admitted
                .push((CreditKey::Recipient(host.clone(), recipient.clone()), bytes));
        }
        if self.creates_in.insert(host.clone()) && self.table.needs_queue_credit(host) {
            let bytes = self.table.bounds.host_queue(host, self.resources.host(host));
            if bytes > 0 {
                self.admitted.push((CreditKey::HostQueue(host.clone()), bytes));
            }
        }
    }

    /// Lists again the credits a recipient another preparation created already holds, for a preparation that creates
    /// it again or reopens it: the caller acquires those it does not hold, such as one the budget refused the other
    /// preparation, whose recipient this one keeps.
    fn hold_recipient(&mut self, host: &HostId, recipient: &RecipientRef) {
        for credit in [
            CreditKey::Recipient(host.clone(), recipient.clone()),
            CreditKey::HostQueue(host.clone()),
        ] {
            if let Some(bytes) = self.table.credits.get(&credit) {
                if !self.admitted.iter().any(|(listed, _)| *listed == credit) {
                    self.admitted.push((credit, *bytes));
                }
            }
        }
    }

    /// Returns the lease of an output without a binding.
    fn unbound(&mut self, output: &OutputId) -> Option<EntryId> {
        if let Some(lease) = self.table.unbound.get(output).or_else(|| self.unbound.get(output)) {
            return Some(*lease);
        }
        let lease = EntryId::new(u16::try_from(self.next_unbound).ok()?);
        self.next_unbound += 1;
        self.unbound.insert(output.clone(), lease);
        Some(lease)
    }
}

/// Returns every 16-bit value once, starting at `start` and wrapping around.
fn rotation(start: u32) -> impl Iterator<Item = u16> {
    let start = start % (u32::from(u16::MAX) + 1);
    (start..=u32::from(u16::MAX))
        .chain(0..start)
        .map(|value| u16::try_from(value).unwrap_or(u16::MAX))
}

/// Returns the recipients a plan's generation uses, by host and identity, with their declarations: every recipient an
/// output's binding names, and every recipient a resources artifact declared.
fn used_recipients(plan: &Plan) -> BTreeMap<(HostId, RecipientId), &RecipientDecl> {
    let mut used = BTreeMap::new();
    for output in &plan.outputs {
        let Some(binding) = &output.binding else { continue };
        for id in &output.recipients {
            if let Some(decl) = plan.resources.recipient(id).filter(|decl| decl.host == binding.host) {
                used.insert((decl.host.clone(), decl.id.clone()), decl);
            }
        }
    }
    for decl in &plan.resources.recipients {
        if matches!(decl.provenance, ResourceProvenance::Declared(_)) {
            used.insert((decl.host.clone(), decl.id.clone()), decl);
        }
    }
    used
}

/// Returns the delivery hosts a lowering names: those it declares settings, recipients, or bound outputs for.
fn lowered_hosts(resources: &Resources) -> BTreeSet<HostId> {
    resources
        .hosts
        .iter()
        .map(|binding| binding.host.clone())
        .chain(resources.recipients.iter().map(|recipient| recipient.host.clone()))
        .collect()
}

impl ResourceTable {
    /// Creates an empty table whose resources hold no credits.
    pub fn new() -> Self {
        Self::with_bounds(Arc::new(NoResourceBounds))
    }

    /// Creates an empty table whose resources hold the credits `bounds` gives them.
    pub fn with_bounds(bounds: Arc<dyn ResourceBounds>) -> Self {
        Self {
            hosts: BTreeMap::new(),
            unbound: BTreeMap::new(),
            next_unbound: 0,
            revision: ResourcesRevision::default(),
            next_incarnation: 0,
            booted: BTreeSet::new(),
            uses: BTreeMap::new(),
            transaction: None,
            dropped_parts: VecDeque::new(),
            credits: BTreeMap::new(),
            bounds,
        }
    }

    /// Returns the revision of the latest resource transaction.
    pub fn revision(&self) -> ResourcesRevision {
        self.revision
    }

    /// Returns every recipient the table holds, by host and slot, with its state.
    pub fn recipients(&self) -> Vec<RecipientStatus> {
        let mut recipients = self
            .hosts
            .iter()
            .flat_map(|(host, resources)| {
                resources.recipients.values().map(move |recipient| RecipientStatus {
                    host: host.clone(),
                    recipient: recipient.reference.clone(),
                    state: recipient.state(),
                    provenance: recipient.decl.provenance.clone(),
                })
            })
            .collect::<Vec<_>>();
        recipients.sort_by(|a, b| (&a.host, a.recipient.slot).cmp(&(&b.host, b.recipient.slot)));
        recipients
    }

    /// Returns every lease of a host's encode host, draining ones included, by identifier.
    pub fn leases(&self, host: &HostId) -> Vec<LeaseRef> {
        self.lease_states(host).into_iter().map(|(lease, _)| lease).collect()
    }

    /// Returns every lease the table holds in the encode host of `host`, with its state, by identifier.
    pub fn lease_states(&self, host: &HostId) -> Vec<(LeaseRef, LeaseState)> {
        self.hosts.get(host).map_or_else(Vec::new, |resources| {
            resources
                .leases
                .iter()
                .map(|(lease, held)| {
                    let lease = LeaseRef {
                        lease: *lease,
                        key: held.key.clone(),
                    };
                    (lease, held.state)
                })
                .collect()
        })
    }

    /// Returns what a host was last sent to publish, and the revision, if it was sent a publication.
    pub fn published(&self, host: &HostId) -> Option<(ResourcesRevision, &HostPublish)> {
        self.hosts
            .get(host)
            .and_then(|resources| resources.published.as_ref())
            .map(|published| (published.revision, &published.publish))
    }

    /// Returns every credit the table listed and has not released yet, with its bytes.
    pub fn credits(&self) -> impl Iterator<Item = (&CreditKey, u64)> {
        self.credits.iter().map(|(credit, bytes)| (credit, *bytes))
    }

    /// Returns `true` if the table listed the credit and has not released it.
    pub(super) fn holds_credit(&self, credit: &CreditKey) -> bool {
        self.credits.contains_key(credit)
    }

    /// Returns the revision of the resource transaction in flight, if one is.
    pub fn transaction_in_flight(&self) -> Option<ResourcesRevision> {
        self.transaction.as_ref().map(|transaction| transaction.revision)
    }

    /// Returns `true` if a host's last publication withdraws other recipients than those of its table that `lowered`
    /// does not declare: an identity the table no longer holds, such as a retired recipient's or a released failed
    /// one's, which would withdraw a later incarnation, or a recipient created since by a generation of an older
    /// lowering, which would receive what that generation's leases send. A publication that follows `lowered` is due.
    pub(super) fn withdrawal_differs(&self, lowered: &Resources) -> bool {
        self.hosts.values().any(|resources| {
            resources.published.as_ref().is_some_and(|published| {
                let withdrawn = resources
                    .recipients
                    .keys()
                    .filter(|id| lowered.recipient(id).is_none())
                    .cloned()
                    .collect::<BTreeSet<_>>();
                published.publish.withdrawn != withdrawn
            })
        })
    }

    /// Returns `true` if a host's last publication withdraws a recipient that a generation of `plan` uses and that
    /// `lowered` declares again: a publication that follows `lowered` restores it, and until then every payload the
    /// generation's leases send to it is dropped.
    pub(super) fn withdraws_declared(&self, plan: &Plan, lowered: &Resources) -> bool {
        used_recipients(plan).into_keys().any(|(host, id)| {
            lowered.recipient(&id).is_some()
                && self
                    .published(&host)
                    .is_some_and(|(_, publish)| publish.withdrawn.contains(&id))
        })
    }

    /// Returns `true` if an idle host's first recipient would hold its queue's credit: the host had no recipient at
    /// boot, holds none now, and holds no such credit.
    fn needs_queue_credit(&self, host: &HostId) -> bool {
        !self.booted.contains(host)
            && self
                .hosts
                .get(host)
                .is_none_or(|resources| resources.recipients.is_empty())
            && !self.credits.contains_key(&CreditKey::HostQueue(host.clone()))
    }

    /// Returns the recipients a candidate needs whose incarnation's fate is not known yet: a loop known to be exiting,
    /// whose exit its next incarnation waits for, or a loop that failed before any preparation that created it
    /// committed, which goes with those preparations if they abort. The candidate's preparation waits for them.
    pub fn waiting_for(&self, candidate: &Plan) -> Vec<RecipientRef> {
        used_recipients(candidate)
            .keys()
            .filter_map(|(host, id)| self.hosts.get(host)?.recipients.get(id))
            .filter(|recipient| match recipient.phase {
                Phase::Exiting => true,
                Phase::Failed => !recipient.committed,
                Phase::Draining => recipient.failed_exit,
                Phase::Live => false,
            })
            .map(|recipient| recipient.reference.clone())
            .collect()
    }

    /// Plans a candidate's resources: per host, the leases to reuse or create, the recipients to require, reopen, or
    /// create (never to reconfigure: only resource transactions change a live recipient's settings), the lease table
    /// of every slot's preparation, and the credits the preparation adds to the resource budget, per resource.
    ///
    /// Nothing changes until the core records the plan for the generation it prepares. A lease is reused when the
    /// table holds a live lease of the same key; an output without a binding gets the lease it always had, which no
    /// host runs. An output whose lease cannot be allocated, once all 65,536 identifiers of its host are taken, gets
    /// none, and a region rejects a program that delivers to it.
    pub fn plan(&self, candidate: &Plan) -> ResourcePlan {
        let mut planner = Planner::new(self, &candidate.resources);
        let mut parts = BTreeMap::<HostId, HostPrepare>::new();
        let mut refs = BTreeMap::<(HostId, RecipientId), RecipientRef>::new();
        let mut prepared = BTreeSet::<(HostId, RecipientId)>::new();
        for ((host, id), decl) in used_recipients(candidate) {
            let part = parts
                .entry(host.clone())
                .or_insert_with(|| HostPrepare::empty(host.clone()));
            let held = self
                .hosts
                .get(&host)
                .and_then(|resources| resources.recipients.get(&id));
            let reference = match held {
                None => {
                    let Some(recipient) = planner.new_recipient(decl) else {
                        continue;
                    };
                    part.create.push(RecipientPrepare {
                        recipient: recipient.clone(),
                        decl: decl.clone(),
                    });
                    prepared.insert((host.clone(), id.clone()));
                    recipient
                }
                Some(held) => match held.phase {
                    Phase::Live if !held.committed => {
                        part.create.push(RecipientPrepare {
                            recipient: held.reference.clone(),
                            decl: held.decl.clone(),
                        });
                        prepared.insert((host.clone(), id.clone()));
                        planner.hold_recipient(&host, &held.reference);
                        held.reference.clone()
                    }
                    Phase::Draining => {
                        part.reopen.push(RecipientPrepare {
                            recipient: held.reference.clone(),
                            decl: held.decl.clone(),
                        });
                        prepared.insert((host.clone(), id.clone()));
                        planner.hold_recipient(&host, &held.reference);
                        held.reference.clone()
                    }
                    Phase::Live | Phase::Failed | Phase::Exiting => held.reference.clone(),
                },
            };
            refs.insert((host, id), reference);
        }

        let mut leases = LeaseTable::new();
        let mut required = BTreeSet::<(HostId, RecipientId)>::new();
        for output in &candidate.outputs {
            let Some(key) = LeaseKey::of(output, &candidate.resources) else {
                if let Some(lease) = planner.unbound(&output.id) {
                    leases.insert(output.id.clone(), lease);
                }
                continue;
            };
            let Some(binding) = &output.binding else { continue };
            let host = &binding.host;
            let named = output
                .recipients
                .iter()
                .filter_map(|id| refs.get(&(host.clone(), id.clone())).cloned())
                .collect::<Vec<_>>();
            for recipient in &named {
                let held = self
                    .hosts
                    .get(host)
                    .and_then(|resources| resources.recipients.get(&recipient.id));
                let committed_live = held.is_some_and(|held| held.phase == Phase::Live && held.committed);
                if committed_live && !prepared.contains(&(host.clone(), recipient.id.clone())) {
                    required.insert((host.clone(), recipient.id.clone()));
                }
            }
            let reused = self
                .hosts
                .get(host)
                .and_then(|resources| resources.live.get(&key).copied());
            let lease = match reused {
                Some(lease) => lease,
                None => {
                    let Some((lease, sequence)) = planner.lease(host) else {
                        continue;
                    };
                    let bytes = self.bounds.lease(host, output);
                    if bytes > 0 {
                        planner
                            .admitted
                            .push((CreditKey::Lease(host.clone(), lease, sequence), bytes));
                    }
                    parts
                        .entry(host.clone())
                        .or_insert_with(|| HostPrepare::empty(host.clone()))
                        .leases
                        .push(LeasePrepare {
                            lease: LeaseRef { lease, key },
                            output: output.clone(),
                            recipients: named,
                        });
                    lease
                }
            };
            leases.insert(output.id.clone(), lease);
        }
        for (host, id) in required {
            if let (Some(part), Some(reference)) = (parts.get_mut(&host), refs.get(&(host.clone(), id))) {
                part.requires.push(reference.clone());
            }
        }
        ResourcePlan {
            hosts: parts.into_values().filter(|part| !part.is_empty()).collect(),
            leases: Arc::new(leases),
            admitted: planner.admitted,
        }
    }

    /// Returns the resource transaction a lowering needs, if a host's native set, recipient or host settings,
    /// withdrawn set, or failed recipients differ from what it published: the native recipients its native set adds,
    /// and the next incarnations of failed recipients whose backoff passed, to prepare, then what each host publishes
    /// (its native set, every recipient's settings, native and declared, and the recipients of its table the lowering
    /// no longer declares, which it withdraws), with the credits its additions hold.
    ///
    /// A declared recipient, which no native set names, is created by the generation that first declares it: the
    /// transaction only publishes its settings once it is live. Nothing changes until the core begins the
    /// transaction.
    pub fn transaction(&self, lowered: &Resources) -> Option<ResourceTransaction> {
        let revision = self.revision.next();
        let mut planner = Planner::new(self, lowered);
        let mut prepare = Vec::new();
        let mut publish = Vec::new();
        let hosts = lowered_hosts(lowered)
            .into_iter()
            .chain(self.hosts.keys().cloned())
            .collect::<BTreeSet<_>>();
        for host in hosts {
            let resources = self.hosts.get(&host);
            let mut part = HostPrepare::empty(host.clone());
            let mut native_set = Vec::new();
            let mut recipients = Vec::new();
            for decl in lowered.recipients.iter().filter(|decl| decl.host == host) {
                let native = matches!(decl.provenance, ResourceProvenance::Native);
                let held = resources.and_then(|resources| resources.recipients.get(&decl.id));
                let reference = match held {
                    None if native => planner.new_recipient(decl).inspect(|recipient| {
                        part.create.push(RecipientPrepare {
                            recipient: recipient.clone(),
                            decl: decl.clone(),
                        });
                    }),
                    None => None,
                    Some(held) => match held.phase {
                        Phase::Live if held.committed => Some(held.reference.clone()),
                        Phase::Live if native => {
                            part.create.push(RecipientPrepare {
                                recipient: held.reference.clone(),
                                decl: held.decl.clone(),
                            });
                            planner.hold_recipient(&host, &held.reference);
                            Some(held.reference.clone())
                        }
                        Phase::Draining if native && !held.failed_exit => {
                            part.reopen.push(RecipientPrepare {
                                recipient: held.reference.clone(),
                                decl: decl.clone(),
                            });
                            planner.hold_recipient(&host, &held.reference);
                            Some(held.reference.clone())
                        }
                        Phase::Failed if held.due && (native || !held.users.is_empty()) => {
                            let next = RecipientRef {
                                slot: held.reference.slot,
                                id: decl.id.clone(),
                                incarnation: planner.incarnation(),
                            };
                            planner.credit_recipient(&next, decl);
                            part.create.push(RecipientPrepare {
                                recipient: next.clone(),
                                decl: decl.clone(),
                            });
                            Some(next)
                        }
                        // A failed native recipient waits for its backoff in the native set, as a publication names it.
                        Phase::Failed if native && held.committed => Some(held.reference.clone()),
                        // A failed declared one waits for it left out of the publication.
                        Phase::Live | Phase::Draining | Phase::Failed | Phase::Exiting => None,
                    },
                };
                let Some(reference) = reference else { continue };
                if native {
                    native_set.push(reference.clone());
                }
                recipients.push(RecipientPrepare {
                    recipient: reference,
                    decl: decl.clone(),
                });
            }
            native_set.sort();
            recipients.sort_by(|a, b| a.recipient.cmp(&b.recipient));
            let settings = lowered.host(&host).cloned().unwrap_or_else(|| HostBinding {
                host: host.clone(),
                settings: Params::default(),
            });
            let withdrawn = resources
                .map(|resources| {
                    resources
                        .recipients
                        .keys()
                        .filter(|id| lowered.recipient(id).is_none())
                        .cloned()
                        .collect::<BTreeSet<_>>()
                })
                .unwrap_or_default();
            let published = resources.and_then(|resources| resources.published.as_ref());
            let differs = !part.is_empty()
                || match published {
                    None => {
                        !native_set.is_empty()
                            || !recipients.is_empty()
                            || !withdrawn.is_empty()
                            || lowered.host(&host).is_some()
                    }
                    Some(published) => {
                        let published = &published.publish;
                        // A withdrawn set that names an identity the table no longer holds, such as a retired
                        // recipient's, is published again without it, so that its next incarnation is not withdrawn.
                        published.native_set != native_set
                            || published.settings != settings
                            || published.recipients != recipients
                            || published.withdrawn != withdrawn
                    }
                };
            if !differs {
                continue;
            }
            if !part.is_empty() {
                prepare.push(part);
            }
            publish.push(HostPublish {
                host,
                native_set,
                settings,
                recipients,
                withdrawn,
            });
        }
        if publish.is_empty() {
            return None;
        }
        Some(ResourceTransaction {
            revision,
            prepare,
            publish,
            admitted: planner.admitted,
        })
    }

    /// Lists credits, returning those it listed.
    fn list_credits(&mut self, admitted: &[(CreditKey, u64)]) {
        for (credit, bytes) in admitted {
            self.credits.insert(credit.clone(), *bytes);
        }
    }

    /// Releases a credit the table listed, once.
    fn release_credit(&mut self, credit: CreditKey, released: &mut Released) {
        if self.credits.remove(&credit).is_some() {
            released.credits.push(credit);
        }
    }

    fn host_mut(&mut self, host: &HostId) -> &mut HostResources {
        self.hosts.entry(host.clone()).or_default()
    }

    /// Records a recipient a preparation creates, under `creator`: a new incarnation, the next incarnation of a failed
    /// one, or another creation of one being prepared.
    fn record_create(&mut self, create: &RecipientPrepare, creator: Creator) {
        let host = create.decl.host.clone();
        self.next_incarnation = self.next_incarnation.max(create.recipient.incarnation);
        let resources = self.hosts.entry(host).or_default();
        match resources.recipients.get_mut(&create.recipient.id) {
            Some(held) if held.reference == create.recipient => {
                if !held.committed {
                    held.creators.insert(creator);
                }
            }
            Some(held) if held.phase == Phase::Failed && held.reference.slot == create.recipient.slot => {
                // A failed incarnation that failed before it was committed keeps the one it replaced, which takes the
                // slot back if nothing holds either.
                let older = held.replaces.take().map(Box::new);
                held.replaces = Some(Replaced {
                    reference: std::mem::replace(&mut held.reference, create.recipient.clone()),
                    committed: held.committed,
                    creators: std::mem::replace(&mut held.creators, BTreeSet::from([creator])),
                    replaces: older,
                });
                held.decl = create.decl.clone();
                held.phase = Phase::Live;
                held.committed = false;
                held.reopened_by.clear();
                held.due = false;
            }
            Some(_) => debug_assert!(false, "{} is prepared over another incarnation", create.recipient),
            None => {
                // Slots are allocated in rotation, from the one after the slot allocated last.
                resources.next_slot = (u32::from(create.recipient.slot.get()) + 1) % 65_536;
                resources.recipients.insert(
                    create.recipient.id.clone(),
                    Recipient {
                        reference: create.recipient.clone(),
                        replaces: None,
                        decl: create.decl.clone(),
                        phase: Phase::Live,
                        committed: false,
                        creators: BTreeSet::from([creator]),
                        reopened_by: BTreeSet::new(),
                        users: BTreeSet::new(),
                        failures: 0,
                        due: false,
                        failed_exit: false,
                    },
                );
            }
        }
    }

    /// Records a reopen of a draining recipient under `creator`.
    fn record_reopen(&mut self, reopen: &RecipientPrepare, creator: Creator) {
        if let Some(held) = self
            .hosts
            .get_mut(&reopen.decl.host)
            .and_then(|resources| resources.recipients.get_mut(&reopen.recipient.id))
            .filter(|held| held.reference == reopen.recipient && held.phase == Phase::Draining)
        {
            held.reopened_by.insert(creator);
        }
    }

    /// Records what a generation's preparation planned: its new leases and recipients, those it reopens or requires,
    /// what it uses, and the credits it lists. `kernels` holds, per slot, the kernels its programs hold with the
    /// generation that built each, and the bytes of those it builds.
    pub(super) fn record(
        &mut self, generation: GenerationId, candidate: &Plan, plan: &ResourcePlan,
        kernels: BTreeMap<SlotId, (Vec<(KernelKey, GenerationId)>, u64)>,
    ) -> Vec<(CreditKey, u64)> {
        let creator = Creator::Generation(generation);
        let mut uses = Uses::default();
        for part in &plan.hosts {
            uses.hosts.insert(part.host.clone());
            for create in &part.create {
                let again = self
                    .hosts
                    .get(&part.host)
                    .and_then(|resources| resources.recipients.get(&create.recipient.id))
                    .is_some_and(|held| held.reference == create.recipient);
                if !again {
                    uses.created.insert((part.host.clone(), create.recipient.id.clone()));
                }
                self.record_create(create, creator);
            }
            for reopen in &part.reopen {
                self.record_reopen(reopen, creator);
            }
            let resources = self.host_mut(&part.host);
            for lease in &part.leases {
                let entry = lease.lease.lease;
                resources.next_entry = (u32::from(entry.get()) + 1) % 65_536;
                resources.leases.insert(
                    entry,
                    Lease {
                        key: lease.lease.key.clone(),
                        sequence: resources.next_sequence,
                        recipients: lease.recipients.iter().map(|recipient| recipient.id.clone()).collect(),
                        users: BTreeSet::new(),
                        creator: Some(generation),
                        state: LeaseState::Preparing,
                    },
                );
                resources.next_sequence += 1;
            }
        }
        for output in &candidate.outputs {
            let (Some(key), Some(lease)) = (
                LeaseKey::of(output, &candidate.resources),
                plan.leases.lease(&output.id),
            ) else {
                if output.binding.is_none() {
                    if let Some(lease) = plan.leases.lease(&output.id) {
                        if !self.unbound.contains_key(&output.id) {
                            self.unbound.insert(output.id.clone(), lease);
                            self.next_unbound = self.next_unbound.max(u32::from(lease.get()) + 1);
                        }
                    }
                }
                continue;
            };
            let Some(binding) = &output.binding else { continue };
            if let Some(held) = self
                .hosts
                .get_mut(&binding.host)
                .and_then(|resources| resources.leases.get_mut(&lease))
                .filter(|held| held.key == key)
            {
                held.users.insert(generation);
                uses.leases.insert((binding.host.clone(), lease));
            }
        }
        for (host, id) in used_recipients(candidate).into_keys() {
            if let Some(held) = self
                .hosts
                .get_mut(&host)
                .and_then(|resources| resources.recipients.get_mut(&id))
            {
                held.users.insert(User::Generation(generation));
                uses.recipients.insert((host, id));
            }
        }
        let mut admitted = plan.admitted.clone();
        for (slot, (held, bytes)) in kernels {
            if bytes > 0 {
                admitted.push((CreditKey::Instances(generation, slot), bytes));
            }
            uses.kernels.insert(slot, held);
        }
        self.list_credits(&admitted);
        self.uses.insert(generation, uses);
        admitted
    }

    /// Returns the generation that built the kernel `key` that `generation`'s program holds in `slot`, if it records
    /// one.
    pub(super) fn builder_of(&self, generation: GenerationId, slot: SlotId, key: &KernelKey) -> Option<GenerationId> {
        self.uses
            .get(&generation)?
            .kernels
            .get(&slot)?
            .iter()
            .find(|(held, _)| held == key)
            .map(|(_, builder)| *builder)
    }

    /// Returns what a generation's preparation creates first in a host: the recipients no other preparation created
    /// before it, and the outputs of the leases it creates there.
    ///
    /// Only these can fail for what their declarations say. A recipient the part requires or reopens, or creates again
    /// under another preparation's reference, was prepared before, so a failure that names it is a race the next
    /// preparation recovers from, such as a loop that failed before its exit was reported.
    pub(super) fn created_first(&self, generation: GenerationId, host: &HostId) -> (Vec<&RecipientId>, Vec<&OutputId>) {
        let recipients = self
            .uses
            .get(&generation)
            .map(|uses| {
                uses.created
                    .iter()
                    .filter(|(created_in, _)| created_in == host)
                    .map(|(_, id)| id)
                    .collect()
            })
            .unwrap_or_default();
        let outputs = self
            .hosts
            .get(host)
            .map(|resources| {
                resources
                    .leases
                    .values()
                    .filter(|lease| lease.creator == Some(generation))
                    .map(|lease| &lease.key.output)
                    .collect()
            })
            .unwrap_or_default();
        (recipients, outputs)
    }

    /// Returns the output a lease of a host's encode host runs, if the table holds the lease.
    pub(super) fn lease_output(&self, host: &HostId, lease: EntryId) -> Option<&OutputId> {
        self.hosts.get(host)?.leases.get(&lease).map(|held| &held.key.output)
    }

    /// Records that a host's part of a preparation holds nothing any more.
    fn drop_part(&mut self, creator: Creator, host: &HostId) {
        /// The most dropped parts remembered: those of the latest preparations, which a worker restart repeats.
        const REMEMBERED: usize = 64;
        let part = (creator, host.clone());
        if !self.dropped_parts.contains(&part) {
            self.dropped_parts.push_back(part);
        }
        while self.dropped_parts.len() > REMEMBERED {
            self.dropped_parts.pop_front();
        }
    }

    /// Returns `true` if a host's part of a preparation holds nothing in the table: an acknowledgment of it that
    /// arrives now means the host prepared it again, after it was aborted or while the table counted it as failed,
    /// and the host must abort it again.
    pub(super) fn is_dropped_part(&self, creator: Creator, host: &HostId) -> bool {
        self.dropped_parts.contains(&(creator, host.clone()))
    }

    /// Returns the delivery hosts sent a part of a generation's preparation.
    pub(super) fn hosts_of(&self, generation: GenerationId) -> Vec<HostId> {
        self.uses
            .get(&generation)
            .map(|uses| uses.hosts.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Commits what a generation prepared: its new leases are live, and the recipients it created or reopened are
    /// committed.
    pub(super) fn commit(&mut self, generation: GenerationId) {
        let creator = Creator::Generation(generation);
        for resources in self.hosts.values_mut() {
            for (entry, lease) in resources.leases.iter_mut() {
                if lease.creator == Some(generation) {
                    lease.creator = None;
                    lease.state = LeaseState::Live;
                    resources.live.insert(lease.key.clone(), *entry);
                }
            }
            for recipient in resources.recipients.values_mut() {
                commit_recipient(recipient, creator);
            }
        }
    }

    /// Aborts what a generation prepared, which never committed: its new leases drain, the recipients only it created
    /// are dropped, those it reopened go back to the drain they were in, and its credits for what it dropped return.
    pub(super) fn abort(&mut self, generation: GenerationId) -> Released {
        let mut released = Released::default();
        let creator = Creator::Generation(generation);
        let uses = self.uses.remove(&generation).unwrap_or_default();
        for host in &uses.hosts {
            self.drop_part(creator, host);
        }
        let hosts = self.hosts.keys().cloned().collect::<Vec<_>>();
        for host in &hosts {
            let Some(resources) = self.hosts.get_mut(host) else {
                continue;
            };
            let mut drained = Vec::new();
            for (entry, lease) in resources.leases.iter_mut() {
                lease.users.remove(&generation);
                if lease.creator == Some(generation) {
                    lease.creator = None;
                    lease.state = LeaseState::Draining;
                    drained.push(LeaseRef {
                        lease: *entry,
                        key: lease.key.clone(),
                    });
                }
            }
            for lease in drained {
                released.lease(host, lease);
            }
            self.drop_creator(host, creator, &mut released);
        }
        for (host, id) in &uses.recipients {
            self.remove_user(host, id, User::Generation(generation), &mut released);
        }
        for (host, entry) in &uses.leases {
            self.release_unused_lease(host, *entry, &mut released);
        }
        for slot in uses.kernels.keys() {
            self.release_credit(CreditKey::Instances(generation, *slot), &mut released);
        }
        self.release_unheld_kernels(generation, &uses, &mut released);
        released
    }

    /// Removes `creator` from every recipient of `host` it created or reopened and that did not commit: a recipient
    /// that no other preparation holds is dropped, and its credits return; a failed one being replaced goes back to
    /// its failed incarnation; a reopened one goes back to the drain it was in.
    fn drop_creator(&mut self, host: &HostId, creator: Creator, released: &mut Released) {
        let Some(resources) = self.hosts.get_mut(host) else {
            return;
        };
        let mut dropped = Vec::new();
        let mut reopened = Vec::new();
        for recipient in resources.recipients.values_mut() {
            if recipient.reopened_by.remove(&creator) && recipient.failed_exit {
                reopened.push(recipient.reference.id.clone());
            }
            if let Some(replaced) = recipient.replaces.as_mut() {
                replaced.forget(creator);
            }
            if recipient.committed || !recipient.creators.remove(&creator) || !recipient.creators.is_empty() {
                continue;
            }
            dropped.push(recipient.reference.clone());
            // The next incarnation of a failed recipient was never prepared: the failed one takes its slot back, as
            // its host restores it, unless nothing holds it either. Its backoff passed already, so the next
            // transaction prepares its next incarnation again.
            if let Some(failed) = recipient.replaces.take().and_then(Replaced::restorable) {
                recipient.reference = failed.reference;
                recipient.committed = failed.committed;
                recipient.creators = failed.creators;
                recipient.replaces = failed.replaces.map(|older| *older);
                recipient.phase = Phase::Failed;
                recipient.due = true;
            }
        }
        for reference in dropped {
            // What only an aborted preparation created never ran for anyone: its slot and identity are free at once,
            // and the host's report of its exit is ignored, since the table no longer holds the incarnation.
            if let Some(resources) = self.hosts.get_mut(host) {
                if resources
                    .recipients
                    .get(&reference.id)
                    .is_some_and(|held| held.reference == reference)
                {
                    resources.recipients.remove(&reference.id);
                }
            }
            self.release_credit(CreditKey::Recipient(host.clone(), reference), released);
        }
        // A released recipient whose loop failed while the aborted preparation reopened it is gone once nothing else
        // reopens it, whichever came first in its host.
        for id in reopened {
            self.end_failed_reopen(host, &id, released);
        }
        self.release_queue_if_empty(host, released);
    }

    /// Returns an idle host's queue credit once it holds no recipient.
    fn release_queue_if_empty(&mut self, host: &HostId, released: &mut Released) {
        if self
            .hosts
            .get(host)
            .is_some_and(|resources| resources.recipients.is_empty())
        {
            self.release_credit(CreditKey::HostQueue(host.clone()), released);
        }
    }

    /// Removes a user of a recipient, and releases the recipient if nothing holds it any more.
    fn remove_user(&mut self, host: &HostId, id: &RecipientId, user: User, released: &mut Released) {
        if let Some(recipient) = self
            .hosts
            .get_mut(host)
            .and_then(|resources| resources.recipients.get_mut(id))
        {
            recipient.users.remove(&user);
        }
        self.release_if_unused(host, id, released);
    }

    /// Releases a recipient that no user holds and no lease names: a live one drains, and a failed one, whose exit was
    /// reported, frees its slot.
    fn release_if_unused(&mut self, host: &HostId, id: &RecipientId, released: &mut Released) {
        let Some(resources) = self.hosts.get_mut(host) else {
            return;
        };
        if resources.named_by_lease(id) {
            return;
        }
        let Some(recipient) = resources.recipients.get_mut(id) else {
            return;
        };
        if !recipient.users.is_empty() || !recipient.committed || !recipient.reopened_by.is_empty() {
            return;
        }
        match recipient.phase {
            Phase::Live if recipient.creators.is_empty() => {
                recipient.phase = Phase::Draining;
                let reference = recipient.reference.clone();
                released.recipient(host, reference);
            }
            Phase::Failed if recipient.creators.is_empty() => {
                let reference = recipient.reference.clone();
                resources.recipients.remove(id);
                released.recipient(host, reference);
                self.release_queue_if_empty(host, released);
            }
            Phase::Live | Phase::Failed | Phase::Draining | Phase::Exiting => {}
        }
    }

    /// Drains a live lease that no generation uses any more.
    fn release_unused_lease(&mut self, host: &HostId, entry: EntryId, released: &mut Released) {
        let Some(resources) = self.hosts.get_mut(host) else {
            return;
        };
        let Some(lease) = resources.leases.get_mut(&entry) else {
            return;
        };
        if lease.state != LeaseState::Live || !lease.users.is_empty() {
            return;
        }
        lease.state = LeaseState::Draining;
        if resources.live.get(&lease.key) == Some(&entry) {
            resources.live.remove(&lease.key);
        }
        released.lease(
            host,
            LeaseRef {
                lease: entry,
                key: lease.key.clone(),
            },
        );
    }

    /// Returns the credits of the kernel instances that `generation`, whose uses are `uses`, built or held, once no
    /// live generation holds any instance their builder built in the slot.
    fn release_unheld_kernels(&mut self, generation: GenerationId, uses: &Uses, released: &mut Released) {
        for (slot, held) in &uses.kernels {
            let builders = held
                .iter()
                .map(|(_, builder)| *builder)
                .chain([generation])
                .collect::<BTreeSet<_>>();
            for builder in builders {
                let still_held = self.uses.values().any(|other| {
                    other
                        .kernels
                        .get(slot)
                        .is_some_and(|held| held.iter().any(|(_, built)| *built == builder))
                });
                if !still_held {
                    self.release_credit(CreditKey::Instances(builder, *slot), released);
                }
            }
        }
    }

    /// Releases what a retired generation used: leases no other live generation uses drain, recipients nothing holds
    /// any more are released, and the credits of kernel instances no live generation holds return.
    pub(super) fn retire(&mut self, generation: GenerationId) -> Released {
        let mut released = Released::default();
        let Some(uses) = self.uses.remove(&generation) else {
            return released;
        };
        for (host, entry) in &uses.leases {
            if let Some(lease) = self
                .hosts
                .get_mut(host)
                .and_then(|resources| resources.leases.get_mut(entry))
            {
                lease.users.remove(&generation);
            }
            self.release_unused_lease(host, *entry, &mut released);
        }
        for (host, id) in &uses.recipients {
            self.remove_user(host, id, User::Generation(generation), &mut released);
        }
        self.release_unheld_kernels(generation, &uses, &mut released);
        released
    }

    /// Records that an encode host drained a lease: its identifier is free, its credit returns, and the recipients it
    /// named are released if nothing else holds them. Returns `None` if the table is not draining that lease.
    pub(super) fn lease_released(&mut self, host: &HostId, lease: &LeaseRef) -> Option<Released> {
        let resources = self.hosts.get_mut(host)?;
        let held = resources
            .leases
            .get(&lease.lease)
            .filter(|held| held.key == lease.key && held.state == LeaseState::Draining)?;
        let sequence = held.sequence;
        let named = held.recipients.clone();
        resources.leases.remove(&lease.lease);
        let mut released = Released::default();
        self.release_credit(CreditKey::Lease(host.clone(), lease.lease, sequence), &mut released);
        for id in named {
            self.release_if_unused(host, &id, &mut released);
        }
        Some(released)
    }

    /// Finds the host and the record holding exactly the incarnation `recipient`.
    fn holding(&self, recipient: &RecipientRef) -> Option<HostId> {
        self.hosts.iter().find_map(|(host, resources)| {
            resources
                .recipients
                .get(&recipient.id)
                .filter(|held| held.reference == *recipient)
                .map(|_| host.clone())
        })
    }

    /// Records that a recipient incarnation's loop exited after its release, or after a reopen found it exited: its
    /// slot is free and its credits return. Returns `None` if the table does not hold that incarnation, such as one
    /// an abort dropped, whose exit changes nothing.
    pub(super) fn recipient_retired(&mut self, recipient: &RecipientRef) -> Option<Released> {
        let host = self.holding(recipient)?;
        let resources = self.hosts.get_mut(&host)?;
        let held = resources.recipients.get(&recipient.id)?;
        let mut released = Released::default();
        match held.phase {
            Phase::Draining | Phase::Exiting => {
                resources.recipients.remove(&recipient.id);
                self.release_credit(CreditKey::Recipient(host.clone(), recipient.clone()), &mut released);
                self.release_queue_if_empty(&host, &mut released);
            }
            // A loop that retires without a release ended on its own: it is failed, as an error would leave it.
            Phase::Live | Phase::Failed => return self.recipient_failed(recipient),
        }
        Some(released)
    }

    /// Records that a recipient incarnation's loop ended on an error. A failed loop keeps its slot in its host, as a
    /// marker payloads naming it are dropped at, until its next incarnation is prepared into the slot or it is
    /// released. So a live one is failed in its slot, and its next incarnation is prepared by a transaction after its
    /// backoff. A released one is gone, and the release follows again, since its host keeps its slot if the failure
    /// came after the release, and ignores the release if it came before. A released one that preparations reopen
    /// waits for their answers: if a reopen reached the host before the failure, the loop failed live.
    ///
    /// Returns what was released, or `None` if the table does not hold that incarnation, or already counts it failed.
    pub(super) fn recipient_failed(&mut self, recipient: &RecipientRef) -> Option<Released> {
        let host = self.holding(recipient)?;
        let resources = self.hosts.get_mut(&host)?;
        let held = resources.recipients.get_mut(&recipient.id)?;
        let mut released = Released::default();
        match held.phase {
            Phase::Failed => return None,
            Phase::Live => {
                held.phase = Phase::Failed;
                held.failures = held.failures.saturating_add(1);
                held.due = false;
                self.release_credit(CreditKey::Recipient(host.clone(), recipient.clone()), &mut released);
                self.release_if_unused(&host, &recipient.id, &mut released);
            }
            Phase::Draining if !held.reopened_by.is_empty() => {
                if held.failed_exit {
                    return None;
                }
                held.failed_exit = true;
                self.release_credit(CreditKey::Recipient(host.clone(), recipient.clone()), &mut released);
            }
            Phase::Draining | Phase::Exiting => {
                resources.recipients.remove(&recipient.id);
                released.recipient(&host, recipient.clone());
                self.release_credit(CreditKey::Recipient(host.clone(), recipient.clone()), &mut released);
                self.release_queue_if_empty(&host, &mut released);
            }
        }
        Some(released)
    }

    /// Ends a released recipient whose loop failed while preparations reopened it, once no reopen of it reached its
    /// host before the failure: it is gone, and the release follows again, as for a failure during a drain.
    fn end_failed_reopen(&mut self, host: &HostId, id: &RecipientId, released: &mut Released) {
        let Some(resources) = self.hosts.get_mut(host) else {
            return;
        };
        let Some(held) = resources.recipients.get(id) else {
            return;
        };
        if !held.failed_exit || !held.reopened_by.is_empty() {
            return;
        }
        let reference = held.reference.clone();
        resources.recipients.remove(id);
        released.recipient(host, reference);
        self.release_queue_if_empty(host, released);
    }

    /// Returns the host of `recipient` and the failures so far of its identity, if the table holds that incarnation
    /// failed, waiting for its backoff before its next incarnation is prepared.
    pub(super) fn failed(&self, recipient: &RecipientRef) -> Option<(HostId, u32)> {
        let host = self.holding(recipient)?;
        let held = self.hosts.get(&host)?.recipients.get(&recipient.id)?;
        (held.phase == Phase::Failed && !held.due).then_some((host, held.failures))
    }

    /// Makes the next incarnation of a failed recipient due: its own backoff passed.
    pub(super) fn failure_due(&mut self, host: &HostId, id: &RecipientId) {
        if let Some(recipient) = self
            .hosts
            .get_mut(host)
            .and_then(|resources| resources.recipients.get_mut(id))
            .filter(|recipient| recipient.phase == Phase::Failed)
        {
            recipient.due = true;
        }
    }

    /// Records that a host answered a reopen of `recipients` for `creator` with `Exited`: their loops exited, so each
    /// waits for its exit, after which its next incarnation is prepared. One whose failure was reported already, and
    /// that no other reopen holds, is gone.
    pub(super) fn reopen_exited(&mut self, creator: Creator, recipients: &[RecipientRef], released: &mut Released) {
        for recipient in recipients {
            let Some(host) = self.holding(recipient) else { continue };
            if let Some(held) = self
                .hosts
                .get_mut(&host)
                .and_then(|resources| resources.recipients.get_mut(&recipient.id))
            {
                held.reopened_by.remove(&creator);
                if held.phase == Phase::Draining && !held.failed_exit {
                    held.phase = Phase::Exiting;
                }
            }
            self.end_failed_reopen(&host, &recipient.id, released);
        }
    }

    /// Begins a resource transaction: records what it creates and reopens, and the credits it lists, then supersedes
    /// the transaction in flight, if any, which has not published: what both prepare stays prepared for the newer
    /// one, and what only the older one prepared is dropped. Returns the superseded transaction's revision, the hosts
    /// it was sent a part to, which the caller aborts after sending the newer one's parts, and what it released.
    pub(super) fn begin(
        &mut self, transaction: &ResourceTransaction, lowered: &Resources,
    ) -> Option<(ResourcesRevision, Vec<HostId>, Released)> {
        let creator = Creator::Transaction(transaction.revision);
        for part in &transaction.prepare {
            for create in &part.create {
                self.record_create(create, creator);
            }
            for reopen in &part.reopen {
                self.record_reopen(reopen, creator);
            }
        }
        self.list_credits(&transaction.admitted);
        let superseded = self.supersede();
        self.revision = transaction.revision;
        self.transaction = Some(InFlight {
            revision: transaction.revision,
            lowered: lowered.clone(),
            parts: transaction
                .prepare
                .iter()
                .map(|part| (part.host.clone(), part.clone()))
                .collect(),
            answers: BTreeMap::new(),
            publish: transaction.publish.iter().map(|publish| publish.host.clone()).collect(),
        });
        superseded
    }

    /// Returns `true` if the transaction in flight follows a lowering other than `lowered`.
    pub(super) fn transaction_follows_other(&self, lowered: &Resources) -> bool {
        self.transaction
            .as_ref()
            .is_some_and(|transaction| transaction.lowered != *lowered)
    }

    /// Records a host's answer to its part of the transaction in flight, and returns whether every host answered, with
    /// what the answer released. Returns `None` if no transaction of that revision is in flight, or the host has no
    /// part in it, or answered already.
    ///
    /// A failed part holds nothing in its host, which changed nothing, or was sent nothing after a refusal of the
    /// budget: what the transaction created there is dropped at once, so that no later preparation counts on the host
    /// holding it for the transaction. A part whose reopens found loops exited waits for their exits at once too.
    pub(super) fn answer(
        &mut self, revision: ResourcesRevision, host: &HostId, answer: PartAnswer,
    ) -> Option<(bool, Released)> {
        let transaction = self
            .transaction
            .as_mut()
            .filter(|transaction| transaction.revision == revision)?;
        if !transaction.parts.contains_key(host) || transaction.answers.contains_key(host) {
            return None;
        }
        let creator = Creator::Transaction(revision);
        transaction.answers.insert(host.clone(), answer.clone());
        let answered = transaction.answers.len() == transaction.parts.len();
        let mut released = Released::default();
        match &answer {
            PartAnswer::Prepared => {}
            PartAnswer::Exited(exited) => self.reopen_exited(creator, exited, &mut released),
            PartAnswer::Failed(_) => {
                self.drop_creator(host, creator, &mut released);
                self.drop_part(creator, host);
            }
        }
        Some((answered, released))
    }

    /// Returns `true` if a transaction of that revision is in flight.
    pub(super) fn in_flight(&self, revision: ResourcesRevision) -> bool {
        self.transaction_in_flight() == Some(revision)
    }

    /// Supersedes the transaction in flight, which has not published: every host sent a part drops what only the
    /// transaction created and returns what it reopened to draining. Returns its revision and those hosts.
    pub(super) fn supersede(&mut self) -> Option<(ResourcesRevision, Vec<HostId>, Released)> {
        let transaction = self.transaction.take()?;
        let creator = Creator::Transaction(transaction.revision);
        let mut released = Released::default();
        let hosts = transaction.parts.keys().cloned().collect::<Vec<_>>();
        for host in &hosts {
            self.drop_creator(host, creator, &mut released);
            self.drop_part(creator, host);
        }
        Some((transaction.revision, hosts, released))
    }

    /// Completes the transaction in flight once every host answered, or its timeout passed: what each answering host
    /// prepared is committed, what a failed or unanswered host was asked to prepare is dropped, and each host whose
    /// publication changes publishes its native set without the additions that failed, its settings, every live
    /// recipient's settings, and its withdrawn set. The published native set holds the recipients it names, and those
    /// it no longer names are released if nothing else holds them.
    pub(super) fn complete(&mut self) -> Option<Completed> {
        let transaction = self.transaction.take()?;
        let creator = Creator::Transaction(transaction.revision);
        let mut completed = Completed {
            prepared: !transaction.parts.is_empty(),
            ..Completed::default()
        };
        for (host, part) in &transaction.parts {
            match transaction.answers.get(host) {
                Some(PartAnswer::Prepared) => self.commit_part(host, creator),
                // The exited reopens already wait for their exits.
                Some(PartAnswer::Exited(_)) => {
                    self.commit_part(host, creator);
                    completed.left_out = true;
                }
                // What the part created was dropped when it answered.
                Some(PartAnswer::Failed(reason)) => {
                    for create in part.create.iter().chain(&part.reopen) {
                        completed.failures.push((create.recipient.clone(), reason.clone()));
                    }
                    completed.left_out = true;
                }
                None => {
                    completed.aborted.push(host.clone());
                    self.drop_creator(host, creator, &mut completed.released);
                    self.drop_part(creator, host);
                    completed.left_out = true;
                }
            }
        }
        for host in &transaction.publish {
            let publish = self.publication(host, &transaction.lowered);
            let resources = self.host_mut(host);
            resources.published = Some(Published {
                revision: transaction.revision,
                publish: publish.clone(),
            });
            // The native set holds every native recipient the lowering declares, those the publication left out
            // included: a failed one keeps its slot for its next incarnation.
            let named = transaction
                .lowered
                .recipients
                .iter()
                .filter(|decl| decl.host == *host && matches!(decl.provenance, ResourceProvenance::Native))
                .map(|decl| decl.id.clone())
                .collect::<BTreeSet<_>>();
            for (id, recipient) in resources.recipients.iter_mut() {
                if named.contains(id) {
                    recipient.users.insert(User::NativeSet);
                } else {
                    recipient.users.remove(&User::NativeSet);
                }
                if let Some(settings) = publish
                    .recipients
                    .iter()
                    .find(|settings| settings.recipient == recipient.reference)
                {
                    recipient.decl = settings.decl.clone();
                }
            }
            let ids = resources.recipients.keys().cloned().collect::<Vec<_>>();
            for id in ids {
                self.release_if_unused(host, &id, &mut completed.released);
            }
            completed.publish.push(publish);
        }
        Some(completed)
    }

    /// Commits what a transaction prepared in one host.
    fn commit_part(&mut self, host: &HostId, creator: Creator) {
        if let Some(resources) = self.hosts.get_mut(host) {
            for recipient in resources.recipients.values_mut() {
                commit_recipient(recipient, creator);
            }
        }
    }

    /// Returns what a host publishes for a lowering, given what the table holds: every native recipient the lowering
    /// declares that a publication may name, every such recipient's settings, the host's settings, and every recipient
    /// of its table the lowering does not declare, withdrawn.
    fn publication(&self, host: &HostId, lowered: &Resources) -> HostPublish {
        let resources = self.hosts.get(host);
        let mut native_set = Vec::new();
        let mut recipients = Vec::new();
        for decl in lowered.recipients.iter().filter(|decl| decl.host == *host) {
            let Some(held) = resources
                .and_then(|resources| resources.recipients.get(&decl.id))
                .filter(|held| held.publishable())
            else {
                continue;
            };
            if matches!(decl.provenance, ResourceProvenance::Native) {
                native_set.push(held.reference.clone());
            }
            recipients.push(RecipientPrepare {
                recipient: held.reference.clone(),
                decl: decl.clone(),
            });
        }
        native_set.sort();
        recipients.sort_by(|a, b| a.recipient.cmp(&b.recipient));
        let withdrawn = resources
            .map(|resources| {
                resources
                    .recipients
                    .keys()
                    .filter(|id| lowered.recipient(id).is_none())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        HostPublish {
            host: host.clone(),
            native_set,
            settings: lowered.host(host).cloned().unwrap_or_else(|| HostBinding {
                host: host.clone(),
                settings: Params::default(),
            }),
            recipients,
            withdrawn,
        }
    }

    /// Records the boot generation's resources and the boot transaction, which the topology builds the hosts with:
    /// everything they prepare is committed at once, the hosts that hold a recipient are not idle, and the
    /// transaction is published. Returns what each host is built with, what each publishes, and the credits listed.
    pub(super) fn boot(
        &mut self, generation: GenerationId, candidate: &Plan, plan: &ResourcePlan,
        kernels: BTreeMap<SlotId, (Vec<(KernelKey, GenerationId)>, u64)>,
    ) -> (Vec<HostPrepare>, Vec<HostPublish>, Vec<(CreditKey, u64)>) {
        let mut admitted = self.record(generation, candidate, plan, kernels);
        self.commit(generation);
        let mut hosts = plan
            .hosts
            .iter()
            .map(|part| (part.host.clone(), part.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut publish = Vec::new();
        if let Some(transaction) = self.transaction(&candidate.resources) {
            let superseded = self.begin(&transaction, &candidate.resources);
            debug_assert!(superseded.is_none(), "no transaction is in flight at boot");
            for part in &transaction.prepare {
                let host = hosts
                    .entry(part.host.clone())
                    .or_insert_with(|| HostPrepare::empty(part.host.clone()));
                host.create.extend(part.create.iter().cloned());
                host.reopen.extend(part.reopen.iter().cloned());
                if let Some(transaction) = self.transaction.as_mut() {
                    transaction.answers.insert(part.host.clone(), PartAnswer::Prepared);
                }
            }
            admitted.extend(transaction.admitted.iter().cloned());
            if let Some(completed) = self.complete() {
                publish = completed.publish;
            }
        }
        self.booted = self
            .hosts
            .iter()
            .filter(|(_, resources)| !resources.recipients.is_empty())
            .map(|(host, _)| host.clone())
            .collect();
        // The boot generation's resources are counted in the boot bound, and acquired at once: no idle host's queue
        // is among them, since a host with a recipient at boot is not idle.
        admitted.retain(|(credit, _)| !matches!(credit, CreditKey::HostQueue(_)));
        self.credits
            .retain(|credit, _| !matches!(credit, CreditKey::HostQueue(_)));
        (hosts.into_values().collect(), publish, admitted)
    }
}

/// Commits a recipient that `creator` created or reopened.
///
/// A failed incarnation `creator` created that a preparation of its next incarnation replaces is committed too: if
/// every preparation of the next one aborts, it takes its slot back as committed, so that it is released once nothing
/// uses it rather than waiting for a creator that will never abort or commit again.
fn commit_recipient(recipient: &mut Recipient, creator: Creator) {
    if let Some(replaced) = recipient.replaces.as_mut() {
        replaced.commit(creator);
    }
    if recipient.creators.contains(&creator) {
        recipient.committed = true;
        recipient.creators.clear();
        recipient.replaces = None;
    }
    if recipient.reopened_by.contains(&creator) && recipient.phase == Phase::Draining {
        recipient.committed = true;
        recipient.reopened_by.clear();
        if recipient.failed_exit {
            // The reopen reached the host before the loop failed: it failed live, in its slot, and its backoff is
            // spent.
            recipient.failed_exit = false;
            recipient.phase = Phase::Failed;
            recipient.failures = recipient.failures.saturating_add(1);
            recipient.due = true;
        } else {
            recipient.phase = Phase::Live;
        }
    }
}

#[cfg(test)]
mod tests;
