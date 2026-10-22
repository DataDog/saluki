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

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

use datadog_agent_config::datadog_leaf_paths;
use serde::de::{self, DeserializeSeed, IntoDeserializer as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use snafu::Snafu;

/// An error reading the configuration file text.
#[derive(Debug, Snafu)]
pub(crate) enum Error {
    /// The text is not valid YAML, or a map holds two keys that are the same key.
    #[snafu(display("{message}"))]
    Parse {
        /// The message from the YAML parser. It carries a location and never any source text.
        message: String,
    },

    /// Two spellings of one Datadog path both carry a value.
    #[snafu(display("`{path}` is set more than once, as `{spelling}` and as `{other}`; remove one of them"))]
    Conflict {
        /// The canonical dotted path both spellings resolve to.
        path: String,
        /// One spelling, with each nested key separated by ` > `.
        spelling: String,
        /// The other spelling, written the same way.
        other: String,
    },
}

/// Parses `text` into the base value, resolving Datadog keys the way the Agent does.
///
/// Parser options, limits, merge keys and the secret-free error text come from
/// `saluki_config::yaml::from_str`. A document with no Datadog path parses exactly as
/// `serde_json::Value` would.
///
/// # Errors
///
/// Returns an error if the text is not valid YAML, if two keys of a Datadog leaf's map render to
/// the same key, or if two spellings of one Datadog path both carry a value.
pub(crate) fn from_str(text: &str) -> Result<Value, Error> {
    let Document(value) = saluki_config::yaml::from_str(text).map_err(|e| Error::Parse { message: e.to_string() })?;
    match value {
        Value::Object(root) => Normalizer::default().run(root).map(Value::Object),
        other => Ok(other),
    }
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

/// The whole document, read as the root section.
struct Document(Value);

impl<'de> Deserialize<'de> for Document {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Shape::Scoped(Scope::Section(Vec::new()))
            .deserialize(deserializer)
            .map(Document)
    }
}

/// How a YAML node is read, which depends on where its path falls in the Datadog schema.
#[derive(Clone)]
enum Shape {
    /// Outside the Datadog schema: built exactly as `serde_json::Value` builds it.
    Plain,
    /// Inside the Datadog schema.
    Scoped(Scope),
}

#[derive(Clone)]
enum Scope {
    /// A Datadog section at the given canonical path: its keys are read as written, and each value's
    /// shape follows from the key's canonical path.
    Section(Vec<String>),
    /// A Datadog leaf's value: every map key, at any depth, is rendered like Go's `%v`.
    Leaf,
}

impl Shape {
    /// The shape of the value at `key` inside the section at `section`.
    fn of_child(section: &[String], key: &str) -> Self {
        let path = canonical_path(section, key);
        match DatadogPaths::get().kind(&path) {
            Some(PathKind::Leaf) => Shape::Scoped(Scope::Leaf),
            Some(PathKind::Section) => Shape::Scoped(Scope::Section(path)),
            None => Shape::Plain,
        }
    }
}

impl<'de> DeserializeSeed<'de> for Shape {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        match self {
            Shape::Plain => Value::deserialize(deserializer),
            Shape::Scoped(scope) => deserializer.deserialize_any(ScopeVisitor(scope)),
        }
    }
}

/// Builds a `Value` for a node inside the Datadog schema.
///
/// Scalars are built by `serde_json::Value`'s own deserializer; only maps and lists differ.
struct ScopeVisitor(Scope);

impl<'de> Visitor<'de> for ScopeVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any valid YAML value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Value::deserialize(v.into_deserializer())
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Value::deserialize(v.into_deserializer())
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Value::deserialize(v.into_deserializer())
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Value::deserialize(v.into_deserializer())
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Value::deserialize(v.into_deserializer())
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        Value::deserialize(v.into_deserializer())
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        // Keys inside a leaf's list of maps are rendered too; a section's list is outside the schema.
        let element = match &self.0 {
            Scope::Leaf => Shape::Scoped(Scope::Leaf),
            Scope::Section(_) => Shape::Plain,
        };
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(element.clone())? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut entries = Map::new();
        match self.0 {
            Scope::Section(section) => {
                while let Some(key) = map.next_key_seed(WrittenKey)? {
                    let value = map.next_value_seed(Shape::of_child(&section, &key))?;
                    entries.insert(key, value);
                }
            }
            Scope::Leaf => {
                while let Some(key) = map.next_key_seed(RenderedKey)? {
                    if entries.contains_key(&key) {
                        return Err(de::Error::custom(format!(
                            "duplicate mapping key `{key}` (unquoted keys are read as YAML values; remove one, or quote it to keep its spelling)"
                        )));
                    }
                    entries.insert(key, map.next_value_seed(Shape::Scoped(Scope::Leaf))?);
                }
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

/// A map key typed by YAML 1.1 rules, then rendered like Go's `fmt.Sprintf("%v")`.
struct RenderedKey;

impl<'de> DeserializeSeed<'de> for RenderedKey {
    type Value = String;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<String, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl Visitor<'_> for RenderedKey {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a scalar mapping key")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<String, E> {
        Ok(v.to_string())
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<String, E> {
        Ok(v.to_string())
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<String, E> {
        Ok(v.to_string())
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<String, E> {
        Ok(go_float(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
        Ok(v.to_owned())
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<String, E> {
        Ok(v)
    }

    fn visit_none<E: de::Error>(self) -> Result<String, E> {
        Err(de::Error::custom(
            "null mapping keys are not supported; quote the key to use it as text",
        ))
    }

    fn visit_unit<E: de::Error>(self) -> Result<String, E> {
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

/// Moves each Datadog key at the root and in the Datadog sections to its canonical path.
///
/// A key whose canonical path is not a Datadog leaf or section stays exactly where and as it was.
/// The result is assembled in one tree, so spellings that resolve to the same section merge by key.
#[derive(Default)]
struct Normalizer {
    root: Map<String, Value>,
    // How each placed canonical path was written, for conflict messages.
    spellings: HashMap<Vec<String>, String>,
}

impl Normalizer {
    fn run(mut self, input: Map<String, Value>) -> Result<Map<String, Value>, Error> {
        self.absorb(input, &[], "")?;
        Ok(self.root)
    }

    /// Places each entry of `input`, a map written at `spelled` for the section at `section`.
    fn absorb(&mut self, input: Map<String, Value>, section: &[String], spelled: &str) -> Result<(), Error> {
        for (key, value) in input {
            let spelling = if spelled.is_empty() {
                key.clone()
            } else {
                format!("{spelled} > {key}")
            };
            let path = canonical_path(section, &key);
            match (DatadogPaths::get().kind(&path), value) {
                (Some(PathKind::Section), Value::Object(children)) => {
                    self.ensure_object(&path, &spelling)?;
                    self.absorb(children, &path, &spelling)?;
                }
                (Some(_), value) => self.place(&path, value, spelling)?,
                (None, value) => {
                    let unmoved = [section, &[key]].concat();
                    self.place(&unmoved, value, spelling)?;
                }
            }
        }
        Ok(())
    }

    /// Ensures an object exists at `path`, creating it and its parents as needed.
    ///
    /// A null counts as absent. Any other value that is not an object conflicts with `spelling`.
    fn ensure_object(&mut self, path: &[String], spelling: &str) -> Result<(), Error> {
        let mut current = &mut self.root;
        for (depth, segment) in path.iter().enumerate() {
            let entry = current.entry(segment.clone()).or_insert(Value::Null);
            if entry.is_null() {
                *entry = Value::Object(Map::new());
                self.spellings.insert(path[..=depth].to_vec(), spelling.to_owned());
            }
            match entry {
                Value::Object(children) => current = children,
                _ => return Err(conflict(&self.spellings, &path[..=depth], spelling)),
            }
        }
        Ok(())
    }

    /// Places `value` at `path`, which must not already hold a non-null value unless `value` is null.
    fn place(&mut self, path: &[String], value: Value, spelling: String) -> Result<(), Error> {
        let Some((last, parents)) = path.split_last() else {
            return Ok(());
        };
        self.ensure_object(parents, &spelling)?;

        let mut parent = &mut self.root;
        for segment in parents {
            parent = match parent.get_mut(segment) {
                Some(Value::Object(children)) => children,
                _ => unreachable!("`ensure_object` created an object at every parent"),
            };
        }
        match parent.get_mut(last) {
            None => {
                parent.insert(last.clone(), value);
            }
            Some(existing) if existing.is_null() => *existing = value,
            Some(_) if value.is_null() => return Ok(()),
            Some(_) => return Err(conflict(&self.spellings, path, &spelling)),
        }
        self.spellings.insert(path.to_vec(), spelling);
        Ok(())
    }
}

fn conflict(spellings: &HashMap<Vec<String>, String>, path: &[String], other: &str) -> Error {
    let path_text = path.join(".");
    Error::Conflict {
        spelling: spellings.get(path).cloned().unwrap_or_else(|| path_text.clone()),
        other: other.to_owned(),
        path: path_text,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Reads a document with its root as a Datadog leaf's value.
    struct LeafDocument(Value);

    impl<'de> Deserialize<'de> for LeafDocument {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            Shape::Scoped(Scope::Leaf).deserialize(deserializer).map(LeafDocument)
        }
    }

    /// The keys of `additional_endpoints`, a Datadog leaf, in `yaml`.
    fn leaf_keys(yaml: &str) -> Result<Vec<String>, Error> {
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
    fn leaf_map_keys_that_render_the_same_are_a_duplicate_key() {
        let cases = [
            ("  yes: [a]\n  true: [b]\n", "true"),
            ("  010: [a]\n  8.0: [b]\n", "8"),
            ("  on: [a]\n  y: [b]\n", "true"),
            ("  1e3: [a]\n  1000: [b]\n", "1000"),
        ];
        for (yaml, key) in cases {
            let error = leaf_keys(yaml).expect_err(yaml).to_string();
            assert!(
                error.starts_with(&format!(
                    "duplicate mapping key `{key}` (unquoted keys are read as YAML values"
                )),
                "{yaml:?}: {error}"
            );
        }
    }

    #[test]
    fn leaf_map_keys_that_differ_after_rendering_are_both_kept() {
        assert_eq!(
            leaf_keys("  010: [a]\n  '010': [b]\n  yes: [c]\n  'on ': [d]\n").unwrap(),
            ["010", "8", "on ", "true"]
        );
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
