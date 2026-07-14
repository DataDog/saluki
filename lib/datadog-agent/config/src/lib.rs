pub mod classifier;

mod duration_de;

/// Decoders that turn a raw environment-variable string into the JSON shape a schema leaf declares.
pub mod env_decode;
mod list_de;
mod string_de;

/// Builds the typed configuration base by reading environment variables directly and decoding them
/// into the nested configuration shape.
pub mod env_reader;

/// Relocates underscore-joined environment keys into the nested Datadog configuration shape.
// TODO: remove when all callers use the direct typed environment reader.
pub mod env_overlay;

/// Build-time generated code, produced from `core_schema.yaml` plus `schema_overlay.yaml`.
mod generated;

/// Compatibility support for the by-key configuration path (key aliases and environment remapping).
// TODO: remove when all callers use the typed configuration path.
pub mod remapper;

/// The translation error type recorded by the translator and surfaced by the witness driver.
mod translate_error;

pub use env_decode::EnvDecode;
pub use env_overlay::{apply_env_overlay, EnvOverlayMode};
pub use env_reader::{apply_datadog_env, apply_env_at_path, datadog_leaf_paths, EnvKey};
pub use generated::{drive, DatadogConfigWitness, DatadogConfiguration};
pub use remapper::{DatadogRemapper, KEY_ALIASES};
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

#[cfg(test)]
mod byte_size_shape_tests {
    use super::DatadogConfiguration;

    // A byte-size leaf declared `input_shape: string_or_integer` in the overlay must accept both the
    // canonical string form (`"10MB"`) and the documented bare-integer byte count (`10485760`). The
    // generated deserializer normalizes the integer to a decimal string; these assertions guard that
    // wiring so a regenerate that drops it fails loudly instead of failing config load on the numeric
    // form (which previously aborted a strict-gate startup).

    #[test]
    fn byte_size_leaf_accepts_a_string() {
        let config: DatadogConfiguration =
            serde_json::from_value(serde_json::json!({ "dogstatsd_log_file_max_size": "10MB" }))
                .expect("string byte size deserializes");
        assert_eq!(config.dogstatsd_log_file_max_size, "10MB");
    }

    #[test]
    fn byte_size_leaf_accepts_a_numeric_byte_count() {
        let config: DatadogConfiguration =
            serde_json::from_value(serde_json::json!({ "dogstatsd_log_file_max_size": 10485760 }))
                .expect("numeric byte size deserializes");
        assert_eq!(config.dogstatsd_log_file_max_size, "10485760");

        let config: DatadogConfiguration = serde_json::from_value(serde_json::json!({ "log_file_max_size": 10485760 }))
            .expect("numeric byte size deserializes");
        assert_eq!(config.log_file_max_size, "10485760");
    }

    #[test]
    fn byte_size_leaf_rejects_a_float() {
        let result: Result<DatadogConfiguration, _> =
            serde_json::from_value(serde_json::json!({ "dogstatsd_log_file_max_size": 10.5 }));
        assert!(result.is_err());
    }
}
