//! Datadog Agent configuration registry entries.

/// Aggregate transform configuration annotations.
pub mod aggregate;
/// DogStatsD mapper transform configuration annotations.
pub mod dogstatsd_mapper;
/// DogStatsD prefix filter transform configuration annotations.
pub mod dogstatsd_prefix_filter;
/// Shared encoder configuration annotations.
pub mod encoders;
/// OTLP source, decoder, and relay configuration annotations.
pub mod otlp;
/// Proxy configuration annotations.
pub mod proxy;

use std::sync::LazyLock;

use super::{ConfigKey, SalukiAnnotation};

/// All saluki annotations across every sub-system, in registration order.
///
/// The source of truth for which config keys saluki knows about and how they are consumed.
/// Used by the smoke test runner and runtime unknown-key detection.
pub static ALL_ANNOTATIONS: LazyLock<Vec<&'static SalukiAnnotation>> = LazyLock::new(|| {
    let mut v = Vec::new();
    v.extend_from_slice(aggregate::ALL);
    v.extend_from_slice(dogstatsd_mapper::ALL);
    v.extend_from_slice(dogstatsd_prefix_filter::ALL);
    v.extend_from_slice(encoders::ALL);
    v.extend_from_slice(otlp::ALL);
    v.extend_from_slice(proxy::ALL);
    v
});

/// All resolved [`ConfigKey`] entries, derived from [`ALL_ANNOTATIONS`] at first access.
///
/// Provides a flattened, owned view suitable for runtime unknown-key detection.
pub static ALL_KEYS: LazyLock<Vec<ConfigKey>> =
    LazyLock::new(|| ALL_ANNOTATIONS.iter().map(|a| ConfigKey::from(*a)).collect());

#[cfg(test)]
mod registry_tests {
    use super::*;
    use crate::config_registry::SupportLevel;

    #[test]
    fn annotation_invariants() {
        for annotation in ALL_ANNOTATIONS.iter() {
            let path = annotation.yaml_path();
            match annotation.support_level {
                SupportLevel::Full | SupportLevel::Partial => {
                    assert!(
                        !annotation.used_by.is_empty(),
                        "annotation '{}' has support level {:?} but used_by is empty — \
                         add the consuming struct name(s) to used_by, or change the support level",
                        path,
                        annotation.support_level,
                    );
                }
                SupportLevel::Incompatible => {
                    assert!(
                        annotation.used_by.is_empty(),
                        "annotation '{}' has support level Incompatible but used_by is not empty — \
                         remove the struct name(s) from used_by, or change the support level",
                        path,
                    );
                }
                SupportLevel::Ignored => {
                    panic!(
                        "annotation '{}' has support level Ignored — \
                         Ignored is reserved for unannotated schema keys and must not appear \
                         in a hand-written SalukiAnnotation",
                        path,
                    );
                }
            }
        }
    }
}
