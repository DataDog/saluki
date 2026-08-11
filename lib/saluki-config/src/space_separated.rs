//! Serde deserializer for space-separated string lists.
//!
//! The Datadog Agent passes some list-typed configuration values as space-separated strings when
//! using environment variables (for example, `DD_PROXY_NO_PROXY="host1 host2"`), while the same keys
//! appear as YAML sequences in config files. This module provides a deserializer that accepts
//! both representations.

use std::fmt;

use serde::de::{self, Deserializer, SeqAccess, Visitor};

/// Deserializes a `Vec<String>` from either a sequence or a space-separated string.
pub fn deserialize_space_separated_or_seq<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct SpaceSeparatedOrSeq;

    impl<'de> Visitor<'de> for SpaceSeparatedOrSeq {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a sequence or a space-separated string")
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

/// Deserializes an `Option<Vec<String>>` from either a sequence or a space-separated string.
///
/// Pair with `#[serde(default)]` so that absent fields deserialize to `None`.
pub fn deserialize_opt_space_separated_or_seq<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_space_separated_or_seq(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    #[derive(Deserialize)]
    struct Holder {
        #[serde(deserialize_with = "deserialize_space_separated_or_seq")]
        values: Vec<String>,
    }

    #[test]
    fn deserializes_space_separated_strings_and_sequences() {
        // The `DD_PROXY_NO_PROXY="host1 host2"` env-var form arrives as a single space-separated string, while the same
        // key in a YAML/JSON config file arrives as a sequence; both must produce the same `Vec<String>`.
        let cases: &[(&str, serde_json::Value, Vec<&str>)] = &[
            (
                "space-separated string",
                json!("host1 host2 host3"),
                vec!["host1", "host2", "host3"],
            ),
            (
                "surrounding and repeated whitespace collapses",
                json!("  host1   host2  "),
                vec!["host1", "host2"],
            ),
            ("empty string yields an empty list", json!(""), vec![]),
            (
                "sequence form is preserved verbatim",
                json!(["host1", "host2"]),
                vec!["host1", "host2"],
            ),
        ];

        for (description, input, expected) in cases {
            let holder: Holder =
                serde_json::from_value(json!({ "values": input })).unwrap_or_else(|e| panic!("{description}: {e}"));
            assert_eq!(holder.values, *expected, "case: {description}");
        }
    }
}
