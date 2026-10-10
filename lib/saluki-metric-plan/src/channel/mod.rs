//! The artifact channel: how approved artifacts reach the installer, and how the installer reports back.
//!
//! The channel's contract follows a remote configuration model:
//!
//! - each update is a complete [snapshot](ArtifactSnapshot) of a [product](ArtifactProduct)'s configs, never a delta,
//!   and an empty snapshot revokes, so the default program runs;
//! - each [config](ArtifactConfig) has an ID, a version, and a transport hash checked on receipt, and its payload is an
//!   [envelope](Envelope) holding the artifact, its revision, and the hash approval bound to;
//! - delivery can repeat or arrive after a restart, so installation is idempotent by artifact hash;
//! - outcomes are reported per config as an [acknowledgment](ConfigAcknowledgment), with a [status
//!   summary](StatusSummary) beside them;
//! - a transport failure delivers nothing, so the current snapshot stays in effect: it never revokes.
//!
//! The host persists nothing: persistence and redelivery belong to the channel. A transport, such as the local
//! development stub, and the installer meet through [`artifact_channel`]: the transport publishes each snapshot it
//! receives with its [`ArtifactPublisher`] and sends back what its [`ApplyStateReceiver`] holds; the installer reads
//! snapshots from its [`ArtifactFeed`] and reports with its [`ApplyStateReporter`].

mod envelope;
pub use self::envelope::{ApprovedAgainst, Envelope, EnvelopeError};

mod feed;
pub use self::feed::{
    artifact_channel, ApplyStateReceiver, ApplyStateReporter, ArtifactFeed, ArtifactPublisher, InstallerEnd,
    TransportEnd,
};

mod snapshot;
pub use self::snapshot::{
    ArtifactConfig, ArtifactConfigId, ArtifactProduct, ArtifactSnapshot, TransportHashMismatch, UnknownProduct,
};

mod state;
pub use self::state::{ActiveArtifact, ApplyState, ApplyStates, ConfigAcknowledgment, StatusSummary};
