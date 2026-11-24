//! Logs domain. Carries the configuration of the logs pipeline that check and OTLP logs share.

use std::fmt;

use serde::Serialize;
use serde_json::Value;

use crate::agent_decode::{field, list, map, string, unexpected, DecodeError, Decoder};

/// Resolved logs configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Domain {
    /// Processing rules applied, in order, to every log: `logs_config.processing_rules`.
    ///
    /// Empty unless the logs agent is enabled, since the Agent only reads the rules then. The rules aren't
    /// validated: like the Agent, ADP validates them when it starts the logs pipeline. See
    /// [`Domain::validate_processing_rules`].
    pub processing_rules: ProcessingRules,
}

impl Domain {
    /// Decodes and validates the processing rules as the Agent's `GlobalProcessingRules` does
    /// (`comp/logs/agent/config/config.go`), and returns them in order.
    ///
    /// A JSON string is decoded first. Then the rules are validated in order, as the Agent's `ValidateProcessingRules`
    /// does (`comp/logs/agent/config/processing_rules.go`), and the error is the Agent's message for the first problem
    /// found. The patterns aren't compiled here: the component that applies the rules compiles them.
    ///
    /// # Errors
    ///
    /// Returns an error if a JSON string isn't a JSON array of rules or has a field of the wrong type, a rule has no
    /// name, no type or an unknown type,
    /// no pattern where its type needs one, or a `remap_source` rule has no matching entries or an entry with an empty
    /// field.
    pub fn validate_processing_rules(&self) -> Result<Vec<ProcessingRule>, InvalidProcessingRules> {
        let decoded;
        let entries = match &self.processing_rules {
            ProcessingRules::Entries(entries) => entries,
            // Like the Agent, read an empty string as no rules.
            ProcessingRules::Json(json) if json.is_empty() => return Ok(Vec::new()),
            ProcessingRules::Json(json) => {
                let invalid = |error: &dyn fmt::Display| InvalidProcessingRules(format!("invalid JSON: {error}"));
                let rules = match serde_json::from_str::<Value>(json).map_err(|error| invalid(&error))? {
                    // Like `encoding/json`, read `null` as no rules.
                    Value::Null => Vec::new(),
                    Value::Array(rules) => rules,
                    other => return Err(invalid(&unexpected("rules", "a list", &other))),
                };
                decoded = rules
                    .iter()
                    .enumerate()
                    .map(|(index, rule)| {
                        ProcessingRuleEntry::decode(rule, Decoder::Json)
                            .map_err(|error| invalid(&format_args!("processing rule {index}: {error}")))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                &decoded
            }
        };
        entries.iter().map(ProcessingRuleEntry::validate).collect()
    }
}

/// Logs processing rules as configured.
///
/// Serializes as the configured value: a list of rules, or the JSON string.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ProcessingRules {
    /// Rules configured as a sequence.
    Entries(Vec<ProcessingRuleEntry>),

    /// Rules configured as a JSON-encoded string, such as `'[{"type": "exclude_at_match", ...}]'`.
    ///
    /// Like the Agent, ADP doesn't decode the string until it validates the rules.
    Json(String),
}

impl Default for ProcessingRules {
    fn default() -> Self {
        Self::Entries(Vec::new())
    }
}

/// A logs processing rule as configured, before validation.
///
/// Every field is empty when the configuration doesn't set it, or sets it to null.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ProcessingRuleEntry {
    /// Type of the rule, such as `exclude_at_match`.
    #[serde(rename = "type")]
    pub rule_type: String,
    /// Name of the rule.
    pub name: String,
    /// Regular expression of the rule.
    pub pattern: String,
    /// Replacement for each match of a `mask_sequences` rule.
    pub replace_placeholder: String,
    /// Attribute values that a `remap_source` rule matches.
    pub matching: Vec<SourceMatch>,
}

impl ProcessingRuleEntry {
    /// Decodes one rule of a sequence as the Agent's `structure.UnmarshalKey` does: with `mapstructure`, and weakly
    /// typed input.
    ///
    /// # Errors
    ///
    /// Returns an error if the rule isn't a map, or a field holds a value that doesn't convert: a list or a map where a
    /// string is expected, or a `matching` entry that isn't a map.
    pub fn from_sequence_value(value: &Value) -> Result<Self, InvalidProcessingRules> {
        Self::decode(value, Decoder::Mapstructure)
    }

    fn decode(value: &Value, decoder: Decoder) -> Result<Self, InvalidProcessingRules> {
        let entry = map("rule", value)?;
        Ok(Self {
            // The Agent's `Type`, `Name`, and `Pattern` fields have no tag, so the exact keys are capitalized.
            rule_type: string("type", field(entry, "Type"), decoder)?,
            name: string("name", field(entry, "Name"), decoder)?,
            pattern: string("pattern", field(entry, "Pattern"), decoder)?,
            replace_placeholder: string("replace_placeholder", field(entry, "replace_placeholder"), decoder)?,
            matching: list("matching", field(entry, "matching"), decoder)?
                .into_iter()
                .enumerate()
                .map(|(index, value)| SourceMatch::decode(&format!("matching[{index}]"), value, decoder))
                .collect::<Result<_, _>>()?,
        })
    }

    fn validate(&self) -> Result<ProcessingRule, InvalidProcessingRules> {
        let invalid = |message: String| Err(InvalidProcessingRules(message));
        let Self {
            rule_type,
            name,
            pattern,
            replace_placeholder,
            matching,
        } = self.clone();

        if name.is_empty() {
            return invalid("all processing rules must have a name".to_string());
        }
        if matches!(
            rule_type.as_str(),
            "exclude_at_match" | "include_at_match" | "mask_sequences" | "multi_line"
        ) && pattern.is_empty()
        {
            return invalid(format!("no pattern provided for processing rule: {name}"));
        }
        Ok(match rule_type.as_str() {
            "exclude_at_match" => ProcessingRule::ExcludeAtMatch { name, pattern },
            "include_at_match" => ProcessingRule::IncludeAtMatch { name, pattern },
            "mask_sequences" => ProcessingRule::MaskSequences {
                name,
                pattern,
                replace_placeholder,
            },
            "multi_line" => ProcessingRule::MultiLine { name, pattern },
            "exclude_truncated" => ProcessingRule::ExcludeTruncated { name },
            "remap_source" => {
                if matching.is_empty() {
                    return invalid(format!("no matching entries provided for processing rule: {name}"));
                }
                for (index, entry) in matching.iter().enumerate() {
                    for (field, value) in [
                        ("attribute", &entry.attribute),
                        ("value", &entry.value),
                        ("new_source", &entry.new_source),
                    ] {
                        if value.is_empty() {
                            return invalid(format!("match {index} has empty {field} in processing rule: {name}"));
                        }
                    }
                }
                ProcessingRule::RemapSource { name, matching }
            }
            "" => return invalid(format!("type must be set for processing rule `{name}`")),
            other => return invalid(format!("type {other} is not supported for processing rule `{name}`")),
        })
    }
}

/// A validated logs processing rule.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessingRule {
    /// Drops logs that match `pattern`.
    ExcludeAtMatch {
        /// Name of the rule.
        name: String,
        /// Regular expression matched against the content of each log.
        pattern: String,
    },

    /// Drops logs that don't match `pattern`.
    IncludeAtMatch {
        /// Name of the rule.
        name: String,
        /// Regular expression matched against the content of each log.
        pattern: String,
    },

    /// Replaces each match of `pattern` with `replace_placeholder`.
    MaskSequences {
        /// Name of the rule.
        name: String,
        /// Regular expression matched against the content of each log.
        pattern: String,
        /// Replacement for each match, where `$1` and `${name}` refer to capture groups.
        replace_placeholder: String,
    },

    /// Drops truncated logs.
    ExcludeTruncated {
        /// Name of the rule.
        name: String,
    },

    /// Changes the source of structured logs whose attribute has a given value.
    RemapSource {
        /// Name of the rule.
        name: String,
        /// Attribute values to match, and the source each one maps to.
        matching: Vec<SourceMatch>,
    },

    /// Aggregates multi-line logs.
    MultiLine {
        /// Name of the rule.
        name: String,
        /// Regular expression matching the start of a log.
        pattern: String,
    },
}

/// An attribute value that a `remap_source` rule maps to a new source.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SourceMatch {
    /// Path of the attribute.
    pub attribute: String,
    /// Value the attribute must have.
    pub value: String,
    /// Source given to a log that matches.
    pub new_source: String,
}

impl SourceMatch {
    fn decode(path: &str, value: &Value, decoder: Decoder) -> Result<Self, InvalidProcessingRules> {
        let entry = map(path, value)?;
        Ok(Self {
            attribute: string(&format!("{path}.attribute"), field(entry, "attribute"), decoder)?,
            value: string(&format!("{path}.value"), field(entry, "value"), decoder)?,
            new_source: string(&format!("{path}.new_source"), field(entry, "new_source"), decoder)?,
        })
    }
}

/// Reports why the logs processing rules are invalid, with the Agent's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidProcessingRules(String);

impl fmt::Display for InvalidProcessingRules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidProcessingRules {}

impl From<DecodeError> for InvalidProcessingRules {
    fn from(error: DecodeError) -> Self {
        Self(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(rule_type: &str, name: &str, pattern: &str) -> ProcessingRuleEntry {
        ProcessingRuleEntry {
            rule_type: rule_type.to_string(),
            name: name.to_string(),
            pattern: pattern.to_string(),
            ..Default::default()
        }
    }

    fn source_match(attribute: &str, value: &str, new_source: &str) -> SourceMatch {
        SourceMatch {
            attribute: attribute.to_string(),
            value: value.to_string(),
            new_source: new_source.to_string(),
        }
    }

    #[test]
    fn valid_rules_keep_their_order_and_fields() {
        let domain = Domain {
            processing_rules: ProcessingRules::Entries(vec![
                entry("exclude_at_match", "exclude", "^debug"),
                entry("include_at_match", "include", "keep"),
                ProcessingRuleEntry {
                    replace_placeholder: "${1}x".to_string(),
                    ..entry("mask_sequences", "mask", "(id=)[0-9]+")
                },
                entry("exclude_truncated", "truncated", ""),
                ProcessingRuleEntry {
                    matching: vec![source_match("vendor", "acme", "acme_logs")],
                    ..entry("remap_source", "remap", "")
                },
                entry("multi_line", "multi_line", "[0-9]{4}-"),
            ]),
        };

        assert_eq!(
            domain.validate_processing_rules(),
            Ok(vec![
                ProcessingRule::ExcludeAtMatch {
                    name: "exclude".into(),
                    pattern: "^debug".into(),
                },
                ProcessingRule::IncludeAtMatch {
                    name: "include".into(),
                    pattern: "keep".into(),
                },
                ProcessingRule::MaskSequences {
                    name: "mask".into(),
                    pattern: "(id=)[0-9]+".into(),
                    replace_placeholder: "${1}x".into(),
                },
                ProcessingRule::ExcludeTruncated {
                    name: "truncated".into(),
                },
                ProcessingRule::RemapSource {
                    name: "remap".into(),
                    matching: vec![source_match("vendor", "acme", "acme_logs")],
                },
                ProcessingRule::MultiLine {
                    name: "multi_line".into(),
                    pattern: "[0-9]{4}-".into(),
                },
            ])
        );
    }

    #[test]
    fn invalid_rules_fail_with_the_agents_message() {
        // The messages are those of the Agent's `ValidateProcessingRules`.
        let remap = |matching| ProcessingRuleEntry {
            matching,
            ..entry("remap_source", "remap", "")
        };
        let cases = [
            (
                entry("exclude_at_match", "", "a"),
                "all processing rules must have a name",
            ),
            (
                entry("", "untyped", "a"),
                "type must be set for processing rule `untyped`",
            ),
            (
                entry("drop_everything", "unknown", ""),
                "type drop_everything is not supported for processing rule `unknown`",
            ),
            (
                entry("include_at_match", "no_pattern", ""),
                "no pattern provided for processing rule: no_pattern",
            ),
            (
                entry("multi_line", "no_pattern", ""),
                "no pattern provided for processing rule: no_pattern",
            ),
            (remap(vec![]), "no matching entries provided for processing rule: remap"),
            (
                remap(vec![source_match("", "v", "s")]),
                "match 0 has empty attribute in processing rule: remap",
            ),
            (
                remap(vec![source_match("a", "", "s")]),
                "match 0 has empty value in processing rule: remap",
            ),
            (
                remap(vec![source_match("a", "v", "s"), source_match("a", "v", "")]),
                "match 1 has empty new_source in processing rule: remap",
            ),
        ];

        for (rule, expected) in cases {
            // A valid rule before the invalid one doesn't change the result.
            let domain = Domain {
                processing_rules: ProcessingRules::Entries(vec![entry("exclude_truncated", "valid", ""), rule.clone()]),
            };
            assert_eq!(
                domain.validate_processing_rules().map_err(|error| error.to_string()),
                Err(expected.to_string()),
                "{rule:?}"
            );
        }
    }

    #[test]
    fn the_first_invalid_rule_is_reported() {
        let domain = Domain {
            processing_rules: ProcessingRules::Entries(vec![
                entry("exclude_at_match", "first", ""),
                entry("", "second", ""),
            ]),
        };
        assert_eq!(
            domain.validate_processing_rules().map_err(|error| error.to_string()),
            Err("no pattern provided for processing rule: first".to_string())
        );
    }

    #[test]
    fn a_json_string_is_decoded_and_validated() {
        let json = |rules: &str| Domain {
            processing_rules: ProcessingRules::Json(rules.to_string()),
        };

        assert_eq!(
            json(r#"[{"type": "exclude_at_match", "name": "exclude", "pattern": "^debug", "matching": null}]"#)
                .validate_processing_rules(),
            Ok(vec![ProcessingRule::ExcludeAtMatch {
                name: "exclude".into(),
                pattern: "^debug".into(),
            }])
        );
        assert_eq!(
            json(r#"[{"type": "exclude_at_match", "name": "no_pattern"}]"#)
                .validate_processing_rules()
                .map_err(|error| error.to_string()),
            Err("no pattern provided for processing rule: no_pattern".to_string())
        );
    }

    #[test]
    fn field_names_match_ignoring_case() {
        let rules = r#"[{
            "TYPE": "remap_source",
            "Name": "remap",
            "Unknown": 7,
            "Matching": [{"Attribute": "vendor", "VALUE": "acme", "New_Source": "acme_logs"}]
        }]"#;
        let domain = Domain {
            processing_rules: ProcessingRules::Json(rules.to_string()),
        };
        assert_eq!(
            domain.validate_processing_rules(),
            Ok(vec![ProcessingRule::RemapSource {
                name: "remap".into(),
                matching: vec![source_match("vendor", "acme", "acme_logs")],
            }])
        );
    }

    #[test]
    fn the_exact_field_name_wins_over_other_spellings() {
        // The Agent's `Name` field has no tag, so `Name` is its exact name. `replace_placeholder` is tagged.
        let rule = serde_json::json!({
            "name": "other",
            "Name": "exact",
            "NAME": "other",
            "replace_placeholder": "exact",
            "Replace_Placeholder": "other",
        });
        let entry = ProcessingRuleEntry::from_sequence_value(&rule).unwrap();
        assert_eq!(
            (entry.name.as_str(), entry.replace_placeholder.as_str()),
            ("exact", "exact")
        );
    }

    #[test]
    fn a_sequence_converts_scalars_to_strings() {
        // Like `mapstructure` with weakly typed input.
        let rule = serde_json::json!({
            "type": true,
            "name": 123,
            "pattern": 1.5,
            "replace_placeholder": 1e21,
            "matching": { "attribute": false, "value": -7, "new_source": null },
        });
        assert_eq!(
            ProcessingRuleEntry::from_sequence_value(&rule),
            Ok(ProcessingRuleEntry {
                rule_type: "1".into(),
                name: "123".into(),
                pattern: "1.5".into(),
                replace_placeholder: "1000000000000000000000".into(),
                matching: vec![source_match("0", "-7", "")],
            })
        );

        let rule = serde_json::json!({ "name": "x", "matching": {} });
        assert_eq!(
            ProcessingRuleEntry::from_sequence_value(&rule).map(|entry| entry.matching),
            Ok(vec![])
        );
    }

    #[test]
    fn a_sequence_value_that_does_not_convert_is_invalid() {
        for (rule, expected) in [
            (serde_json::json!("rule"), "`rule` expected a map, got a string"),
            (
                serde_json::json!({ "name": ["a"] }),
                "`name` expected a string, got a list",
            ),
            (
                serde_json::json!({ "pattern": {} }),
                "`pattern` expected a string, got a map",
            ),
            (
                serde_json::json!({ "matching": "a,b" }),
                "`matching[0]` expected a map, got a string",
            ),
            (
                serde_json::json!({ "matching": [{ "value": ["a"] }] }),
                "`matching[0].value` expected a string, got a list",
            ),
        ] {
            assert_eq!(
                ProcessingRuleEntry::from_sequence_value(&rule).map_err(|error| error.to_string()),
                Err(expected.to_string()),
                "{rule}"
            );
        }
    }

    #[test]
    fn a_json_string_does_not_convert_values() {
        // Like `encoding/json`.
        for (rules, expected) in [
            (
                r#"[{"name": 7}]"#,
                "invalid JSON: processing rule 0: `name` expected a string, got a number",
            ),
            (
                r#"[{"name": "x"}, {"name": "y", "matching": {"attribute": "a"}}]"#,
                "invalid JSON: processing rule 1: `matching` expected a list, got a map",
            ),
            (r#"{"name": "x"}"#, "invalid JSON: `rules` expected a list, got a map"),
        ] {
            let domain = Domain {
                processing_rules: ProcessingRules::Json(rules.to_string()),
            };
            assert_eq!(
                domain.validate_processing_rules().map_err(|error| error.to_string()),
                Err(expected.to_string()),
                "{rules}"
            );
        }

        let domain = Domain {
            processing_rules: ProcessingRules::Json("null".to_string()),
        };
        assert_eq!(domain.validate_processing_rules(), Ok(vec![]));
    }

    #[test]
    fn an_empty_string_is_no_rules() {
        let domain = Domain {
            processing_rules: ProcessingRules::Json(String::new()),
        };
        assert_eq!(domain.validate_processing_rules(), Ok(vec![]));
    }

    #[test]
    fn a_string_that_is_not_a_json_array_of_rules_is_invalid() {
        for rules in [
            "- type: exclude_at_match\n  name: drop_app_debug_info\n  pattern: DEBUG",
            r#"{"type": "exclude_at_match", "name": "x", "pattern": "a"}"#,
            r#"[{"type": "exclude_at_match", "name": 7, "pattern": "a"}]"#,
        ] {
            let domain = Domain {
                processing_rules: ProcessingRules::Json(rules.to_string()),
            };
            let error = domain.validate_processing_rules().expect_err(rules).to_string();
            assert!(error.starts_with("invalid JSON: "), "{rules:?}: {error}");
        }
    }

    #[test]
    fn rules_serialize_as_configured() {
        let entries = ProcessingRules::Entries(vec![entry("exclude_at_match", "exclude", "^debug")]);
        assert_eq!(
            serde_json::to_value(&entries).unwrap(),
            serde_json::json!([{
                "type": "exclude_at_match",
                "name": "exclude",
                "pattern": "^debug",
                "replace_placeholder": "",
                "matching": [],
            }])
        );

        let json = ProcessingRules::Json("- type: exclude_at_match".to_string());
        assert_eq!(
            serde_json::to_value(&json).unwrap(),
            serde_json::json!("- type: exclude_at_match")
        );
    }

    #[test]
    fn patterns_are_not_compiled() {
        let domain = Domain {
            processing_rules: ProcessingRules::Entries(vec![entry("mask_sequences", "unclosed", "(")]),
        };
        assert!(domain.validate_processing_rules().is_ok());
    }
}
