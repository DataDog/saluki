/// Deserializers that coerce a scalar leaf the way the Agent's own type cast does.
mod cast_de;

pub mod classifier;

mod duration_de;

/// Decoders that turn a raw environment-variable string into the JSON shape a schema leaf declares.
pub mod env_decode;

/// Source types for object arrays without `items` schemas.
pub mod array_objects;
mod list_de;

/// A Figment provider that reads the schema's environment variables into their canonical shape.
pub mod env_provider;

/// Builds the typed configuration base by reading environment variables directly and decoding them
/// into the nested configuration shape.
pub mod env_reader;

/// Build-time generated code, produced from `core_schema.yaml` plus `schema_overlay.yaml`.
mod generated;

/// The translation error type recorded by the translator and surfaced by the witness driver.
mod translate_error;

pub use cast_de::cast_to_string;
pub use env_decode::EnvDecode;
pub use env_provider::DatadogEnvProvider;
pub use env_reader::{apply_datadog_env, apply_datadog_env_vars, apply_env_at_path, datadog_leaf_paths, EnvKey};
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

#[cfg(test)]
mod string_map_list_shape_tests {
    use super::DatadogConfiguration;

    #[test]
    fn additional_endpoints_accept_scalar_values() {
        let config: DatadogConfiguration = serde_json::from_value(serde_json::json!({
            "additional_endpoints": {
                "https://agent.datadoghq.com.": "ENC[vault://api-key]"
            }
        }))
        .expect("scalar additional endpoint API key deserializes");

        assert_eq!(
            config.additional_endpoints["https://agent.datadoghq.com."],
            ["ENC[vault://api-key]"]
        );
    }

    #[test]
    fn additional_endpoints_accept_sequence_values() {
        let config: DatadogConfiguration = serde_json::from_value(serde_json::json!({
            "additional_endpoints": {
                "https://agent.datadoghq.com.": ["first", "second"]
            }
        }))
        .expect("additional endpoint API key sequence deserializes");

        assert_eq!(
            config.additional_endpoints["https://agent.datadoghq.com."],
            ["first", "second"]
        );
    }
}

#[cfg(test)]
mod scalar_shape_tests {
    use serde_json::{json, Value};

    use super::env_decode::EnvDecode;
    use super::generated::env_keys::DATADOG_ENV_KEYS;
    use super::DatadogConfiguration;

    // The Agent reads a setting by casting whatever is stored to the accessor's type, so a leaf must
    // accept more than the JSON type its schema declares. These assertions cover the wiring generated
    // for that (`crate::cast_de`), so a regenerate that drops it fails here instead of rejecting a
    // configuration the Agent accepts — which, at the strict startup gate, means ADP fails to boot.

    #[test]
    fn boolean_leaf_accepts_a_boolean_string() {
        let config: DatadogConfiguration = serde_json::from_value(json!({ "dogstatsd_non_local_traffic": "true" }))
            .expect("boolean string deserializes");
        assert!(config.dogstatsd_non_local_traffic);
    }

    #[test]
    fn integer_leaf_accepts_a_numeric_string() {
        let config: DatadogConfiguration =
            serde_json::from_value(json!({ "dogstatsd_port": "8125" })).expect("numeric string deserializes");
        assert_eq!(config.dogstatsd_port, 8125);
    }

    #[test]
    fn string_leaf_accepts_a_boolean() {
        // The Agent reads this leaf with `GetString`, so a YAML boolean reaches it as `"true"`.
        let config: DatadogConfiguration =
            serde_json::from_value(json!({ "use_v3_api": { "series": { "enabled": true } } }))
                .expect("boolean V3 series mode deserializes");
        assert_eq!(config.use_v3_api.series.enabled, "true");
    }

    #[test]
    fn string_leaf_accepts_a_numeric_byte_count() {
        // A byte size is schema-typed as a string but documented as a bare byte count as well.
        let config: DatadogConfiguration = serde_json::from_value(json!({ "dogstatsd_log_file_max_size": 10485760 }))
            .expect("byte count deserializes");
        assert_eq!(config.dogstatsd_log_file_max_size, "10485760");
    }

    #[test]
    fn every_scalar_leaf_accepts_the_agent_castable_form_of_its_type() {
        // The environment table is the runtime inventory of leaves with their declared types, so this
        // reaches every scalar leaf rather than the handful spelled out above.
        let defaults = serde_json::to_value(DatadogConfiguration::default()).expect("defaults serialize");

        for key in DATADOG_ENV_KEYS {
            let pointer = format!("/{}", key.path.join("/"));
            let current = defaults.pointer(&pointer);

            // A boolean, integer, or float leaf must accept its string spelling (how it arrives from an
            // environment variable, and how an operator may write it in YAML); a string leaf must accept
            // a boolean. Each written value differs from the leaf's default, so a coercion that silently
            // failed to land cannot be mistaken for one that worked.
            let (written, expected) = match key.decode {
                EnvDecode::Bool => {
                    let flipped = !current.and_then(Value::as_bool).unwrap_or(false);
                    (json!(flipped.to_string()), json!(flipped))
                }
                EnvDecode::Integer => {
                    let bumped = current.and_then(Value::as_i64).unwrap_or(0) + 1;
                    (json!(bumped.to_string()), json!(bumped))
                }
                EnvDecode::Float => {
                    let bumped = current.and_then(Value::as_f64).unwrap_or(0.0) + 1.5;
                    (json!(bumped.to_string()), json!(bumped))
                }
                EnvDecode::RawString => {
                    let flag = current.and_then(Value::as_str) != Some("true");
                    (json!(flag), json!(flag.to_string()))
                }
                _ => continue,
            };

            let mut tree = written.clone();
            for segment in key.path.iter().rev() {
                tree = json!({ *segment: tree });
            }

            let leaf = key.path.join(".");
            let config: DatadogConfiguration =
                serde_json::from_value(tree).unwrap_or_else(|e| panic!("leaf `{leaf}` rejected {written}: {e}"));
            let coerced = serde_json::to_value(config).expect("the configuration serializes");
            assert_eq!(
                coerced.pointer(&pointer),
                Some(&expected),
                "leaf `{leaf}` did not coerce {written}"
            );
        }
    }

    #[test]
    fn a_malformed_scalar_is_still_rejected() {
        // Permissive is not unconditional: a value the Agent's cast cannot convert must fail here
        // rather than silently reading as the type's zero value, which is what the Agent does.
        for malformed in [
            json!({ "dogstatsd_non_local_traffic": "yes" }),
            json!({ "dogstatsd_port": "8125ms" }),
            json!({ "dogstatsd_log_file_max_size": ["10MB"] }),
        ] {
            let result: Result<DatadogConfiguration, _> = serde_json::from_value(malformed.clone());
            assert!(result.is_err(), "{malformed} should be rejected");
        }
    }

    #[test]
    fn a_null_scalar_reads_as_the_type_zero_value() {
        let config: DatadogConfiguration =
            serde_json::from_value(json!({ "api_key": Value::Null })).expect("an explicitly null leaf deserializes");
        assert_eq!(config.api_key, "");
    }
}
