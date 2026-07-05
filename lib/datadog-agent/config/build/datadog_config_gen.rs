//! Build-time codegen for the typed Datadog configuration deserializer.
//!
//! Projects the Datadog Agent schema down to the keys the overlay marks `support: full` or
//! `support: partial`, preserving the YAML nesting, and hands that pruned JSON Schema to `typify`
//! to generate the nested `DatadogConfiguration` struct tree.
//!
//! The overlay decides *which* keys appear; `typify` decides *how* the Rust types are shaped. We
//! deliberately do not rewrite schema types here (for example, the schema types ports as `number`,
//! so they surface as `f64`); semantic refinement belongs in the downstream translator, not in this
//! faithful mirror of the vendored schema.
//!
//! The one exception is `format: duration`: those leaves are retyped to `std::time::Duration` with a
//! deserializer that accepts either nanoseconds or a Go duration string, because the schema encodes
//! a duration inconsistently (a `number` typed key with a Go-duration-string default) and typify
//! cannot render it faithfully. Parsing the duration once, at the deserialization boundary, keeps a
//! bare number from reaching the translator as an ambiguous unit.
//!
//! Every `Vec<String>` leaf also gets a shape-tolerant deserializer: a config file or the remote
//! Agent stream supplies a real sequence, but an environment variable supplies a single
//! space-separated string (`DD_DOGSTATSD_TAGS="env:prod team:core"`), and the field must accept
//! both. Handling that in the deserializer (rather than pre-splitting env values elsewhere) keeps
//! the concern in one place; see `stringlistize` and `crate::list_de`.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use datadog_agent_config_overlay_model::schema_gen::{FieldInfo, FieldType};
use datadog_agent_config_overlay_model::{load_resolved_schema, KnownEntry, SchemaOverlay};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use syn::visit_mut::{self, VisitMut};
use syn::{parse_quote, Attribute, Item, Meta};
use typify::{TypeSpace, TypeSpaceSettings};

/// JSON Schema keywords copied verbatim from a leaf setting node into the pruned schema. Everything
/// else (`node_type`, `env_vars`, `tags`, `visibility`, ...) is Datadog-specific and ignored.
const LEAF_KEYWORDS: &[&str] = &[
    "type",
    "default",
    "items",
    "additionalProperties",
    "description",
    "enum",
];

pub fn generate(
    overlay: &SchemaOverlay, schema_path: &Path, schema_map: &IndexMap<String, FieldInfo>, manifest_dir: &Path,
) {
    let supported = supported_keys(overlay);
    let override_default = override_default_keys(overlay);

    // Load the schema with all `$ref` files inlined (apm_config, multi_region_failover, ...).
    // Without $ref resolution those subsystem keys are silently absent and the witness driver
    // cannot cover them. Delegate to overlay-model's resolver rather than duplicating it.
    let schema_yaml = load_resolved_schema(schema_path).unwrap_or_else(|e| panic!("failed to load schema: {e}"));
    let schema: Value =
        serde_json::to_value(schema_yaml).unwrap_or_else(|e| panic!("failed to convert schema to JSON: {e}"));
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("schema root must have a 'properties' mapping");

    // `durations` maps each duration leaf's field name to its default in nanoseconds; `leaf_names`
    // counts every emitted leaf so we can reject a duration whose name is ambiguous (see below).
    let mut durations: BTreeMap<String, u64> = BTreeMap::new();
    let mut leaf_names: BTreeMap<String, usize> = BTreeMap::new();
    let pruned_properties = prune(
        properties,
        "",
        &supported,
        &override_default,
        schema_map,
        &mut durations,
        &mut leaf_names,
    );

    // `durationize` matches duration fields by name and `duration_defaults` names its functions the
    // same way, so a duration leaf that shares its name with any other field is unresolvable. Fail
    // the build loudly rather than silently mis-typing or mis-defaulting a collision.
    for name in durations.keys() {
        let count = leaf_names.get(name).copied().unwrap_or_default();
        assert!(
            count == 1,
            "duration leaf `{name}` shares its name with {} other config field(s); durationize keys \
             by field name and cannot disambiguate — switch duration handling to full-path keys",
            count - 1
        );
    }

    let mut root = Map::new();
    root.insert(
        "$schema".into(),
        Value::String("http://json-schema.org/draft-07/schema#".into()),
    );
    root.insert("title".into(), Value::String("DatadogConfiguration".into()));
    root.insert("type".into(), Value::String("object".into()));
    root.insert("properties".into(), Value::Object(pruned_properties));
    let pruned_schema = Value::Object(root);

    let aliases = field_aliases(overlay);
    let body = render(pruned_schema, &aliases, &durations);

    let mut out = String::new();
    out.push_str("// @generated by build.rs from core_schema.yaml + schema_overlay.yaml — DO NOT EDIT\n");
    out.push_str("// Regenerate by running `cargo build -p datadog-agent-config`.\n");
    out.push_str("#![allow(clippy::all, dead_code, unused)]\n\n");
    out.push_str("// Including the schema documentation here is noisy and causes issues with doc lints.\n");
    out.push_str("#![allow(missing_docs)]\n\n");
    out.push_str(&body);

    let path = manifest_dir.join("src/generated/datadog_configuration.rs");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing != out {
        std::fs::write(&path, out).unwrap_or_else(|e| panic!("cannot write {}: {}", path.display(), e));
    }
}

/// Collect field-name -> alias list for every supported entry that has `additional_yaml_paths`.
///
/// Only flat (non-dotted) aliases are collected; the overlay validator rejects dotted ones.
fn field_aliases(overlay: &SchemaOverlay) -> HashMap<String, Vec<String>> {
    let mut map = HashMap::new();
    for (key, entry) in &overlay.inventory {
        let paths = match entry {
            KnownEntry::Full(f) => &f.test_support.additional_yaml_paths,
            KnownEntry::Partial(p) => &p.test_support.additional_yaml_paths,
            _ => continue,
        };
        if !paths.is_empty() {
            map.insert(key.clone(), paths.clone());
        }
    }
    map
}

/// Collect the dotted paths of every `support: full` / `support: partial` overlay entry.
fn supported_keys(overlay: &SchemaOverlay) -> HashSet<String> {
    overlay
        .inventory
        .iter()
        .filter(|(_, entry)| matches!(entry, KnownEntry::Full(_) | KnownEntry::Partial(_)))
        .map(|(key, _)| key.clone())
        .collect()
}

/// Collect the dotted paths of supported entries flagged `saluki_overrides_default`.
///
/// For these keys the schema `default` is omitted from the pruned leaf, so typify emits an
/// absence-aware `Option<T>` rather than a concrete field defaulted to the Agent value. The
/// translator then supplies ADP's effective default when the key is absent (a `None`) while any
/// operator-set value round-trips as `Some`. The classifier is unaffected: it reads the schema
/// default from `schema_map`, not from this pruned copy.
fn override_default_keys(overlay: &SchemaOverlay) -> HashSet<String> {
    overlay
        .inventory
        .iter()
        .filter(|(_, entry)| match entry {
            KnownEntry::Full(f) => f.saluki_overrides_default,
            KnownEntry::Partial(p) => p.saluki_overrides_default,
            _ => false,
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// Recursively project the schema's `properties` onto the supported key set.
///
/// Section nodes are kept only when they contain at least one supported descendant. Leaf settings
/// are kept only when their dotted path is supported, carrying just the JSON Schema keywords typify
/// understands.
fn prune(
    properties: &Map<String, Value>, prefix: &str, supported: &HashSet<String>, override_default: &HashSet<String>,
    schema_map: &IndexMap<String, FieldInfo>, durations: &mut BTreeMap<String, u64>,
    leaf_names: &mut BTreeMap<String, usize>,
) -> Map<String, Value> {
    let mut out = Map::new();

    for (name, node) in properties {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };

        if let Some(child_props) = node.get("properties").and_then(Value::as_object) {
            let pruned_children = prune(
                child_props,
                &path,
                supported,
                override_default,
                schema_map,
                durations,
                leaf_names,
            );
            if pruned_children.is_empty() {
                continue;
            }

            let mut section = Map::new();
            section.insert(
                "type".into(),
                node.get("type")
                    .cloned()
                    .unwrap_or_else(|| Value::String("object".into())),
            );
            if let Some(description) = node.get("description") {
                section.insert("description".into(), description.clone());
            }
            section.insert("properties".into(), Value::Object(pruned_children));
            out.insert(name.clone(), Value::Object(section));
        } else if supported.contains(&path) {
            *leaf_names.entry(name.clone()).or_default() += 1;

            let is_duration = schema_map
                .get(&path)
                .is_some_and(|info| matches!(info.value_type, FieldType::Duration));

            let mut leaf = Map::new();
            if is_duration {
                // Hand typify a plain integer with a nanosecond default so it emits a non-optional
                // field it can render (it cannot coerce the schema's Go-duration string default into
                // a number). `durationize` later rewrites the field to `std::time::Duration` with a
                // duration-aware deserializer, and the real default is parsed from the schema string
                // to nanoseconds at build time.
                let nanos = duration_default_nanos(schema_map.get(&path), &path);
                durations.insert(name.clone(), nanos);
                leaf.insert("type".into(), Value::String("integer".into()));
                leaf.insert("default".into(), Value::from(nanos));
            } else {
                // A key flagged `saluki_overrides_default` drops its schema `default` here so typify
                // renders it as `Option<T>`; ADP owns the effective default downstream.
                let drop_default = override_default.contains(&path);
                for keyword in LEAF_KEYWORDS {
                    if *keyword == "default" && drop_default {
                        continue;
                    }
                    if let Some(value) = node.get(*keyword) {
                        leaf.insert((*keyword).to_string(), value.clone());
                    }
                }
            }
            out.insert(name.clone(), Value::Object(leaf));
        }
    }

    out
}

/// Parses a duration leaf's schema default into nanoseconds, or `0` when the schema omits one.
///
/// Schema duration defaults are JSON string literals such as `"10s"` (mirroring the classifier's
/// `schema_default_expr`). A default that cannot be parsed is a schema bug, so it fails the build
/// here rather than surfacing at runtime.
fn duration_default_nanos(info: Option<&FieldInfo>, path: &str) -> u64 {
    let Some(default) = info.and_then(|info| info.default.as_ref()) else {
        return 0;
    };
    let default_value: Value =
        serde_json::from_str(default).unwrap_or_else(|e| panic!("duration default for '{path}' must be JSON: {e}"));
    let default_str = default_value
        .as_str()
        .unwrap_or_else(|| panic!("duration default for '{path}' must be a JSON string"));
    go_duration::parse_duration(default_str)
        .unwrap_or_else(|e| panic!("duration default '{default_str}' for '{path}' is invalid: {e}"))
        .as_nanos() as u64
}

/// Run typify over the pruned schema and pretty-print the generated module body.
fn render(pruned_schema: Value, aliases: &HashMap<String, Vec<String>>, durations: &BTreeMap<String, u64>) -> String {
    let root_schema: schemars::schema::RootSchema =
        serde_json::from_value(pruned_schema).expect("pruned schema is not a valid JSON Schema document");

    let mut settings = TypeSpaceSettings::default();
    settings.with_struct_builder(false);

    let mut type_space = TypeSpace::new(&settings);
    type_space
        .add_root_schema(root_schema)
        .expect("typify failed to generate types from the pruned schema");

    let mut file = syn::parse2::<syn::File>(type_space.to_stream())
        .expect("typify produced a token stream that is not a valid Rust file");

    for item in &mut file.items {
        strip_docs_in_item(item);
    }

    inject_serde_aliases(&mut file, aliases);

    // Shorten typify's fully-qualified prelude paths (`::std::option::Option` -> `Option`, etc.) so
    // the data model reads cleanly. The serde derive paths and the boilerplate `error` module's
    // `::std::fmt`/`Cow` paths are left untouched (shortening `::std::fmt::Result` would collide
    // with the prelude `Result`).
    let mut shortener = PathShortener::default();
    shortener.visit_file_mut(&mut file);

    materialize_sections(&mut file);
    durationize(&mut file, durations);
    stringlistize(&mut file);
    strip_section_prefixes(&mut file);

    let rendered = blank_lines_between_fields(&prettyplease::unparse(&file));
    let rendered = blank_lines_between_items(&rendered);

    let mut body = String::new();
    if shortener.needs_hashmap {
        body.push_str("use std::collections::HashMap;\n\n");
    }
    body.push_str(&rendered);
    body.push_str(&duration_defaults_module(durations));
    body
}

/// Retype every duration leaf to `std::time::Duration` and give it a duration-aware deserializer.
///
/// typify emitted these as integer fields (see `prune`); here we swap the type and replace their
/// serde attributes with a `deserialize_with` that accepts nanoseconds or a Go duration string, plus
/// a `default` pointing at the matching `duration_defaults` function. We also rewrite the field's
/// value in the generated `Default` implementation, which otherwise assigns the integer default
/// typify computed (a type mismatch for any non-zero duration). Fields are matched by name, which
/// `generate` has already proven unambiguous.
fn durationize(file: &mut syn::File, durations: &BTreeMap<String, u64>) {
    if durations.is_empty() {
        return;
    }
    for item in &mut file.items {
        let Item::Struct(s) = item else { continue };
        let syn::Fields::Named(fields) = &mut s.fields else {
            continue;
        };
        for field in &mut fields.named {
            let Some(ident) = &field.ident else { continue };
            if !durations.contains_key(&ident.to_string()) {
                continue;
            }
            let default_path = format!("duration_defaults::{ident}");
            field.ty = parse_quote!(std::time::Duration);
            field.attrs.retain(|attr| !attr.path().is_ident("serde"));
            field.attrs.push(parse_quote!(
                #[serde(default = #default_path, deserialize_with = "crate::duration_de::deserialize_go_duration")]
            ));
        }
    }

    DurationDefaultInit { durations }.visit_file_mut(file);
}

/// Rewrites duration field values in generated `Default` implementations to call the matching
/// `duration_defaults` function, keeping the `Default` implementation consistent with the serde
/// default and well-typed once the field is a `Duration`.
struct DurationDefaultInit<'a> {
    durations: &'a BTreeMap<String, u64>,
}

impl VisitMut for DurationDefaultInit<'_> {
    fn visit_expr_struct_mut(&mut self, node: &mut syn::ExprStruct) {
        visit_mut::visit_expr_struct_mut(self, node);
        for field in &mut node.fields {
            let syn::Member::Named(ident) = &field.member else {
                continue;
            };
            if self.durations.contains_key(&ident.to_string()) {
                let path: syn::Path = parse_quote!(duration_defaults::#ident);
                field.expr = parse_quote!(#path());
            }
        }
    }
}

/// Render the `duration_defaults` module: one function per duration leaf returning its schema
/// default as a `std::time::Duration` built from the nanoseconds computed at build time.
fn duration_defaults_module(durations: &BTreeMap<String, u64>) -> String {
    if durations.is_empty() {
        return String::new();
    }
    let mut module = String::from("\nmod duration_defaults {\n");
    for (name, nanos) in durations {
        module.push_str(&format!(
            "    pub(super) fn {name}() -> std::time::Duration {{\n        std::time::Duration::from_nanos({nanos})\n    }}\n"
        ));
    }
    module.push_str("}\n");
    module
}

/// Give every `Vec<String>` leaf a shape-tolerant deserializer.
///
/// String lists arrive as a real sequence from a file or the remote Agent stream, but as a single
/// space-separated string from an environment variable (`DD_DOGSTATSD_TAGS="a b"`), so each field
/// must accept both. We push an extra `#[serde(deserialize_with = ...)]` attribute rather than
/// replacing the field's existing serde attributes: serde merges multiple `#[serde(...)]`, so the
/// field keeps its `default`/`skip_serializing_if`/`alias` and only gains the tolerant reader (the
/// same additive technique as `inject_serde_aliases`).
///
/// Fields are matched by their Rust type, not by name: `Vec<String>` is unambiguous, needs no
/// schema lookup, and correctly skips `HashMap<String, Vec<String>>` (a map, not a list) and the
/// duration leaves (already retyped to `Duration`). Runs after `PathShortener`, so the type reads
/// as `Vec<String>` rather than `::std::vec::Vec<::std::string::String>`.
fn stringlistize(file: &mut syn::File) {
    for item in &mut file.items {
        let Item::Struct(s) = item else { continue };
        let syn::Fields::Named(fields) = &mut s.fields else {
            continue;
        };
        for field in &mut fields.named {
            if !is_vec_string(&field.ty) {
                continue;
            }
            field.attrs.push(parse_quote!(
                #[serde(deserialize_with = "crate::list_de::deserialize_space_separated_or_seq")]
            ));
        }
    }
}

/// Returns whether `ty` is exactly `Vec<String>` (the shape typify emits for a string-list leaf
/// once `PathShortener` has collapsed the prelude paths).
fn is_vec_string(ty: &syn::Type) -> bool {
    let syn::Type::Path(tp) = ty else { return false };
    let Some(last) = tp.path.segments.last() else {
        return false;
    };
    if last.ident != "Vec" {
        return false;
    }
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return false;
    };
    let Some(syn::GenericArgument::Type(syn::Type::Path(inner))) = args.args.first() else {
        return false;
    };
    inner.path.segments.last().is_some_and(|seg| seg.ident == "String")
}

/// Make nested-section fields non-optional with `#[serde(default)]`.
///
/// typify renders each non-required object property as `Option<SectionStruct>`. Every section
/// struct derives `Default`, so dropping the `Option` and defaulting an absent section materializes
/// it (recursively applying leaf defaults). This lets the witness driver navigate sections directly
/// (`config.a.b.leaf`) without unwrapping or synthesizing defaults at the call site; defaults live
/// here, in the data model. Optional scalar leaves (`Option<String>`, etc.) are left untouched.
fn materialize_sections(file: &mut syn::File) {
    for item in &mut file.items {
        let Item::Struct(s) = item else { continue };
        let syn::Fields::Named(fields) = &mut s.fields else {
            continue;
        };
        for field in &mut fields.named {
            let Some(inner) = option_section_inner(&field.ty) else {
                continue;
            };
            field.ty = inner;
            // Replace the field's serde attributes: the original carried
            // `skip_serializing_if = "Option::is_none"`, invalid once the field is no longer an
            // `Option`. A bare `#[serde(default)]` is all a section field needs.
            field.attrs.retain(|attr| !attr.path().is_ident("serde"));
            field.attrs.push(parse_quote!(#[serde(default)]));
        }
    }
}

/// Rename every nested section struct that `typify` names `DatadogConfiguration<Path>` to its bare
/// path name (`DatadogConfigurationApmConfigObfuscation` -> `ApmConfigObfuscation`), leaving the
/// root `DatadogConfiguration` untouched.
///
/// `typify` names a nested type by concatenating the root schema title with the property path. That
/// stutters against the crate name (`datadog_agent_config::DatadogConfiguration...`) and buries the
/// descriptive segments behind `Configuration`. Stripping the prefix leaves the full path (so names
/// stay unique) while reading cleanly at every field and reference site. Runs last, after the
/// passes above navigate the tree by that prefix; downstream detection (`witness_gen`) keys off the
/// set of struct names instead.
fn strip_section_prefixes(file: &mut syn::File) {
    struct Renamer;
    impl VisitMut for Renamer {
        fn visit_ident_mut(&mut self, ident: &mut syn::Ident) {
            let name = ident.to_string();
            if let Some(stripped) = name.strip_prefix("DatadogConfiguration") {
                if !stripped.is_empty() {
                    *ident = syn::Ident::new(stripped, ident.span());
                }
            }
        }
    }
    Renamer.visit_file_mut(file);
}

/// If `ty` is `Option<DatadogConfiguration...>` (a nested section), return the inner section type.
///
/// Runs during `materialize_sections`, before `strip_section_prefixes`, so section types still
/// carry the `DatadogConfiguration` prefix `typify` emits here.
fn option_section_inner(ty: &syn::Type) -> Option<syn::Type> {
    let syn::Type::Path(tp) = ty else { return None };
    let last = tp.path.segments.last()?;
    if last.ident != "Option" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    let syn::GenericArgument::Type(inner) = args.args.first()? else {
        return None;
    };
    let syn::Type::Path(inner_path) = inner else {
        return None;
    };
    if inner_path
        .path
        .segments
        .last()?
        .ident
        .to_string()
        .starts_with("DatadogConfiguration")
    {
        Some(inner.clone())
    } else {
        None
    }
}

/// Add `#[serde(alias = "...")]` attributes to fields that have `additional_yaml_paths` in the
/// overlay. Walks every struct in the file and checks field names against the alias map.
fn inject_serde_aliases(file: &mut syn::File, aliases: &HashMap<String, Vec<String>>) {
    if aliases.is_empty() {
        return;
    }
    for item in &mut file.items {
        let Item::Struct(s) = item else { continue };
        let syn::Fields::Named(fields) = &mut s.fields else {
            continue;
        };
        for field in &mut fields.named {
            let Some(ident) = &field.ident else { continue };
            let Some(field_aliases) = aliases.get(&ident.to_string()) else {
                continue;
            };
            field
                .attrs
                .push(parse_quote!(#[doc = " Alias defined in schema overlay."]));
            for alias in field_aliases {
                field.attrs.push(parse_quote!(#[serde(alias = #alias)]));
            }
        }
    }
}

/// Rewrites the fully qualified prelude paths that typify emits to their short, in-scope names.
#[derive(Default)]
struct PathShortener {
    needs_hashmap: bool,
}

impl VisitMut for PathShortener {
    fn visit_path_mut(&mut self, path: &mut syn::Path) {
        // Recurse first so generic arguments (for example `HashMap<String, Vec<String>>`) are
        // shortened before the outer path is collapsed to its final segment.
        visit_mut::visit_path_mut(self, path);

        if path.leading_colon.is_none() {
            return;
        }
        let idents: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        let segments: Vec<&str> = idents.iter().map(String::as_str).collect();
        let shorten = match segments.as_slice() {
            ["std", "option", "Option"]
            | ["std", "string", "String"]
            | ["std", "vec", "Vec"]
            | ["std", "boxed", "Box"]
            | ["std", "default", "Default"]
            | ["std", "convert", "TryFrom"]
            | ["std", "convert", "TryInto"] => true,
            ["std", "collections", "HashMap"] => {
                self.needs_hashmap = true;
                true
            }
            _ => false,
        };
        if shorten {
            let last = path.segments.last().cloned().expect("non-empty path");
            path.leading_colon = None;
            path.segments = std::iter::once(last).collect();
        }
    }
}

/// Insert a blank line between top-level items (structs, implementations, modules) for readability.
///
/// Like field spacing, `prettyplease` emits no blank lines between items. Every top-level item ends
/// with a closing brace in the first column, so a blank line is inserted after each `}` that starts
/// at column zero, unless it is the final item.
fn blank_lines_between_items(body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 64);

    for (i, line) in lines.iter().enumerate() {
        out.push((*line).to_string());

        if *line == "}" {
            if let Some(next) = lines.get(i + 1) {
                if !next.trim().is_empty() {
                    out.push(String::new());
                }
            }
        }
    }

    let mut result = out.join("\n");
    result.push('\n');
    result
}

/// Recursively strip all `#[doc]` attributes from structs, enums, and modules.
fn strip_docs_in_item(item: &mut Item) {
    match item {
        Item::Struct(s) => {
            s.attrs.retain(|a| !is_doc(a));
            for field in &mut s.fields {
                field.attrs.retain(|a| !is_doc(a));
            }
        }
        Item::Enum(e) => {
            e.attrs.retain(|a| !is_doc(a));
            for variant in &mut e.variants {
                variant.attrs.retain(|a| !is_doc(a));
            }
        }
        Item::Mod(m) => {
            if let Some((_, items)) = &mut m.content {
                for inner in items {
                    strip_docs_in_item(inner);
                }
            }
        }
        _ => {}
    }
}

fn is_doc(attr: &Attribute) -> bool {
    matches!(&attr.meta, Meta::NameValue(nv) if nv.path.is_ident("doc"))
}

/// Insert a blank line between struct fields for readability.
///
/// syn/prettyplease do not model blank lines, so this is a text pass over the rendered output. It
/// only acts inside struct field lists: after a field's final line (the one ending in `,`), unless
/// the next line closes the struct.
fn blank_lines_between_fields(body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_struct = false;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        out.push((*line).to_string());

        if !in_struct {
            if trimmed.starts_with("pub struct ") && trimmed.ends_with('{') {
                in_struct = true;
            }
            continue;
        }

        if trimmed == "}" {
            in_struct = false;
            continue;
        }

        // A line ending in `,` terminates a field (attribute lines end in `]`, the `pub name:`
        // line of a wrapped field ends in `:`). Separate it from the next field.
        if trimmed.ends_with(',') {
            let next = lines.get(i + 1).map(|l| l.trim()).unwrap_or("");
            if next != "}" {
                out.push(String::new());
            }
        }
    }

    let mut result = out.join("\n");
    result.push('\n');
    result
}
