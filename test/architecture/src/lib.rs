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
//! Module guards restrict which modules of a crate code may name. [`symbols::SourceTree::module_mentions`] matches
//! only paths that start at `crate`, `self` or `super`, so a variable that shares a module's name doesn't match.
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
    const CONFIG_DOCS: &str = "datadog-agent-config-docs";
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

    /// The `panoramic` source directory and the names of its top-level modules, from `main.rs`.
    fn panoramic() -> (PathBuf, BTreeSet<String>) {
        let workspace = workspace();
        let src = workspace
            .member("panoramic")
            .unwrap()
            .manifest_path()
            .parent()
            .unwrap()
            .as_std_path()
            .join("src");
        let main = syn::parse_file(&std::fs::read_to_string(src.join("main.rs")).unwrap()).unwrap();
        let modules = main
            .items
            .iter()
            .filter_map(|item| match item {
                syn::Item::Mod(item) => Some(item.ident.to_string()),
                _ => None,
            })
            .collect();
        (src, modules)
    }

    #[track_caller]
    fn assert_panoramic_module_names_none_of(module: &str, guarded: &BTreeSet<String>) {
        let (src, modules) = panoramic();
        assert!(modules.contains(module), "`panoramic` has no `{module}` module");
        let tree = SourceTree::read(&[src.join(module)]).unwrap_or_else(|error| panic!("{error}"));
        assert_no_mentions(
            &format!("panoramic's `{module}` module must not name {guarded:?}"),
            workspace().root(),
            &tree.module_mentions(&src, guarded),
        );
    }

    /// Adds to `out` each name `tree` binds from a module in `modules`.
    fn bindings_from(
        tree: &syn::UseTree, modules: &BTreeSet<String>, path: &mut Vec<String>, out: &mut BTreeSet<String>,
    ) {
        let from_module = |path: &[String]| {
            path.iter()
                .find(|segment| !matches!(segment.as_str(), "crate" | "self"))
                .is_some_and(|segment| modules.contains(segment))
        };
        match tree {
            syn::UseTree::Path(next) => {
                path.push(next.ident.to_string());
                bindings_from(&next.tree, modules, path, out);
                path.pop();
            }
            syn::UseTree::Name(name) if from_module(path) => {
                out.insert(name.ident.to_string());
            }
            syn::UseTree::Rename(rename) if from_module(path) => {
                out.insert(rename.rename.to_string());
            }
            syn::UseTree::Group(group) => {
                for tree in &group.items {
                    bindings_from(tree, modules, path, out);
                }
            }
            syn::UseTree::Name(_) | syn::UseTree::Rename(_) | syn::UseTree::Glob(_) => {}
        }
    }

    /// Panoramic's core runs participants without knowing what they run or what steps check.
    #[test]
    fn panoramic_core_names_neither_participants_nor_steps() {
        let guarded = BTreeSet::from(["participants".to_string(), "steps".to_string()]);
        assert_panoramic_module_names_none_of("core", &guarded);
    }

    /// Participant types and steps meet only through the core.
    #[test]
    fn panoramic_participants_and_steps_do_not_name_each_other() {
        assert_panoramic_module_names_none_of("participants", &BTreeSet::from(["steps".to_string()]));
        assert_panoramic_module_names_none_of("steps", &BTreeSet::from(["participants".to_string()]));
    }

    /// The new modules stand apart from the legacy runners, so deleting those leaves them intact.
    // TODO: remove this rule with the legacy runners.
    #[test]
    fn panoramic_new_modules_name_no_legacy_module() {
        let new = ["core", "participants", "steps"];
        let (src, modules) = panoramic();
        let mut legacy: BTreeSet<String> = modules
            .into_iter()
            .filter(|module| !new.contains(&module.as_str()))
            .collect();
        // The crate root's imports from legacy modules are visible to every module below it as `crate::<name>`.
        let main = syn::parse_file(&std::fs::read_to_string(src.join("main.rs")).unwrap()).unwrap();
        let mut imported = BTreeSet::new();
        for item in &main.items {
            if let syn::Item::Use(item) = item {
                bindings_from(&item.tree, &legacy, &mut Vec::new(), &mut imported);
            }
        }
        legacy.extend(imported);
        for module in new {
            assert_panoramic_module_names_none_of(module, &legacy);
        }
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
    /// Code generation build-depends on it, and the corpus tests check recordings against the support inventory.
    #[test]
    fn overlay_model_is_used_only_by_code_generation_and_configuration_tests() {
        let build_dependents = [DATADOG_CONFIG, CONFIG_DOCS];
        let dev_dependents = [CONFIG_SYSTEM, CORPUS];
        let workspace = workspace();
        let links: Vec<String> = workspace
            .member(OVERLAY_MODEL)
            .unwrap()
            .reverse_direct_links()
            .filter(|link| {
                let from = link.from().name();
                link.normal().is_present()
                    || (link.build().is_present() && !build_dependents.contains(&from))
                    || (link.dev().is_present() && !dev_dependents.contains(&from))
            })
            .map(|link| format!("{} -[{}]-> {OVERLAY_MODEL}", link.from().name(), LinkKinds(&link)))
            .collect();
        assert!(
            links.is_empty(),
            "`{OVERLAY_MODEL}` allows build links from {build_dependents:?} and dev links from {dev_dependents:?} \
             only:\n{}",
            links.join("\n")
        );
    }

    /// The source model stays independent of workspace runtime code.
    #[test]
    fn datadog_configuration_source_depends_only_on_go_duration_through_normal_links() {
        let workspace = workspace();
        let dependencies = Dependencies::new(workspace.member(DATADOG_CONFIG).unwrap(), Links::Normal);
        let mut routes: Vec<String> = dependencies
            .workspace_members()
            .filter(|package| package.name() != "go-duration")
            .filter_map(|package| dependencies.route_to(&package))
            .map(|route| route.to_string())
            .collect();
        routes.sort();
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
