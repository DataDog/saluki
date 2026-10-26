//! Build-time codegen for the Datadog configuration witness trait and driver.
//!
//! Generates, from the same `support: full` / `support: partial` overlay key set that drives
//! `datadog_config_gen`, a `DatadogConfigWitness` trait with one `consume_<key>` method per
//! supported key plus a fallible `drive` function that calls every method exactly once.
//!
//! Rather than reconstruct the `typify` struct-naming convention from dotted key segments, this
//! generator PARSES the already-written `src/generated/datadog_configuration.rs` with `syn` and
//! navigates the real struct tree by field name. Each non-final path segment is a section field (a
//! nested struct, made non-optional by `datadog_config_gen`), so the driver navigates straight to
//! the leaf with no defaulting at the call site. The final segment's field type is the `consume`
//! method's by-value parameter type.
//!
//! Section structs carry no naming convention (`datadog_config_gen` strips the `DatadogConfiguration`
//! prefix `typify` emits down to the bare path name), so a field is a section only when its type
//! names one of the structs defined in the file.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use datadog_agent_config_overlay_model::{KnownEntry, SchemaOverlay};
use syn::{Fields, Item, Type};

/// A single field of a generated section struct: its Rust type and, when the type names a nested
/// section struct, the name of that struct.
struct FieldInfo {
    /// The field's Rust type rendered as source text (verbatim from the generated file).
    type_text: String,
    /// `Some(struct_name)` when this field is a nested-section struct.
    section_struct: Option<String>,
    /// Every `#[serde(alias = "...")]` value carried on this field.
    aliases: Vec<String>,
}

/// `struct name -> { field name -> field info }`, built from the generated configuration file.
type StructMap = BTreeMap<String, BTreeMap<String, FieldInfo>>;

/// Resolution of one supported dotted key against the generated struct tree.
struct ResolvedKey {
    /// The dotted key, exactly as written in the schema overlay.
    dotted: String,
    /// The `consume_<sanitized>` method name.
    method: String,
    /// The by-value parameter type, verbatim from the generated file.
    type_text: String,
    /// The section field names traversed from the root to the leaf, parent first (for example
    /// `["apm_config", "obfuscation", "credit_cards"]`). Navigated directly: each is a non-optional
    /// nested struct field.
    sections: Vec<String>,
    /// The field name of the leaf on its owning struct (root or the innermost section).
    leaf_field: String,
    /// Every `#[serde(alias = "...")]` value on the leaf field.
    aliases: Vec<String>,
}

/// Writes `witness.rs` to `src/generated/` and `leaves.rs` to `out_dir`.
pub fn generate(overlay: &SchemaOverlay, manifest_dir: &Path, out_dir: &Path) {
    let generated_path = manifest_dir.join("src/generated/datadog_configuration.rs");
    let src = std::fs::read_to_string(&generated_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", generated_path.display(), e));
    let file =
        syn::parse_file(&src).unwrap_or_else(|e| panic!("failed to parse {} as Rust: {}", generated_path.display(), e));

    let structs = collect_structs(&file);

    let mut supported: Vec<String> = supported_keys(overlay);
    supported.sort();

    let resolved: Vec<ResolvedKey> = supported.iter().map(|key| resolve_key(key, &structs)).collect();

    let body = render(&resolved);

    crate::generated_file::write_formatted(&manifest_dir.join("src/generated/witness.rs"), &body);

    let leaves_body = render_leaves(&resolved);
    crate::generated_file::write_formatted(&out_dir.join("leaves.rs"), &leaves_body);
}

/// Collect the dotted paths of every `support: full` / `support: partial` overlay entry.
fn supported_keys(overlay: &SchemaOverlay) -> Vec<String> {
    overlay
        .inventory
        .iter()
        .filter(|(_, entry)| matches!(entry, KnownEntry::Full(_) | KnownEntry::Partial(_)))
        .map(|(key, _)| key.clone())
        .collect()
}

/// Build the `struct name -> { field -> info }` map from the generated configuration file.
fn collect_structs(file: &syn::File) -> StructMap {
    // Every top-level struct is a config struct (the root plus its sections); the only other struct,
    // `error::ConversionError`, is nested inside a module. A field whose type names one of these is a
    // section; anything else (`String`, `Vec<_>`, `Duration`, ...) is a leaf.
    let struct_names: BTreeSet<String> = file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Struct(s) => Some(s.ident.to_string()),
            _ => None,
        })
        .collect();

    let mut out = StructMap::new();
    for item in &file.items {
        let Item::Struct(s) = item else { continue };
        let name = s.ident.to_string();
        let Fields::Named(named) = &s.fields else { continue };
        let mut fields = BTreeMap::new();
        for field in &named.named {
            let ident = field.ident.as_ref().expect("named field has an identifier").to_string();
            let type_text = type_to_string(&field.ty);
            let section_struct = section_inner(&field.ty, &struct_names);
            let aliases = field_aliases(field);
            fields.insert(
                ident,
                FieldInfo {
                    type_text,
                    section_struct,
                    aliases,
                },
            );
        }
        out.insert(name, fields);
    }
    out
}

/// Collects every `#[serde(alias = "...")]` value on a field.
///
/// Scans rendered `#[serde(...)]` arguments instead of parsing `syn::Meta`: the attributes also
/// contain keys such as `default` and `deserialize_with`, which this generator does not need.
fn field_aliases(field: &syn::Field) -> Vec<String> {
    let mut aliases = Vec::new();
    for attr in &field.attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let Ok(list) = attr.meta.require_list() else { continue };
        let rendered = list.tokens.to_string();
        for item in rendered.split(',') {
            let item = item.trim();
            let Some(value) = item.strip_prefix("alias") else {
                continue;
            };
            let Some(value) = value.trim_start().strip_prefix('=') else {
                continue;
            };
            aliases.push(value.trim().trim_matches('"').to_string());
        }
    }
    aliases
}

/// If `ty` names a nested section struct (one of `struct_names`), return that struct name.
///
/// Sections are non-optional (`datadog_config_gen` strips the `Option`), so this matches a bare
/// path. Leaf fields (`String`, `Vec<_>`, `Option<String>`, ...) name no config struct and return
/// `None`.
fn section_inner(ty: &Type, struct_names: &BTreeSet<String>) -> Option<String> {
    let Type::Path(tp) = ty else { return None };
    let ident = tp.path.segments.last()?.ident.to_string();
    if struct_names.contains(&ident) {
        Some(ident)
    } else {
        None
    }
}

/// Render a `syn::Type` to compact source text by round-tripping through `prettyplease`.
///
/// `prettyplease` only formats whole files, so the type is wrapped in a throwaway type alias,
/// printed, and the alias scaffolding stripped back off. This avoids a `quote`/`proc-macro2`
/// build-dependency just to render a type as a string.
fn type_to_string(ty: &Type) -> String {
    let alias: syn::ItemType = syn::parse_quote!(type __WitnessType = #ty;);
    let file = syn::File {
        shebang: None,
        frontmatter: None,
        attrs: Vec::new(),
        items: vec![Item::Type(alias)],
    };
    let rendered = prettyplease::unparse(&file);
    let rendered = rendered.trim();
    let inner = rendered
        .strip_prefix("type __WitnessType = ")
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or_else(|| panic!("unexpected prettyplease type rendering: {rendered}"));
    inner.trim().to_string()
}

/// Navigate the generated struct tree for one supported dotted key.
fn resolve_key(dotted: &str, structs: &StructMap) -> ResolvedKey {
    let segments: Vec<&str> = dotted.split('.').collect();
    let method = format!("consume_{}", dotted.replace('.', "_"));

    let mut current_struct = "DatadogConfiguration".to_string();
    let mut sections: Vec<String> = Vec::new();

    for (i, segment) in segments.iter().enumerate() {
        let fields = structs
            .get(&current_struct)
            .unwrap_or_else(|| panic!("generated struct {current_struct} not found while resolving key {dotted}"));
        let info = fields.get(*segment).unwrap_or_else(|| {
            panic!("field {segment} not found on struct {current_struct} while resolving key {dotted}")
        });

        let is_last = i == segments.len() - 1;
        if is_last {
            if let Some(section) = &info.section_struct {
                panic!(
                    "supported key {dotted} resolves to section struct {section} (a section node, not a leaf \
                     field); only leaf keys may be supported"
                );
            }
            return ResolvedKey {
                dotted: dotted.to_string(),
                method,
                type_text: info.type_text.clone(),
                sections,
                leaf_field: (*segment).to_string(),
                aliases: info.aliases.clone(),
            };
        }

        // Non-final segment: must be a nested section struct.
        let section_struct = info.section_struct.clone().unwrap_or_else(|| {
            panic!(
                "intermediate segment {segment} of key {dotted} is not a section struct (field type {})",
                info.type_text
            )
        });
        sections.push((*segment).to_string());
        current_struct = section_struct;
    }

    unreachable!("loop returns on the final segment for key {dotted}");
}

/// Render the full `witness.rs` module source.
fn render(resolved: &[ResolvedKey]) -> String {
    let mut out = String::new();
    out.push_str("// @generated by build.rs from core_schema.yaml + schema_overlay.yaml — DO NOT EDIT\n");
    out.push_str("// Regenerate by running `cargo build -p datadog-agent-config`.\n");
    out.push_str("#![allow(clippy::all, dead_code, missing_docs, unused)]\n\n");
    out.push_str("use std::collections::HashMap;\n\n");
    out.push_str("use super::datadog_configuration::*;\n");
    out.push_str("use crate::translate_error::{TranslateError, TranslateErrors};\n\n");

    // Trait.
    out.push_str("/// Witness trait over every supported Datadog configuration key.\n");
    out.push_str("///\n");
    out.push_str("/// One `consume_<key>` method per `support: full` / `support: partial` key from the Datadog\n");
    out.push_str("/// schema overlay. This trait must be implemented by hand to ensure that each supported value\n");
    out.push_str("/// from Datadog configuration finds a home in Saluki configuration.\n");
    out.push_str("pub trait DatadogConfigWitness {\n");
    for key in resolved {
        out.push_str(&format!(
            "    fn {}(&mut self, value: {});\n",
            key.method, key.type_text
        ));
    }
    out.push('\n');
    out.push_str("    /// Drains every translation error recorded while consuming.\n");
    out.push_str("    fn translate_errors(&mut self) -> Vec<TranslateError> {\n");
    out.push_str("        Vec::new()\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    // Driver.
    out.push_str("/// Drives a consumer over every supported key in `config`, calling each `consume_<key>` exactly\n");
    out.push_str("/// once with the value navigated from `config`.\n");
    out.push_str("///\n");
    out.push_str("/// # Errors\n");
    out.push_str("///\n");
    out.push_str("/// Returns every [`TranslateError`] the consumer recorded, once all keys are consumed.\n");
    out.push_str("pub fn drive(config: &DatadogConfiguration, consumer: &mut impl DatadogConfigWitness) -> Result<(), TranslateErrors> {\n");

    for key in resolved {
        let mut owner = "config".to_string();
        for field in &key.sections {
            owner = format!("{owner}.{field}");
        }
        out.push_str(&format!(
            "    consumer.{}({}.{}.clone());\n",
            key.method, owner, key.leaf_field
        ));
    }

    out.push('\n');
    out.push_str("    let errors = consumer.translate_errors();\n");
    out.push_str("    if errors.is_empty() {\n");
    out.push_str("        Ok(())\n");
    out.push_str("    } else {\n");
    out.push_str("        Err(TranslateErrors::new(errors))\n");
    out.push_str("    }\n");
    out.push_str("}\n");

    out
}

/// How one leaf's Rust type maps onto a `LeafValue` variant: the variant name, the payload type
/// carried in the `LeafValue<'a>` definition, and how to turn a field access expression (for
/// example `config.apm_config.enabled`) into that variant's accessor expression.
struct LeafKind {
    /// The leaf's Rust type, as rendered from the generated struct.
    rust_type: &'static str,
    /// The `LeafValue` variant name, for example `Bool`.
    variant: &'static str,
    /// The variant's payload type as written in the `LeafValue<'a>` enum definition.
    payload: &'static str,
    /// Wraps a field access expression into this variant's accessor expression.
    wrap: fn(String) -> String,
}

/// Every Rust leaf type with a `LeafValue` variant.
///
/// `LeafValue` declares a variant for each entry, whether or not a supported key has that type, so
/// code that names a variant compiles whatever the overlay supports.
const LEAF_KINDS: &[LeafKind] = &[
    LeafKind {
        rust_type: "bool",
        variant: "Bool",
        payload: "bool",
        wrap: |a| a,
    },
    LeafKind {
        rust_type: "i64",
        variant: "I64",
        payload: "i64",
        wrap: |a| a,
    },
    LeafKind {
        rust_type: "f64",
        variant: "F64",
        payload: "f64",
        wrap: |a| a,
    },
    LeafKind {
        rust_type: "Option<i64>",
        variant: "OptionI64",
        payload: "Option<i64>",
        wrap: |a| a,
    },
    LeafKind {
        rust_type: "std::time::Duration",
        variant: "Duration",
        payload: "std::time::Duration",
        wrap: |a| a,
    },
    LeafKind {
        rust_type: "String",
        variant: "Str",
        payload: "&'a str",
        wrap: |a| format!("{a}.as_str()"),
    },
    LeafKind {
        rust_type: "Option<String>",
        variant: "OptionStr",
        payload: "Option<&'a str>",
        wrap: |a| format!("{a}.as_deref()"),
    },
    LeafKind {
        rust_type: "Vec<String>",
        variant: "StringList",
        payload: "&'a [String]",
        wrap: |a| format!("{a}.as_slice()"),
    },
    LeafKind {
        rust_type: "Vec<::serde_json::Value>",
        variant: "JsonList",
        payload: "&'a [::serde_json::Value]",
        wrap: |a| format!("{a}.as_slice()"),
    },
    LeafKind {
        rust_type: "Vec<HashMap<String, String>>",
        variant: "StringMapList",
        payload: "&'a [HashMap<String, String>]",
        wrap: |a| format!("{a}.as_slice()"),
    },
    LeafKind {
        rust_type: "HashMap<String, f64>",
        variant: "F64Map",
        payload: "&'a HashMap<String, f64>",
        wrap: |a| format!("&{a}"),
    },
    LeafKind {
        rust_type: "::serde_json::Map<String, ::serde_json::Value>",
        variant: "JsonMap",
        payload: "&'a ::serde_json::Map<String, ::serde_json::Value>",
        wrap: |a| format!("&{a}"),
    },
    LeafKind {
        rust_type: "HashMap<String, String>",
        variant: "StringMap",
        payload: "&'a HashMap<String, String>",
        wrap: |a| format!("&{a}"),
    },
    LeafKind {
        rust_type: "HashMap<String, Vec<String>>",
        variant: "StringListMap",
        payload: "&'a HashMap<String, Vec<String>>",
        wrap: |a| format!("&{a}"),
    },
];

/// Renders the `leaves.rs` source: `LeafValue`, `Leaf`, and the sorted `LEAVES` table.
///
/// The table has one entry per supported key.
fn render_leaves(resolved: &[ResolvedKey]) -> String {
    // Sorted by name for a deterministic enum declaration.
    let variants: BTreeMap<&'static str, &'static str> =
        LEAF_KINDS.iter().map(|kind| (kind.variant, kind.payload)).collect();

    let mut out = String::new();
    out.push_str("// @generated by build.rs from core_schema.yaml + schema_overlay.yaml — DO NOT EDIT\n");
    out.push_str("// Regenerated in `OUT_DIR` by every build of `datadog-agent-config`; not checked in.\n\n");
    out.push_str("use std::collections::HashMap;\n\n");
    out.push_str("use super::datadog_configuration::DatadogConfiguration;\n\n");

    // Enum.
    out.push_str("/// A supported configuration leaf's typed value, borrowed from a `DatadogConfiguration`.\n");
    out.push_str("///\n");
    out.push_str("/// One variant per Rust leaf type the generator knows, plus `Uncompared` for any other type.\n");
    out.push_str("/// Pair this with [`LEAVES`] to read any supported dotted key's typed value without cloning it.\n");
    out.push_str("#[derive(Debug, Clone, Copy)]\n");
    out.push_str("pub enum LeafValue<'a> {\n");
    for (variant, payload) in &variants {
        out.push_str(&format!("    /// A `{payload}` leaf.\n"));
        out.push_str(&format!("    {variant}({payload}),\n"));
    }
    out.push_str("    /// A leaf whose Rust type no other variant covers. Its value is not read.\n");
    out.push_str("    Uncompared,\n");
    out.push_str("}\n\n");

    // Descriptor.
    out.push_str("/// One supported configuration leaf: its dotted key, serde aliases, and a borrowing accessor.\n");
    out.push_str("#[derive(Debug, Clone, Copy)]\n");
    out.push_str("pub struct Leaf {\n");
    out.push_str("    /// The dotted key, exactly as written in the schema overlay.\n");
    out.push_str("    pub key: &'static str,\n");
    out.push_str("    /// Every `#[serde(alias = \"...\")]` value this leaf's field also accepts, if any.\n");
    out.push_str("    pub aliases: &'static [&'static str],\n");
    out.push_str("    /// Reads this leaf's typed value out of a `DatadogConfiguration`, borrowing where the type\n");
    out.push_str("    /// allows it.\n");
    out.push_str("    pub get: for<'a> fn(&'a DatadogConfiguration) -> LeafValue<'a>,\n");
    out.push_str("}\n\n");

    // Table.
    out.push_str("/// Every supported configuration leaf, sorted by dotted key.\n");
    out.push_str("///\n");
    out.push_str("/// One entry per `DatadogConfigWitness` method (see `witness.rs`), so the two stay in lockstep:\n");
    out.push_str("/// both are generated from the same schema overlay key set.\n");
    out.push_str("pub static LEAVES: &[Leaf] = &[\n");
    for key in resolved {
        let mut access = "config".to_string();
        for field in &key.sections {
            access = format!("{access}.{field}");
        }
        access = format!("{access}.{}", key.leaf_field);
        let get = match LEAF_KINDS.iter().find(|kind| kind.rust_type == key.type_text) {
            Some(kind) => format!("|config| LeafValue::{}({})", kind.variant, (kind.wrap)(access)),
            None => "|_| LeafValue::Uncompared".to_string(),
        };

        let aliases = if key.aliases.is_empty() {
            "&[]".to_string()
        } else {
            let items: Vec<String> = key.aliases.iter().map(|a| format!("\"{a}\"")).collect();
            format!("&[{}]", items.join(", "))
        };

        out.push_str(&format!(
            "    Leaf {{ key: \"{}\", aliases: {}, get: {} }},\n",
            key.dotted, aliases, get
        ));
    }
    out.push_str("];\n");

    out
}
