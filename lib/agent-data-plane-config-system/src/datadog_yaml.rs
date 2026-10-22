//! Reads `datadog.yaml` text into the base configuration value.
//!
//! The Datadog Agent resolves keys while it loads the file, in two ways that a plain YAML parse
//! does not reproduce:
//!
//! - Inside the value of a Datadog leaf, a map key is typed by YAML 1.1 rules and rendered with Go's
//!   `%v` (`010` is `"8"`, `yes` is `"true"`, `1e3` is `"1000"`). Quoted keys stay as written.
//! - At the root and in each Datadog section, a key is lowercased and split on `.` before it is
//!   matched against the schema. `Log_Level` and `data_plane.api_listen_address` therefore address
//!   the same settings as their canonical spellings. The walk stops at a leaf.
//!
//! A Datadog leaf is a path from `datadog_leaf_paths`, and a section is a proper prefix of one. A key
//! whose path is neither, including every Saluki-only key, keeps its spelling and its place in its
//! map. When that map is a Datadog section written in another spelling, the key moves with the
//! section: `DATA_PLANE: {standalone_mode: true}` sets `data_plane.standalone_mode`.
//!
//! # Collisions
//!
//! Spellings that resolve to the same place never fail the load. Which one the Agent keeps mostly
//! depends on Go map order, so ADP applies its own stable rules:
//!
//! - A key that is already in its canonical place wins. This is a key in its nested lowercase path,
//!   or a leaf map key that is already the text it renders to, such as `'8'`. Reading every key as
//!   written gives the same value.
//! - Otherwise the spelling written last wins, as the Agent's parser does for a repeated key.
//! - A respelled key whose value is not a map stays where it was written instead of replacing a
//!   Datadog section.
//! - A null counts as absent.
//!
//! The other spellings are dropped.

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

use datadog_agent_config::datadog_leaf_paths;
use serde::de::{self, DeserializeSeed, IntoDeserializer as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

/// Parses `text` into the base value, resolving Datadog keys the way the Agent does.
///
/// Parser options, limits, merge keys and the secret-free error text come from
/// `saluki_config::yaml::from_str`. A document with no Datadog path parses exactly as
/// `serde_json::Value` would.
///
/// # Errors
///
/// Returns the YAML error message if the text is not valid YAML, or if a Datadog leaf's map has a null
/// key.
pub(crate) fn from_str(text: &str) -> Result<Value, String> {
    let Document(node) = saluki_config::yaml::from_str(text).map_err(|e| e.to_string())?;
    Ok(match node {
        Node::Section(entries) => {
            let mut placements = Placements::default();
            placements.collect(entries, &[], &[]);
            Value::Object(placements.resolve())
        }
        Node::Value(value) => value,
    })
}

/// The Datadog paths, by kind.
struct DatadogPaths(HashMap<Vec<String>, PathKind>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PathKind {
    /// A modeled setting. The walk stops here.
    Leaf,
    /// A proper prefix of a leaf. The walk descends into it.
    Section,
}

impl DatadogPaths {
    fn get() -> &'static Self {
        static PATHS: OnceLock<DatadogPaths> = OnceLock::new();
        PATHS.get_or_init(|| {
            let mut kinds = HashMap::new();
            for leaf in datadog_leaf_paths() {
                let leaf: Vec<String> = leaf.iter().map(|segment| (*segment).to_string()).collect();
                for end in 1..leaf.len() {
                    kinds.insert(leaf[..end].to_vec(), PathKind::Section);
                }
                kinds.insert(leaf, PathKind::Leaf);
            }
            DatadogPaths(kinds)
        })
    }

    fn kind(&self, path: &[String]) -> Option<PathKind> {
        self.0.get(path).copied()
    }
}

/// The canonical path of `key` inside the section at `section`: the key lowercased and split on `.`.
fn canonical_path(section: &[String], key: &str) -> Vec<String> {
    let segments = key.to_lowercase().split('.').map(str::to_owned).collect::<Vec<_>>();
    [section, &segments].concat()
}

/// A node read at the root or inside a Datadog section.
enum Node {
    /// A map at the root or at a Datadog section path, with its keys as written, in document order.
    Section(Vec<(String, Node)>),
    /// Any other value.
    Value(Value),
}

/// The whole document, read as the root section.
struct Document(Node);

impl<'de> Deserialize<'de> for Document {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(SectionVisitor(Vec::new())).map(Document)
    }
}

/// How the value at `key` inside the section at `section` is read.
#[derive(Clone)]
enum Shape {
    /// Outside the Datadog schema: built exactly as `serde_json::Value` builds it.
    Plain,
    /// A Datadog section at the given canonical path.
    Section(Vec<String>),
    /// A Datadog leaf's value: every map key, at any depth, is rendered like Go's `%v`.
    Leaf,
}

impl Shape {
    fn of_child(section: &[String], key: &str) -> Self {
        let path = canonical_path(section, key);
        match DatadogPaths::get().kind(&path) {
            Some(PathKind::Leaf) => Shape::Leaf,
            Some(PathKind::Section) => Shape::Section(path),
            None => Shape::Plain,
        }
    }
}

impl<'de> DeserializeSeed<'de> for Shape {
    type Value = Node;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Node, D::Error> {
        match self {
            Shape::Plain => Value::deserialize(deserializer).map(Node::Value),
            Shape::Section(path) => deserializer.deserialize_any(SectionVisitor(path)),
            Shape::Leaf => LeafValue.deserialize(deserializer).map(Node::Value),
        }
    }
}

/// Builds scalars exactly as `serde_json::Value`'s own deserializer builds them.
macro_rules! visit_scalars {
    ($de:lifetime) => {
        fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
            Value::deserialize(v.into_deserializer()).map(Into::into)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Value::deserialize(v.into_deserializer()).map(Into::into)
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Value::deserialize(v.into_deserializer()).map(Into::into)
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
            Value::deserialize(v.into_deserializer()).map(Into::into)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Value::deserialize(v.into_deserializer()).map(Into::into)
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Value::deserialize(v.into_deserializer()).map(Into::into)
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Value::Null.into())
        }

        fn visit_some<D: Deserializer<$de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
            deserializer.deserialize_any(self)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Value::Null.into())
        }
    };
}

impl From<Value> for Node {
    fn from(value: Value) -> Self {
        Node::Value(value)
    }
}

/// Reads a map at the root or at the Datadog section at its canonical path, keeping keys as written.
struct SectionVisitor(Vec<String>);

impl<'de> Visitor<'de> for SectionVisitor {
    type Value = Node;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any valid YAML value")
    }

    visit_scalars!('de);

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Node, A::Error> {
        // A list where a section belongs is outside the schema.
        Value::deserialize(de::value::SeqAccessDeserializer::new(seq)).map(Node::Value)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Node, A::Error> {
        let mut entries = Vec::new();
        while let Some(key) = map.next_key_seed(WrittenKey)? {
            let value = map.next_value_seed(Shape::of_child(&self.0, &key))?;
            entries.push((key, value));
        }
        Ok(Node::Section(entries))
    }
}

/// A Datadog leaf's value, with every map key rendered like Go's `%v`.
struct LeafValue;

impl<'de> DeserializeSeed<'de> for LeafValue {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for LeafValue {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any valid YAML value")
    }

    visit_scalars!('de);

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(LeafValue)? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        // Keys that were written as the text they render to. Such a key keeps its entry when a typed
        // key renders to the same text; otherwise the later entry wins.
        let mut written = std::collections::HashSet::new();
        let mut entries = Map::new();
        while let Some(key) = map.next_key_seed(RenderedKey)? {
            let value = map.next_value_seed(LeafValue)?;
            match key {
                Key::Written(text) => {
                    written.insert(text.clone());
                    entries.insert(text, value);
                }
                Key::Typed(text) if !written.contains(&text) => {
                    entries.insert(text, value);
                }
                Key::Typed(_) => {}
            }
        }
        Ok(Value::Object(entries))
    }
}

/// A map key read as the string it was written as.
struct WrittenKey;

impl<'de> DeserializeSeed<'de> for WrittenKey {
    type Value = String;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<String, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl Visitor<'_> for WrittenKey {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a string")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
        Ok(v.to_owned())
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<String, E> {
        Ok(v)
    }
}

/// A leaf map key, rendered like Go's `fmt.Sprintf("%v")`.
enum Key {
    /// A key that is a string in YAML: quoted, or plain text that resolves to no other type.
    Written(String),
    /// A key YAML 1.1 types as a boolean or a number, rendered to text.
    Typed(String),
}

/// Reads a map key typed by YAML 1.1 rules and renders it like Go's `fmt.Sprintf("%v")`.
struct RenderedKey;

impl<'de> DeserializeSeed<'de> for RenderedKey {
    type Value = Key;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Key, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl Visitor<'_> for RenderedKey {
    type Value = Key;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a scalar mapping key")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Key, E> {
        Ok(Key::Typed(v.to_string()))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Key, E> {
        Ok(Key::Typed(v.to_string()))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Key, E> {
        Ok(Key::Typed(v.to_string()))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Key, E> {
        Ok(Key::Typed(go_float(v)))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Key, E> {
        Ok(Key::Written(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Key, E> {
        Ok(Key::Written(v))
    }

    fn visit_none<E: de::Error>(self) -> Result<Key, E> {
        Err(de::Error::custom(
            "null mapping keys are not supported; quote the key to use it as text",
        ))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Key, E> {
        Err(de::Error::custom(
            "null mapping keys are not supported; quote the key to use it as text",
        ))
    }
}

/// Renders `v` like Go's `%v` for a `float64`: the shortest digits that round-trip, in `%e` form when
/// the decimal exponent is below -4 or at least 6, and in plain decimal otherwise.
fn go_float(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_owned();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-Inf" } else { "+Inf" }.to_owned();
    }

    // Rust's `{:e}` yields the shortest round-trip digits as `d[.ddd]e<exp>`.
    let scientific = format!("{:e}", v.abs());
    let (mantissa, exponent) = scientific.split_once('e').expect("`{:e}` output has an exponent");
    let exponent: i32 = exponent.parse().expect("`{:e}` exponent is an integer");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let sign = if v.is_sign_negative() { "-" } else { "" };

    let body = if !(-4..6).contains(&exponent) {
        let (head, tail) = digits.split_at(1);
        let fraction = if tail.is_empty() {
            String::new()
        } else {
            format!(".{tail}")
        };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        format!("{head}{fraction}e{exponent_sign}{:02}", exponent.abs())
    } else if exponent < 0 {
        format!("0.{}{digits}", "0".repeat(exponent.unsigned_abs() as usize - 1))
    } else {
        // The decimal point sits after `exponent + 1` digits.
        let integer_len = exponent as usize + 1;
        if digits.len() <= integer_len {
            format!("{digits}{}", "0".repeat(integer_len - digits.len()))
        } else {
            format!("{}.{}", &digits[..integer_len], &digits[integer_len..])
        }
    };
    format!("{sign}{body}")
}

/// The values of a document, each at the place it resolves to, gathered before any is written so that
/// colliding spellings resolve the same way whatever order they were written in.
#[derive(Default)]
struct Placements {
    // Values whose written path is the path they resolve to, in document order.
    in_place: Vec<(Vec<String>, Value)>,
    // Values a respelling moved, in document order.
    moved: Vec<(Vec<String>, Value)>,
    // Null values in place, kept only where nothing else is written.
    nulls: Vec<Vec<String>>,
    // Every Datadog section a map was written for, so an empty section is kept.
    sections: Vec<Vec<String>>,
}

impl Placements {
    /// Gathers `entries`, a map written at `written` for the section at `section`.
    fn collect(&mut self, entries: Vec<(String, Node)>, section: &[String], written: &[String]) {
        for (key, node) in entries {
            let written_path = [written, std::slice::from_ref(&key)].concat();
            let canonical = canonical_path(section, &key);
            match node {
                // Only a map at a Datadog section path is read as a section.
                Node::Section(children) => {
                    self.collect(children, &canonical, &written_path);
                    self.sections.push(canonical);
                }
                Node::Value(value) => {
                    let path = match DatadogPaths::get().kind(&canonical) {
                        Some(PathKind::Leaf) => canonical,
                        // A non-map where a section belongs stays where it was written.
                        Some(PathKind::Section) => written_path.clone(),
                        // An unknown key keeps its spelling inside the section it resolves to.
                        None => [section, &[key]].concat(),
                    };
                    match (path == written_path, value) {
                        (true, Value::Null) => self.nulls.push(path),
                        (false, Value::Null) => {}
                        (true, value) => self.in_place.push((path, value)),
                        (false, value) => self.moved.push((path, value)),
                    }
                }
            }
        }
    }

    /// Writes the gathered values into one map, applying the collision rules in the module docs.
    fn resolve(self) -> Map<String, Value> {
        let mut root = Map::new();
        // In-place paths cannot collide unless the parser accepted two keys with the same text,
        // where the later one won before this module existed too.
        for (path, value) in self.in_place {
            write(&mut root, &path, value);
        }
        for (path, value) in self.moved.into_iter().rev() {
            if is_vacant(&root, &path) {
                write(&mut root, &path, value);
            }
        }
        for path in self.sections {
            if is_vacant(&root, &path) {
                write(&mut root, &path, Value::Object(Map::new()));
            }
        }
        for path in self.nulls {
            if is_vacant(&root, &path) {
                write(&mut root, &path, Value::Null);
            }
        }
        root
    }
}

/// Returns whether `path` can be written without replacing a value: nothing is at it, and nothing on
/// the way to it is anything but an object.
fn is_vacant(root: &Map<String, Value>, path: &[String]) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let mut current = root;
    for segment in parents {
        match current.get(segment) {
            None => return true,
            Some(Value::Object(children)) => current = children,
            Some(_) => return false,
        }
    }
    !current.contains_key(last)
}

/// Writes `value` at `path`, making an object of every parent.
fn write(root: &mut Map<String, Value>, path: &[String], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = root;
    for segment in parents {
        let parent = current
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if !parent.is_object() {
            *parent = Value::Object(Map::new());
        }
        current = parent.as_object_mut().expect("the parent was just made an object");
    }
    current.insert(last.clone(), value);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Reads a document with its root as a Datadog leaf's value.
    struct LeafDocument(Value);

    impl<'de> Deserialize<'de> for LeafDocument {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            LeafValue.deserialize(deserializer).map(LeafDocument)
        }
    }

    /// The keys of `additional_endpoints`, a Datadog leaf, in `yaml`.
    fn leaf_keys(yaml: &str) -> Result<Vec<String>, String> {
        let value = from_str(&format!("additional_endpoints:\n{yaml}"))?;
        Ok(value["additional_endpoints"]
            .as_object()
            .expect("the leaf is a map")
            .keys()
            .cloned()
            .collect())
    }

    #[test]
    fn leaf_map_keys_render_like_go_percent_v() {
        // Each spelling and rendering was produced by `fmt.Sprintf("%v", key)` on a key parsed by
        // go.yaml.in/yaml/v2 v2.4.4 into `map[interface{}]interface{}`, the Agent's parser.
        let cases = [
            // YAML 1.1 booleans.
            ("yes", "true"),
            ("No", "false"),
            ("on", "true"),
            ("Off", "false"),
            ("y", "true"),
            ("n", "false"),
            ("TRUE", "true"),
            // Integers.
            ("0x1F", "31"),
            ("0xAbC", "2748"),
            ("010", "8"),
            ("007", "7"),
            ("0o17", "15"),
            ("0b11", "3"),
            ("1_000", "1000"),
            ("+1", "1"),
            ("-1", "-1"),
            ("-0", "0"),
            ("9223372036854775807", "9223372036854775807"),
            ("9223372036854775808", "9223372036854775808"),
            ("18446744073709551615", "18446744073709551615"),
            ("!!int '010'", "8"),
            // Floats, in Go's shortest `%g` form.
            ("08", "8"),
            ("8.0", "8"),
            ("1e3", "1000"),
            ("1E3", "1000"),
            ("1e6", "1e+06"),
            ("100000.0", "100000"),
            ("123456.0", "123456"),
            ("1234567.0", "1.234567e+06"),
            ("1.5", "1.5"),
            (".5", "0.5"),
            ("1.", "1"),
            ("0.0001", "0.0001"),
            ("0.00001", "1e-05"),
            ("0.5e-3", "0.0005"),
            ("0.1", "0.1"),
            ("1e-7", "1e-07"),
            ("-1e6", "-1e+06"),
            ("1e20", "1e+20"),
            ("1e21", "1e+21"),
            ("-0.0", "-0"),
            ("18446744073709551616", "1.8446744073709552e+19"),
            ("-9223372036854775809", "-9.223372036854776e+18"),
            // Strings stay as written.
            ("1,000", "1,000"),
            ("1:20", "1:20"),
            ("2001-12-14", "2001-12-14"),
            ("https://x.test", "https://x.test"),
            ("host.name", "host.name"),
            ("Mixed.Case", "Mixed.Case"),
            // Quoted keys are strings, however they would resolve.
            ("'010'", "010"),
            ("\"yes\"", "yes"),
            ("'1e3'", "1e3"),
            ("'0x1F'", "0x1F"),
            ("!!str 010", "010"),
        ];
        for (written, rendered) in cases {
            assert_eq!(
                leaf_keys(&format!("  {written}: [k]\n")).unwrap(),
                [rendered],
                "key `{written}`"
            );
        }
    }

    #[test]
    fn go_float_rendering_covers_the_exponent_thresholds() {
        // The exponent form starts at 1e6 and below 1e-4; digits are the shortest that round-trip.
        let cases = [
            (0.0, "0"),
            (1.0, "1"),
            (-2.5, "-2.5"),
            (99999.9, "99999.9"),
            (100000.0, "100000"),
            (999999.0, "999999"),
            (1_000_000.0, "1e+06"),
            (1_234_567.0, "1.234567e+06"),
            (0.001, "0.001"),
            (0.0001, "0.0001"),
            (0.000_099, "9.9e-05"),
            (1e100, "1e+100"),
            (1.5e-7, "1.5e-07"),
            (f64::MAX, "1.7976931348623157e+308"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "+Inf"),
            (f64::NEG_INFINITY, "-Inf"),
        ];
        for (value, rendered) in cases {
            assert_eq!(go_float(value), rendered, "{value:e}");
        }
    }

    #[test]
    fn leaf_map_keys_that_render_the_same_keep_one_entry() {
        // A key written as its rendered text keeps its entry in either order; among typed keys the
        // later one wins.
        let cases = [
            ("  yes: [a]\n  'true': [b]\n", json!({"true": ["b"]})),
            ("  'true': [b]\n  yes: [a]\n", json!({"true": ["b"]})),
            ("  010: [a]\n  '8': [b]\n", json!({"8": ["b"]})),
            ("  '8': [b]\n  010: [a]\n", json!({"8": ["b"]})),
            ("  yes: [a]\n  true: [b]\n", json!({"true": ["b"]})),
            ("  true: [b]\n  yes: [a]\n", json!({"true": ["a"]})),
            ("  010: [a]\n  8.0: [b]\n", json!({"8": ["b"]})),
            ("  8.0: [b]\n  010: [a]\n", json!({"8": ["a"]})),
            (
                "  on: [a]\n  y: [b]\n  '1000': [c]\n  1e3: [d]\n",
                json!({"true": ["b"], "1000": ["c"]}),
            ),
        ];
        for (yaml, expected) in cases {
            let value = from_str(&format!("additional_endpoints:\n{yaml}")).expect(yaml);
            assert_eq!(value["additional_endpoints"], expected, "{yaml:?}");
        }
    }

    #[test]
    fn a_null_leaf_map_key_is_an_error() {
        for key in ["~", "null"] {
            let error = leaf_keys(&format!("  {key}: [k]\n")).expect_err(key).to_string();
            assert!(
                error.starts_with("null mapping keys are not supported"),
                "{key}: {error}"
            );
        }
        assert_eq!(leaf_keys("  '': [k]\n").unwrap(), [""]);
    }

    #[test]
    fn leaf_keys_are_rendered_at_every_depth_and_inside_lists() {
        let value = from_str(
            "dogstatsd_mapper_profiles:\n  - name: p\n    mappings:\n      - tags: {010: v, yes: w}\n  - [{0x1F: x}]\nadditional_endpoints:\n  a: {on: {1e3: 1}}\n",
        )
        .unwrap();

        assert_eq!(
            value,
            json!({
                "dogstatsd_mapper_profiles": [
                    {"name": "p", "mappings": [{"tags": {"8": "v", "true": "w"}}]},
                    [{"31": "x"}],
                ],
                "additional_endpoints": {"a": {"true": {"1000": 1}}},
            })
        );
    }

    #[test]
    fn leaf_values_are_built_like_serde_json_builds_them() {
        // Values, as opposed to keys, are untouched: the comparison is against the library's own reading.
        let yaml = "num: 010\nflag: yes\nfloat: 1.5\ntext: '010'\nnothing: ~\nlist: [1, [x], {}]\ninf: .inf\nnan: .nan\nbig: 18446744073709551615\n";
        let expected: Value = saluki_config::yaml::from_str(yaml).unwrap();

        let LeafDocument(actual) = saluki_config::yaml::from_str(yaml).unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn keys_outside_the_datadog_schema_are_read_as_written() {
        let value = from_str("unknown_labels_xyz:\n  host.name: a\n  010: b\n  Yes: c\n  'on': d\n").unwrap();

        assert_eq!(
            value,
            json!({"unknown_labels_xyz": {"host.name": "a", "010": "b", "Yes": "c", "on": "d"}})
        );
    }

    #[test]
    fn documents_without_datadog_paths_parse_like_serde_json_value() {
        let documents = [
            "",
            "# only a comment\n",
            "~\n",
            "just a string\n",
            "- 1\n- a: b\n",
            "a: 1\nB.c: 2\n'd.e': [x, {F: 3}]\n",
            "yes: 1\n010: 2\nno: {on: off}\n",
            "base: &base {a: 1, b: 2}\nderived:\n  <<: *base\n  b: 3\n",
            "merged:\n  <<: [{a: 1}, {a: 2, b: 2}]\n  c: 3\n",
            "a: .inf\nb: -.inf\nc: .nan\nd: [.nan, {e: .inf}]\n",
            "n: 010\nh: 0x1F\nnot_octal: 08\nq: '010'\nz:\nk: 1e3\n",
            "unicode: \"\\u00e9\"\n\"É\": ok\n",
            "nested:\n  deeper:\n    deepest: [1, 2, {x: y}]\n",
        ];
        for document in documents {
            let expected: Value = saluki_config::yaml::from_str(document).unwrap();
            assert_eq!(from_str(document).unwrap(), expected, "{document:?}");
        }
    }

    #[test]
    fn documents_without_datadog_paths_fail_like_serde_json_value() {
        let documents = [
            "a: 1\na: 2\n",
            "a: [unclosed\n",
            "a: 1\nB: {x: 1, x: 2}\n",
            "a: *missing\n",
        ];
        for document in documents {
            let expected = saluki_config::yaml::from_str::<Value>(document)
                .unwrap_err()
                .to_string();
            let actual = from_str(document).unwrap_err().to_string();
            assert_eq!(actual, expected, "{document:?}");
        }
    }

    #[test]
    fn datadog_paths_can_be_matched_at_the_section_level_without_key_resolution() {
        // Section-level keys are read as written, which is only the Agent's reading if no segment of
        // a Datadog path changes under case folding, splitting, or YAML 1.1 typing.
        let paths = DatadogPaths::get();
        assert!(!paths.0.is_empty());
        for (path, kind) in &paths.0 {
            for segment in path {
                assert_eq!(&segment.to_lowercase(), segment, "`{path:?}` has an uppercase segment");
                assert!(!segment.contains('.'), "`{path:?}` has a segment with a dot");
                let LeafDocument(parsed) = saluki_config::yaml::from_str(&format!("{segment}: 1\n"))
                    .unwrap_or_else(|e| panic!("segment `{segment}` of `{path:?}` is not a key: {e}"));
                assert_eq!(
                    parsed.as_object().map(|map| map.keys().collect::<Vec<_>>()),
                    Some(vec![segment]),
                    "segment `{segment}` of `{path:?}` resolves to a non-string scalar"
                );
            }
            if *kind == PathKind::Leaf {
                for end in 1..path.len() {
                    assert_eq!(
                        paths.kind(&path[..end]),
                        Some(PathKind::Section),
                        "`{path:?}` descends through a leaf"
                    );
                }
            }
        }
    }
}
