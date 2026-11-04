/// Deserializers that coerce a scalar leaf the way the Agent's own type cast does.
mod cast_de;

pub mod classifier;

mod duration_de;

/// Decoders that turn a raw environment-variable string into the JSON shape a schema leaf declares.
pub mod env_decode;
mod list_de;

/// A Figment provider that reads the schema's environment variables into their canonical shape.
pub mod env_provider;

/// Builds the typed configuration base by reading environment variables directly and decoding them
/// into the nested configuration shape.
pub mod env_reader;

/// Build-time generated code, produced from `core_schema.yaml` plus `schema_overlay.yaml`.
mod generated;

/// Runtime selection of a schema `platform_default` value, called by generated default functions.
mod platform_default;

/// A shared, immutable view of the vendored schema's defaults.
mod schema_defaults;

/// Parses a byte-size setting the way the Agent's `GetSizeInBytes` does.
mod size_in_bytes;

/// The translation error type recorded by the translator and surfaced by the witness driver.
mod translate_error;

pub use cast_de::cast_to_f64;
pub use cast_de::cast_to_string;
pub use env_decode::EnvDecode;
pub use env_provider::DatadogEnvProvider;
pub use env_reader::{apply_datadog_env, apply_datadog_env_vars, apply_env_at_path, datadog_leaf_paths, EnvKey};
pub use generated::{drive, DatadogConfigWitness, DatadogConfiguration, Leaf, LeafValue, LEAVES};
pub use platform_default::is_containerized;
pub use size_in_bytes::parse_size_in_bytes;
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
    fn string_list_leaf_accepts_a_json_encoded_list() {
        let config: DatadogConfiguration = serde_json::from_value(serde_json::json!({
            "apm_config": { "obfuscation": { "elasticsearch": { "keep_values": r#"["cr-a","cr-b"]"# } } }
        }))
        .expect("JSON-encoded list deserializes into the string-list leaf");
        assert_eq!(
            config.apm_config.obfuscation.elasticsearch.keep_values,
            vec!["cr-a", "cr-b"]
        );
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
    fn number_map_leaf_accepts_numeric_string_values() {
        let config: DatadogConfiguration = serde_json::from_value(json!({
            "apm_config": { "analyzed_rate_by_service": { "service": "0.8" } }
        }))
        .expect("numeric string map deserializes");
        assert_eq!(config.apm_config.analyzed_rate_by_service["service"], 0.8);
    }

    #[test]
    fn integer_leaf_accepts_a_numeric_string() {
        let config: DatadogConfiguration =
            serde_json::from_value(json!({ "dogstatsd_port": "8125" })).expect("numeric string deserializes");
        assert_eq!(config.dogstatsd_port, 8125);
    }

    #[test]
    fn integer_leaf_reads_a_quoted_zero_padded_number_as_octal() {
        let config: DatadogConfiguration =
            serde_json::from_value(json!({ "dogstatsd_buffer_size": "010" })).expect("numeric string deserializes");
        assert_eq!(config.dogstatsd_buffer_size, 8);
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
            // a boolean. An integer leaf must also accept the base-prefixed spelling the Agent's integer
            // cast reads. Each written value differs from the leaf's default, so a coercion that silently
            // failed to land cannot be mistaken for one that worked.
            let cases = match key.decode {
                EnvDecode::Bool => {
                    let flipped = !current.and_then(Value::as_bool).unwrap_or(false);
                    vec![(json!(flipped.to_string()), json!(flipped))]
                }
                EnvDecode::Integer => {
                    let bumped = current.and_then(Value::as_i64).unwrap_or(0) + 1;
                    let sign = if bumped < 0 { "-" } else { "" };
                    let hex = format!("{sign}{:#x}", bumped.unsigned_abs());
                    vec![(json!(bumped.to_string()), json!(bumped)), (json!(hex), json!(bumped))]
                }
                EnvDecode::Float => {
                    let bumped = current.and_then(Value::as_f64).unwrap_or(0.0) + 1.5;
                    vec![(json!(bumped.to_string()), json!(bumped))]
                }
                EnvDecode::RawString => {
                    let flag = current.and_then(Value::as_str) != Some("true");
                    vec![(json!(flag), json!(flag.to_string()))]
                }
                _ => continue,
            };

            for (written, expected) in cases {
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

#[cfg(test)]
mod null_collection_tests {
    use serde_json::{json, Value};

    use super::env_decode::EnvDecode;
    use super::generated::env_keys::DATADOG_ENV_KEYS;
    use super::DatadogConfiguration;

    // The Agent's configuration stream carries `null` for an empty or cleared list or map, and the
    // Agent's accessors read it as empty. A key that is absent is different: it keeps the schema default.

    #[test]
    fn every_collection_leaf_accepts_null() {
        // Scalar leaves are covered by `scalar_shape_tests`; durations are not collections.
        let collections = DATADOG_ENV_KEYS.iter().filter(|key| {
            !matches!(
                key.decode,
                EnvDecode::Bool
                    | EnvDecode::Integer
                    | EnvDecode::Float
                    | EnvDecode::RawString
                    | EnvDecode::DurationString
            )
        });

        for key in collections {
            let mut tree = Value::Null;
            for segment in key.path.iter().rev() {
                tree = json!({ *segment: tree });
            }

            let leaf = key.path.join(".");
            serde_json::from_value::<DatadogConfiguration>(tree)
                .unwrap_or_else(|e| panic!("leaf `{leaf}` rejected null: {e}"));
        }
    }

    #[test]
    fn a_null_string_list_is_empty_while_an_absent_one_keeps_its_default() {
        let absent: DatadogConfiguration = serde_json::from_value(json!({})).expect("empty configuration deserializes");
        assert!(
            !absent.histogram_aggregates.is_empty(),
            "the schema default is not empty"
        );

        let null: DatadogConfiguration =
            serde_json::from_value(json!({ "histogram_aggregates": null, "proxy": { "no_proxy": null } }))
                .expect("null string lists deserialize");
        assert!(null.histogram_aggregates.is_empty());
        assert!(null.proxy.no_proxy.is_empty());
    }

    #[test]
    fn null_maps_and_object_arrays_are_empty() {
        let config: DatadogConfiguration = serde_json::from_value(json!({
            "additional_endpoints": null,
            "dogstatsd_mapper_profiles": null,
            "apm_config": { "replace_tags": null },
            "use_v3_api": { "series": { "endpoints": null } }
        }))
        .expect("null collections deserialize");

        assert!(config.additional_endpoints.is_empty());
        assert!(config.dogstatsd_mapper_profiles.is_empty());
        assert!(config.apm_config.replace_tags.is_empty());
        assert!(config.use_v3_api.series.endpoints.is_empty());
    }

    #[test]
    fn a_null_replace_tags_rule_is_an_empty_map() {
        let config: DatadogConfiguration =
            serde_json::from_value(json!({ "apm_config": { "replace_tags": [null, { "name": "*" }] } }))
                .expect("a null rule deserializes");

        assert_eq!(config.apm_config.replace_tags.len(), 2);
        assert!(config.apm_config.replace_tags[0].is_empty());
        assert_eq!(config.apm_config.replace_tags[1]["name"], "*");
    }
}

#[cfg(test)]
mod leaves_tests {
    use serde_json::json;

    use super::generated::env_keys::DATADOG_ENV_KEYS;
    use super::{DatadogConfiguration, LeafValue, LEAVES};

    // `LEAVES` and `DATADOG_ENV_KEYS` are independently generated (by `witness_gen.rs` and
    // `env_reader_gen.rs` respectively) from the same overlay key set, so comparing them catches a
    // divergence between the two generators rather than just a generator agreeing with itself.
    #[test]
    fn leaves_has_one_entry_per_supported_key() {
        assert_eq!(LEAVES.len(), DATADOG_ENV_KEYS.len());
        for env_key in DATADOG_ENV_KEYS {
            let dotted = env_key.path.join(".");
            assert!(
                LEAVES.iter().any(|leaf| leaf.key == dotted),
                "no LEAVES entry for supported key `{dotted}`"
            );
        }
    }

    #[test]
    fn leaves_are_sorted_and_unique_by_key() {
        for pair in LEAVES.windows(2) {
            assert!(
                pair[0].key < pair[1].key,
                "LEAVES is not sorted (or has a duplicate) between `{}` and `{}`",
                pair[0].key,
                pair[1].key
            );
        }
    }

    #[test]
    fn leaves_read_typed_values_of_several_kinds() {
        let config: DatadogConfiguration = serde_json::from_value(json!({
            "dogstatsd_non_local_traffic": true,
            "dogstatsd_port": 9999,
            "dogstatsd_tags": ["env:prod", "team:core"],
            "additional_endpoints": { "https://x.": ["k1", "k2"] },
            "apm_config": { "obfuscation": { "credit_cards": { "enabled": true } } },
        }))
        .expect("small configuration deserializes");

        let leaf = |key: &str| {
            LEAVES
                .iter()
                .find(|l| l.key == key)
                .unwrap_or_else(|| panic!("no leaf `{key}`"))
        };

        assert!(matches!(
            (leaf("dogstatsd_non_local_traffic").get)(&config),
            LeafValue::Bool(true)
        ));
        assert!(matches!((leaf("dogstatsd_port").get)(&config), LeafValue::I64(9999)));

        match (leaf("dogstatsd_tags").get)(&config) {
            LeafValue::StringList(tags) => assert_eq!(tags, ["env:prod", "team:core"]),
            other => panic!("unexpected leaf value: {other:?}"),
        }

        match (leaf("additional_endpoints").get)(&config) {
            LeafValue::StringListMap(map) => {
                assert_eq!(map["https://x."], vec!["k1".to_string(), "k2".to_string()]);
            }
            other => panic!("unexpected leaf value: {other:?}"),
        }

        assert!(matches!(
            (leaf("apm_config.obfuscation.credit_cards.enabled").get)(&config),
            LeafValue::Bool(true)
        ));

        // A key the JSON above never set reads back as the leaf's serde default.
        match (leaf("syslog_rfc").get)(&config) {
            LeafValue::Bool(value) => assert_eq!(value, DatadogConfiguration::default().syslog_rfc),
            other => panic!("unexpected leaf value: {other:?}"),
        }
    }

    #[test]
    fn aliased_leaf_lists_its_alias() {
        let leaf = LEAVES
            .iter()
            .find(|l| l.key == "statsd_metric_namespace_blacklist")
            .expect("aliased leaf is present");
        assert_eq!(leaf.aliases, ["statsd_metric_namespace_blocklist"]);
    }
}
