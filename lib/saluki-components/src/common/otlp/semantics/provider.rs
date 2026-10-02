//! Access to embedded or remotely updated semantic mappings through one provider.
//!
//! A semantic registry maps concepts such as HTTP status code to attribute names and types. Trace translation and
//! APM stats use [`SemanticRegistryProvider`] without choosing between embedded and remotely supplied mappings.
//!
//! With Remote Configuration disabled, the default provider always uses the embedded registry. A subscribed provider
//! also starts with the embedded registry, then returns the latest accepted `APM_SEMANTIC_CORE_DD` update. Reading one
//! snapshot per batch keeps its mappings consistent even if an update arrives during processing.

use std::fmt;
use std::sync::Arc;

use datadog_agent_remote_config::{ApplyError, ConfigId, Subscription};

use super::registry::{Registry, EMBEDDED_REGISTRY};

/// Identifies whether a [`SemanticCore`] registry is embedded or supplied by a remote configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// The registry embedded in the binary, used when no configuration is assigned.
    Embedded,

    /// The ID of the selected remote configuration.
    Remote(ConfigId),
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Match upstream registry source labels (`pkg/trace/semantics/registry.go:22-25`).
        match self {
            Self::Embedded => f.write_str("embedded"),
            Self::Remote(id) => write!(f, "remote-config ({id})"),
        }
    }
}

/// The registry selected from a set of remote semantic-mapping configurations.
///
/// `APM_SEMANTIC_CORE_DD` is the Remote Configuration product that distributes semantic registries. An assignment is
/// the set of configurations currently supplied for that product. This snapshot holds the selected registry and its
/// origin, or the embedded registry when the assignment is empty.
pub struct SemanticCore {
    /// The complete registry to use, not a set of changes to the previous registry.
    pub(crate) registry: Arc<Registry>,

    /// Where the registry came from.
    pub(crate) origin: Origin,

    /// How many configurations were assigned, including ones that failed to decode.
    pub(crate) assigned: usize,
}

#[cfg(test)]
impl SemanticCore {
    /// Creates a snapshot holding `registry`, as if selected from configuration `id`.
    pub(crate) fn remote(registry: Registry, id: &str) -> Self {
        Self {
            registry: Arc::new(registry),
            origin: Origin::Remote(ConfigId::new(id)),
            assigned: 1,
        }
    }
}

impl fmt::Debug for SemanticCore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SemanticCore")
            .field("content_hash", &self.registry.content_hash())
            .field("version", &self.registry.version())
            .field("origin", &self.origin)
            .field("assigned", &self.assigned)
            .finish()
    }
}

/// Explains why a remote semantic registry could not be parsed or selected.
///
/// The Remote Configuration client reports this error to the server. Error text must not include payload contents.
/// A rejected update leaves the provider's last accepted registry unchanged.
#[derive(Debug)]
pub enum SemanticCoreError {
    /// A configuration's payload is not a valid registry.
    InvalidRegistry {
        /// Rejection reason safe to send to the server, without quoting payload contents.
        reason: String,
    },

    /// Configurations were assigned but none of them is a valid registry.
    NoValidConfiguration {
        /// How many configurations were assigned.
        assigned: usize,
    },
}

impl fmt::Display for SemanticCoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRegistry { reason } => f.write_str(reason),
            Self::NoValidConfiguration { assigned } => write!(
                f,
                "None of the {assigned} assigned APM_SEMANTIC_CORE_DD configurations is a valid registry."
            ),
        }
    }
}

impl std::error::Error for SemanticCoreError {}

impl ApplyError for SemanticCoreError {
    fn apply_error(&self) -> String {
        self.to_string()
    }
}

/// Supplies semantic mappings from either the embedded registry or the latest accepted remote update.
///
/// Consumers find attribute names for concepts such as HTTP status code without choosing a registry source.
/// The default provider always returns the embedded registry: it never subscribes or waits for updates. Use it when
/// Remote Configuration is disabled by configuration or deployment policy.
///
/// With a live subscription to `APM_SEMANTIC_CORE_DD`, the provider uses the embedded registry until an update is
/// accepted, then returns the latest accepted registry. Rejected updates leave that registry unchanged.
///
/// Clones share the subscription. Each component calls [`Self::snapshot`] at its batch boundary to keep one set of
/// mappings throughout processing.
#[derive(Clone, Debug)]
pub struct SemanticRegistryProvider {
    subscription: Subscription<SemanticCore, SemanticCoreError>,
}

impl SemanticRegistryProvider {
    /// Wraps a registry subscription. An inert subscription keeps using the embedded registry.
    pub(crate) fn from_subscription(subscription: Subscription<SemanticCore, SemanticCoreError>) -> Self {
        Self { subscription }
    }

    /// Returns the latest accepted registry, or the embedded registry if the subscription has no accepted value.
    ///
    /// Returns immediately, without waiting for an update. The registry is immutable and remains valid after later
    /// updates. Call once per batch and use the result throughout; calling again can return a newer registry.
    pub fn snapshot(&self) -> Arc<Registry> {
        match self.subscription.current() {
            Some(core) => Arc::clone(&core.registry),
            None => Arc::clone(&EMBEDDED_REGISTRY),
        }
    }
}

impl Default for SemanticRegistryProvider {
    fn default() -> Self {
        Self::from_subscription(Subscription::inert())
    }
}

#[cfg(test)]
mod tests {
    use datadog_agent_remote_config::TestPublisher;

    use super::*;
    use crate::common::otlp::semantics::registry::registry_json;

    fn remote(concepts: &str, id: &str) -> SemanticCore {
        let registry = Registry::from_json(&registry_json(concepts)).expect("registry should parse");
        SemanticCore::remote(registry, id)
    }

    const DB_STATEMENT: &str =
        r#"{"db.statement":{"fallbacks":[{"name":"db.statement","provider":"datadog","type":"string"}]}}"#;

    #[test]
    fn default_provider_returns_the_embedded_registry() {
        let provider = SemanticRegistryProvider::default();
        assert!(Arc::ptr_eq(&provider.snapshot(), &EMBEDDED_REGISTRY));
    }

    #[test]
    fn provider_returns_the_embedded_registry_before_a_snapshot_is_accepted() {
        let (_publisher, subscription) = TestPublisher::new();
        let provider = SemanticRegistryProvider::from_subscription(subscription);
        assert!(Arc::ptr_eq(&provider.snapshot(), &EMBEDDED_REGISTRY));
    }

    #[test]
    fn clones_observe_an_accepted_snapshot() {
        let (publisher, subscription) = TestPublisher::new();
        let provider = SemanticRegistryProvider::from_subscription(subscription);
        let clone = provider.clone();

        let core = remote(DB_STATEMENT, "a");
        let registry = Arc::clone(&core.registry);
        publisher.accept(core);

        assert!(Arc::ptr_eq(&provider.snapshot(), &registry));
        assert!(Arc::ptr_eq(&clone.snapshot(), &registry));
    }

    #[test]
    fn a_snapshot_is_unchanged_by_a_later_publication() {
        let (publisher, subscription) = TestPublisher::new();
        let provider = SemanticRegistryProvider::from_subscription(subscription);

        let pinned = provider.snapshot();
        publisher.accept(remote(DB_STATEMENT, "a"));

        assert!(Arc::ptr_eq(&pinned, &EMBEDDED_REGISTRY));
        assert!(!Arc::ptr_eq(&provider.snapshot(), &EMBEDDED_REGISTRY));
    }

    #[test]
    fn a_rejection_keeps_the_accepted_registry() {
        let (publisher, subscription) = TestPublisher::new();
        let provider = SemanticRegistryProvider::from_subscription(subscription);

        let core = remote(DB_STATEMENT, "a");
        let registry = Arc::clone(&core.registry);
        publisher.accept(core);
        publisher.reject(SemanticCoreError::NoValidConfiguration { assigned: 1 });

        assert!(Arc::ptr_eq(&provider.snapshot(), &registry));
    }

    #[test]
    fn independent_providers_do_not_share_publications() {
        let (publisher, subscription) = TestPublisher::new();
        let (_other_publisher, other_subscription) = TestPublisher::new();
        let provider = SemanticRegistryProvider::from_subscription(subscription);
        let other = SemanticRegistryProvider::from_subscription(other_subscription);

        publisher.accept(remote(DB_STATEMENT, "a"));

        assert!(!Arc::ptr_eq(&provider.snapshot(), &EMBEDDED_REGISTRY));
        assert!(Arc::ptr_eq(&other.snapshot(), &EMBEDDED_REGISTRY));
    }
}
