//! Reading IR documents from JSON text.

use std::fmt;

use serde::{
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
    Deserializer,
};
use serde_json::{Map, Number, Value};

/// Parses JSON text into a value, rejecting any object that repeats a member name.
///
/// Plans and artifacts are identified by the hash of their canonical form, which [RFC 8785] defines only for I-JSON
/// documents: those whose objects never repeat a member name. Parsing straight into a [`Value`] would silently keep the
/// last of any repeated member, so a reviewer's tool that keeps the first one, or rejects the document, would see a
/// different document from the one this crate hashes and runs. Rejecting repeated names makes the document mean one
/// thing to every reader.
///
/// [RFC 8785]: https://www.rfc-editor.org/rfc/rfc8785
///
/// # Errors
///
/// Returns an error if the text is not JSON, or if an object, at any depth, repeats a member name.
pub(crate) fn parse_document(json: &str) -> Result<Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let value = UniqueMembers.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

/// Deserializes any JSON value, rejecting repeated member names in its objects.
#[derive(Clone, Copy)]
struct UniqueMembers;

impl<'de> DeserializeSeed<'de> for UniqueMembers {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for UniqueMembers {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Value, E> {
        // JSON text has no infinities or NaNs, so every parsed float has a number form.
        Ok(Number::from_f64(value).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(UniqueMembers)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut members = Map::new();
        while let Some(name) = map.next_key::<String>()? {
            if members.contains_key(&name) {
                return Err(de::Error::custom(format_args!(
                    "the member `{}` appears more than once in one object; write each member once",
                    name
                )));
            }
            let value = map.next_value_seed(UniqueMembers)?;
            members.insert(name, value);
        }
        Ok(Value::Object(members))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn documents_parse_as_serde_json_parses_them() {
        let text = r#"{"b": [1, -2, 1.5, true, null, "x", {"c": {}}], "a": "€"}"#;
        assert_eq!(
            parse_document(text).unwrap(),
            serde_json::from_str::<Value>(text).unwrap()
        );
        assert_eq!(parse_document("[]").unwrap(), json!([]));
        assert_eq!(parse_document("18446744073709551615").unwrap(), json!(u64::MAX));
    }

    #[test]
    fn repeated_member_names_are_rejected_at_any_depth() {
        for text in [
            r#"{"nodes": [], "nodes": ["d"]}"#,
            r#"{"params": {"predicate": {"const": true}, "predicate": {"const": false}}}"#,
            r#"[{"a": 1}, {"x": {"y": 1, "y": 1}}]"#,
        ] {
            let error = parse_document(text).unwrap_err().to_string();
            assert!(
                error.contains("appears more than once in one object"),
                "{}: {}",
                text,
                error
            );
        }
        // The same name in different objects is fine.
        assert!(parse_document(r#"{"a": {"a": 1}, "b": {"a": 2}}"#).is_ok());
    }

    #[test]
    fn malformed_text_is_rejected() {
        for text in ["", "{", r#"{"a": 1} x"#, r#"{"a" 1}"#] {
            assert!(parse_document(text).is_err(), "{:?}", text);
        }
    }
}
