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
}
