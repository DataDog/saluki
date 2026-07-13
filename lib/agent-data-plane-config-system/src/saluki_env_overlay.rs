//! Overlay flat environment-variable keys onto the nested `SalukiOnly` shape.
//!
//! Environment variables reach the merged map as flat, underscore-joined keys:
//! `DD_DATA_PLANE_STANDALONE_MODE` lands as `data_plane_standalone_mode`, not as the nested
//! `data_plane.standalone_mode` object `SalukiOnly` reads, so without help the value is silently
//! lost. This is the Saluki-only twin of the Datadog `apply_env_overlay`, which repairs the same
//! gap for the Datadog source model.
//!
//! The two differ in where the mapping comes from. The Datadog side relocates keys named in a
//! generated table. Here there is no table and no alias: a Saluki-only key's flat env spelling is
//! its canonical path with the dots replaced by underscores (`path.join("_")`), so the nested slot
//! and its env form stay in lockstep with the struct by construction. The set of canonical paths is
//! read straight off `SalukiOnly` (see the discovery section below), so it cannot drift from the
//! fields the deserializer actually reads.

use std::collections::HashSet;
use std::sync::OnceLock;

use datadog_agent_config::EnvOverlayMode;
use serde::de::value::{Error as ValueError, StrDeserializer};
use serde::de::{DeserializeSeed, Deserializer, IntoDeserializer, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::saluki_only::SalukiOnly;

/// Relocates flat environment-variable keys in `merged` into the nested slots `SalukiOnly` reads,
/// according to `mode`.
///
/// Operates on the caller's owned `merged` value. Only multi-segment paths are relocated;
/// single-segment keys are already flat and need no move.
pub(crate) fn apply(merged: &mut Value, mode: EnvOverlayMode) {
    let clobber = match mode {
        EnvOverlayMode::Disabled => return,
        EnvOverlayMode::Override => true,
        EnvOverlayMode::Fallback => false,
    };

    let Some(root) = merged.as_object_mut() else {
        return;
    };

    for path in leaf_paths() {
        if path.len() < 2 {
            continue;
        }
        let flat = path.join("_");
        let Some(value) = root.get(&flat).cloned() else {
            continue;
        };

        let segments: Vec<&str> = path.iter().map(String::as_str).collect();
        if !clobber && path_present(root, &segments) {
            continue;
        }
        set_at_path(root, &segments, value);
    }
}

// `path_present` and `set_at_path` are copied from the Datadog `apply_env_overlay` rather than
// shared, so exposing them across the crate boundary is not forced for a few lines of JSON walking.

/// Returns whether a leaf already exists at `path`, walking object nodes from `root`.
fn path_present(root: &Map<String, Value>, path: &[&str]) -> bool {
    let Some((first, rest)) = path.split_first() else {
        return false;
    };
    let mut current = match root.get(*first) {
        Some(v) => v,
        None => return false,
    };
    for segment in rest {
        match current.get(*segment) {
            Some(v) => current = v,
            None => return false,
        }
    }
    true
}

/// Sets `value` at `path`, creating intermediate object nodes as needed and clobbering the leaf. An
/// intermediate that exists but is not an object is replaced; a Saluki-only key's parents are always
/// sections, so this cannot lose a real value.
fn set_at_path(root: &mut Map<String, Value>, path: &[&str], value: Value) {
    let Some((leaf, sections)) = path.split_last() else {
        return;
    };

    let mut current = root;
    for segment in sections {
        let node = current
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !node.is_object() {
            *node = Value::Object(Map::new());
        }
        current = node.as_object_mut().expect("node was just ensured to be an object");
    }

    current.insert((*leaf).to_string(), value);
}

// ─────────────────────────────────────────────────────────────────────────────
// Path discovery: the canonical leaf paths, read off `SalukiOnly` itself.
//
// `discover_leaf_paths` drives `SalukiOnly::deserialize` with `PathRecorder`, a deserializer that
// deserializes nothing. It records the field path serde asks for at each leaf, descending through
// nested structs and through `Option<T>` as `Some(T)` so optional sub-structs stay visible.
//
// The one trap: some leaf types are scalar-like but their serde form is not a plain scalar
// (`DurationString` and `ByteSize` deserialize through `deserialize_any`). The recorder must treat
// those as leaves; descending into them would invent bogus segments. Everything that is not a
// struct is therefore a leaf, which lands those types correctly without naming them.
// ─────────────────────────────────────────────────────────────────────────────

/// The canonical leaf paths of `SalukiOnly`, discovered once and cached (they are fixed at compile
/// time).
fn leaf_paths() -> &'static [Vec<String>] {
    static PATHS: OnceLock<Vec<Vec<String>>> = OnceLock::new();
    PATHS.get_or_init(discover_leaf_paths)
}

/// Discovers `SalukiOnly`'s leaf paths by driving its derived `Deserialize` with the recorder.
///
/// A `#[serde(alias)]` field lists both its name and the alias in the `fields` slice serde hands to
/// `deserialize_struct`, and feeding both trips serde's duplicate-field check. The colliding name is
/// the one yielded just before the error, so it is added to `skip` and discovery retried until it
/// runs clean. Discovery depends only on the struct, so any real failure surfaces in tests, never in
/// production.
fn discover_leaf_paths() -> Vec<Vec<String>> {
    let mut skip = HashSet::new();
    loop {
        let mut ctx = DiscoverCtx {
            out: Vec::new(),
            skip: &skip,
            last_yielded: None,
        };
        let recorder = PathRecorder {
            path: Vec::new(),
            ctx: &mut ctx,
        };
        match SalukiOnly::deserialize(recorder) {
            Ok(_) => return ctx.out,
            Err(error) if error.to_string().starts_with("duplicate field ") => {
                let collided = ctx
                    .last_yielded
                    .expect("a duplicate-field error must follow a yielded field name");
                if !skip.insert(collided.clone()) {
                    panic!("SalukiOnly leaf-path discovery cannot make progress past field `{collided:?}`");
                }
            }
            Err(error) => panic!("SalukiOnly leaf-path discovery failed: {error}"),
        }
    }
}

struct DiscoverCtx<'a> {
    out: Vec<Vec<String>>,
    skip: &'a HashSet<Vec<String>>,
    last_yielded: Option<Vec<String>>,
}

struct PathRecorder<'c, 's> {
    path: Vec<String>,
    ctx: &'c mut DiscoverCtx<'s>,
}

impl PathRecorder<'_, '_> {
    fn record_leaf(&mut self) {
        self.ctx.out.push(std::mem::take(&mut self.path));
    }
}

// `PathRecorder` records a leaf at every scalar (and scalar-like) method, descends at `option` and
// `newtype_struct`, and recurses at `struct`. The visited value is thrown away, so each leaf feeds
// the visitor a throwaway of the right shape purely to let deserialization complete. `deserialize_any`
// is a leaf: it is where `DurationString` and `ByteSize` land. `tuple`, `tuple_struct`, and `enum`
// are unused by `SalukiOnly` and fail loudly if that ever changes.
macro_rules! record_scalar {
    ($method:ident, $visit:ident, $dummy:expr) => {
        fn $method<V>(mut self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            self.record_leaf();
            visitor.$visit($dummy)
        }
    };
}

impl<'de> Deserializer<'de> for PathRecorder<'_, '_> {
    type Error = ValueError;

    record_scalar!(deserialize_bool, visit_bool, false);
    record_scalar!(deserialize_i8, visit_i8, 0);
    record_scalar!(deserialize_i16, visit_i16, 0);
    record_scalar!(deserialize_i32, visit_i32, 0);
    record_scalar!(deserialize_i64, visit_i64, 0);
    record_scalar!(deserialize_i128, visit_i128, 0);
    record_scalar!(deserialize_u8, visit_u8, 0);
    record_scalar!(deserialize_u16, visit_u16, 0);
    record_scalar!(deserialize_u32, visit_u32, 0);
    record_scalar!(deserialize_u64, visit_u64, 0);
    record_scalar!(deserialize_u128, visit_u128, 0);
    record_scalar!(deserialize_f32, visit_f32, 0.0);
    record_scalar!(deserialize_f64, visit_f64, 0.0);
    record_scalar!(deserialize_char, visit_char, '\0');
    record_scalar!(deserialize_str, visit_str, "");
    record_scalar!(deserialize_string, visit_str, "");
    record_scalar!(deserialize_bytes, visit_bytes, b"");
    record_scalar!(deserialize_byte_buf, visit_bytes, b"");

    fn deserialize_unit<V>(mut self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.record_leaf();
        visitor.visit_unit()
    }

    fn deserialize_any<V>(mut self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.record_leaf();
        visitor.visit_u64(0)
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V>(self, _name: &'static str, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_unit_struct<V>(mut self, _name: &'static str, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.record_leaf();
        visitor.visit_unit()
    }

    fn deserialize_seq<V>(mut self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.record_leaf();
        visitor.visit_seq(EmptyAccess)
    }

    fn deserialize_map<V>(mut self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.record_leaf();
        visitor.visit_map(EmptyAccess)
    }

    fn deserialize_tuple<V>(self, _len: usize, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(unsupported("tuple"))
    }

    fn deserialize_tuple_struct<V>(self, _name: &'static str, _len: usize, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(unsupported("tuple struct"))
    }

    fn deserialize_enum<V>(
        self, _name: &'static str, _variants: &'static [&'static str], _visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(unsupported("enum"))
    }

    fn deserialize_struct<V>(
        self, _name: &'static str, fields: &'static [&'static str], visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_map(StructWalker {
            fields,
            index: 0,
            base: self.path,
            ctx: self.ctx,
        })
    }

    fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_str("")
    }

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }
}

fn unsupported(kind: &str) -> ValueError {
    serde::de::Error::custom(format!("SalukiOnly env tracer does not support {kind} leaves"))
}

/// Feeds every field name to the derived struct visitor in turn, so serde requests each field's
/// value and the recorder sees every leaf. Names in `skip` (see [`discover_leaf_paths`]) are passed
/// over.
struct StructWalker<'c, 's> {
    fields: &'static [&'static str],
    index: usize,
    base: Vec<String>,
    ctx: &'c mut DiscoverCtx<'s>,
}

impl<'de> MapAccess<'de> for StructWalker<'_, '_> {
    type Error = ValueError;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: DeserializeSeed<'de>,
    {
        while self.index < self.fields.len() {
            let mut path = self.base.clone();
            path.push(self.fields[self.index].to_string());
            if !self.ctx.skip.contains(&path) {
                self.ctx.last_yielded = Some(path);
                break;
            }
            self.index += 1;
        }
        if self.index >= self.fields.len() {
            return Ok(None);
        }
        let name = self.fields[self.index];
        let key: StrDeserializer<'_, ValueError> = name.into_deserializer();
        seed.deserialize(key).map(Some)
    }

    fn next_value_seed<Vs>(&mut self, seed: Vs) -> Result<Vs::Value, Self::Error>
    where
        Vs: DeserializeSeed<'de>,
    {
        let field = self.fields[self.index];
        self.index += 1;

        let mut path = self.base.clone();
        path.push(field.to_string());
        seed.deserialize(PathRecorder {
            path,
            ctx: &mut *self.ctx,
        })
    }
}

/// An empty sequence and map, supplied to the visitor for `Vec` and map leaves, which are recorded
/// but not descended into.
struct EmptyAccess;

impl<'de> SeqAccess<'de> for EmptyAccess {
    type Error = ValueError;

    fn next_element_seed<T>(&mut self, _seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        Ok(None)
    }
}

impl<'de> MapAccess<'de> for EmptyAccess {
    type Error = ValueError;

    fn next_key_seed<K>(&mut self, _seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: DeserializeSeed<'de>,
    {
        Ok(None)
    }

    fn next_value_seed<Vs>(&mut self, _seed: Vs) -> Result<Vs::Value, Self::Error>
    where
        Vs: DeserializeSeed<'de>,
    {
        unreachable!("next_value_seed is never called: next_key_seed always returns None")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn has_path(segments: &[&str]) -> bool {
        leaf_paths()
            .iter()
            .any(|p| p.iter().map(String::as_str).eq(segments.iter().copied()))
    }

    #[test]
    fn tracer_descends_through_optional_struct() {
        assert!(has_path(&["ottl_filter_config", "error_mode"]));
        assert!(has_path(&["ottl_filter_config", "traces", "span"]));
    }

    fn assert_unsplit_leaf(field: &str) {
        assert!(has_path(&[field]));
        assert!(
            !leaf_paths()
                .iter()
                .any(|path| path.len() > 1 && path.first().map(String::as_str) == Some(field)),
            "scalar-like leaf split into sub-fields: {field}",
        );
    }

    #[test]
    fn duration_string_is_a_leaf() {
        assert_unsplit_leaf("aggregate_flush_interval");
    }

    #[test]
    fn byte_size_is_a_leaf() {
        assert_unsplit_leaf("memory_limit");
    }

    #[test]
    fn top_level_key_is_not_relocated_and_still_deserializes() {
        let mut v = json!({ "aggregate_context_limit": 250000 });
        apply(&mut v, EnvOverlayMode::Fallback);

        assert_eq!(v.get("aggregate_context_limit"), Some(&json!(250000)));
        let parsed: SalukiOnly = serde_json::from_value(v).expect("deserializes");
        assert_eq!(parsed.aggregate_context_limit, Some(250000));
    }

    #[test]
    fn flat_env_key_reaches_nested_slot() {
        let mut v = json!({ "data_plane_standalone_mode": true });
        apply(&mut v, EnvOverlayMode::Fallback);

        assert_eq!(
            v.get("data_plane").and_then(|d| d.get("standalone_mode")),
            Some(&Value::Bool(true)),
        );
        let parsed: SalukiOnly = serde_json::from_value(v).expect("deserializes");
        assert_eq!(parsed.data_plane.standalone_mode, Some(true));
    }

    #[test]
    fn deep_flat_env_key_reaches_nested_slot() {
        let mut v = json!({ "data_plane_checks_enabled": true });
        apply(&mut v, EnvOverlayMode::Fallback);

        assert_eq!(
            v.get("data_plane")
                .and_then(|d| d.get("checks"))
                .and_then(|c| c.get("enabled")),
            Some(&Value::Bool(true)),
        );
        let parsed: SalukiOnly = serde_json::from_value(v).expect("deserializes");
        assert_eq!(parsed.data_plane.checks.enabled, Some(true));
    }

    #[test]
    fn mode_governs_relocation() {
        let seed = || json!({ "data_plane_standalone_mode": true, "data_plane": { "standalone_mode": false } });
        let nested = |v: &Value| v.get("data_plane").and_then(|d| d.get("standalone_mode")).cloned();

        let mut disabled = seed();
        apply(&mut disabled, EnvOverlayMode::Disabled);
        assert_eq!(nested(&disabled), Some(Value::Bool(false)));

        let mut fallback = seed();
        apply(&mut fallback, EnvOverlayMode::Fallback);
        assert_eq!(nested(&fallback), Some(Value::Bool(false)));

        let mut over = seed();
        apply(&mut over, EnvOverlayMode::Override);
        assert_eq!(nested(&over), Some(Value::Bool(true)));

        let mut fill = json!({ "data_plane_standalone_mode": true });
        apply(&mut fill, EnvOverlayMode::Fallback);
        assert_eq!(nested(&fill), Some(Value::Bool(true)));
    }
}
