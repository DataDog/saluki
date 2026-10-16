//! YAML parsing for configuration files.

use serde::de::DeserializeOwned;

/// Deserializes a YAML document using YAML 1.1 scalar resolution.
///
/// Configuration files are commonly written against YAML 1.1 parsers, so plain scalars resolve the way those parsers
/// resolve them rather than by the stricter YAML 1.2 core schema:
///
/// - `yes`/`no`, `on`/`off`, and `y`/`n` (in any case) are booleans.
/// - Integers with a leading zero, such as `012`, are octal.
/// - `<<` merge keys are expanded.
///
/// Non-finite floats (`.inf`, `.nan`) in untyped positions are kept as their string spelling rather than rejecting
/// the whole document, leaving it to the consumer of that value to decide whether it's valid.
///
/// The default limits on the number of parsed nodes and events are lifted, as configuration files can legitimately
/// hold very large lists. Nesting depth, alias expansion, and total scalar size are still bounded.
///
/// # Errors
///
/// If the document isn't valid YAML, or doesn't deserialize as `T`, an error is returned.
pub fn from_str<T>(input: &str) -> Result<T, serde_saphyr::Error>
where
    T: DeserializeOwned,
{
    let mut budget = serde_saphyr::Budget::default();
    budget.max_nodes = usize::MAX;
    budget.max_events = usize::MAX;

    let options = serde_saphyr::options! {
        budget: Some(budget),
        legacy_octal_numbers: true,
        reject_non_finite_typeless_float: false,
    };
    serde_saphyr::from_str_with_options(input, options)
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;

    #[test]
    fn resolves_plain_scalars_with_yaml_1_1_rules() {
        let value: Value = from_str(
            "yes: yes\noff: Off\nshort: n\noctal: 012\nhex: 0x1F\nnot_octal: 08\nquoted: '012'\nkeyword: \"on\"\n",
        )
        .unwrap();

        assert_eq!(
            value,
            json!({
                "yes": true,
                "off": false,
                "short": false,
                "octal": 10,
                "hex": 31,
                "not_octal": 8.0,
                "quoted": "012",
                "keyword": "on",
            })
        );
    }

    #[test]
    fn expands_merge_keys() {
        let value: Value = from_str("base: &base {a: 1, b: 2}\nderived:\n  <<: *base\n  b: 3\n").unwrap();

        assert_eq!(value["derived"], json!({"a": 1, "b": 3}));
    }

    #[test]
    fn keeps_non_finite_floats_as_strings() {
        let value: Value = from_str("a: .inf\nb: -.inf\nc: .nan\n").unwrap();

        assert_eq!(value, json!({"a": ".inf", "b": "-.inf", "c": ".nan"}));
    }

    #[test]
    fn parses_documents_with_very_large_lists() {
        let mut input = String::from("items:\n");
        for i in 0..300_000 {
            input.push_str(&format!("  - item_{i}\n"));
        }

        let value: Value = from_str(&input).unwrap();

        assert_eq!(value["items"].as_array().unwrap().len(), 300_000);
    }

    #[test]
    fn rejects_duplicate_keys() {
        assert!(from_str::<Value>("a: 1\na: 2\n").is_err());
    }
}
