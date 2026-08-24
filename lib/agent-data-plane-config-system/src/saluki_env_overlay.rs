//! Reads Saluki-only configuration keys from the environment into the nested `SalukiOnly` shape.
//!
//! This is the Saluki-only counterpart to `datadog_agent_config::apply_datadog_env`. The Datadog
//! side drives a generated table of declared variable names; here there is no table and no alias. A
//! Saluki-only key's environment name is its canonical path in upper case, with the segments joined
//! by underscores and a `DD_` prefix, so the nested slot and its environment form stay in lockstep with
//! the struct by construction. The set of canonical paths is read straight off `SalukiOnly` (see the
//! discovery section below), so it cannot drift from the fields the deserializer actually reads. One
//! legacy key that is now a Datadog-schema alias is applied explicitly to preserve its historical
//! environment spelling.

use std::collections::HashSet;
use std::sync::OnceLock;

use datadog_agent_config::{apply_env_at_path, EnvDecode};
use serde::de::value::{Error as ValueError, SeqDeserializer, StrDeserializer, UnitDeserializer};
use serde::de::{
    DeserializeSeed, Deserializer, EnumAccess, IntoDeserializer, MapAccess, SeqAccess, VariantAccess, Visitor,
};
use serde::Deserialize;
use serde_json::Value;

use crate::saluki_only::{SalukiOnly, JSON_SEQUENCE_MARKER};

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

/// Reads every Saluki-only key from the environment and writes decoded values into `base` at their
/// nested paths. This is the Saluki-only counterpart to
/// `datadog_agent_config::apply_datadog_env`.
///
/// A Saluki-only key's environment name is the Agent's standard form: `DD_` + `UPPER(path)` with the
/// path segments joined by `_`. This surface declares no overridden names. `overwrite` follows the
/// same file-vs-environment precedence as the Datadog reader.
///
/// # Errors
///
/// Returns a message when an environment value is malformed for its leaf's decode strategy.
pub(crate) fn apply_env(base: &mut Value, overwrite: bool) -> Result<(), String> {
    for (path, decode) in leaf_specs() {
        let name = format!("DD_{}", path.join("_").to_uppercase());
        let segments: Vec<&str> = path.iter().map(String::as_str).collect();
        apply_env_at_path(base, &[name.as_str()], &segments, *decode, overwrite)?;
    }

    // `counter_expiry_seconds` predates the typed boundary. It is now a serde alias on the
    // Datadog-schema `dogstatsd_expiry_seconds` field, so it is intentionally absent from
    // `SalukiOnly` and cannot be discovered above.
    apply_env_at_path(
        base,
        &["DD_COUNTER_EXPIRY_SECONDS"],
        &["counter_expiry_seconds"],
        EnvDecode::Integer,
        overwrite,
    )?;
    Ok(())
}

/// The nested path of every `SalukiOnly` leaf.
///
/// The Agent-stream merge uses these alongside the Datadog leaf paths to distinguish a schema
/// section from a leaf when deciding whether to descend or replace wholesale.
pub(crate) fn leaf_paths() -> impl Iterator<Item = &'static [String]> {
    leaf_specs().iter().map(|(path, _)| path.as_slice())
}

/// The canonical leaf paths of `SalukiOnly` paired with each leaf's environment decode strategy,
/// discovered once and cached (they are fixed at compile time).
fn leaf_specs() -> &'static [(Vec<String>, EnvDecode)] {
    static SPECS: OnceLock<Vec<(Vec<String>, EnvDecode)>> = OnceLock::new();
    SPECS.get_or_init(discover_leaf_specs)
}

/// Discovers `SalukiOnly`'s leaf paths by driving its derived `Deserialize` with the recorder.
///
/// A `#[serde(alias)]` field lists both its name and the alias in the `fields` slice serde hands to
/// `deserialize_struct`, and feeding both trips serde's duplicate-field check. The colliding name is
/// the one yielded just before the error, so it is added to `skip` and discovery retried until it
/// runs clean. Discovery depends only on the struct, so any real failure surfaces in tests, never in
/// production.
fn discover_leaf_specs() -> Vec<(Vec<String>, EnvDecode)> {
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
    out: Vec<(Vec<String>, EnvDecode)>,
    skip: &'a HashSet<Vec<String>>,
    last_yielded: Option<Vec<String>>,
}

struct PathRecorder<'c, 's> {
    path: Vec<String>,
    ctx: &'c mut DiscoverCtx<'s>,
}

impl PathRecorder<'_, '_> {
    fn record_leaf(&mut self, decode: EnvDecode) {
        self.ctx.out.push((std::mem::take(&mut self.path), decode));
    }
}

// `PathRecorder` records a leaf at every scalar (and scalar-like) method, descends at `option` and
// `newtype_struct`, and recurses at `struct`. The visited value is thrown away, so each leaf feeds
// the visitor a throwaway of the right shape purely to let deserialization complete. Integers feed
// `1` rather than `0` because a `NonZero` leaf rejects zero, which would abort discovery. `deserialize_any`
// is a leaf: it is where `DurationString` and `ByteSize` land. A fieldless (unit-variant) `enum` is
// also a leaf, recorded as `RawString` since its environment and JSON forms are both a plain string.
// `tuple`, `tuple_struct`, and enum variants carrying data return an error.
macro_rules! record_scalar {
    ($method:ident, $visit:ident, $dummy:expr, $decode:expr) => {
        fn $method<V>(mut self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            self.record_leaf($decode);
            visitor.$visit($dummy)
        }
    };
}

impl<'de> Deserializer<'de> for PathRecorder<'_, '_> {
    type Error = ValueError;

    record_scalar!(deserialize_bool, visit_bool, false, EnvDecode::Bool);
    record_scalar!(deserialize_i8, visit_i8, 1, EnvDecode::Integer);
    record_scalar!(deserialize_i16, visit_i16, 1, EnvDecode::Integer);
    record_scalar!(deserialize_i32, visit_i32, 1, EnvDecode::Integer);
    record_scalar!(deserialize_i64, visit_i64, 1, EnvDecode::Integer);
    record_scalar!(deserialize_i128, visit_i128, 1, EnvDecode::Integer);
    record_scalar!(deserialize_u8, visit_u8, 1, EnvDecode::Integer);
    record_scalar!(deserialize_u16, visit_u16, 1, EnvDecode::Integer);
    record_scalar!(deserialize_u32, visit_u32, 1, EnvDecode::Integer);
    record_scalar!(deserialize_u64, visit_u64, 1, EnvDecode::Integer);
    record_scalar!(deserialize_u128, visit_u128, 1, EnvDecode::Integer);
    record_scalar!(deserialize_f32, visit_f32, 0.0, EnvDecode::Float);
    record_scalar!(deserialize_f64, visit_f64, 0.0, EnvDecode::Float);
    record_scalar!(deserialize_char, visit_char, '\0', EnvDecode::RawString);
    record_scalar!(deserialize_str, visit_str, "", EnvDecode::RawString);
    record_scalar!(deserialize_string, visit_str, "", EnvDecode::RawString);
    record_scalar!(deserialize_bytes, visit_bytes, b"", EnvDecode::RawString);
    record_scalar!(deserialize_byte_buf, visit_bytes, b"", EnvDecode::RawString);

    fn deserialize_unit<V>(mut self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.record_leaf(EnvDecode::RawString);
        visitor.visit_unit()
    }

    fn deserialize_any<V>(mut self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        // `DurationString` and `ByteSize` land here; both accept a raw string, so the environment
        // value is carried through verbatim for the leaf's own `deserialize_any` to interpret. Use
        // a non-zero throwaway so constrained byte-size fields can complete discovery.
        self.record_leaf(EnvDecode::RawString);
        visitor.visit_u64(1)
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V>(mut self, name: &'static str, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        if name == JSON_SEQUENCE_MARKER {
            self.record_leaf(EnvDecode::JsonValue);
            let empty = SeqDeserializer::<_, ValueError>::new(std::iter::empty::<UnitDeserializer<ValueError>>());
            visitor.visit_newtype_struct(empty)
        } else {
            visitor.visit_newtype_struct(self)
        }
    }

    fn deserialize_unit_struct<V>(mut self, _name: &'static str, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.record_leaf(EnvDecode::RawString);
        visitor.visit_unit()
    }

    fn deserialize_seq<V>(mut self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.record_leaf(EnvDecode::StringList);
        visitor.visit_seq(EmptyAccess)
    }

    fn deserialize_map<V>(mut self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.record_leaf(EnvDecode::JsonValue);
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
        mut self, _name: &'static str, variants: &'static [&'static str], visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        // A fieldless enum is a leaf: its environment and JSON forms are both the variant's plain
        // string spelling. Which variant is fed to the visitor does not matter; the recorder
        // discards the result once the leaf is noted.
        self.record_leaf(EnvDecode::RawString);
        let variant = variants.first().copied().unwrap_or_default();
        visitor.visit_enum(UnitVariantAccess { variant })
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

/// Feeds a fixed variant name to a fieldless-enum visitor during leaf discovery.
///
/// Only unit variants are supported. A variant carrying data would need its own path recording, so
/// those forms return an error instead of guessing.
struct UnitVariantAccess {
    variant: &'static str,
}

impl<'de> EnumAccess<'de> for UnitVariantAccess {
    type Error = ValueError;
    type Variant = Self;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self::Variant), Self::Error>
    where
        V: DeserializeSeed<'de>,
    {
        let key: StrDeserializer<'_, ValueError> = self.variant.into_deserializer();
        let value = seed.deserialize(key)?;
        Ok((value, self))
    }
}

impl<'de> VariantAccess<'de> for UnitVariantAccess {
    type Error = ValueError;

    fn unit_variant(self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, _seed: T) -> Result<T::Value, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        Err(unsupported("enum newtype variant"))
    }

    fn tuple_variant<V>(self, _len: usize, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(unsupported("enum tuple variant"))
    }

    fn struct_variant<V>(self, _fields: &'static [&'static str], _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(unsupported("enum struct variant"))
    }
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
        leaf_specs()
            .iter()
            .any(|(p, _)| p.iter().map(String::as_str).eq(segments.iter().copied()))
    }

    #[test]
    fn tracer_descends_through_optional_struct() {
        assert!(has_path(&["ottl_filter_config", "error_mode"]));
        assert!(has_path(&["ottl_filter_config", "traces", "span"]));
    }

    fn assert_unsplit_leaf(field: &str) {
        assert!(has_path(&[field]));
        assert!(
            !leaf_specs()
                .iter()
                .any(|(path, _)| path.len() > 1 && path.first().map(String::as_str) == Some(field)),
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
    fn metric_aggregation_intervals_uses_json_environment_decoding() {
        let (_, decode) = leaf_specs()
            .iter()
            .find(|(path, _)| path.as_slice() == ["metric_aggregation_intervals"])
            .expect("metric aggregation intervals must be a discovered leaf");
        assert_eq!(*decode, EnvDecode::JsonValue);
    }

    #[test]
    fn a_top_level_key_deserializes_from_its_own_name() {
        let v = json!({ "aggregate_context_limit": 250000 });
        let parsed: SalukiOnly = serde_json::from_value(v).expect("deserializes");
        assert_eq!(parsed.aggregate_context_limit, Some(250000));
    }

    #[test]
    fn a_nested_key_deserializes_from_its_canonical_path() {
        let v = json!({ "data_plane": { "standalone_mode": true, "checks": { "enabled": true } } });
        let parsed: SalukiOnly = serde_json::from_value(v).expect("deserializes");
        assert_eq!(parsed.data_plane.standalone_mode, Some(true));
        assert_eq!(parsed.data_plane.checks.enabled, Some(true));
    }

    /// Every nested Saluki-only key in the canonical inventory must exist as a `SalukiOnly` leaf.
    ///
    /// A flat key survives without one: Figment's prefix scan turns `DD_FOO_BAR` into the key
    /// `foo_bar`, which is already the canonical spelling. A *nested* key has no such fallback. The
    /// scan would produce the flat `foo_bar` and nothing places it at `foo.bar`, so discovery off
    /// `SalukiOnly` is the only thing that makes `DD_FOO_BAR` reach it. An inventory key with no
    /// field here is therefore unreachable from the environment, and silently so. That is exactly how
    /// `data_plane.serializer_zstd_compressor_level` lost its documented override.
    #[test]
    fn every_nested_inventory_key_is_a_leaf() {
        let missing: Vec<&str> = datadog_agent_config_overlay_model::saluki_keys::SALUKI_KEYS
            .iter()
            .map(|key| key.yaml_path)
            .filter(|path| path.contains('.'))
            .filter(|path| !has_path(&path.split('.').collect::<Vec<_>>()))
            .collect();

        assert!(
            missing.is_empty(),
            "nested Saluki-only key(s) have no `SalukiOnly` field, so no environment variable \
             reaches them: {missing:?}",
        );
    }
}
