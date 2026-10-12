//! Delivery hosts: the Datadog forwarder, defunctionalized.
//!
//! A native forwarder captures its behavior when it is built: one I/O loop per resolved endpoint, and fixed routing
//! and retry policy. A delivery host interprets the same behavior as data instead, so that recipients can be prepared,
//! drained, retired, and reopened while it runs:
//!
//! - a **recipient table** maps each [`RecipientSlot`] to one recipient's I/O loop, built from its [`RecipientSpec`]
//!   with the pieces the host's [`RecipientFactory`] builds off the data path, and run by [`RecipientLoop`];
//! - a payload that names its recipients ([`PayloadRecipients`], stamped by an encode host's lease) goes exactly
//!   there, in slot order; any other payload goes to the published **native set**, filtered by payload class with the
//!   native rule;
//! - the host's settings ([`DeliverySettings`]) and each recipient's credential cell, fed through the host's
//!   [`CredentialSource`], follow the current configuration.
//!
//! A controller drives the host through its [control mailbox](saluki_core::execution::control) with
//! [`DeliveryControl`] messages: every recipient message, and every event the host reports, names a recipient by its
//! `(slot, identity, incarnation)` ([`RecipientRef`]), and a host ignores, and logs, a message whose reference does
//! not match its table. Every preparation names the generation or resource transaction it is for ([`Creator`]), and
//! an abort names only that, so a controller keeps no record of what it asked a host to prepare.
//!
//! # The routing loop
//!
//! The host's component loop is its routing loop: one task serves the control mailbox, routes payloads, and follows
//! the recipients' exits, so that control messages and payloads are ordered in one place. A payload goes to its
//! recipients one at a time, in order, and a send that waits for room in a recipient is one branch of the loop, so
//! control and health are served while data waits.
//!
//! A release is ordered after every payload the host received before it by the **input barrier**: the host counts
//! the payloads waiting in its input when the release arrives, and keeps routing to the released recipient until it
//! has received that many more and no send is pending. An encode host acknowledges a lease's drain once the lease's
//! last payloads are in this host's input, so a controller that releases a recipient after the leases naming it were
//! drained never loses their last payloads.
//!
//! Without a control mailbox, a forwarder runs the native table it builds from its configuration instead, as natively.

use std::{collections::BTreeSet, fmt, sync::Arc};

use agent_data_plane_config::Live;
use saluki_core::execution::GenerationId;
use saluki_error::GenericError;
use stringtheory::MetaString;
use tokio::{sync::oneshot, time::Instant};

mod client;
mod credentials;
mod host;
pub(crate) use self::host::HostKind;
pub use self::host::{
    recipient_queue_bound, BootPublication, DeliveryHostConfiguration, DeliveryHostStatus, RecipientConflict,
    RecipientState, RecipientStatus, DRAIN_DEADLINE, MAX_DRAINING_RECIPIENTS,
};
mod router;
pub(crate) use self::router::transaction_metadata;
mod spec;
pub use self::spec::{
    DeliverySettings, EncodeLeaseSpec, EncoderSettings, IntakeAuthorities, LeaseKeyDigest, PayloadClasses,
    PayloadRecipients, ProtocolVariants, ProxyEndpoint, ProxySettings, RecipientOrigin, RecipientPolicy,
    RecipientProtocol, RecipientRef, RecipientSlot, RecipientSpec, ResolvedRecipients, RetireClosure, RetrySettings,
    SeriesVariant, SpillPolicy, V3Acceptance,
};
pub use super::io::{
    CloseRequests, DetachedLoop, RecipientCloser, RecipientHandle, RecipientLoop, RecipientPieces,
    RecipientSettingsSender,
};
#[cfg(test)]
mod tests;

/// Which preparation a delivery host prepares recipients for: a generation's, or a resource transaction's.
///
/// The host records, for every recipient, the preparations that created or reopened it, so that an abort names only
/// the preparation ([`DeliveryControl::Abort`], [`DeliveryControl::AbortResources`]) and undoes what it alone did.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Creator {
    /// The preparation of a generation.
    Generation(GenerationId),

    /// A resource transaction, by the revision it publishes.
    Transaction(u64),
}

impl fmt::Display for Creator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Generation(generation) => write!(f, "generation {}", generation.get()),
            Self::Transaction(revision) => write!(f, "the resource transaction of revision {}", revision),
        }
    }
}

/// A control message for a delivery host.
pub enum DeliveryControl<B> {
    /// Adds prepared recipients to the table, each addressable only by its slot, and spawns their I/O loops, after
    /// checking that every recipient the preparation requires is live.
    ///
    /// Nothing routes to a recipient before a generation whose payloads name it commits, or a publication names it in
    /// the native set, except to the next incarnation of a failed recipient, prepared into its slot: payloads that name
    /// the slot, or the failed incarnation in the native set, reach it at once. A recipient the host cannot add (a
    /// slot taken by another recipient, a directory a loop of the table still owns, an incarnation created before, a
    /// declared recipient that duplicates a live one), or a required recipient that is not live, rejects the message,
    /// naming the recipient. A new recipient whose directory is still owned by a loop an abort dropped makes the
    /// message wait for that loop's exit: the host answers it once it proceeds, after the exit, and an abort of its
    /// preparation meanwhile rejects it. A recipient the table holds under the same reference is acknowledged as
    /// prepared, so a preparation repeated after an installer restart changes nothing; when another preparation
    /// prepares it again, the host records that preparation as one of its creators too.
    Prepare {
        /// The preparation the recipients are created for.
        creator: Creator,

        /// The recipients to add.
        recipients: Vec<PreparedRecipient<B>>,

        /// The recipients the preparation's leases name that it neither adds nor reopens: a generation never commits
        /// over a recipient whose resource transaction failed to prepare it.
        requires: Vec<RecipientRef>,
    },

    /// Swaps the native set, the host's settings, every recipient's settings, and the withdrawn set, unless the host
    /// runs a newer revision: the only path that changes a live recipient's settings.
    Publish {
        /// The revision published.
        revision: u64,

        /// The recipients payloads that do not name their own are routed over, filtered by payload class.
        native_set: Vec<RecipientRef>,

        /// The host's settings, which each recipient picks up at its next request boundary.
        settings: Arc<DeliverySettings>,

        /// The settings of every recipient of the host, native and declared.
        recipients: Vec<RecipientSettings>,

        /// The recipients the current lowering no longer declares, by identity: every send of a payload to one is
        /// dropped and counted, whatever lease stamped it, until a later publication no longer lists it.
        withdrawn: BTreeSet<MetaString>,
    },

    /// Releases recipients: each one is removed from routing once every payload received before the release was
    /// routed (the input barrier), then drains its queued work for at most the drain deadline, and retires.
    ///
    /// The message is acknowledged once the release is accepted; each recipient's exit is reported on the host's
    /// event channel ([`DeliveryEvent::RecipientExited`]). A failed recipient, whose exit was reported already, frees
    /// its slot at once: a recipient prepared into the slot next receives only payloads received after the release,
    /// and those received before it that name the slot are dropped and counted.
    Release {
        /// The recipients.
        recipients: Vec<RecipientRef>,
    },

    /// Reopens recipients declared again while their release waits at the input barrier or their loop drains: the
    /// removal is cancelled, or the close request cleared, and the new declaration's settings apply. The loop
    /// continues with the same queue and directory.
    ///
    /// The host records the preparation as holding each recipient reopened, so that its abort returns the recipient to
    /// draining once no other preparation holds it; a reopen repeated by the same preparation changes nothing.
    Reopen {
        /// The preparation that reopens the recipients.
        creator: Creator,

        /// The recipients, with their declarations.
        recipients: Vec<RecipientSettings>,

        /// Receives, for each recipient, whether it was reopened or its loop had already exited, in which case the
        /// controller waits for its exit event and prepares its next incarnation.
        reply: oneshot::Sender<Vec<(RecipientRef, ReopenReply)>>,
    },

    /// Undoes what an aborted generation's preparation did: drops the recipients it created that no other
    /// preparation created too, and returns those it reopened, once no other preparation holds them reopened, to
    /// draining, with their original deadlines.
    ///
    /// A dropped recipient's loop closes as at shutdown, and its exit is reported on the event channel as every
    /// incarnation's is, exactly once; a controller ignores the exits of the incarnations it aborted. Its queue
    /// directory stays owned until then, so a preparation of the same identity waits for that exit
    /// ([`Prepare`][Self::Prepare]); a preparation of the aborted generation that still waits so is rejected. Aborting
    /// a generation the host prepared nothing for does nothing.
    Abort {
        /// The generation.
        generation: GenerationId,
    },

    /// Undoes what a resource transaction that has not published prepared, as [`Abort`][Self::Abort] does for a
    /// generation: a newer lowering replaced it, or the host did not acknowledge it in time.
    ///
    /// A recipient the newer transaction prepared again is kept, since that transaction is one of its creators. A
    /// publication commits what its own revision's transaction created or reopened, and every recipient its native set
    /// names, so no abort undoes those; a superseded transaction that never published can still be aborted, even once a
    /// newer one published.
    AbortResources {
        /// The revision the transaction would have published.
        revision: u64,
    },
}

impl<B> fmt::Debug for DeliveryControl<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepare {
                creator,
                recipients,
                requires,
            } => f
                .debug_struct("Prepare")
                .field("creator", creator)
                .field("recipients", recipients)
                .field("requires", requires)
                .finish(),
            Self::Publish {
                revision,
                native_set,
                withdrawn,
                ..
            } => f
                .debug_struct("Publish")
                .field("revision", revision)
                .field("native_set", native_set)
                .field("withdrawn", withdrawn)
                .finish_non_exhaustive(),
            Self::Release { recipients } => f.debug_struct("Release").field("recipients", recipients).finish(),
            Self::Reopen {
                creator, recipients, ..
            } => f
                .debug_struct("Reopen")
                .field("creator", creator)
                .field("recipients", recipients)
                .finish_non_exhaustive(),
            Self::Abort { generation } => f.debug_struct("Abort").field("generation", generation).finish(),
            Self::AbortResources { revision } => f.debug_struct("AbortResources").field("revision", revision).finish(),
        }
    }
}

/// One recipient, with the pieces of its I/O loop that the host's [`RecipientFactory`] built off the data path.
pub struct PreparedRecipient<B> {
    /// The recipient's slot, identity, and incarnation.
    pub recipient: RecipientRef,

    /// What the recipient is.
    pub spec: RecipientSpec,

    /// The pieces of its I/O loop.
    pub pieces: RecipientPieces<B>,
}

impl<B> fmt::Debug for PreparedRecipient<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedRecipient")
            .field("recipient", &self.recipient)
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}

/// One recipient's declaration, as a publication or a reopen applies it.
#[derive(Clone, Debug, PartialEq)]
pub struct RecipientSettings {
    /// The recipient's slot, identity, and incarnation.
    pub recipient: RecipientRef,

    /// What the recipient is.
    pub spec: RecipientSpec,
}

/// What a reopen did for one recipient.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReopenReply {
    /// The recipient was reopened: its loop continues with the same queue and directory.
    Reopened,

    /// The recipient's loop had already exited: its exit is reported on the event channel, after which the controller
    /// prepares its next incarnation, whose queue adopts the directory.
    Exited,
}

/// What a delivery host reports on its event channel.
///
/// The control mailbox is request and reply, so what happens later travels on a channel of its own, bounded at the
/// table's size plus the most recipients that drain at once, so a send never waits.
#[derive(Clone, Debug, PartialEq)]
pub enum DeliveryEvent {
    /// A recipient incarnation's I/O loop exited: each incarnation's exit is reported exactly once.
    RecipientExited {
        /// The recipient.
        recipient: RecipientRef,

        /// How it exited.
        outcome: RecipientExit,
    },
}

/// How a recipient incarnation's I/O loop exited.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecipientExit {
    /// It retired after a release, or closed as at shutdown (a shutdown request, its input or its closer dropped, an
    /// error while draining), with what its drain and its close did.
    Retired(DrainReport),

    /// It ended on an error outside a drain, after its native closure: its slot stays allocated, and payloads naming
    /// it are dropped and counted, until its next incarnation is live.
    Failed(String),
}

/// What a recipient's I/O loop is asked to do about closing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CloseRequest {
    /// Keep running.
    #[default]
    None,

    /// Retire: stop taking new work, keep dispatching queued work until the queue and the requests in flight are
    /// empty, or, at the deadline, close as at shutdown.
    Retire {
        /// When the drain gives up on queued work.
        deadline: Instant,
    },

    /// Close as at process shutdown: stop popping queued work, finish requests in flight, and persist queued work
    /// where disk storage is configured, or drop and count it.
    Shutdown,
}

/// What a retired recipient's drain did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DrainReport {
    /// Requests delivered while draining.
    pub delivered: u64,

    /// Requests persisted at the close, of the work the loop held when its close became final.
    pub persisted: u64,

    /// Requests dropped, and counted, at the close, of the work the loop held when its close became final.
    pub dropped: u64,

    /// Whether the drain reached its deadline before its queue was empty.
    pub deadline_reached: bool,
}

/// Builds the pieces of recipients' I/O loops, off the data path: a credential cell bound to its live view, the retry
/// queue with its persisted directory listed but not read, the request mapper, and the telemetry handles.
///
/// A delivery host exposes its factory when it is built, so that a controller can build a recipient's pieces in its
/// own task before it sends them to the host.
pub trait RecipientFactory<B>: Send + Sync {
    /// Builds the pieces of one recipient's I/O loop, with the host's settings.
    ///
    /// # Errors
    ///
    /// Returns an error naming the recipient if a piece cannot be built: a client, a directory, or a credential that
    /// does not resolve.
    fn build(&self, spec: &RecipientSpec, settings: &DeliverySettings) -> Result<RecipientPieces<B>, GenericError>;
}

/// Resolves credential references to live views of their values.
///
/// The host application implements it over its configuration: each recipient's credential cell follows the view its
/// reference resolves to, with the native rules for storing a key (trimmed; a blank or invalid update keeps the last
/// usable key), so that rotating a credential is not a plan change.
pub trait CredentialSource: Send + Sync {
    /// Returns the live view of the credential `reference` names, or `None` if the reference is not one this source
    /// knows. A view whose value is `None` is a credential that is not configured.
    fn resolve(&self, reference: &MetaString) -> Option<Live<Option<MetaString>>>;
}
