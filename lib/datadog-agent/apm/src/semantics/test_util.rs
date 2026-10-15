//! Fixtures for consumers of semantic registries.

use datadog_agent_remote_config::TestPublisher;

use super::provider::{SemanticCore, SemanticCoreError};
pub use super::registry::registry_json;
use super::{Registry, SemanticRegistryProvider};

/// Publishes semantic registries into a provider without a Remote Configuration connection.
#[derive(Debug)]
pub struct TestRegistryPublisher {
    publisher: TestPublisher<SemanticCore, SemanticCoreError>,
}

impl TestRegistryPublisher {
    /// Creates a publisher and a provider that initially uses the embedded registry.
    pub fn new() -> (Self, SemanticRegistryProvider) {
        let (publisher, subscription) = TestPublisher::new();
        (
            Self { publisher },
            SemanticRegistryProvider::from_subscription(subscription),
        )
    }

    /// Publishes a registry selected from the given configuration ID.
    pub fn accept_registry(&self, registry: Registry, id: &str) {
        self.publisher.accept(SemanticCore::remote(registry, id));
    }

    /// Decodes and publishes an assignment using the semantic-core decoder.
    ///
    /// Empty assignments restore the embedded registry; rejected assignments retain the last accepted registry.
    ///
    /// # Panics
    ///
    /// Panics if configuration IDs are not unique within the assignment.
    pub fn assign(&self, assignment: impl IntoIterator<Item = (impl AsRef<str>, impl AsRef<[u8]>)>) {
        self.publisher
            .assign::<super::remote_config::SemanticCoreDecoder>(assignment);
    }
}
