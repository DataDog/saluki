//! A public utility module for users of this library who want to write tests of their [`ProductDecoder`]
//! implementations and of the components that consume their subscriptions. It is compiled with the `test-util` cargo
//! feature.

use crate::decoder::evaluate;
use crate::subscription::Publisher;
use crate::{ConfigId, ProductDecoder, Subscription};

/// Publishes into a [`Subscription`] by hand, so a subscriber can test its component without an Agent.
///
/// The subscription returned alongside the publisher is the production type: [`current`](Subscription::current) keeps
/// the last accepted snapshot through a rejection. After the publisher is dropped and pending publications are
/// observed, [`changed`](Subscription::changed) waits indefinitely. Slow consumers may skip intermediate publications.
///
/// A component under test should take a [`Subscription`] rather than a
/// [`RemoteConfigurationClient`](crate::RemoteConfigurationClient). Production wiring subscribes once per product and
/// hands out clones, and a component built that way can be tested with this publisher alone.
///
/// # Examples
///
/// ```
/// use datadog_agent_remote_config::{ConfigId, ProductDecoder, TestPublisher};
///
/// /// Counts the configurations with a non-empty payload.
/// #[derive(Default)]
/// struct CountDecoder(usize);
///
/// impl ProductDecoder for CountDecoder {
///     const PRODUCT: &'static str = "EXAMPLE_COUNT";
///
///     type Snapshot = usize;
///     type Error = String;
///
///     fn decode(&mut self, _id: &ConfigId, payload: &[u8]) -> Result<(), Self::Error> {
///         if payload.is_empty() {
///             return Err("Payload is empty.".to_owned());
///         }
///         self.0 += 1;
///         Ok(())
///     }
///
///     fn build(self) -> Result<Self::Snapshot, Self::Error> {
///         Ok(self.0)
///     }
/// }
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() {
/// let (publisher, mut subscription) = TestPublisher::<usize>::new();
///
/// // Publish a finished snapshot, or a rejection:
/// publisher.accept(1);
/// assert_eq!(*subscription.changed().await.unwrap(), 1);
/// publisher.reject("Invalid.".to_owned());
/// assert!(subscription.changed().await.is_err());
/// assert_eq!(*subscription.current().unwrap(), 1);
///
/// // Or publish whatever the product's decoder makes of these payloads:
/// publisher.assign::<CountDecoder>([("a", "x"), ("b", ""), ("c", "y")]);
/// assert_eq!(*subscription.changed().await.unwrap(), 2);
/// # }
/// ```
pub struct TestPublisher<T, E = String> {
    publisher: Publisher<T, E>,
}

impl<T, E> TestPublisher<T, E> {
    /// Creates a publisher and the subscription it publishes into.
    ///
    /// The subscription starts with no accepted snapshot, so [`current`](Subscription::current) returns `None` until
    /// the first successful publish.
    pub fn new() -> (Self, Subscription<T, E>) {
        let (publisher, subscription) = Publisher::new();
        (Self { publisher }, subscription)
    }

    /// Publishes an accepted snapshot.
    ///
    /// [`current`](Subscription::current) returns this snapshot. Subscribers waiting for a change are notified.
    pub fn accept(&self, snapshot: T) {
        self.publisher.accept(snapshot);
    }

    /// Publishes a rejection.
    ///
    /// Subscribers waiting for a change receive the error from [`changed`](Subscription::changed), and
    /// [`current`](Subscription::current) keeps returning the last accepted snapshot.
    pub fn reject(&self, error: E) {
        self.publisher.reject(error);
    }

    /// Runs `P` over an assignment of configurations and publishes the outcome, exactly as the client's worker would.
    ///
    /// Each item is a configuration ID, such as `semantic.v1`, and its payload. The decoding rules are the production
    /// code, not a copy: a fresh decoder receives the configurations in ascending ID order, a configuration that
    /// [`decode`](ProductDecoder::decode) rejects is skipped while the rest are still decoded, and then
    /// [`build`](ProductDecoder::build) decides the outcome. A successful build publishes as [`accept`](Self::accept)
    /// does, and a failed build publishes as [`reject`](Self::reject) does. A panic in the decoder is caught and
    /// publishes nothing, so subscribers are not notified.
    ///
    /// An empty assignment is valid and is what a product with no configurations assigned receives, including when the
    /// Agent reports its configuration expired.
    ///
    /// Unlike the worker, this decodes on every call, even when the assignment is unchanged. Production publishes
    /// identical snapshots after a worker restart, so subscribers must tolerate them regardless.
    ///
    /// The apply statuses the client would report to the Agent are not returned; they are the client's concern.
    ///
    /// # Panics
    ///
    /// Panics if two items share a configuration ID. Configuration IDs are unique within a product's assignment, so a
    /// duplicate indicates a mistake in the test.
    pub fn assign<P>(&self, assignment: impl IntoIterator<Item = (impl AsRef<str>, impl AsRef<[u8]>)>)
    where
        P: ProductDecoder<Snapshot = T, Error = E>,
    {
        let mut assignment: Vec<_> = assignment
            .into_iter()
            .map(|(id, payload)| (ConfigId::new(id.as_ref()), payload.as_ref().to_vec()))
            .collect();
        assignment.sort_by(|(left, _), (right, _)| left.cmp(right));
        assert!(
            assignment.windows(2).all(|pair| pair[0].0 != pair[1].0),
            "duplicate configuration ID"
        );

        let assignment = assignment
            .iter()
            .map(|(id, payload)| (id.clone(), payload.as_slice()))
            .collect();
        self.publisher.publish(evaluate::<P>(assignment).outcome);
    }
}
