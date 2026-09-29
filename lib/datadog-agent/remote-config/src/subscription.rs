//! Typed subscriptions to a product's configuration.

use std::sync::Arc;

use tokio::sync::watch;

use crate::decoder::Outcome;

/// The published state of one product.
///
/// A rejection never evicts `accepted`, so retaining the last known-good configuration is the client's behavior rather
/// than each consumer's bookkeeping.
pub(crate) struct Snapshot<T, E> {
    /// The most recently accepted configuration, absent until the first snapshot is accepted.
    pub(crate) accepted: Option<Arc<T>>,

    /// The rejection of the most recent snapshot, absent while the most recent snapshot was accepted.
    pub(crate) rejection: Option<Arc<E>>,
}

/// A handle that observes one product's configuration as the client publishes it.
///
/// `T` is the snapshot the product's [`ProductDecoder`](crate::ProductDecoder) builds and `E` is the error it produces,
/// so a subscription reads as the value it delivers rather than as the decoder that produced it. A product with nothing
/// richer to report leaves `E` at its default of [`String`].
///
/// A consumer reads [`current`](Self::current) and then loops on [`changed`](Self::changed). A subscription created
/// after the client has already published treats that value as observed and receives no notification for it, so a
/// consumer that only awaited `changed` would wait for a snapshot that may never arrive.
///
/// Cloning shares one subscription between several consumers: each clone tracks its own position, while decoding
/// happens once per snapshot. Slow consumers may skip intermediate publications and observe only the latest state.
/// Dropping the last clone unsubscribes the product.
pub struct Subscription<T, E = String> {
    pub(crate) receiver: watch::Receiver<Snapshot<T, E>>,
}

impl<T, E> Subscription<T, E> {
    /// Returns the most recently accepted configuration.
    ///
    /// Returns `None` until the first snapshot is accepted. `None` does not mean the product has no configuration
    /// assigned: an empty assignment is a snapshot that a default decoder's [`build`](crate::ProductDecoder::build) may
    /// accept.
    pub fn current(&self) -> Option<Arc<T>> {
        self.receiver.borrow().accepted.clone()
    }

    /// Waits for a newly published snapshot.
    ///
    /// Slow consumers may skip intermediate publications and observe only the latest state. Once the worker stops,
    /// this waits indefinitely after any pending publication has been observed, so a caller may `select!` on it.
    ///
    /// A published snapshot is not guaranteed to differ from the previous one: after the worker restarts, it decodes
    /// every product again.
    ///
    /// # Errors
    ///
    /// Returns the subscriber's own decoding error when the snapshot was rejected. The client reports that rejection to
    /// the Agent independently; a consumer with nothing to report can ignore it. [`current`](Self::current) continues
    /// to return the last accepted configuration.
    pub async fn changed(&mut self) -> Result<Arc<T>, Arc<E>> {
        if self.receiver.changed().await.is_err() {
            return std::future::pending().await;
        }

        let snapshot = self.receiver.borrow_and_update();
        if let Some(error) = &snapshot.rejection {
            Err(Arc::clone(error))
        } else {
            Ok(Arc::clone(
                snapshot.accepted.as_ref().expect("published snapshot has a value"),
            ))
        }
    }
}

impl<T, E> Clone for Subscription<T, E> {
    fn clone(&self) -> Self {
        Self {
            receiver: self.receiver.clone(),
        }
    }
}

/// The publishing side of one product's [`Subscription`] and its clones.
///
/// The client's registry and [`TestPublisher`](crate::TestPublisher) both publish through this, so a subscription
/// behaves the same in a subscriber's test as in production.
pub(crate) struct Publisher<T, E> {
    sender: watch::Sender<Snapshot<T, E>>,
}

impl<T, E> Publisher<T, E> {
    /// Creates a publisher and a subscription with no accepted snapshot.
    pub(crate) fn new() -> (Self, Subscription<T, E>) {
        let (sender, receiver) = watch::channel(Snapshot {
            accepted: None,
            rejection: None,
        });
        (Self { sender }, Subscription { receiver })
    }

    pub(crate) fn accept(&self, snapshot: T) {
        self.sender.send_modify(|state| {
            state.accepted = Some(Arc::new(snapshot));
            state.rejection = None;
        });
    }

    pub(crate) fn reject(&self, error: E) {
        self.sender.send_modify(|state| {
            state.rejection = Some(Arc::new(error));
        });
    }

    /// Publishes the outcome of a decoder run. A panicked run publishes nothing, so subscribers are not notified.
    pub(crate) fn publish(&self, outcome: Outcome<T, E>) {
        match outcome {
            Outcome::Accepted(snapshot) => self.accept(snapshot),
            Outcome::Rejected(error) => self.reject(error),
            Outcome::Panicked => {}
        }
    }

    /// Returns whether any clone of the subscription is still alive.
    ///
    /// Once this is false it stays false: a subscription can only be cloned from a live one.
    pub(crate) fn is_subscribed(&self) -> bool {
        self.sender.receiver_count() > 0
    }
}
