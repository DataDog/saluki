//! Build-time codegen for the Datadog configuration witness trait and driver.
//!
//! Generates, from the same `support: full` / `support: partial` overlay key set that drives
//! `datadog_config_gen`, a `DatadogConfigConsumer` trait with one `consume_<key>` method per
//! supported key plus a fallible `drive` function that calls every method exactly once.
//!
//! Rather than reconstruct typify's struct-naming convention from dotted key segments, this
//! generator PARSES the already-written `src/datadog_configuration.rs` with `syn` and navigates the
//! real struct tree by field name. Each path segment is a snake_case field on the current struct;
//! at a non-final segment the field type is `Option<SomeSectionStruct>`, whose inner struct name is
//! the next node. The final segment's field type is the `consume` method's by-value parameter type.
use std::collections::BTreeMap;
use std::path::Path;

use datadog_agent_config_overlay_model::{KnownEntry, SchemaOverlay};
use syn::{Fields, GenericArgument, Item, PathArguments, Type};

/// A single field of a generated section struct: its Rust type and, when the type is
/// `Option<SectionStruct>`, the name of the nested section struct it points at.
struct FieldInfo {
    /// The field's Rust type rendered as source text (verbatim from the generated file).
    type_text: String,
    /// `Some(struct_name)` when this field is an `Option<SectionStruct>` nested-section pointer.
    section_struct: Option<String>,
}

/// `struct name -> { field name -> field info }`, built from the generated configuration file.
type StructMap = BTreeMap<String, BTreeMap<String, FieldInfo>>;

/// Resolution of one supported dotted key against the generated struct tree.
struct ResolvedKey {
    /// The dotted key (for example, `data_plane.dogstatsd.enabled`).
    dotted: String,
    /// The `consume_<sanitized>` method name.
    method: String,
    /// The by-value parameter type, verbatim from the generated file.
    type_text: String,
    /// Ordered optional sections traversed from the root to reach the leaf, parent first. Each is
    /// `(binding_name, parent_expr_field, section_struct)` where `binding_name` is the `let`
    /// variable and `parent_expr_field` is the field on the parent (root or previous section) that
    /// holds this section.
    sections: Vec<SectionStep>,
    /// The field name of the leaf on its owning struct (root or the innermost section).
    leaf_field: String,
}

/// One optional-section hop while navigating to a leaf.
#[derive(Clone)]
struct SectionStep {
    /// The `let` binding name (the field name, which is unique along any single path).
    binding: String,
    /// The field name on the parent struct that holds this section.
    field: String,
    /// The parent binding name, or `None` when the parent is the root `config`.
    parent: Option<String>,
}

pub fn generate(overlay: &SchemaOverlay, manifest_dir: &Path) {
    let generated_path = manifest_dir.join("src/datadog_configuration.rs");
    let src = std::fs::read_to_string(&generated_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", generated_path.display(), e));
    let file =
        syn::parse_file(&src).unwrap_or_else(|e| panic!("failed to parse {} as Rust: {}", generated_path.display(), e));

    let structs = collect_structs(&file);

    let mut supported: Vec<String> = supported_keys(overlay);
    supported.sort();

    let resolved: Vec<ResolvedKey> = supported.iter().map(|key| resolve_key(key, &structs)).collect();

    let body = render(&resolved);

    let path = manifest_dir.join("src/witness.rs");
    std::fs::write(&path, body).unwrap_or_else(|e| panic!("cannot write {}: {}", path.display(), e));

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
            let section_struct = option_inner_section(&field.ty);
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

/// If `ty` is `Option<DatadogConfiguration...>`, return that inner struct name.
fn option_inner_section(ty: &Type) -> Option<String> {
    let Type::Path(tp) = ty else { return None };
    let last = tp.path.segments.last()?;
    if last.ident != "Option" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    let GenericArgument::Type(Type::Path(inner)) = args.args.first()? else {
        return None;
    };
    let inner_ident = inner.path.segments.last()?.ident.to_string();
    if inner_ident.starts_with("DatadogConfiguration") {
        Some(inner_ident)
    } else {
        None
    }
}

/// Render a `syn::Type` to compact source text by round-tripping through `prettyplease`.
///
/// `prettyplease` only formats whole files, so the type is wrapped in a throwaway type alias,
/// unparsed, and the alias scaffolding stripped back off. This avoids a `quote`/`proc-macro2`
/// build-dependency just to stringify a type.
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
    let mut sections: Vec<SectionStep> = Vec::new();
    let mut parent_binding: Option<String> = None;
    let mut path_prefix = String::new();

    for (i, segment) in segments.iter().enumerate() {
        let fields = structs
            .get(&current_struct)
            .unwrap_or_else(|| panic!("generated struct {current_struct} not found while resolving key {dotted}"));
        let info = fields.get(*segment).unwrap_or_else(|| {
            panic!("field {segment} not found on struct {current_struct} while resolving key {dotted}")
        });

        let is_last = i == segments.len() - 1;
        if is_last {
            if info.section_struct.is_some() {
                panic!(
                    "supported key {dotted} resolves to section struct {} (a section node, not a leaf field); \
                     only leaf keys may be supported",
                    info.section_struct.as_deref().unwrap_or("")
                );
            }
            return ResolvedKey {
                dotted: dotted.to_string(),
                method,
                type_text: info.type_text.clone(),
                sections,
                leaf_field: (*segment).to_string(),
            };
        }

        // Non-final segment: must be an Option<SectionStruct>.
        let section_struct = info.section_struct.clone().unwrap_or_else(|| {
            panic!(
                "intermediate segment {segment} of key {dotted} is not an Option<SectionStruct> \
                 (field type {})",
                info.type_text
            )
        });
        // The binding name is the full dotted path to this section (joined with `_`), so two
        // distinct sections that share a final segment (for example `apm_config.obfuscation.http`
        // and `otlp_config.receiver.protocols.http`) get distinct `let` bindings.
        let binding = if path_prefix.is_empty() {
            (*segment).to_string()
        } else {
            format!("{path_prefix}_{segment}")
        };
        sections.push(SectionStep {
            binding: binding.clone(),
            field: (*segment).to_string(),
            parent: parent_binding.clone(),
        });
        parent_binding = Some(binding.clone());
        path_prefix = binding;
        current_struct = section_struct;
    }

    unreachable!("loop returns on the final segment for key {dotted}");
}

/// Render the full `witness.rs` module source.
fn render(resolved: &[ResolvedKey]) -> String {
    let mut out = String::new();
    out.push_str("// @generated by build.rs from core_schema.yaml + schema_overlay.yaml — DO NOT EDIT\n");
    out.push_str("// Regenerate by running `cargo build -p datadog-agent-config`.\n");
    out.push_str("#![allow(clippy::all, dead_code, unused)]\n\n");
    out.push_str("use std::collections::HashMap;\n\n");
    out.push_str("use crate::datadog_configuration::*;\n");
    out.push_str("use crate::translate_error::TranslateError;\n\n");

    // Trait.
    out.push_str("/// Witness trait over every supported Datadog configuration key.\n");
    out.push_str("///\n");
    out.push_str("/// One `consume_<key>` method per `support: full` / `support: partial` overlay key. A\n");
    out.push_str("/// hand-written translator implements this trait; adding support for a key adds a method here\n");
    out.push_str("/// and so creates a compile-time obligation to choose a translation destination.\n");
    out.push_str("pub trait DatadogConfigConsumer {\n");
    for key in resolved {
        out.push_str(&format!("    /// Consumes the value of the `{}` key.\n", key.dotted));
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

    // Section defaults helper.
    out.push_str("/// Materializes a generated section struct from an empty object so absent optional sections\n");
    out.push_str("/// still yield their schema defaults when driven.\n");
    out.push_str("fn section_defaults<T: serde::de::DeserializeOwned>() -> T {\n");
    out.push_str("    serde_json::from_value(serde_json::Value::Object(serde_json::Map::new()))\n");
    out.push_str("        .expect(\"generated section defaults must deserialize from an empty object\")\n");
    out.push_str("}\n\n");

    // Driver.
    out.push_str("/// Drives a consumer over every supported key in `config`, calling each `consume_<key>` exactly\n");
    out.push_str("/// once with the value navigated from `config` (absent optional sections materialize defaults).\n");
    out.push_str("///\n");
    out.push_str("/// # Errors\n");
    out.push_str("///\n");
    out.push_str("/// Returns the first [`TranslateError`] the consumer recorded, surfaced after all keys are\n");
    out.push_str("/// consumed.\n");
    out.push_str("pub fn drive(config: &DatadogConfiguration, consumer: &mut impl DatadogConfigConsumer) -> Result<(), TranslateError> {\n");

    // Emit section bindings in dependency order, deduplicated. A binding is identified by its full
    // path of fields from the root; since field names are unique along any single path and the same
    // section reached via the same path uses the same binding name, we key on (parent, field).
    let mut emitted: std::collections::BTreeSet<(Option<String>, String)> = std::collections::BTreeSet::new();
    for key in resolved {
        for step in &key.sections {
            let id = (step.parent.clone(), step.field.clone());
            if !emitted.insert(id) {
                continue;
            }
            let parent_expr = match &step.parent {
                Some(p) => p.clone(),
                None => "config".to_string(),
            };
            out.push_str(&format!(
                "    let {} = {}.{}.clone().unwrap_or_else(section_defaults);\n",
                step.binding, parent_expr, step.field
            ));
        }
    }
    if !emitted.is_empty() {
        out.push('\n');
    }

    // Emit the consume calls.
    for key in resolved {
        let owner = match key.sections.last() {
            Some(step) => step.binding.clone(),
            None => "config".to_string(),
        };
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
