pub mod classifier;

mod duration_de;
mod list_de;

/// Overlay of flat environment-variable keys onto the nested Datadog configuration shape.
pub mod env_overlay;

/// Build-time generated code, produced from `core_schema.yaml` plus `schema_overlay.yaml`.
mod generated;

/// The translation error type recorded by the translator and surfaced by the witness driver.
mod translate_error;

pub use env_overlay::{apply_env_overlay, EnvOverlayMode};
pub use generated::{drive, DatadogConfigWitness, DatadogConfiguration};
pub use translate_error::{TranslateError, TranslateErrors};

#[cfg(test)]
mod string_list_shape_tests {
    use super::DatadogConfiguration;

    // A string-list leaf must accept both shapes the config sources produce: a real sequence (from a
    // file or the remote Agent stream) and a single space-separated string (from an environment
    // variable, e.g. `DD_DOGSTATSD_TAGS="env:prod team:core"`). The generated deserializer wires the
    // shape-tolerant reader onto every `Vec<String>` leaf; these assertions guard that wiring so a
    // regenerate that drops it fails loudly instead of crashing config load on the string form.

    #[test]
    fn string_list_leaf_accepts_a_space_separated_string() {
        let config: DatadogConfiguration =
            serde_json::from_value(serde_json::json!({ "dogstatsd_tags": "env:prod team:core" }))
                .expect("space-separated string deserializes into the string-list leaf");
        assert_eq!(config.dogstatsd_tags, vec!["env:prod", "team:core"]);
    }

    #[test]
    fn string_list_leaf_accepts_a_sequence() {
        let config: DatadogConfiguration =
            serde_json::from_value(serde_json::json!({ "dogstatsd_tags": ["env:prod", "team:core"] }))
                .expect("sequence deserializes into the string-list leaf");
        assert_eq!(config.dogstatsd_tags, vec!["env:prod", "team:core"]);
    }
}
