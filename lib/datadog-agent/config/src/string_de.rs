//! Deserialize a schema `string` leaf that also accepts a bare non-negative integer.

use std::fmt;

use serde::de;

/// Deserialize a value documented as a string that may also arrive as a bare non-negative integer,
/// normalizing the integer to its decimal string form.
///
/// The Datadog schema types settings such as `dogstatsd_log_file_max_size` as `string` (for example
/// `"10MB"`) but documents and accepts an equivalent byte count as a bare integer (`10485760`). A
/// configuration file, the Agent config stream, or an environment value may present either shape;
/// both normalize to the string the translator later parses (for example into a byte size).
///
/// Floats, negative integers, booleans, and compound values are rejected: the schema's canonical
/// form is a string, and only a non-negative integer is a meaningful shorthand for one.
pub fn deserialize_string_or_integer<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: de::Deserializer<'de>,
{
    struct StringOrIntegerVisitor;

    impl de::Visitor<'_> for StringOrIntegerVisitor {
        type Value = String;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a string or a non-negative integer")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
            Ok(value.to_owned())
        }

        fn visit_string<E: de::Error>(self, value: String) -> Result<String, E> {
            Ok(value)
        }

        fn visit_u64<E: de::Error>(self, value: u64) -> Result<String, E> {
            Ok(value.to_string())
        }

        fn visit_i64<E: de::Error>(self, value: i64) -> Result<String, E> {
            if value < 0 {
                return Err(E::invalid_value(de::Unexpected::Signed(value), &self));
            }
            Ok(value.to_string())
        }
    }

    // `deserialize_any` lets a self-describing format (serde_json/serde_yaml values, the shapes this
    // config path uses) route to the matching visit method; unhandled shapes (floats, bools,
    // sequences, maps) fall through to the visitor's default `Err`.
    deserializer.deserialize_any(StringOrIntegerVisitor)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Deserialize)]
    struct Holder {
        #[serde(deserialize_with = "super::deserialize_string_or_integer")]
        value: String,
    }

    fn parse(value: serde_json::Value) -> Result<String, serde_json::Error> {
        serde_json::from_value::<Holder>(json!({ "value": value })).map(|h| h.value)
    }

    #[test]
    fn accepts_a_string_unchanged() {
        assert_eq!(parse(json!("10MB")).expect("string parses"), "10MB");
    }

    #[test]
    fn normalizes_a_non_negative_integer_to_a_string() {
        assert_eq!(parse(json!(10485760)).expect("integer parses"), "10485760");
        assert_eq!(parse(json!(0)).expect("zero parses"), "0");
    }

    #[test]
    fn rejects_a_negative_integer() {
        assert!(parse(json!(-1)).is_err());
    }

    #[test]
    fn rejects_a_float() {
        assert!(parse(json!(10.5)).is_err());
    }

    #[test]
    fn rejects_a_boolean() {
        assert!(parse(json!(true)).is_err());
    }

    #[test]
    fn rejects_a_compound_value() {
        assert!(parse(json!(["10MB"])).is_err());
        assert!(parse(json!({ "size": "10MB" })).is_err());
    }
}
