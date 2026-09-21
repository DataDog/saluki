//! Compatibility checks against the raw Datadog configuration.
//!
//! The typed model excludes unsupported keys, so classification uses the by-key view.

use std::collections::HashSet;

use datadog_agent_config::classifier::{ConfigClassifier, Pipeline, PipelineAffinity, Severity, SupportLevel};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tracing::{debug, error, trace, warn};

use crate::ConfigurationSystem;

impl ConfigurationSystem {
    /// Checks non-default settings that affect active pipelines and logs their severity.
    ///
    /// # Errors
    ///
    /// Returns an error if flattening fails or high-severity incompatibilities exist. All keys are
    /// checked before returning, so the error includes the total count.
    pub fn check_compatibility(&self, active_pipelines: &HashSet<Pipeline>) -> Result<(), GenericError> {
        let classifier = ConfigClassifier::new();
        let mut high_severity_incompatibilities = 0u32;
        debug!("Analyzing configuration.");
        for (key, val) in self
            .raw_map()
            .flattened_keys()
            .error_context("Unable to flatten configuration into a list of dot-separated keys.")?
        {
            let Some(classification) = classifier.classify(&key, &val) else {
                continue;
            };

            let pipeline_is_active = match &classification.pipeline_affinity {
                PipelineAffinity::Pipelines(affected) => affected.iter().any(|p| active_pipelines.contains(p)),
                PipelineAffinity::CrossCutting => true,
            };
            if !pipeline_is_active {
                continue;
            }

            // The Agent includes schema defaults even when the operator did not set them.
            if classification.is_default {
                trace!(key = %key, "Configuration key has a default value.");
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
    use saluki_config::ConfigurationLoader;
    use serde_json::{json, Value};

    use crate::system::translate_strict;
    use crate::{source::SourceTree, ConfigurationSystem};

    async fn system_with(file: Value) -> ConfigurationSystem {
        let (raw_map, _) = ConfigurationLoader::for_tests(Some(file), None, false).await;
        let base = SourceTree::all_explicit(raw_map.as_typed::<Value>().expect("base extracts"));
        let config = translate_strict(&base).expect("sources translate");
        ConfigurationSystem::standalone(raw_map, config)
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
    async fn a_high_severity_key_holding_its_default_is_skipped() {
        let system = system_with(otlp_tls_settings("", "")).await;

        system
            .check_compatibility(&pipelines(&[Pipeline::Otlp]))
            .expect("default-valued keys are not incompatibilities");
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
