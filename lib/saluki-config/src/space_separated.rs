//! Serde deserializer for space-separated string lists.
//!
//! The Datadog Agent passes some list-typed configuration values as space-separated strings when
//! using environment variables (e.g. `DD_PROXY_NO_PROXY="host1 host2"`), while the same keys
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
