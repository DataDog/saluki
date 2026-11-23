//! Compatibility checks against the merged configuration sources.
//!
//! The typed model is the Agent schema pruned to the keys ADP supports, so the unsupported keys this
//! check exists to catch are absent from it by construction. It reads the by-key view of the merged
//! sources instead, which holds every key any input supplied along with the provenance of each.

use std::collections::HashSet;
use std::sync::OnceLock;

use agent_data_plane_config::Provenance;
use datadog_agent_config::classifier::{ConfigClassifier, Pipeline, PipelineAffinity, Severity, SupportLevel};
use saluki_error::{generic_error, GenericError};
use tracing::{debug, error, trace, warn};

use crate::ConfigurationSystem;

/// Returns the compatibility classifier, built once for the process.
pub(crate) fn classifier() -> &'static ConfigClassifier {
    static CLASSIFIER: OnceLock<ConfigClassifier> = OnceLock::new();
    CLASSIFIER.get_or_init(ConfigClassifier::new)
}

impl ConfigurationSystem {
    /// Checks settings that an input set explicitly to a non-default value and that affect active
    /// pipelines, logging the severity of each.
    ///
    /// # Errors
    ///
    /// Returns an error if high-severity incompatibilities exist. All keys are checked before
    /// returning, so the error includes the total count.
    pub fn check_compatibility(&self, active_pipelines: &HashSet<Pipeline>) -> Result<(), GenericError> {
        let mut high_severity_incompatibilities = 0u32;
        debug!("Analyzing configuration.");
        for (key, support_level) in self.compatibility_findings(active_pipelines) {
            match support_level {
                SupportLevel::Incompatible(Severity::Low) => {
                    debug!("Low-severity incompatible key detected. Proceeding.")
                }
                SupportLevel::Partial => {
                    warn!(key = %key, "Partially supported configuration key. See documentation for details. Proceeding.")
                }
                SupportLevel::Incompatible(Severity::Medium) => {
                    warn!(key = %key, "Unsupported configuration key. Proceeding.")
                }
                SupportLevel::Incompatible(Severity::High) => {
                    error!(key = %key, "Unsupported configuration key is set to a value other than its default. ADP \
                    cannot run safely with this setting.");
                    high_severity_incompatibilities += 1;
                }
                SupportLevel::Ignored | SupportLevel::Unrecognized => {
                    trace!(key = %key, "Configuration key not-applicable. Silently ignoring.")
                }
            }
        }

        if high_severity_incompatibilities > 0 {
            return Err(generic_error!(
                "{high_severity_incompatibilities} incompatible configuration detected. ADP cannot start. Review error \
                logs for details."
            ));
        }

        Ok(())
    }

    /// Returns each classified key that an input set explicitly to a non-default value and that
    /// affects an active pipeline, with its support level.
    fn compatibility_findings(&self, active_pipelines: &HashSet<Pipeline>) -> Vec<(String, SupportLevel)> {
        let classifier = classifier();
        let mut findings = Vec::new();
        for (key, value, provenance) in self.sources().flattened_keys() {
            let Some(classification) = classifier.classify(&key, value) else {
                continue;
            };

            let pipeline_is_active = match &classification.pipeline_affinity {
                PipelineAffinity::Pipelines(affected) => affected.iter().any(|p| active_pipelines.contains(p)),
                PipelineAffinity::CrossCutting => true,
            };
            if !pipeline_is_active {
                continue;
            }

            // A producer publishes every key it knows about, the ones nobody configured included, so
            // only a key an input set explicitly says anything about what the operator asked for.
            if provenance == Provenance::Default {
                trace!(key = %key, "Configuration key is not set by any input.");
                continue;
            }

            // A key set explicitly to its schema default asks for the behavior ADP has when nobody
            // sets it. The comparison can miss a default written in another form, such as a duration
            // or a coerced scalar. A miss reports the key, so the check fails safe.
            if classification.is_default {
                trace!(key = %key, "Configuration key is set to its default value.");
                continue;
            }

            findings.push((key, classification.support_level));
        }

        findings
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use datadog_agent_config::classifier::{Classification, Pipeline, PipelineAffinity, Severity, SupportLevel};
    use saluki_config::dynamic::{ConfigSetting, Provenance as StreamProvenance};
    use serde_json::{json, Value};

    use super::classifier;
    use crate::system::translate_strict;
    use crate::{source::SourceTree, ConfigurationSystem};

    /// Builds a system whose sources are a local configuration file, so every key present was set
    /// explicitly.
    fn system_with(file: Value) -> ConfigurationSystem {
        system_from(SourceTree::all_explicit(file))
    }

    /// Builds a system whose sources are the settings a configuration producer published, each
    /// carrying its own provenance.
    fn system_from_settings(settings: &[(&str, Value, StreamProvenance)]) -> ConfigurationSystem {
        let settings: Vec<_> = settings
            .iter()
            .map(|(key, value, provenance)| ConfigSetting::new(*key, value.clone(), *provenance))
            .collect();

        system_from(SourceTree::from_settings(&settings))
    }

    fn system_from(sources: SourceTree) -> ConfigurationSystem {
        let config = translate_strict(&sources).expect("sources translate");
        ConfigurationSystem::standalone(config, sources)
    }

    /// Classifies `key` as the compatibility check sees it among the merged sources.
    fn classification_of(system: &ConfigurationSystem, key: &str) -> Option<Classification> {
        system
            .sources()
            .flattened_keys()
            .into_iter()
            .find(|(flattened, _, _)| flattened == key)
            .and_then(|(flattened, value, _)| classifier().classify(&flattened, value))
    }

    fn pipelines(active: &[Pipeline]) -> HashSet<Pipeline> {
        active.iter().copied().collect()
    }

    fn otlp_tls_settings(cert_pem: &str, key_pem: &str) -> Value {
        json!({
            "otlp_config": {
                "receiver": {
                    "protocols": {
                        "http": {
                            "tls": { "cert_pem": cert_pem, "key_pem": key_pem }
                        }
                    }
                }
            }
        })
    }

    #[test]
    fn high_severity_keys_fail_the_check_and_are_all_counted() {
        let system = system_with(otlp_tls_settings("/etc/adp/cert.pem", "/etc/adp/key.pem"));

        let error = system
            .check_compatibility(&pipelines(&[Pipeline::Otlp]))
            .expect_err("a high-severity incompatible key should fail the check");

        assert!(error.to_string().contains("2 incompatible configuration detected"));
    }

    #[test]
    fn a_high_severity_key_nobody_set_is_skipped() {
        // The Agent publishes every key it knows about, so a key it reports at its own default is a
        // key nobody configured, whatever value it holds.
        let system = system_from_settings(&[
            (
                "otlp_config.receiver.protocols.http.tls.cert_pem",
                json!("/etc/adp/cert.pem"),
                StreamProvenance::Default,
            ),
            (
                "otlp_config.receiver.protocols.http.tls.key_pem",
                json!("/etc/adp/key.pem"),
                StreamProvenance::Default,
            ),
        ]);

        system
            .check_compatibility(&pipelines(&[Pipeline::Otlp]))
            .expect("a key nobody set is not an incompatibility");
    }

    #[test]
    fn a_high_severity_key_set_to_its_default_value_is_skipped() {
        // A key written at its schema default asks for the behavior ADP has when nobody sets it, so
        // it isn't an incompatibility.
        let system = system_with(otlp_tls_settings("", ""));

        system
            .check_compatibility(&pipelines(&[Pipeline::Otlp]))
            .expect("a key set to its default value is not an incompatibility");
        assert!(system.compatibility_findings(&pipelines(&[Pipeline::Otlp])).is_empty());

        let cross_cutting = system_with(json!({ "heroku_dyno": false }));
        cross_cutting
            .check_compatibility(&pipelines(&[]))
            .expect("a cross-cutting key set to its default value is not an incompatibility");
    }

    #[test]
    fn a_warning_key_set_to_its_default_value_is_skipped() {
        let system = system_with(json!({
            "min_tls_version": "tlsv1.2",
            "dogstatsd_telemetry_enabled_listener_id": false,
        }));

        assert!(system
            .compatibility_findings(&pipelines(&[Pipeline::DogStatsD]))
            .is_empty());
    }

    #[test]
    fn a_warning_key_set_to_a_non_default_value_is_reported() {
        let system = system_with(json!({
            "min_tls_version": "tlsv1.3",
            "dogstatsd_telemetry_enabled_listener_id": true,
        }));

        let mut findings = system.compatibility_findings(&pipelines(&[Pipeline::DogStatsD]));
        findings.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            findings,
            vec![
                (
                    "dogstatsd_telemetry_enabled_listener_id".to_string(),
                    SupportLevel::Incompatible(Severity::Medium)
                ),
                ("min_tls_version".to_string(), SupportLevel::Partial),
            ]
        );
    }

    #[test]
    fn a_default_value_the_comparison_misses_is_still_reported() {
        // The default comparison does not coerce a string to a boolean, or a float to integer
        // nanoseconds. A value it can't match is reported, so a miss fails safe.
        let coerced_scalar = system_with(json!({ "heroku_dyno": "false" }));
        coerced_scalar
            .check_compatibility(&pipelines(&[]))
            .expect_err("a default value the comparison misses is still checked");

        let duration = system_with(json!({ "dogstatsd_packet_buffer_flush_timeout": 100_000_000.0 }));
        assert_eq!(
            duration.compatibility_findings(&pipelines(&[Pipeline::DogStatsD])),
            vec![(
                "dogstatsd_packet_buffer_flush_timeout".to_string(),
                SupportLevel::Incompatible(Severity::Low)
            )]
        );
    }

    #[test]
    fn a_high_severity_key_affecting_no_active_pipeline_is_skipped() {
        let system = system_with(otlp_tls_settings("/etc/adp/cert.pem", "/etc/adp/key.pem"));

        system
            .check_compatibility(&pipelines(&[Pipeline::DogStatsD]))
            .expect("an inactive pipeline's keys are not incompatibilities");
    }

    #[test]
    fn lower_severity_keys_pass_and_cross_cutting_keys_ignore_active_pipelines() {
        let system = system_with(json!({ "dogstatsd_pipeline_count": 4, "min_tls_version": "tlsv1.3" }));
        system
            .check_compatibility(&pipelines(&[Pipeline::DogStatsD]))
            .expect("only high-severity incompatibilities fail the check");

        let cross_cutting = system_with(json!({ "heroku_dyno": true }));
        cross_cutting
            .check_compatibility(&pipelines(&[]))
            .expect_err("a cross-cutting high-severity key fails the check with no pipeline active");
    }

    #[test]
    fn an_unsupported_object_setting_is_classified_under_its_own_key() {
        // `use_v3_api.distributions.endpoints` holds an object and no model describes it. The check must see
        // it as the setting it is, whether it comes from a file or from the configuration stream.
        let key = "use_v3_api.distributions.endpoints";
        let endpoints = json!({ "https://example.test": ["api-key"] });
        let from_file = system_with(json!({ "use_v3_api": { "distributions": { "endpoints": endpoints } } }));
        let from_stream = system_from_settings(&[(key, endpoints.clone(), StreamProvenance::Explicit)]);

        for system in [from_file, from_stream] {
            let classification = classification_of(&system, key).expect("the setting is classified under its own key");
            assert_eq!(
                classification.support_level,
                SupportLevel::Incompatible(Severity::Medium)
            );
            assert_eq!(
                classification.pipeline_affinity,
                PipelineAffinity::Pipelines(&[Pipeline::DogStatsD])
            );
        }
    }

    #[test]
    fn keys_the_registry_does_not_know_are_ignored() {
        let system = system_with(json!({ "not_a_real_agent_setting": true, "dogstatsd_port": 9125 }));

        system
            .check_compatibility(&pipelines(&[Pipeline::DogStatsD]))
            .expect("unclassified keys are not incompatibilities");
    }
}
