//! Serde deserialization for string-list schema fields (`type: array, items: string`).
//!
//! A string list reaches the configuration system in one of two shapes. A config file or the
//! remote Agent stream carries a real sequence, while an environment variable carries one
//! space-separated string (for example, `DD_DOGSTATSD_TAGS="env:prod team:core"`). A single field
//! must accept both forms.
//!
//! Map values containing string lists have a similar compatibility shape: a single value can arrive
//! as a scalar string, while multiple values arrive as a sequence. Deserializing here keeps those
//! differences at the boundary, while downstream consumers always receive a `Vec<String>`.
//!
//! Map-typed settings may also arrive as a JSON-encoded string, because the Agent streams a map-typed
//! environment variable as the raw string it stores.

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, DeserializeOwned, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;

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

/// Deserialize string-list map values from either scalar strings or sequences.
///
/// Scalar values are normalized into one-element vectors. Unlike standalone string-list fields,
/// scalar map values are not split on whitespace because each scalar represents one complete value.
/// The map may also arrive as a JSON-encoded string (see [`deserialize_map_or_json_string`]).
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

    let values = deserialize_map_or_json_string::<_, ScalarOrSeq>(deserializer)?;
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

/// Deserializes a map, or a string holding a JSON-encoded map (`cast.ToStringMap*E`).
///
/// The Agent stores a map-typed environment variable as its raw string and decodes the JSON when the
/// key is read, so its configuration stream carries the string.
///
/// # Errors
///
/// Returns an error when the value is neither a map nor a string that decodes to one, or when a map
/// value does not deserialize as `V`.
pub(crate) fn deserialize_map_or_json_string<'de, D, V>(deserializer: D) -> Result<HashMap<String, V>, D::Error>
where
    D: Deserializer<'de>,
    V: DeserializeOwned,
{
    struct MapOrJsonString<V>(PhantomData<V>);

    impl<'de, V: DeserializeOwned> Visitor<'de> for MapOrJsonString<V> {
        type Value = HashMap<String, V>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a map or a JSON-encoded map string")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            serde_json::from_str(value).map_err(|e| E::custom(format_args!("invalid JSON-encoded map: {e}")))
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            HashMap::deserialize(de::value::MapAccessDeserializer::new(map))
        }
    }

    deserializer.deserialize_any(MapOrJsonString(PhantomData))
}

/// Deserializes a free-form JSON object, or a string holding one (`cast.ToStringMapE`).
///
/// # Errors
///
/// Returns an error when the value is neither a map nor a string that decodes to one (see
/// [`deserialize_map_or_json_string`]).
pub(crate) fn deserialize_json_object_or_string<'de, D>(
    deserializer: D,
) -> Result<serde_json::Map<String, serde_json::Value>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_map_or_json_string::<_, serde_json::Value>(deserializer).map(|values| values.into_iter().collect())
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

    #[test]
    fn string_map_accepts_a_json_encoded_string() {
        let parsed = parse_map(r#"{"map":"{\"one\":\"api-key\",\"many\":[\"first\",\"second\"]}"}"#);
        assert_eq!(parsed["one"], ["api-key"]);
        assert_eq!(parsed["many"], ["first", "second"]);
    }

    #[test]
    fn string_map_rejects_a_string_that_is_not_a_json_map() {
        for encoded in [r#""not json""#, r#""[\"api-key\"]""#, r#""{\"endpoint\":5}""#] {
            let json = format!(r#"{{"map":{encoded}}}"#);
            assert!(serde_json::from_str::<MapHolder>(&json).is_err(), "{json}");
        }
    }

    #[derive(serde::Deserialize)]
    struct JsonObjectHolder {
        #[serde(deserialize_with = "deserialize_json_object_or_string")]
        object: serde_json::Map<String, serde_json::Value>,
    }

    #[test]
    fn json_object_accepts_a_map_or_a_json_encoded_string() {
        for json in [
            r#"{"object":{"https://app.datadoghq.com":true}}"#,
            r#"{"object":"{\"https://app.datadoghq.com\":true}"}"#,
        ] {
            let object = serde_json::from_str::<JsonObjectHolder>(json)
                .unwrap_or_else(|e| panic!("{json}: {e}"))
                .object;
            assert_eq!(object["https://app.datadoghq.com"], serde_json::json!(true), "{json}");
        }
        for json in [
            r#"{"object":"not json"}"#,
            r#"{"object":"[\"a\"]"}"#,
            r#"{"object":["a"]}"#,
        ] {
            assert!(serde_json::from_str::<JsonObjectHolder>(json).is_err(), "{json}");
        }
    }
}
