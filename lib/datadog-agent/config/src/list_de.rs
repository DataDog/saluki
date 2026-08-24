//! Serde deserialization for list schema fields with multiple source shapes.
//!
//! String lists can arrive as sequences or space-separated environment strings. Map values
//! containing string lists can likewise arrive as scalars or sequences. JSON arrays can arrive as
//! sequences or JSON-encoded strings. These adapters normalize each form at the boundary.

use std::collections::HashMap;
use std::fmt;

use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;

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

/// Deserialize a JSON array from either a sequence or a JSON-encoded string.
pub(crate) fn deserialize_json_array_or_string<'de, D>(deserializer: D) -> Result<Vec<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum JsonArrayOrString {
        Array(Vec<Value>),
        String(String),
    }

    match JsonArrayOrString::deserialize(deserializer)? {
        JsonArrayOrString::Array(values) => Ok(values),
        JsonArrayOrString::String(value) => serde_json::from_str(&value).map_err(de::Error::custom),
    }
}

/// Deserialize string-list map values from either scalar strings or sequences.
///
/// Scalar values are normalized into one-element vectors. Unlike standalone string-list fields,
/// scalar map values are not split on whitespace because each scalar represents one complete value.
pub(crate) fn deserialize_string_map_scalar_or_seq<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ScalarOrSeq {
        Scalar(String),
        Seq(Vec<String>),
    }

    let values = HashMap::<String, ScalarOrSeq>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .map(|(key, value)| {
            let value = match value {
                ScalarOrSeq::Scalar(value) => vec![value],
                ScalarOrSeq::Seq(values) => values,
            };
            (key, value)
        })
        .collect())
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

    #[derive(serde::Deserialize)]
    struct MapHolder {
        #[serde(deserialize_with = "deserialize_string_map_scalar_or_seq")]
        map: HashMap<String, Vec<String>>,
    }

    fn parse_map(json: &str) -> HashMap<String, Vec<String>> {
        serde_json::from_str::<MapHolder>(json).unwrap().map
    }

    #[test]
    fn string_map_accepts_scalar_and_sequence_values() {
        let parsed = parse_map(r#"{"map":{"one":"api-key","many":["first","second"],"none":[]}}"#);
        assert_eq!(parsed["one"], ["api-key"]);
        assert_eq!(parsed["many"], ["first", "second"]);
        assert!(parsed["none"].is_empty());
    }

    #[test]
    fn string_map_rejects_non_string_values() {
        assert!(serde_json::from_str::<MapHolder>(r#"{"map":{"endpoint":5}}"#).is_err());
    }

    #[derive(serde::Deserialize)]
    struct JsonArrayHolder {
        #[serde(deserialize_with = "deserialize_json_array_or_string")]
        values: Vec<Value>,
    }

    #[test]
    fn json_array_accepts_a_sequence_or_encoded_string() {
        let sequence: JsonArrayHolder = serde_json::from_str(r#"{"values":[{"name":"one"}]}"#).unwrap();
        let encoded: JsonArrayHolder = serde_json::from_str(r#"{"values":"[{\"name\":\"one\"}]"}"#).unwrap();

        assert_eq!(sequence.values, encoded.values);
        assert_eq!(sequence.values, [serde_json::json!({ "name": "one" })]);
    }

    #[test]
    fn json_array_rejects_a_non_array_encoded_string() {
        assert!(serde_json::from_str::<JsonArrayHolder>(r#"{"values":"{\"name\":\"one\"}"}"#).is_err());
    }
}
