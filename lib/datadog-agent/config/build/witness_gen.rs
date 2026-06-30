//! Build-time codegen for the Datadog configuration witness trait and driver.
//!
//! Generates, from the same `support: full` / `support: partial` overlay key set that drives
//! `datadog_config_gen`, a `DatadogConfigWitness` trait with one `consume_<key>` method per
//! supported key plus a fallible `drive` function that calls every method exactly once.
//!
//! Rather than reconstruct the `typify` struct-naming convention from dotted key segments, this
//! generator PARSES the already-written `src/generated/datadog_configuration.rs` with `syn` and
//! navigates the real struct tree by field name. Each non-final path segment is a section field
//! (a nested `DatadogConfiguration*` struct, made non-optional by `datadog_config_gen`), so the
//! driver navigates straight to the leaf with no defaulting at the call site. The final segment's
//! field type is the `consume` method's by-value parameter type.
use std::collections::BTreeMap;
use std::path::Path;

use datadog_agent_config_overlay_model::{KnownEntry, SchemaOverlay};
use syn::{Fields, Item, Type};

/// A single field of a generated section struct: its Rust type and, when the type is a nested
/// `DatadogConfiguration*` struct, the name of that section struct.
struct FieldInfo {
    /// The field's Rust type rendered as source text (verbatim from the generated file).
    type_text: String,
    /// `Some(struct_name)` when this field is a nested-section struct.
    section_struct: Option<String>,
}

/// `struct name -> { field name -> field info }`, built from the generated configuration file.
type StructMap = BTreeMap<String, BTreeMap<String, FieldInfo>>;

/// Resolution of one supported dotted key against the generated struct tree.
struct ResolvedKey {
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
}

pub fn generate(overlay: &SchemaOverlay, manifest_dir: &Path) {
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

    let path = manifest_dir.join("src/generated/witness.rs");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing != body {
        std::fs::write(&path, &body).unwrap_or_else(|e| panic!("cannot write {}: {}", path.display(), e));
    }

    // Canonicalize to the repo's rustfmt style. Best-effort: a missing rustfmt is not fatal.
    let _ = std::process::Command::new("rustfmt")
        .arg("--edition")
        .arg("2021")
        .arg(&path)
        .status();
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
    let mut out = StructMap::new();
    for item in &file.items {
        let Item::Struct(s) = item else { continue };
        let name = s.ident.to_string();
        if !name.starts_with("DatadogConfiguration") {
            continue;
        }
        let Fields::Named(named) = &s.fields else { continue };
        let mut fields = BTreeMap::new();
        for field in &named.named {
            let ident = field.ident.as_ref().expect("named field has an identifier").to_string();
            let type_text = type_to_string(&field.ty);
            let section_struct = section_inner(&field.ty);
            fields.insert(
                ident,
                FieldInfo {
                    type_text,
                    section_struct,
                },
            );
        }
        out.insert(name, fields);
    }
    out
}

/// If `ty` names a nested `DatadogConfiguration*` section struct, return that struct name.
///
/// Sections are non-optional (`datadog_config_gen` strips the `Option`), so this matches a bare
/// `DatadogConfiguration*` path. Leaf fields (`String`, `Vec<_>`, `Option<String>`, ...) return
/// `None`.
fn section_inner(ty: &Type) -> Option<String> {
    let Type::Path(tp) = ty else { return None };
    let ident = tp.path.segments.last()?.ident.to_string();
    if ident.starts_with("DatadogConfiguration") {
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
                method,
                type_text: info.type_text.clone(),
                sections,
                leaf_field: (*segment).to_string(),
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
    out.push_str("use crate::translate_error::TranslateError;\n\n");

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
    out.push_str("    /// Returns the first translation error recorded while consuming, if any.\n");
    out.push_str("    fn translate_error(&mut self) -> Option<TranslateError> {\n");
    out.push_str("        None\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    // Driver.
    out.push_str("/// Drives a consumer over every supported key in `config`, calling each `consume_<key>` exactly\n");
    out.push_str("/// once with the value navigated from `config`.\n");
    out.push_str("///\n");
    out.push_str("/// # Errors\n");
    out.push_str("///\n");
    out.push_str("/// Returns the first [`TranslateError`] the consumer recorded, surfaced after all keys are\n");
    out.push_str("/// consumed.\n");
    out.push_str("pub fn drive(config: &DatadogConfiguration, consumer: &mut impl DatadogConfigWitness) -> Result<(), TranslateError> {\n");

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
    out.push_str("    match consumer.translate_error() {\n");
    out.push_str("        Some(e) => Err(e),\n");
    out.push_str("        None => Ok(()),\n");
    out.push_str("    }\n");
    out.push_str("}\n");

    out
}
