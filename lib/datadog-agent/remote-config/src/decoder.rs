//! The subscriber's decoding contract.

use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::{ApplyError, ConfigId};

/// Decodes one product's assigned configurations into a snapshot.
///
/// A decoder names the product it decodes, so subscribing with a decoder cannot attach it to the wrong product. Define
/// a decoder in the crate that owns its snapshot type; this crate knows no product's payload.
///
/// The client drives an implementation once per published snapshot: it constructs a decoder with [`Default`], calls
/// [`decode`](Self::decode) once per assigned configuration in ascending [`ConfigId`] order, and then calls
/// [`build`](Self::build). Ascending order is part of the contract, so a reduction that keeps the last valid
/// configuration is well defined.
///
/// A product is decoded again only when its assigned configurations or their contents change, so a rejected
/// assignment is not retried until it changes. When the Agent reports its configuration expired, every product is
/// decoded as an empty assignment.
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
pub trait ProductDecoder: Default + Send + 'static {
    /// The product this decoder decodes, by its protocol name, such as `APM_SEMANTIC_CORE_DD`.
    ///
    /// The client requests this product from the Agent and keys the product's subscription by this name. The client
    /// can only subscribe once per product: attempting a second subscription to the same product while the first is
    /// live returns [`Error::AlreadySubscribed`](crate::Error::AlreadySubscribed), whichever decoder it uses.
    const PRODUCT: &'static str;

    /// The type that holds the product's decoded configuration.
    ///
    /// [`build`](Self::build) produces it from the product's configurations, and subscribers receive it.
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

    /// Validates the accumulated configurations and produces the snapshot to publish.
    ///
    /// Validation that spans configurations belongs here, including checking that a required configuration is present.
    /// When the product has no configurations, such as after the backend removes the last one or when the Agent reports
    /// its configuration expired, the client calls this without having called [`decode`](Self::decode). This method
    /// therefore decides whether a product with no configurations is acceptable.
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

    /// `build` failed; the error is published as a rejection and `current` keeps the last accepted snapshot.
    Rejected(E),

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
            Ok(snapshot) => Outcome::Accepted(snapshot),
            Err(error) => {
                let reason = error.apply_error();
                for (_, verdict) in &mut verdicts {
                    if matches!(verdict, Verdict::Acknowledged) {
                        *verdict = Verdict::BuildRejected(reason.clone());
                    }
                }
                Outcome::Rejected(error)
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
