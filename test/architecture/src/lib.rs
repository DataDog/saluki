//! Architecture tests for the Saluki workspace.
//!
//! Unit tests in this crate report violations to intended architectural invariants.
//!
//! # Dependency guards
//!
//! Dependency guards reject unwanted direct or transitive dependencies between crates. [`graph::Workspace`] loads
//! the package graph from Cargo metadata, including optional, feature-gated, and target-specific dependencies.
//! Links join package IDs, so dependency renames don't hide them. Each guard chooses which dependency kinds to
//! follow through [`graph::Links`]. Violations report the dependency route.
//!
//! # Symbol guards
//!
//! Symbol guards restrict which names code may mention, enforcing boundaries within a crate that dependency guards
//! can't express. [`symbols::SourceTree`] parses every `.rs` file under selected directories, including test code.
//! It matches names in paths, imports, and macro or attribute tokens, but ignores comments, doc text, and strings.
//! Guards can also collect aliases with [`symbols::SourceTree::aliases`]. Matches report the source location;
//! unreadable, invalid, or missing sources fail the scan.
//!
//! # Limits
//!
//! Symbol guards don't resolve names. Unrelated items sharing a guarded identifier cause false positives.
//! The guards can miss:
//!
//! - types inferred without a guarded identifier at the use site
//! - aliases from private glob imports below crate-root scope or private bindings in other non-root files
//! - aliases defined inside blocks or macro invocations
//! - modules placed outside the scanned directories by `#[path]`
//!
//! Check for these gaps during review.
#![deny(missing_docs)]

pub mod graph;
pub mod symbols;

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        path::{Path, PathBuf},
    };

    use guppy::graph::BuildTargetId;

    use crate::{
        graph::{direct_links_to_target_or_its_dependents, Dependencies, LinkKinds, Links, Workspace},
        symbols::{Mention, SourceTree},
    };

    const MODEL: &str = "agent-data-plane-config";
    const CONFIG_SYSTEM: &str = "agent-data-plane-config-system";
    const DATADOG_CONFIG: &str = "datadog-agent-config";
    const OVERLAY_MODEL: &str = "datadog-agent-config-overlay-model";
    const CORPUS: &str = "datadog-agent-config-corpus";
    const BIN: &str = "agent-data-plane";

    fn workspace() -> Workspace {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
        Workspace::load(&manifest).expect("load the workspace package graph")
    }

    #[track_caller]
    fn assert_does_not_depend_on(from: &str, targets: &[&str]) {
        let workspace = workspace();
        let dependencies = Dependencies::new(workspace.member(from).unwrap(), Links::Built);
        let routes: Vec<String> = targets
            .iter()
            .filter_map(|target| dependencies.route_to(&workspace.member(target).unwrap()))
            .map(|route| route.to_string())
            .collect();
        assert!(
            routes.is_empty(),
            "`{from}` must not depend on {targets:?}:\n{}",
            routes.join("\n")
        );
    }

    #[track_caller]
    fn assert_no_mentions(rule: &str, root: &Path, mentions: &[Mention]) {
        let lines: Vec<String> = mentions
            .iter()
            .map(|mention| {
                let path = mention.path.strip_prefix(root).unwrap_or(&mention.path);
                Mention {
                    path: path.to_path_buf(),
                    ..mention.clone()
                }
                .to_string()
            })
            .collect();
        assert!(lines.is_empty(), "{rule}:\n{}", lines.join("\n"));
    }

    /// Components take component-owned inputs; the binary adapts the shared model.
    #[test]
    fn saluki_components_does_not_depend_on_the_configuration_model() {
        assert_does_not_depend_on("saluki-components", &[MODEL]);
    }

    /// Loading and translation belong outside runtime components.
    #[test]
    fn saluki_components_does_not_depend_on_the_configuration_system() {
        assert_does_not_depend_on("saluki-components", &[CONFIG_SYSTEM]);
    }

    /// Components must not interpret source-specific configuration.
    #[test]
    fn saluki_components_does_not_depend_on_the_datadog_configuration_source() {
        assert_does_not_depend_on("saluki-components", &[DATADOG_CONFIG]);
    }

    /// The configuration system produces data, not component instances.
    #[test]
    fn configuration_system_does_not_depend_on_saluki_components() {
        assert_does_not_depend_on(CONFIG_SYSTEM, &["saluki-components"]);
    }

    /// Keep shared utilities independent of higher-level subsystems.
    #[test]
    fn saluki_common_depends_only_on_foundational_workspace_crates() {
        let allowed = [
            "saluki-antithesis",
            "saluki-error",
            "saluki-metrics",
            "saluki-metrics-macros",
            "stringtheory",
        ];
        let workspace = workspace();
        let dependencies = Dependencies::new(workspace.member("saluki-common").unwrap(), Links::Built);
        let mut routes: Vec<String> = dependencies
            .workspace_members()
            .filter(|package| !allowed.contains(&package.name()))
            .filter_map(|package| dependencies.route_to(&package))
            .map(|route| route.to_string())
            .collect();
        routes.sort();
        assert!(
            routes.is_empty(),
            "`saluki-common` may depend only on {allowed:?} workspace crates:\n{}",
            routes.join("\n")
        );
    }

    /// The model is pure data, independent of other workspace crates.
    #[test]
    fn configuration_model_links_no_workspace_crate() {
        let workspace = workspace();
        let links: Vec<String> = workspace
            .member(MODEL)
            .unwrap()
            .direct_links()
            .filter(|link| link.to().in_workspace())
            .map(|link| format!("{MODEL} -[{}]-> {}", LinkKinds(&link), link.to().name()))
            .collect();
        assert!(
            links.is_empty(),
            "`{MODEL}` must not link a workspace crate:\n{}",
            links.join("\n")
        );
    }

    /// Overlay metadata belongs to code generation and configuration validation, not runtime code.
    /// The corpus tests check recordings against the support inventory.
    #[test]
    fn overlay_model_has_no_normal_dependents_and_only_configuration_tests_use_it() {
        let dev_dependents = [CONFIG_SYSTEM, CORPUS];
        let workspace = workspace();
        let links: Vec<String> = workspace
            .member(OVERLAY_MODEL)
            .unwrap()
            .reverse_direct_links()
            .filter(|link| {
                link.normal().is_present() || (link.dev().is_present() && !dev_dependents.contains(&link.from().name()))
            })
            .map(|link| format!("{} -[{}]-> {OVERLAY_MODEL}", link.from().name(), LinkKinds(&link)))
            .collect();
        assert!(
            links.is_empty(),
            "`{OVERLAY_MODEL}` allows build links, and dev links from {dev_dependents:?}, only:\n{}",
            links.join("\n")
        );
    }

    /// The source model stays independent of workspace runtime code.
    #[test]
    fn datadog_configuration_source_depends_only_on_go_duration_through_normal_links() {
        let workspace = workspace();
        let dependencies = Dependencies::new(workspace.member(DATADOG_CONFIG).unwrap(), Links::Normal);
        let routes: Vec<String> = dependencies
            .workspace_members()
            .filter(|package| package.name() != "go-duration")
            .filter_map(|package| dependencies.route_to(&package))
            .map(|route| route.to_string())
            .collect();
        assert!(
            routes.is_empty(),
            "`{DATADOG_CONFIG}` may depend only on `go-duration` through normal links:\n{}",
            routes.join("\n")
        );
    }

    /// The binary needs the model for adapters, but its components must remain model-independent.
    /// Check dependency names and aliases so renamed imports don't bypass this boundary.
    #[test]
    fn bin_components_name_neither_the_configuration_model_nor_crates_depending_on_it() {
        let workspace = workspace();
        let bin = workspace.member(BIN).unwrap();
        let crates: BTreeSet<String> =
            direct_links_to_target_or_its_dependents(&bin, &workspace.member(MODEL).unwrap())
                .iter()
                .map(|link| link.resolved_name().to_string())
                .collect();
        let bin_dir = bin.manifest_path().parent().unwrap().as_std_path();
        let crate_root = bin
            .build_target(&BuildTargetId::Binary(BIN))
            .expect("binary target")
            .path()
            .as_std_path();

        let aliases = SourceTree::read(&[bin_dir.join("src")])
            .and_then(|tree| tree.aliases(crate_root, &crates))
            .unwrap_or_else(|error| panic!("{error}"));
        let names: BTreeSet<String> = crates.union(&aliases).cloned().collect();
        let components = SourceTree::read(&[bin_dir.join("src/components")]).unwrap_or_else(|error| panic!("{error}"));

        assert_no_mentions(
            &format!("binary components must not name {crates:?} or their aliases {aliases:?}"),
            workspace.root(),
            &components.mentions(&names),
        );
    }

    /// The binary uses typed configuration, not the raw-map library API.
    /// Matching type names under any root catches re-exports and renamed imports.
    #[test]
    fn bin_names_no_raw_configuration_map_type() {
        let workspace = workspace();
        let bin_dir = workspace
            .member(BIN)
            .unwrap()
            .manifest_path()
            .parent()
            .unwrap()
            .as_std_path();
        let dirs: Vec<PathBuf> = ["src", "tests"].iter().map(|dir| bin_dir.join(dir)).collect();
        let names = BTreeSet::from(["GenericConfiguration".to_string(), "ConfigurationLoader".to_string()]);
        let tree = SourceTree::read(&dirs).unwrap_or_else(|error| panic!("{error}"));

        assert_no_mentions(
            "the binary must not name the raw configuration map",
            workspace.root(),
            &tree.mentions(&names),
        );
    }
}
