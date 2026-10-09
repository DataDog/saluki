//! Canonical JSON and content hashes.
//!
//! Plans and artifacts are identified by a hash over their canonical JSON form, as defined by
//! [RFC 8785](https://www.rfc-editor.org/rfc/rfc8785) (the JSON Canonicalization Scheme): no insignificant whitespace,
//! object members sorted by the UTF-16 code units of their names, strings with minimal escaping, and numbers in
//! ECMAScript format.
//!
//! Plans only ever contain integers, written without a fraction or exponent, and only integers that every JSON
//! implementation represents exactly: those of magnitude at most 2^53 - 1 ([`MAX_SAFE_INTEGER`]). Decimal values are
//! encoded as strings instead. Canonicalization rejects any other number rather than formatting it, so that two
//! implementations can never disagree about a plan's hash because of how they parse or format floating-point numbers.
//! Within that profile, the ECMAScript number format that RFC 8785 requires is the plain decimal integer.

use std::{fmt, str::FromStr};

use schemars::{gen::SchemaGenerator, schema::Schema, JsonSchema};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Number, Value};
use snafu::{ResultExt as _, Snafu};

/// The largest integer magnitude a plan may contain: 2^53 - 1.
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// The member that marks a JSON object in a node's parameters as a configuration binding, as in
/// `{"config": "<binding>"}`.
///
/// No other object in a parameter value may have a member with this name: maps and records reject it. That
/// reservation is what lets [`strip_resolved_values`] find bindings in a parameter value without knowing its type.
/// Bindings occur only in node parameters, so the reservation covers nothing else: elsewhere in a plan, a map may have
/// a member named `config`, such as an output with that ID.
pub const BINDING_MEMBER: &str = "config";

/// The member of a configuration binding that carries its resolved value.
pub const RESOLVED_MEMBER: &str = "resolved";

/// An error producing canonical JSON.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)), visibility(pub(crate)))]
pub enum CanonicalError {
    /// An integer is too large in magnitude to be represented exactly by every JSON implementation.
    #[snafu(display(
        "The number {} is outside the allowed range of -9007199254740991 to 9007199254740991. Encode larger values, \
         such as durations, in a smaller unit or as a string.",
        number
    ))]
    NumberOutOfRange {
        /// The rejected number.
        number: Number,
    },

    /// A number is not written as an integer.
    #[snafu(display(
        "The number {} is not written as an integer. Plans contain only integers, written without a fraction or \
         exponent, and encode decimal values as strings, such as \"0.25\".",
        number
    ))]
    NonIntegerNumber {
        /// The rejected number.
        number: Number,
    },

    /// A value could not be converted to JSON.
    #[snafu(display("Failed to convert the value to JSON: {}", source))]
    ToJson {
        /// The underlying error.
        source: serde_json::Error,
    },
}

/// Returns the canonical JSON form of `value`.
///
/// # Errors
///
/// Returns an error if `value` contains a number that is not an integer of magnitude at most [`MAX_SAFE_INTEGER`],
/// including a number written with a fraction or exponent, such as `1.0` or `1e3`.
pub fn to_canonical_string(value: &Value) -> Result<String, CanonicalError> {
    let mut out = String::new();
    write_value(value, &mut out)?;
    Ok(out)
}

fn write_value(value: &Value, out: &mut String) -> Result<(), CanonicalError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => write_number(number, out)?,
        Value::String(string) => write_string(string, out),
        Value::Array(values) => {
            out.push('[');
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(value, out)?;
            }
            out.push(']');
        }
        Value::Object(members) => write_object(members, out)?,
    }
    Ok(())
}

fn write_object(members: &Map<String, Value>, out: &mut String) -> Result<(), CanonicalError> {
    // RFC 8785 sorts member names by their UTF-16 code units, which differs from sorting by UTF-8 bytes (or by code
    // points) for names with characters outside the Basic Multilingual Plane. Sort explicitly rather than relying on
    // the map's own order.
    let mut sorted = members.iter().collect::<Vec<_>>();
    sorted.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));

    out.push('{');
    for (i, (name, value)) in sorted.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_string(name, out);
        out.push(':');
        write_value(value, out)?;
    }
    out.push('}');
    Ok(())
}

fn write_number(number: &Number, out: &mut String) -> Result<(), CanonicalError> {
    let integer = safe_integer(number)?;
    out.push_str(&integer.to_string());
    Ok(())
}

/// Returns `number` as an integer, if it is an integer of magnitude at most [`MAX_SAFE_INTEGER`].
///
/// # Errors
///
/// Returns an error if `number` is out of range, or is a floating-point number. A JSON number written with a fraction
/// or exponent is parsed as a floating-point number, even when its value is integral, and so is `-0`.
pub fn safe_integer(number: &Number) -> Result<i64, CanonicalError> {
    match number.as_i64() {
        Some(integer) if integer.unsigned_abs() <= MAX_SAFE_INTEGER as u64 => Ok(integer),
        Some(_) => NumberOutOfRange { number: number.clone() }.fail(),
        // Above `i64::MAX`, which is always out of range.
        None if number.is_u64() => NumberOutOfRange { number: number.clone() }.fail(),
        None => NonIntegerNumber { number: number.clone() }.fail(),
    }
}

fn write_string(string: &str, out: &mut String) {
    out.push('"');
    for c in string.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\u{20}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Removes the resolved value from every configuration binding in `value`, which is one parameter's value.
///
/// A configuration binding is an object with a [`BINDING_MEMBER`] member; its resolved value, if any, is its
/// [`RESOLVED_MEMBER`] member. Lowering fills in resolved values, but exports and artifacts never carry them, and
/// hashes that must not change when only configuration values change are computed without them.
///
/// Apply this to parameter values only, where [`BINDING_MEMBER`] is reserved; [`Plan::hash`](crate::ir::Plan::hash)
/// applies it to the parameters of every node.
pub fn strip_resolved_values(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(strip_resolved_values),
        Value::Object(members) => {
            if members.contains_key(BINDING_MEMBER) {
                members.remove(RESOLVED_MEMBER);
            }
            members.values_mut().for_each(strip_resolved_values);
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// Returns `true` if any configuration binding in `value`, which is one parameter's value, carries a resolved value.
pub fn contains_resolved_values(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(contains_resolved_values),
        Value::Object(members) => {
            (members.contains_key(BINDING_MEMBER) && members.contains_key(RESOLVED_MEMBER))
                || members.values().any(contains_resolved_values)
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

/// Whether a hash covers the resolved values of configuration bindings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolvedValues {
    /// Resolved values are part of the hash, so it changes whenever a bound configuration value changes.
    Include,

    /// Resolved values are removed before hashing, so the hash changes only when the plan's structure, operators,
    /// literal parameters, or binding names change.
    Exclude,
}

const HASH_PREFIX: &str = "sha256:";
const HASH_PATTERN: &str = "^sha256:[0-9a-f]{64}$";

/// A SHA-256 digest, serialized as `sha256:<64 lowercase hexadecimal digits>`.
///
/// Plan, artifact, and binding hashes are digests of canonical JSON.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlanHash([u8; 32]);

impl PlanHash {
    /// Returns the digest of `bytes`.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(saluki_tls::sha256(bytes))
    }

    /// Returns the digest of the canonical JSON form of `value`.
    ///
    /// # Errors
    ///
    /// Returns an error if `value` has no canonical form; see [`to_canonical_string`].
    pub fn of_value(value: &Value) -> Result<Self, CanonicalError> {
        Ok(Self::of_bytes(to_canonical_string(value)?.as_bytes()))
    }

    /// Returns the digest of the canonical JSON form of `value`, as serialized.
    ///
    /// # Errors
    ///
    /// Returns an error if `value` cannot be converted to JSON, or has no canonical form; see [`to_canonical_string`].
    pub fn of_serializable<T>(value: &T) -> Result<Self, CanonicalError>
    where
        T: Serialize + ?Sized,
    {
        Self::of_value(&serde_json::to_value(value).context(ToJson)?)
    }

    /// Creates a hash from a raw digest.
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Returns the raw digest.
    pub const fn digest(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for PlanHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(HASH_PREFIX)?;
        for byte in &self.0 {
            write!(f, "{:02x}", byte)?;
        }
        Ok(())
    }
}

impl fmt::Debug for PlanHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PlanHash({})", self)
    }
}

/// An error parsing a [`PlanHash`].
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(display(
    "Invalid hash `{}`: hashes are `sha256:` followed by 64 lowercase hexadecimal digits.",
    value
))]
pub struct InvalidHash {
    value: String,
}

impl FromStr for PlanHash {
    type Err = InvalidHash;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidHash {
            value: value.to_string(),
        };
        let hex = value.strip_prefix(HASH_PREFIX).ok_or_else(invalid)?.as_bytes();
        if hex.len() != 64 {
            return Err(invalid());
        }

        let nibble = |b: u8| match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        };
        let mut digest = [0; 32];
        for (i, byte) in digest.iter_mut().enumerate() {
            let high = nibble(hex[2 * i]).ok_or_else(invalid)?;
            let low = nibble(hex[2 * i + 1]).ok_or_else(invalid)?;
            *byte = (high << 4) | low;
        }
        Ok(Self(digest))
    }
}

impl Serialize for PlanHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PlanHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

impl JsonSchema for PlanHash {
    fn schema_name() -> String {
        "PlanHash".to_string()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        schemars::schema::SchemaObject {
            instance_type: Some(schemars::schema::InstanceType::String.into()),
            string: Some(Box::new(schemars::schema::StringValidation {
                pattern: Some(HASH_PATTERN.to_string()),
                ..Default::default()
            })),
            metadata: Some(Box::new(schemars::schema::Metadata {
                description: Some(
                    "A SHA-256 digest: `sha256:` followed by 64 lowercase hexadecimal digits.".to_string(),
                ),
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn canonical(json: &str) -> String {
        to_canonical_string(&serde_json::from_str(json).expect("test input is valid JSON")).expect("canonicalizes")
    }

    /// The literal and string parts of the main example in RFC 8785, section 3.2.3. The example's non-integer numbers
    /// are covered by `non_integer_numbers_are_rejected` instead, since plans never contain them.
    #[test]
    fn rfc_8785_example_strings_and_literals() {
        let input = r#"{
            "string": "\u20ac$\u000F\u000aA'\u0042\u0022\u005c\\\"\/",
            "literals": [null, true, false]
        }"#;
        let expected = "{\"literals\":[null,true,false],\"string\":\"€$\\u000f\\nA'B\\\"\\\\\\\\\\\"/\"}";
        assert_eq!(canonical(input), expected);
    }

    /// The property-sorting example in RFC 8785, section 3.2.3. Sorting by UTF-16 code units puts the emoji (a
    /// surrogate pair starting with 0xD83D) before U+FB33, the reverse of their code-point order.
    #[test]
    fn rfc_8785_sorts_member_names_by_utf16_code_units() {
        let input = r#"{
            "\u20ac": "Euro Sign",
            "\r": "Carriage Return",
            "\ufb33": "Hebrew Letter Dalet With Dagesh",
            "1": "One",
            "\ud83d\ude00": "Emoji: Grinning Face",
            "\u0080": "Control",
            "\u00f6": "Latin Small Letter O With Diaeresis"
        }"#;
        let expected = concat!(
            "{\"\\r\":\"Carriage Return\",",
            "\"1\":\"One\",",
            "\"\u{0080}\":\"Control\",",
            "\"\u{00f6}\":\"Latin Small Letter O With Diaeresis\",",
            "\"\u{20ac}\":\"Euro Sign\",",
            "\"\u{1f600}\":\"Emoji: Grinning Face\",",
            "\"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"}"
        );
        assert_eq!(canonical(input), expected);
    }

    #[test]
    fn strings_use_minimal_escaping() {
        // (input character, canonical encoding)
        let cases: &[(&str, &str)] = &[
            ("\u{0}", "\\u0000"),
            ("\u{1f}", "\\u001f"),
            ("\u{8}", "\\b"),
            ("\t", "\\t"),
            ("\n", "\\n"),
            ("\u{c}", "\\f"),
            ("\r", "\\r"),
            ("\"", "\\\""),
            ("\\", "\\\\"),
            // Everything else is literal, including the solidus, DEL, and the line and paragraph separators.
            ("/", "/"),
            ("\u{7f}", "\u{7f}"),
            ("\u{2028}", "\u{2028}"),
            ("\u{2029}", "\u{2029}"),
            ("é", "é"),
            ("🍣", "🍣"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                to_canonical_string(&Value::String(input.to_string())).unwrap(),
                format!("\"{}\"", expected),
                "input: {:?}",
                input
            );
        }
    }

    #[test]
    fn integers_are_formatted_in_ecmascript_form() {
        // (input JSON number, canonical form)
        let cases: &[(&str, &str)] = &[
            ("0", "0"),
            ("1", "1"),
            ("-1", "-1"),
            ("1000", "1000"),
            ("9007199254740991", "9007199254740991"),
            ("-9007199254740991", "-9007199254740991"),
        ];
        for (input, expected) in cases {
            assert_eq!(canonical(input), *expected, "input: {}", input);
        }
    }

    #[test]
    fn out_of_range_integers_are_rejected() {
        for input in [
            "9007199254740992",
            "-9007199254740992",
            "18446744073709551615",
            "-9223372036854775808",
        ] {
            let value: Value = serde_json::from_str(input).unwrap();
            let error = to_canonical_string(&value).unwrap_err();
            assert!(
                matches!(error, CanonicalError::NumberOutOfRange { .. }),
                "{}: {}",
                input,
                error
            );
        }
    }

    #[test]
    fn non_integer_numbers_are_rejected() {
        // The non-integer numbers from the RFC 8785 example, among others. Integral values written with a fraction or
        // exponent are rejected too: how precisely they are parsed varies between JSON implementations.
        for input in [
            "333333333.33333329",
            "4.50",
            "2e-3",
            "1E30",
            "0.000000000000000000000000001",
            "0.5",
            "-1.5",
            "1.0",
            "-0.0",
            "1e3",
            "9007199254740991.0",
            // JSON parsers read a negative zero as a floating-point number; write `0` instead.
            "-0",
        ] {
            let value: Value = serde_json::from_str(input).unwrap();
            let error = to_canonical_string(&value).unwrap_err();
            assert!(
                matches!(error, CanonicalError::NonIntegerNumber { .. }),
                "{}: {}",
                input,
                error
            );
            assert!(
                error.to_string().contains("written without a fraction or exponent"),
                "{}",
                error
            );
        }
    }

    #[test]
    fn nested_structures_have_no_whitespace() {
        let input = r#"{ "b": [ 1, { "z": true, "a": null } ], "a": { } , "c": [ ] }"#;
        assert_eq!(canonical(input), r#"{"a":{},"b":[1,{"a":null,"z":true}],"c":[]}"#);
    }

    /// A plan fragment, brought into canonical form independently of this implementation (with Python's `json.dumps`
    /// using sorted keys, no whitespace, and no ASCII escaping, which agrees with RFC 8785 for this input), and its
    /// SHA-256 digest, computed with `shasum -a 256`.
    #[test]
    fn cross_implementation_fixture() {
        let input = r#"{
            "ir_version": 1,
            "nodes": [
                {"op": "split", "id": "policy/drop_debug",
                 "params": {"predicate": {"name": {"prefix": "demo.debug."}}}},
                {"id": "dd_metrics_encode_emit", "op": "emit", "params": {"output": "dd_metrics_encode"}}
            ],
            "edges": [{"to": "policy/drop_debug.in", "from": "metrics_policy.0"}],
            "outputs": [{"id": "dd_metrics_encode", "accepts": {"timestamps": "mixed", "kinds": "all"},
                         "recipients": ["dd_out/primary"], "delivery": "native",
                         "fingerprint": "sha256:0000000000000000000000000000000000000000000000000000000000000000"}]
        }"#;
        let expected = concat!(
            r#"{"edges":[{"from":"metrics_policy.0","to":"policy/drop_debug.in"}],"ir_version":1,"#,
            r#""nodes":[{"id":"policy/drop_debug","op":"split","#,
            r#""params":{"predicate":{"name":{"prefix":"demo.debug."}}}},"#,
            r#"{"id":"dd_metrics_encode_emit","op":"emit","params":{"output":"dd_metrics_encode"}}],"#,
            r#""outputs":[{"accepts":{"kinds":"all","timestamps":"mixed"},"delivery":"native","#,
            r#""fingerprint":"sha256:0000000000000000000000000000000000000000000000000000000000000000","#,
            r#""id":"dd_metrics_encode","recipients":["dd_out/primary"]}]}"#
        );
        assert_eq!(canonical(input), expected);

        let hash = PlanHash::of_value(&serde_json::from_str(input).unwrap()).unwrap();
        assert_eq!(hash.to_string(), CROSS_IMPLEMENTATION_FIXTURE_HASH);
    }

    const CROSS_IMPLEMENTATION_FIXTURE_HASH: &str =
        "sha256:32f1e3297bda99263f865eadd86db10d29d9ca07ef16082f9d1359cfa9108dd8";

    #[test]
    fn plan_hashes_round_trip_through_strings() {
        let hash = PlanHash::of_bytes(b"abc");
        // The SHA-256 test vector for "abc" from FIPS 180-2.
        let expected = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(hash.to_string(), expected);
        assert_eq!(expected.parse::<PlanHash>().unwrap(), hash);
        assert_eq!(serde_json::to_value(hash).unwrap(), json!(expected));
        assert_eq!(serde_json::from_value::<PlanHash>(json!(expected)).unwrap(), hash);

        for invalid in [
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
            "sha256:ba7816bf",
            "sha512:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "sha256:ga7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ] {
            assert!(invalid.parse::<PlanHash>().is_err(), "{:?}", invalid);
        }
    }

    #[test]
    fn hashes_can_exclude_resolved_binding_values() {
        let bound = |resolved: Option<Value>| {
            let mut binding = json!({"config": "dogstatsd.metric_filter.values"});
            if let Some(resolved) = resolved {
                binding["resolved"] = resolved;
            }
            json!({"params": {"values": binding, "prefix": false}})
        };

        let unresolved = bound(None);
        let first = bound(Some(json!(["a"])));
        let second = bound(Some(json!(["a", "b"])));
        assert!(!contains_resolved_values(&unresolved));
        assert!(contains_resolved_values(&first));

        let hash = |value: &Value, resolved| {
            let mut value = value.clone();
            if resolved == ResolvedValues::Exclude {
                strip_resolved_values(&mut value);
            }
            PlanHash::of_value(&value).unwrap()
        };
        // Excluding resolved values, a configuration change does not change the hash.
        assert_eq!(
            hash(&first, ResolvedValues::Exclude),
            hash(&second, ResolvedValues::Exclude)
        );
        assert_eq!(
            hash(&first, ResolvedValues::Exclude),
            hash(&unresolved, ResolvedValues::Exclude)
        );
        // Including them, it does.
        assert_ne!(
            hash(&first, ResolvedValues::Include),
            hash(&second, ResolvedValues::Include)
        );

        // A `resolved` member outside a binding is ordinary data and is kept.
        let mut not_a_binding = json!({"resolved": 1, "nested": [{"config": "x", "resolved": 2}]});
        strip_resolved_values(&mut not_a_binding);
        assert_eq!(not_a_binding, json!({"resolved": 1, "nested": [{"config": "x"}]}));
    }

    mod properties {
        use proptest::prelude::*;
        use serde_json::Value;

        use super::super::*;

        /// Returns a strategy for JSON values that have a canonical form.
        fn arb_json() -> impl Strategy<Value = Value> {
            let leaf = prop_oneof![
                Just(Value::Null),
                any::<bool>().prop_map(Value::Bool),
                (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).prop_map(Value::from),
                any::<String>().prop_map(Value::String),
            ];
            leaf.prop_recursive(4, 64, 6, |inner| {
                prop_oneof![
                    proptest::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
                    proptest::collection::btree_map(any::<String>(), inner, 0..6)
                        .prop_map(|members| Value::Object(members.into_iter().collect())),
                ]
            })
        }

        proptest! {
            #[test]
            fn property_test_canonical_json_parses_back_to_the_same_value(value in arb_json()) {
                let canonical = to_canonical_string(&value).expect("generated values have a canonical form");
                let parsed: Value = serde_json::from_str(&canonical).expect("canonical JSON is JSON");
                prop_assert_eq!(&parsed, &value);
                prop_assert_eq!(to_canonical_string(&parsed).unwrap(), canonical);
            }

            #[test]
            fn property_test_canonical_member_order_is_by_utf16_code_units(
                members in proptest::collection::btree_map(any::<String>(), any::<bool>(), 0..8),
            ) {
                let value = Value::Object(members.iter().map(|(k, v)| (k.clone(), Value::Bool(*v))).collect());

                // Assemble the expected text directly: members sorted by their names' UTF-16 code units.
                let mut sorted = members.iter().collect::<Vec<_>>();
                sorted.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
                let entries = sorted
                    .into_iter()
                    .map(|(name, value)| {
                        format!("{}:{}", to_canonical_string(&Value::String(name.clone())).unwrap(), value)
                    })
                    .collect::<Vec<_>>();
                prop_assert_eq!(to_canonical_string(&value).unwrap(), format!("{{{}}}", entries.join(",")));
            }
        }
    }
}
