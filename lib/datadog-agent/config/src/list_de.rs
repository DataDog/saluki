//! Serde deserialization for string-list schema fields (`type: array, items: string`).
//!
//! A string list reaches ADP in one of two shapes depending on its source. A config file or the
//! remote Agent config stream carries it as a real sequence (a YAML list, or a protobuf `ListValue`
//! that becomes a JSON array). An environment variable carries it as a single space-separated
//! string, following the Agent's `DD_DOGSTATSD_TAGS="env:prod team:core"` convention, because
//! figment never splits an env value. A single field must accept both.
//!
//! Deserializing here keeps that duality at the deserialization boundary, so every string-list leaf
//! of `DatadogConfiguration` accepts either shape and downstream consumers always receive a
//! `Vec<String>`. This mirrors `saluki_config::deserialize_space_separated_or_seq`, which the
//! hand-written component configs use for the same reason; it is duplicated here to keep the
//! generated crate free of a dependency edge onto `saluki-config`.

use std::fmt;

use serde::de::{self, Deserializer, SeqAccess, Visitor};

/// Deserialize a `Vec<String>` from either a sequence or a space-separated string.
///
/// A string is split on whitespace (matching the Agent's space-separated env convention); a
/// sequence is taken element by element. Any other JSON shape is a type error.
pub(crate) fn deserialize_space_separated_or_seq<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct SpaceSeparatedOrSeq;

    impl<'de> Visitor<'de> for SpaceSeparatedOrSeq {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a sequence or a space-separated string")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Vec<String>, E> {
            Ok(v.split_whitespace().map(str::to_owned).collect())
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
            let mut values = Vec::new();
            while let Some(v) = seq.next_element()? {
                values.push(v);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(SpaceSeparatedOrSeq)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Holder {
        #[serde(deserialize_with = "deserialize_space_separated_or_seq")]
        list: Vec<String>,
    }

    fn parse(json: &str) -> Vec<String> {
        serde_json::from_str::<Holder>(json).unwrap().list
    }

    #[test]
    fn sequence_passes_through() {
        assert_eq!(parse(r#"{"list": ["a", "b"]}"#), vec!["a", "b"]);
        assert_eq!(parse(r#"{"list": []}"#), Vec::<String>::new());
    }

    #[test]
    fn space_separated_string_is_split() {
        assert_eq!(
            parse(r#"{"list": "env:prod team:core"}"#),
            vec!["env:prod", "team:core"]
        );
        assert_eq!(parse(r#"{"list": "solo"}"#), vec!["solo"]);
    }

    #[test]
    fn whitespace_runs_and_padding_are_ignored() {
        assert_eq!(parse(r#"{"list": "  a   b  "}"#), vec!["a", "b"]);
        assert_eq!(parse(r#"{"list": ""}"#), Vec::<String>::new());
    }

    #[test]
    fn wrong_shape_is_rejected() {
        assert!(serde_json::from_str::<Holder>(r#"{"list": 5}"#).is_err());
    }
}
