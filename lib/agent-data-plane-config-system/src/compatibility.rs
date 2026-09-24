//! Compatibility checks against the merged configuration sources.
//!
//! The typed model is the Agent schema pruned to the keys ADP supports, so the unsupported keys this
//! check exists to catch are absent from it by construction. It reads the by-key view of the merged
//! sources instead, which holds every key any input supplied along with the provenance of each.

use std::collections::HashSet;

use agent_data_plane_config::Provenance;
use datadog_agent_config::classifier::{ConfigClassifier, Pipeline, PipelineAffinity, Severity, SupportLevel};
use saluki_error::{generic_error, GenericError};
use tracing::{debug, error, trace, warn};

use crate::ConfigurationSystem;

impl ConfigurationSystem {
    /// Checks settings that an input set explicitly and that affect active pipelines, logging the
    /// severity of each.
    ///
    /// # Errors
    ///
    /// Returns an error if high-severity incompatibilities exist. All keys are checked before
    /// returning, so the error includes the total count.
    pub fn check_compatibility(&self, active_pipelines: &HashSet<Pipeline>) -> Result<(), GenericError> {
        let classifier = ConfigClassifier::new();
        let mut high_severity_incompatibilities = 0u32;
        debug!("Analyzing configuration.");
        for (key, value, provenance) in self.sources.load().flattened_keys() {
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

            match classification.support_level {
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
                    error!(key = %key, "Unsupported configuration key with non-default value. ADP cannot run safely with \
                    this setting.");
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
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use datadog_agent_config::classifier::Pipeline;
    use saluki_config::dynamic::{ConfigSetting, Provenance as StreamProvenance};
    use saluki_config::ConfigurationLoader;
    use serde_json::{json, Value};

    use crate::system::translate_strict;
    use crate::{source::SourceTree, ConfigurationSystem};

    /// Builds a system whose sources are a local configuration file, so every key present was set
    /// explicitly.
    async fn system_with(file: Value) -> ConfigurationSystem {
        system_from(SourceTree::all_explicit(file)).await
    }

    /// Builds a system whose sources are the settings a configuration producer published, each
    /// carrying its own provenance.
    async fn system_from_settings(settings: &[(&str, Value, StreamProvenance)]) -> ConfigurationSystem {
        let settings: Vec<_> = settings
            .iter()
            .map(|(key, value, provenance)| ConfigSetting::new(*key, value.clone(), *provenance))
            .collect();

        system_from(SourceTree::from_settings(&settings)).await
    }

    async fn system_from(sources: SourceTree) -> ConfigurationSystem {
        let (raw_map, _) = ConfigurationLoader::for_tests(None, None, false).await;
        let config = translate_strict(&sources).expect("sources translate");
        ConfigurationSystem::standalone(raw_map, config, sources)
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

    #[tokio::test]
    async fn high_severity_keys_fail_the_check_and_are_all_counted() {
        let system = system_with(otlp_tls_settings("/etc/adp/cert.pem", "/etc/adp/key.pem")).await;

        let error = system
            .check_compatibility(&pipelines(&[Pipeline::Otlp]))
            .expect_err("a high-severity incompatible key should fail the check");

        assert!(error.to_string().contains("2 incompatible configuration detected"));
    }

    #[tokio::test]
    async fn a_high_severity_key_nobody_set_is_skipped() {
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
        ])
        .await;

        system
            .check_compatibility(&pipelines(&[Pipeline::Otlp]))
            .expect("a key nobody set is not an incompatibility");
    }

    #[tokio::test]
    async fn a_high_severity_key_set_to_its_default_value_is_still_checked() {
        // Writing an unsupported key is a request ADP cannot honor, so it is reported even when the
        // value written happens to be the one the schema would have supplied.
        let system = system_with(otlp_tls_settings("", "")).await;

        let error = system
            .check_compatibility(&pipelines(&[Pipeline::Otlp]))
            .expect_err("an explicitly set key should fail the check");

        assert!(error.to_string().contains("2 incompatible configuration detected"));
    }

    #[tokio::test]
    async fn a_high_severity_key_affecting_no_active_pipeline_is_skipped() {
        let system = system_with(otlp_tls_settings("/etc/adp/cert.pem", "/etc/adp/key.pem")).await;

        system
            .check_compatibility(&pipelines(&[Pipeline::DogStatsD]))
            .expect("an inactive pipeline's keys are not incompatibilities");
    }

    #[tokio::test]
    async fn lower_severity_keys_pass_and_cross_cutting_keys_ignore_active_pipelines() {
        let system = system_with(json!({ "dogstatsd_queue_size": 2048, "min_tls_version": "tlsv1.3" })).await;
        system
            .check_compatibility(&pipelines(&[Pipeline::DogStatsD]))
            .expect("only high-severity incompatibilities fail the check");

        let cross_cutting = system_with(json!({ "heroku_dyno": true })).await;
        cross_cutting
            .check_compatibility(&pipelines(&[]))
            .expect_err("a cross-cutting high-severity key fails the check with no pipeline active");
    }

    #[tokio::test]
    async fn keys_the_registry_does_not_know_are_ignored() {
        let system = system_with(json!({ "not_a_real_agent_setting": true, "dogstatsd_port": 9125 })).await;

        system
            .check_compatibility(&pipelines(&[Pipeline::DogStatsD]))
            .expect("unclassified keys are not incompatibilities");
    }
}
