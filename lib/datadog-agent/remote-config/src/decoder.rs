//! The subscriber's decoding contract.

use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::{ApplyError, ConfigId};

/// Decodes one product's assigned configurations into a snapshot.
///
/// A decoder names the product it decodes, so subscribing with a decoder cannot attach it to the wrong product. Define
/// a decoder in the crate that owns its snapshot type. A decoder for a product whose type has no other owner belongs in
/// this crate instead; otherwise this crate knows no product's payload.
///
/// To evaluate an assignment, the client constructs a decoder with [`Default`], calls [`decode`](Self::decode) once
/// per assigned configuration in ascending [`ConfigId`] order, and then calls [`build`](Self::build). On success,
/// [`keeps_previous`](Self::keeps_previous) decides whether to replace or retain the current snapshot. Ascending order
/// is part of the contract, so a reduction that keeps the last valid configuration is well defined.
///
/// A product is decoded again only when its assigned configurations or their contents change, so a rejected
/// assignment is not retried until it changes. When the Agent reports its configuration expired, every product that
/// had configurations is decoded as an empty assignment; a product that already had none is not decoded again. The
/// client then asks the Agent for the full assignment until a response is not expired, so configurations that return
/// are decoded as new.
///
/// A rejected snapshot leaves subscribers with the last accepted one, so an implementation must build a snapshot
/// from an empty assignment; see [`build`](Self::build).
///
/// Decoding runs on the client's worker task and delays polling for every product while it runs, so implementations
/// **MUST NOT** block. A panic in [`decode`](Self::decode) or [`build`](Self::build) is caught: the client discards the
/// decoder, rejects every configuration in the assignment, and publishes nothing, so subscribers keep the last accepted
/// snapshot and are not notified.
///
/// # Design
///
/// A product's assignment is several configurations, each with its own payload, and the protocol carries an apply
/// status for each one separately. Decoding therefore accumulates configuration by configuration rather than consuming
/// the assignment as a whole: a configuration that [`decode`](Self::decode) rejects is attributed to itself and
/// skipped, and the decoder still builds from the rest. Because the client drives the loop, that attribution cannot be
/// forgotten or misdirected. Whether a rejected configuration invalidates the whole snapshot remains the decoder's
/// choice: [`build`](Self::build) may reject the snapshot, for example when a configuration it requires was rejected.
///
/// Accumulating into the decoder, rather than returning one decoded value per configuration, is what lets a product
/// whose configurations have different shapes hold each of them in a field of its own type instead of funneling them
/// through a shared enum.
///
/// # Examples
///
/// A product that accepts at most one configuration records every configuration it is given and rejects more than one
/// in [`build`](Self::build), since only `build` sees the whole assignment:
///
/// ```
/// use datadog_agent_remote_config::{ConfigId, ProductDecoder};
///
/// /// Holds the product's single configuration, if it has one.
/// #[derive(Default)]
/// struct SingleDecoder {
///     payloads: Vec<(ConfigId, Vec<u8>)>,
/// }
///
/// impl ProductDecoder for SingleDecoder {
///     const PRODUCT: &'static str = "EXAMPLE_SINGLE";
///
///     type Snapshot = Option<Vec<u8>>;
///     type Error = String;
///
///     fn decode(&mut self, id: &ConfigId, payload: &[u8]) -> Result<(), Self::Error> {
///         self.payloads.push((id.clone(), payload.to_vec()));
///         Ok(())
///     }
///
///     fn build(mut self) -> Result<Self::Snapshot, Self::Error> {
///         if self.payloads.len() > 1 {
///             return Err(format!("Expected at most one configuration; got {}.", self.payloads.len()));
///         }
///         Ok(self.payloads.pop().map(|(_, payload)| payload))
///     }
/// }
///
/// let mut decoder = SingleDecoder::default();
/// decoder.decode(&ConfigId::new("a"), b"one").unwrap();
/// decoder.decode(&ConfigId::new("b"), b"two").unwrap();
/// assert!(decoder.build().is_err());
/// ```
pub trait ProductDecoder: Default + Send + 'static {
    /// The product this decoder decodes, by its protocol name, such as `APM_SEMANTIC_CORE_DD`.
    ///
    /// The client requests this product from the Agent and keys the product's subscription by this name. The client
    /// can only subscribe once per product: attempting a second subscription to the same product while the first is
    /// live returns [`Error::AlreadySubscribed`](crate::Error::AlreadySubscribed), whichever decoder it uses.
    const PRODUCT: &'static str;

    /// The type that holds the product's decoded configuration.
    ///
    /// [`build`](Self::build) produces it from the product's configurations. [`keeps_previous`](Self::keeps_previous)
    /// decides whether subscribers receive this snapshot or retain the previous one.
    type Snapshot: Send + Sync + 'static;

    /// The error this product's decoding and validation produces.
    ///
    /// Use [`String`] when there is nothing richer to report; a product that wants to describe a failure more
    /// precisely for its own diagnostics defines its own type and implements [`ApplyError`] for it.
    type Error: ApplyError + Send + Sync + 'static;

    /// Accumulates one of the product's assigned configurations.
    ///
    /// The client calls this at most once per distinct `id` within a single snapshot, then calls
    /// [`build`](Self::build) once. A decoder may therefore hold one slot per configuration ID without a second
    /// configuration silently displacing the first.
    ///
    /// # Errors
    ///
    /// Returns [`Self::Error`] to reject this configuration alone. The client attributes the rejection to it and skips
    /// it, and still decodes the product's remaining configurations.
    fn decode(&mut self, id: &ConfigId, payload: &[u8]) -> Result<(), Self::Error>;

    /// Validates the accumulated configurations and produces a snapshot.
    ///
    /// Check requirements across configurations here, including whether a required configuration is present when the
    /// decoder receives a non-empty assignment. The client also calls this without calling [`decode`](Self::decode)
    /// when no configurations are assigned (after the backend removes the last one or the Agent reports its
    /// configuration expired), or when every assigned configuration was rejected for sharing an ID.
    ///
    /// An empty assignment to the decoder means "no configuration," even if files were assigned but all collided.
    /// This method **MUST** build a snapshot representing that state. If the product should retain its previous
    /// configuration on withdrawal, use [`keeps_previous`](Self::keeps_previous) rather than rejecting the assignment.
    ///
    /// The client acknowledges successfully decoded configurations only when this method succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`Self::Error`] to reject the snapshot. The client rejects every successfully decoded configuration
    /// with this error's apply reason; configurations rejected by [`decode`](Self::decode) keep their own errors.
    /// No new snapshot is published, and subscribers retain the last accepted snapshot.
    ///
    /// This method cannot reject selected configurations while publishing the rest. Selective rejection belongs in
    /// [`decode`](Self::decode).
    fn build(self) -> Result<Self::Snapshot, Self::Error>;

    /// Controls whether an unassigned-marker update preserves the last accepted configuration.
    ///
    /// Remote Configuration assigns configuration files to each product. When the backend withdraws the last file, or
    /// the Agent reports that its cached configuration has expired, the product's assignment becomes empty: there is
    /// no longer a remote configuration assigned to it.
    ///
    /// The decoder must still represent this valid state. Its [`build`](Self::build) can return a marker such as an
    /// `Unassigned` variant in its snapshot type, meaning "no remote configuration is assigned." This is a value the
    /// decoder creates, not an error or a special payload in a file. Another decoder might instead build a snapshot
    /// containing its local defaults. Whether either result should replace the previous configuration depends on the
    /// product's rules for withdrawal.
    ///
    /// Use this hook to retain the last accepted configuration when the product becomes unassigned. The client calls
    /// it for every successfully built snapshot; your decoder identifies its product's unassigned marker. Return
    /// `true` **only for that marker** when withdrawal should preserve the previous configuration, and `false` for
    /// ordinary configurations.
    ///
    /// - `false` (the default): publish this snapshot, replacing the previous one.
    /// - `true`: accept this unassigned-marker update without replacing the previous snapshot. Later configuration
    ///   updates still return `false` and replace it normally. If no snapshot exists yet, publish the marker.
    ///
    /// Trace sampling (`APM_SAMPLING`) keeps its last sampling settings on withdrawal. Its decoder returns `true` for
    /// `Unassigned` and `false` for every actual configuration. Even an assigned `{}` or JSON `null` returns `false`:
    /// it is a configuration with no overrides, which restores local settings, not a withdrawal.
    ///
    /// The semantic registry (`APM_SEMANTIC_CORE_DD`) must instead revert to embedded mappings on withdrawal. Its
    /// decoder builds an embedded-registry snapshot and uses the default `false`, so that snapshot replaces the remote
    /// registry. Consumers, including APM stats, then use the embedded mappings.
    ///
    /// Retention happens here because subscriptions hold only the latest snapshot, not a queue. If configuration A is
    /// followed by an `Unassigned` marker before a sampler reads either, publishing the marker would erase A. Retaining
    /// A lets the sampler apply it even after withdrawal. New concrete configurations can still replace one another.
    ///
    /// Both return values accept the update, clear any earlier rejection, notify subscribers, and acknowledge
    /// configuration verdicts normally. Retaining a snapshot does not mean its configuration remains remotely assigned.
    /// The call runs inside the same panic guard as `build`.
    fn keeps_previous(snapshot: &Self::Snapshot) -> bool {
        let _ = snapshot;
        false
    }
}

/// The fixed reason reported for every configuration when a decoder panics.
///
/// A panic is a bug in the decoder rather than a verdict about any one configuration's bytes, so it overwrites any
/// rejection a configuration had already earned, including from an earlier `decode` call.
pub(crate) const PANICKED: &str = "Product decoder panicked.";

/// What one run of a decoder over a product's assignment produced.
pub(crate) struct Evaluation<T, E> {
    pub(crate) outcome: Outcome<T, E>,
    /// Each assigned configuration's verdict, in ascending ID order.
    pub(crate) verdicts: Vec<(ConfigId, Verdict)>,
}

/// The verdict one configuration earned from a run of a decoder.
///
/// The reason is the string the client reports to the Agent; the variant is which rule produced it, which is what
/// the client counts.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// `decode` accepted this configuration and the snapshot it was part of was built.
    Acknowledged,

    /// `decode` rejected this configuration, with the reason it gave.
    DecodeRejected(String),

    /// `decode` accepted this configuration, but `build` rejected the snapshot, with the reason it gave.
    BuildRejected(String),

    /// The decoder panicked, so this configuration is rejected without a reason of its own.
    Panicked,

    /// This configuration shares its ID with another, so decode did not run.
    ///
    /// Set by the client rather than a decoder; see [`Repository`](crate::repository::Repository).
    Collided(String),
}

pub(crate) enum Outcome<T, E> {
    /// `build` succeeded; the snapshot is published.
    Accepted(T),

    /// `build` succeeded and [`ProductDecoder::keeps_previous`] returned `true`; the snapshot is published only if no
    /// snapshot has been accepted yet.
    KeepsPrevious(T),

    /// `build` failed; the error is published as a rejection and `current` keeps the last accepted snapshot.
    Rejected {
        error: E,

        /// The error's apply reason, computed inside the decoder's `catch_unwind` so that a panic in
        /// [`ApplyError::apply_error`] is caught like any other decoder panic.
        reason: String,
    },

    /// `decode` or `build` panicked; nothing is published and subscribers are not notified.
    Panicked,
}

/// Runs a fresh decoder over one product's assignment exactly as the client does.
///
/// This is the only implementation of the decoding rules, shared by the worker and by
/// [`TestPublisher::assign`](crate::TestPublisher::assign), so that what a subscriber tests is what production runs:
/// configurations in ascending [`ConfigId`] order, a rejected configuration skipped while the rest are still decoded,
/// then `build`, with a panic in either caught.
pub(crate) fn evaluate<P: ProductDecoder>(mut assignment: Vec<(ConfigId, &[u8])>) -> Evaluation<P::Snapshot, P::Error> {
    assignment.sort_by(|(left, _), (right, _)| left.cmp(right));

    let mut verdicts = Vec::with_capacity(assignment.len());
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut decoder = P::default();
        for (id, payload) in &assignment {
            let verdict = match decoder.decode(id, payload) {
                Ok(()) => Verdict::Acknowledged,
                Err(error) => Verdict::DecodeRejected(error.apply_error()),
            };
            verdicts.push((id.clone(), verdict));
        }
        match decoder.build() {
            Ok(snapshot) if P::keeps_previous(&snapshot) => Outcome::KeepsPrevious(snapshot),
            Ok(snapshot) => Outcome::Accepted(snapshot),
            Err(error) => {
                let reason = error.apply_error();
                for (_, verdict) in &mut verdicts {
                    if matches!(verdict, Verdict::Acknowledged) {
                        *verdict = Verdict::BuildRejected(reason.clone());
                    }
                }
                Outcome::Rejected { error, reason }
            }
        }
    }));

    let outcome = match result {
        Ok(outcome) => outcome,
        Err(_) => {
            verdicts = assignment.into_iter().map(|(id, _)| (id, Verdict::Panicked)).collect();
            Outcome::Panicked
        }
    };

    Evaluation { outcome, verdicts }
}
