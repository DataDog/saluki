//! Datadog Agent configuration registry entries.

pub mod aggregate;
pub mod dogstatsd;
pub mod dogstatsd_mapper;
pub mod dogstatsd_prefix_filter;
pub mod encoders;
pub mod forwarder;
pub mod otlp;
pub mod proxy;
pub mod trace_obfuscation;
pub mod unsupported;

use std::sync::LazyLock;

use super::{ConfigKey, SalukiAnnotation};

/// Supported saluki annotations across every sub-system, in registration order.
///
/// The source of truth for which config keys saluki consumes partially or fully supports.
/// Used by the smoke test runner and runtime unknown-key detection.
pub static SUPPORTED_ANNOTATIONS: LazyLock<Vec<&'static SalukiAnnotation>> = LazyLock::new(|| {
    let mut v = Vec::new();
    v.extend_from_slice(aggregate::ALL);
    v.extend_from_slice(dogstatsd::ALL);
    v.extend_from_slice(dogstatsd_mapper::ALL);
    v.extend_from_slice(forwarder::ALL);
    v.extend_from_slice(dogstatsd_prefix_filter::ALL);
    v.extend_from_slice(encoders::ALL);
    v.extend_from_slice(otlp::ALL);
    v.extend_from_slice(proxy::ALL);
    v.extend_from_slice(trace_obfuscation::ALL);
    v
});

/// Annotations for keys that Saluki intentionally does not support.
///
/// All entries have [`Incompatible`](super::SupportLevel::Incompatible) and empty `used_by`.
pub static UNSUPPORTED_ANNOTATIONS: LazyLock<Vec<&'static SalukiAnnotation>> = LazyLock::new(|| {
    let mut v = Vec::new();
    v.extend_from_slice(unsupported::ALL);
    v
});

/// All saluki annotations: supported and unsupported combined.
///
/// Used by the adp-runtime config check to classify every key in the resolved config.
pub static ALL_ANNOTATIONS: LazyLock<Vec<&'static SalukiAnnotation>> = LazyLock::new(|| {
    let mut v = SUPPORTED_ANNOTATIONS.clone();
    v.extend_from_slice(&UNSUPPORTED_ANNOTATIONS);
    v
});

/// All resolved [`ConfigKey`] entries, derived from [`SUPPORTED_ANNOTATIONS`] at first access.
///
/// Provides a flattened, owned view suitable for runtime unknown-key detection.
pub static SUPPORTED_KEYS: LazyLock<Vec<ConfigKey>> =
    LazyLock::new(|| SUPPORTED_ANNOTATIONS.iter().map(|a| ConfigKey::from(*a)).collect());

#[cfg(test)]
mod registry_tests {
    use super::*;
    use crate::config_registry::SupportLevel;

    #[test]
    fn annotation_invariants() {
        for annotation in SUPPORTED_ANNOTATIONS.iter() {
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
                SupportLevel::NotApplicable | SupportLevel::Unrecognized => {
                    panic!(
                        "annotation '{}' has support level {:?} — \
                         Ignored is reserved for unannotated schema keys and must not appear \
                         in a hand-written SalukiAnnotation",
                        path, annotation.support_level
                    );
                }
            }
        }
    }

    #[test]
    fn no_overlap_between_supported_and_unsupported() {
        let supported_paths: std::collections::HashSet<&str> =
            SUPPORTED_ANNOTATIONS.iter().map(|a| a.yaml_path()).collect();

        let duplicates: Vec<&str> = UNSUPPORTED_ANNOTATIONS
            .iter()
            .map(|a| a.yaml_path())
            .filter(|p| supported_paths.contains(p))
            .collect();

        if !duplicates.is_empty() {
            panic!(
                "{} key(s) appear in both SUPPORTED_ANNOTATIONS and UNSUPPORTED_ANNOTATIONS:\n{}",
                duplicates.len(),
                duplicates.iter().map(|p| format!("  - {}", p)).collect::<Vec<_>>().join("\n"),
            );
        }
    }
}
