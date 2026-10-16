//! Fixtures for consumers of trace sampling settings.
//!
//! Available in this crate's tests and when the `test-util` feature is enabled.

use datadog_agent_remote_config::TestPublisher;

use super::remote_config::{SamplingDecoder, SamplingError};
use super::{RemoteSampling, TraceSamplingSubscription};

/// Publishes `APM_SAMPLING` updates into a [`TraceSamplingSubscription`] without a Remote Configuration connection.
#[derive(Debug)]
pub struct TestSamplingPublisher {
    publisher: TestPublisher<RemoteSampling, SamplingError>,
}

impl TestSamplingPublisher {
    /// Creates a publisher and a subscription that has no update until one is published.
    pub fn new() -> (Self, TraceSamplingSubscription) {
        let (publisher, subscription) = TestPublisher::new();
        (
            Self { publisher },
            TraceSamplingSubscription::from_subscription(subscription),
        )
    }

    /// Decodes and publishes an assignment using the `APM_SAMPLING` decoder.
    ///
    /// Each element is a configuration ID and its JSON payload bytes; see [`SamplerConfig`](super::SamplerConfig) for the payload format.
    ///
    /// Empty assignments and rejected updates retain the last accepted configuration. Before any configuration has
    /// been accepted, an empty assignment publishes [`RemoteSampling::Unassigned`].
    ///
    /// # Panics
    ///
    /// Panics if configuration IDs are not unique within the assignment.
    pub fn assign(&self, assignment: impl IntoIterator<Item = (impl AsRef<str>, impl AsRef<[u8]>)>) {
        self.publisher.assign::<SamplingDecoder>(assignment);
    }
}
