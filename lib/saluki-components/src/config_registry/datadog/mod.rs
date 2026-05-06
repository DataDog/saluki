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
    use std::collections::HashSet;

    use super::*;
    use crate::config_registry::{Schema, SupportLevel, ALL_SCHEMA_ENTRIES, IGNORED_ENTRIES};

    fn check_for_duplicates(it: impl Iterator<Item = impl AsRef<str>>) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        let mut duplicates = Vec::new();
        for item in it {
            let s = item.as_ref().to_owned();
            if !seen.insert(s.clone()) {
                duplicates.push(s);
            }
        }

        if duplicates.is_empty() {
            Ok(())
        } else {
            Err(format!("\n{}", duplicates.join("\n")))
        }
    }

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

    /// This test ensures that the same config key does not appear in both the supported and unsupported lists of
    /// `SalukiAnnotations`.
    #[test]
    fn no_overlap_between_supported_and_unsupported() {
        let supported_paths: HashSet<&str> = SUPPORTED_ANNOTATIONS.iter().map(|a| a.yaml_path()).collect();

        let duplicates: Vec<&str> = UNSUPPORTED_ANNOTATIONS
            .iter()
            .map(|a| a.yaml_path())
            .filter(|p| supported_paths.contains(p))
            .collect();

        if !duplicates.is_empty() {
            panic!(
                "{} key(s) appear in both SUPPORTED_ANNOTATIONS and UNSUPPORTED_ANNOTATIONS:\n{}",
                duplicates.len(),
                duplicates
                    .iter()
                    .map(|p| format!("  - {}", p))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }

    #[test]
    fn no_duplicates_in_supported_annotations() {
        if let Err(dupes) = check_for_duplicates(SUPPORTED_ANNOTATIONS.iter().map(|&a| a.yaml_path())) {
            panic!("Duplicates in SUPPORTED_ANNOTATIONS:{dupes}");
        }
    }

    #[test]
    fn no_duplicates_in_unsupported_annotations() {
        if let Err(dupes) = check_for_duplicates(UNSUPPORTED_ANNOTATIONS.iter().map(|&a| a.yaml_path())) {
            panic!("Duplicates in UNSUPPORTED_ANNOTATIONS:{dupes}");
        }
    }

    #[test]
    fn no_duplicates_in_ignored_keys() {
        if let Err(dupes) = check_for_duplicates(IGNORED_ENTRIES.iter()) {
            panic!("Duplicates in IGNORED_ENTRIES:{dupes}");
        }
    }

    #[test]
    fn no_overlap_between_annotations_and_ignored() {
        let annotation_paths: HashSet<&str> = ALL_ANNOTATIONS.iter().map(|a| a.yaml_path()).collect();
        let overlaps: Vec<&&str> = IGNORED_ENTRIES
            .iter()
            .filter(|k| annotation_paths.contains(**k))
            .collect();
        if !overlaps.is_empty() {
            panic!(
                "{} key(s) appear in both ALL_ANNOTATIONS and IGNORED_ENTRIES:\n{}",
                overlaps.len(),
                overlaps
                    .iter()
                    .map(|p| format!("  - {}", p))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }

    #[test]
    fn no_stale_entries() {
        let schema_paths: HashSet<&str> = ALL_SCHEMA_ENTRIES.iter().map(|e| e.yaml_path).collect();

        let stale_annotations: Vec<&str> = ALL_ANNOTATIONS
            .iter()
            .filter(|a| a.schema.schema == Schema::Datadog)
            .map(|a| a.yaml_path())
            .filter(|p| !schema_paths.contains(p))
            .collect();

        let stale_ignored: Vec<&&str> = IGNORED_ENTRIES.iter().filter(|k| !schema_paths.contains(**k)).collect();

        if stale_annotations.is_empty() && stale_ignored.is_empty() {
            return;
        }

        let mut msg = String::new();
        if !stale_annotations.is_empty() {
            msg.push_str(&format!(
                "{} stale annotation(s) not in schema:\n{}\n",
                stale_annotations.len(),
                stale_annotations
                    .iter()
                    .map(|p| format!("  - {}", p))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
        }
        if !stale_ignored.is_empty() {
            msg.push_str(&format!(
                "{} stale ignored key(s) not in schema:\n{}",
                stale_ignored.len(),
                stale_ignored
                    .iter()
                    .map(|p| format!("  - {}", p))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
        }
        panic!("{msg}");
    }

    #[test]
    fn all_schema_entries_are_annotated_or_ignored() {
        // The union of all annotated keys plus all the keys we intend to ignore.
        let all_accounted_for_entries: HashSet<&str> = HashSet::from_iter(
            ALL_ANNOTATIONS
                .iter()
                .map(|&annotation| annotation.yaml_path())
                .chain(IGNORED_ENTRIES.iter().copied()),
        );

        // Iterate through all schema entries. If a schema entry is not found, that it bad. We want
        // to add it to a list of missing keys.
        let mut missing_keys = Vec::new();
        for schema_key in ALL_SCHEMA_ENTRIES.iter().map(|&entry| entry.yaml_path) {
            if !all_accounted_for_entries.contains(schema_key) {
                // This schema_key is missing from Saluki's config registry system.
                missing_keys.push(schema_key);
            }
        }

        if missing_keys.is_empty() {
            // The test succeeded. All keys are accounted for.
            return;
        }

        // The test failed. Build a list of missing keys.
        panic!(
            "{} config key(s) are missing from the Saluki registry: \n\n{}",
            missing_keys.len(),
            missing_keys.join("\n")
        );
    }
}
