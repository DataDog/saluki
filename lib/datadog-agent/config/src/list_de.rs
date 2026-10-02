//! Serde deserialization for list schema fields with multiple source shapes.
//!
//! String lists can arrive as sequences or as strings holding a JSON list or space-separated values.
//! Map values containing string lists can likewise arrive as scalars or sequences. Free-form object
//! arrays can arrive as sequences or JSON-encoded strings. These adapters normalize each form at the
//! boundary.

use std::collections::HashMap;
use std::fmt;

use serde::de::{self, DeserializeOwned, Deserializer, SeqAccess, Visitor};
use serde::Deserialize;

/// Deserialize a `Vec<String>` from either a sequence or a string.
///
/// A string is read as the Agent casts it to a `[]string` (see [`crate::cast_de::parse_string_slice`]):
/// a JSON list of strings, or else split on whitespace. A sequence is taken element by element; a
/// null is an empty list. Any other JSON shape is a type error.
pub(crate) fn deserialize_space_separated_or_seq<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct SpaceSeparatedOrSeq;

    impl<'de> Visitor<'de> for SpaceSeparatedOrSeq {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a sequence, a JSON list string, or a space-separated string")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Vec<String>, E> {
            Ok(crate::cast_de::parse_string_slice(v))
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
            let mut values = Vec::new();
            while let Some(v) = seq.next_element()? {
                values.push(v);
            }
            Ok(values)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Vec<String>, E> {
            Ok(Vec::new())
        }

        fn visit_none<E: de::Error>(self) -> Result<Vec<String>, E> {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(SpaceSeparatedOrSeq)
}

/// Deserialize a JSON array from either a sequence or a JSON-encoded string.
///
/// The element type is inferred from the field; an element that does not fit it is a type error,
/// so the schema's item shape is enforced at the boundary. A null array is an empty array, and a
/// null element is the element type's default: the Agent reads a null in a `[]map[string]string`
/// as a nil (empty) map.
pub(crate) fn deserialize_json_array_or_string<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    T: DeserializeOwned + Default,
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum JsonArrayOrString<T> {
        Array(Vec<Option<T>>),
        String(String),
    }

    let values = match Option::<JsonArrayOrString<T>>::deserialize(deserializer)? {
        None => return Ok(Vec::new()),
        Some(JsonArrayOrString::Array(values)) => values,
        Some(JsonArrayOrString::String(value)) => {
            serde_json::from_str::<Vec<Option<T>>>(&value).map_err(de::Error::custom)?
        }
    };
    Ok(values.into_iter().map(Option::unwrap_or_default).collect())
}

/// Deserialize string-list map values from either scalar strings or sequences.
///
/// Scalar values are normalized into one-element vectors. Unlike standalone string-list fields,
/// scalar map values are not split on whitespace because each scalar represents one complete value.
/// The map may also arrive as a JSON-encoded string (see
/// [`deserialize_map_or_json_string`](crate::cast_de::deserialize_map_or_json_string)).
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

    let values = crate::cast_de::deserialize_map_or_json_string::<_, ScalarOrSeq>(deserializer)?;
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
    use serde_json::Value;

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
    fn json_list_string_is_decoded() {
        // A quoted YAML value or a stream string holding a JSON list reads as that list, not as one
        // whitespace-free element.
        assert_eq!(parse(r#"{"list": "[\"cr-a\",\"cr-b\"]"}"#), vec!["cr-a", "cr-b"]);
        assert_eq!(parse(r#"{"list": "[\"a b\"]"}"#), vec!["a b"]);
    }

    #[test]
    fn whitespace_runs_and_padding_are_ignored() {
        assert_eq!(parse(r#"{"list": "  a   b  "}"#), vec!["a", "b"]);
        assert_eq!(parse(r#"{"list": ""}"#), Vec::<String>::new());
    }

    #[test]
    fn null_is_an_empty_list() {
        assert_eq!(parse(r#"{"list": null}"#), Vec::<String>::new());
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
    fn string_map_reads_null_as_empty() {
        assert!(parse_map(r#"{"map":null}"#).is_empty());
        assert!(parse_map(r#"{"map":{}}"#).is_empty());
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
    fn json_array_reads_null_as_empty() {
        let holder: JsonArrayHolder = serde_json::from_str(r#"{"values":null}"#).unwrap();
        assert!(holder.values.is_empty());
    }

    #[test]
    fn json_array_keeps_null_free_form_elements() {
        let holder: JsonArrayHolder = serde_json::from_str(r#"{"values":[null,{"name":"one"}]}"#).unwrap();
        assert_eq!(holder.values, [Value::Null, serde_json::json!({ "name": "one" })]);
    }

    #[derive(serde::Deserialize)]
    struct StringMapArrayHolder {
        #[serde(deserialize_with = "deserialize_json_array_or_string")]
        values: Vec<HashMap<String, String>>,
    }

    #[test]
    fn string_map_array_reads_null_elements_as_empty_maps() {
        // The Agent reads a null in a `[]map[string]string` as a nil map.
        for json in [
            r#"{"values":[null,{"name":"one"}]}"#,
            r#"{"values":"[null,{\"name\":\"one\"}]"}"#,
        ] {
            let holder: StringMapArrayHolder = serde_json::from_str(json).unwrap();
            assert_eq!(holder.values.len(), 2, "{json}");
            assert!(holder.values[0].is_empty(), "{json}");
            assert_eq!(holder.values[1]["name"], "one", "{json}");
        }
    }

    #[test]
    fn json_array_rejects_a_non_array_encoded_string() {
        assert!(serde_json::from_str::<JsonArrayHolder>(r#"{"values":"{\"name\":\"one\"}"}"#).is_err());
    }
}
