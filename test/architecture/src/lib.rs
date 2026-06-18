//! Architecture tests for the configuration system crate boundaries.
//!
//! Each test encodes one forbidden dependency edge or forbidden source-level import.
//! Tests start `#[ignore]`d and are un-ignored when the build-order step that makes
//! them pass lands. The suite stays green throughout (ignored is not failed).
//!
//! Two granularities:
//!
//! - **Cargo.toml dependency-edge guards**: crate A must not declare a dependency on
//!   crate B. Checked by parsing the crate's Cargo.toml.
//!
//! - **Source-import guards**: a forbidden symbol must not appear in production source
//!   (`src/`) even via a re-export. Checked by grepping `.rs` files.
//!
//! Guards apply to production code. Test fixtures and dev-dependencies are excluded.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    fn workspace_root() -> PathBuf {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest_dir
            .parent()
            .expect("test/ dir")
            .parent()
            .expect("workspace root")
            .to_path_buf()
    }

    /// Returns true if the Cargo.toml at `crate_path` lists `dep_name` in
    /// `[dependencies]` or `[build-dependencies]`.
    fn crate_has_dependency(crate_path: &Path, dep_name: &str) -> bool {
        let cargo_toml = crate_path.join("Cargo.toml");
        let contents =
            fs::read_to_string(&cargo_toml).unwrap_or_else(|e| panic!("failed to read {}: {e}", cargo_toml.display()));

        // Match lines like `dep-name = ...` or `dep-name.workspace = ...`.
        // Anchored to line start to avoid matching partial names or comments.
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with(dep_name) && trimmed[dep_name.len()..].starts_with([' ', '=', '.']) {
                return true;
            }
        }
        false
    }

    /// Collects lines in `*.rs` files under `dir` that contain `pattern`.
    /// Returns (file_path, line_number, line_text) triples.
    fn grep_rs_files(dir: &Path, pattern: &str) -> Vec<(PathBuf, usize, String)> {
        let mut hits = Vec::new();
        if !dir.exists() {
            return hits;
        }
        collect_rs_hits(dir, pattern, &mut hits);
        hits
    }

    fn collect_rs_hits(dir: &Path, pattern: &str, hits: &mut Vec<(PathBuf, usize, String)>) {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_hits(&path, pattern, hits);
            } else if path.extension().is_some_and(|e| e == "rs") {
                if let Ok(contents) = fs::read_to_string(&path) {
                    for (i, line) in contents.lines().enumerate() {
                        if line.contains(pattern) {
                            hits.push((path.clone(), i + 1, line.to_string()));
                        }
                    }
                }
            }
        }
    }

    fn assert_no_dependency(crate_rel_path: &str, forbidden_dep: &str) {
        let crate_path = workspace_root().join(crate_rel_path);
        assert!(
            !crate_has_dependency(&crate_path, forbidden_dep),
            "{crate_rel_path} must not depend on {forbidden_dep}",
        );
    }

    fn assert_no_source_symbol(crate_rel_path: &str, symbol: &str) {
        let src_dir = workspace_root().join(crate_rel_path).join("src");
        let hits = grep_rs_files(&src_dir, symbol);
        assert!(
            hits.is_empty(),
            "{crate_rel_path}/src must not contain `{symbol}`.\nFound {} hit(s):\n{}",
            hits.len(),
            hits.iter()
                .take(10)
                .map(|(f, n, l)| format!("  {}:{n}: {}", f.display(), l.trim()))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

    // -------------------------------------------------------------------------
    // Cargo.toml dependency-edge guards: saluki-component-config (leaf)
    //
    // This is the leaf crate. It must not depend on any Datadog source, ADP
    // target model, raw-map, config-system, or component implementation crate.
    // -------------------------------------------------------------------------

    #[test]
    fn leaf_must_not_depend_on_datadog_agent_config() {
        assert_no_dependency("lib/saluki-component-config", "datadog-agent-config");
    }

    #[test]
    fn leaf_must_not_depend_on_agent_data_plane_config() {
        assert_no_dependency("lib/saluki-component-config", "agent-data-plane-config");
    }

    #[test]
    fn leaf_must_not_depend_on_config_system() {
        assert_no_dependency("lib/saluki-component-config", "agent-data-plane-config-system");
    }

    #[test]
    fn leaf_must_not_depend_on_raw_map() {
        assert_no_dependency("lib/saluki-component-config", "saluki-config-tools");
    }

    #[test]
    fn leaf_must_not_depend_on_saluki_components() {
        assert_no_dependency("lib/saluki-component-config", "saluki-components");
    }

    // -------------------------------------------------------------------------
    // Cargo.toml dependency-edge guards: agent-data-plane-config
    //
    // The ADP-native model crate. It embeds saluki-component-config structs but
    // must not reach Datadog source models, raw maps, the config-system facade,
    // or component implementations.
    // -------------------------------------------------------------------------

    #[test]
    fn adp_config_must_not_depend_on_datadog_agent_config() {
        assert_no_dependency("lib/agent-data-plane-config", "datadog-agent-config");
    }

    #[test]
    fn adp_config_must_not_depend_on_raw_map() {
        assert_no_dependency("lib/agent-data-plane-config", "saluki-config-tools");
    }

    #[test]
    fn adp_config_must_not_depend_on_config_system() {
        assert_no_dependency("lib/agent-data-plane-config", "agent-data-plane-config-system");
    }

    #[test]
    fn adp_config_must_not_depend_on_saluki_components() {
        assert_no_dependency("lib/agent-data-plane-config", "saluki-components");
    }

    // -------------------------------------------------------------------------
    // Cargo.toml dependency-edge guards: agent-data-plane-config-system
    //
    // The facade. It wires everything together but must not depend on component
    // implementations.
    // -------------------------------------------------------------------------

    #[test]
    fn config_system_must_not_depend_on_saluki_components() {
        assert_no_dependency("lib/agent-data-plane-config-system", "saluki-components");
    }

    // -------------------------------------------------------------------------
    // Cargo.toml dependency-edge guards: saluki-components
    //
    // Components consume leaf config structs. They must not be able to name
    // SalukiConfiguration or ControlConfiguration (enforced by not depending
    // on agent-data-plane-config).
    // -------------------------------------------------------------------------

    #[test]
    fn components_must_not_depend_on_adp_config() {
        assert_no_dependency("lib/saluki-components", "agent-data-plane-config");
    }

    // -------------------------------------------------------------------------
    // Cargo.toml dependency-edge guards: bin/agent-data-plane
    //
    // The binary consumes typed config-system outputs. It must not depend on
    // the raw-map crate directly.
    // -------------------------------------------------------------------------

    #[test]
    fn binary_must_not_depend_on_raw_map() {
        assert_no_dependency("bin/agent-data-plane", "saluki-config-tools");
    }

    // -------------------------------------------------------------------------
    // Source-import guards: bin/agent-data-plane
    //
    // The binary consumes typed config-system outputs only. No raw maps, no
    // loaders, no Datadog source mechanics in production code.
    // -------------------------------------------------------------------------

    #[test]
    fn binary_must_not_mention_generic_configuration() {
        assert_no_source_symbol("bin/agent-data-plane", "GenericConfiguration");
    }

    #[test]
    fn binary_must_not_mention_configuration_loader() {
        assert_no_source_symbol("bin/agent-data-plane", "ConfigurationLoader");
    }

    #[test]
    fn binary_must_not_import_raw_map_crate() {
        assert_no_source_symbol("bin/agent-data-plane", "saluki_config_tools");
    }

    #[test]
    fn binary_must_not_import_datadog_remapper() {
        assert_no_source_symbol("bin/agent-data-plane", "DatadogRemapper");
    }

    #[test]
    fn binary_must_not_import_key_aliases() {
        assert_no_source_symbol("bin/agent-data-plane", "KEY_ALIASES");
    }

    // -------------------------------------------------------------------------
    // Source-import guards: bin/agent-data-plane/src/components/
    //
    // ADP-local component builders have the same raw-map constraints as
    // saluki-components. They must not use raw-map constructors, raw-map
    // reads, or source config imports.
    // -------------------------------------------------------------------------

    fn assert_no_binary_component_symbol(symbol: &str) {
        let dir = workspace_root().join("bin/agent-data-plane/src/components");
        let hits = grep_rs_files(&dir, symbol);
        assert!(
            hits.is_empty(),
            "bin/agent-data-plane/src/components/ must not contain `{symbol}`.\n\
             Found {} hit(s):\n{}",
            hits.len(),
            hits.iter()
                .take(10)
                .map(|(f, n, l)| format!("  {}:{n}: {}", f.display(), l.trim()))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

    #[test]
    fn binary_components_must_not_use_from_configuration() {
        assert_no_binary_component_symbol("from_configuration");
    }

    #[test]
    fn binary_components_must_not_use_generic_configuration() {
        assert_no_binary_component_symbol("GenericConfiguration");
    }

    #[test]
    fn binary_components_must_not_use_try_get_typed() {
        assert_no_binary_component_symbol("try_get_typed");
    }

    #[test]
    fn binary_components_must_not_import_raw_map_crate() {
        assert_no_binary_component_symbol("saluki_config_tools");
    }

    // -------------------------------------------------------------------------
    // Source-import guards: saluki-components
    //
    // Components must not use raw-map constructors, raw-map reads, string-key
    // watches, or Datadog source mechanics.
    // -------------------------------------------------------------------------

    #[test]
    fn components_must_not_use_from_configuration() {
        assert_no_source_symbol("lib/saluki-components", "from_configuration");
    }

    #[test]
    fn components_must_not_use_try_get_typed() {
        assert_no_source_symbol("lib/saluki-components", "try_get_typed");
    }

    #[test]
    fn components_must_not_use_as_typed() {
        assert_no_source_symbol("lib/saluki-components", "as_typed");
    }

    #[test]
    fn components_must_not_watch_string_keys() {
        assert_no_source_symbol("lib/saluki-components", "watch_for_updates");
    }

    #[test]
    fn components_must_not_subscribe_for_updates() {
        assert_no_source_symbol("lib/saluki-components", "subscribe_for_updates");
    }

    #[test]
    fn components_must_not_import_raw_map_crate() {
        assert_no_source_symbol("lib/saluki-components", "saluki_config_tools");
    }

    #[test]
    fn components_must_not_reference_datadog_remapper() {
        assert_no_source_symbol("lib/saluki-components", "DatadogRemapper");
    }

    #[test]
    fn components_must_not_reference_key_aliases() {
        assert_no_source_symbol("lib/saluki-components", "KEY_ALIASES");
    }
}
