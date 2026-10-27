//! Executable crate boundaries for the Saluki workspace.
//!
//! Unit tests report boundary violations with a dependency route or `file:line`.
//!
//! # Dependency guards
//!
//! The configuration crates are `agent-data-plane-config` (the model), `agent-data-plane-config-system`,
//! `saluki-config`, `datadog-agent-config`, `datadog-agent-config-overlay-model`, and
//! `datadog-agent-config-testing`.
//!
//! - `saluki-components` has no dependency on the model, the configuration system, or `datadog-agent-config`.
//! - The model links no workspace crate.
//! - The configuration system doesn't depend on `saluki-components`.
//! - `saluki-common` depends on no configuration crate.
//! - No crate lists `datadog-agent-config-overlay-model` in `[dependencies]`, and only the configuration system lists
//!   it in `[dev-dependencies]`. Any crate may list it in `[build-dependencies]`.
//! - Through normal links, `datadog-agent-config` may reach no workspace crate except `go-duration`.
//!
//! The graph comes from `cargo metadata --locked --all-features` without platform filtering, so optional,
//! feature-gated, and target-specific links count. Links join package IDs, so dependency renames don't hide them.
//! "Depends on" means reach through [`graph::Links::Built`]: any link out of the crate, then only links with a normal
//! or build entry. Cargo never builds a dependency's `[dev-dependencies]`.
//!
//! # Symbol guards
//!
//! - Code under `bin/agent-data-plane/src/components/` names no crate, or alias of one, that the binary links to and
//!   that is or reaches the model. Crate names are the binary's extern names, so renames in its `Cargo.toml` count.
//!   [`symbols::SourceTree::aliases`] defines aliases, including top-level private imports in `main.rs` accessible
//!   through `crate::` or `super::*`.
//! - Code under `bin/agent-data-plane/{src,tests}/` names neither `GenericConfiguration` nor
//!   `ConfigurationLoader`, under any root, so re-exports and `as` renames fail where they're imported.
//!
//! The scan parses every `.rs` file under these directories, including `#[cfg(test)]` code. It matches path segments,
//! `use` trees, `extern crate` items, and identifier tokens in macro bodies and attribute arguments. Comments, doc text,
//! and string literals never match. Read or parse errors, or a scan with no `.rs` files, fail the guard.
//!
//! # Limits
//!
//! The symbol guards don't resolve names. An unrelated item sharing a guarded identifier causes a false positive.
//! The guards can miss:
//!
//! - model values inferred from function results or struct fields, with no guarded identifier at the use site
//! - aliases reached through private glob imports below crate-root scope or private bindings in other non-root files
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
        graph::{direct_links_reaching, LinkKinds, Links, Reach, Workspace},
        symbols::{Mention, SourceTree},
    };

    const MODEL: &str = "agent-data-plane-config";
    const CONFIG_SYSTEM: &str = "agent-data-plane-config-system";
    const DATADOG_CONFIG: &str = "datadog-agent-config";
    const OVERLAY_MODEL: &str = "datadog-agent-config-overlay-model";
    const CONFIG_CRATES: [&str; 6] = [
        MODEL,
        CONFIG_SYSTEM,
        "saluki-config",
        DATADOG_CONFIG,
        OVERLAY_MODEL,
        "datadog-agent-config-testing",
    ];
    const BIN: &str = "agent-data-plane";

    fn workspace() -> Workspace {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
        Workspace::load(&manifest).expect("load the workspace package graph")
    }

    #[track_caller]
    fn assert_unreachable(from: &str, targets: &[&str]) {
        let workspace = workspace();
        let reach = Reach::new(workspace.member(from).unwrap(), Links::Built);
        let routes: Vec<String> = targets
            .iter()
            .filter_map(|target| reach.route_to(&workspace.member(target).unwrap()))
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

    #[test]
    fn saluki_components_does_not_depend_on_the_configuration_model() {
        assert_unreachable("saluki-components", &[MODEL]);
    }

    #[test]
    fn saluki_components_does_not_depend_on_the_configuration_system() {
        assert_unreachable("saluki-components", &[CONFIG_SYSTEM]);
    }

    #[test]
    fn saluki_components_does_not_depend_on_the_datadog_configuration_source() {
        assert_unreachable("saluki-components", &[DATADOG_CONFIG]);
    }

    #[test]
    fn configuration_system_does_not_depend_on_saluki_components() {
        assert_unreachable(CONFIG_SYSTEM, &["saluki-components"]);
    }

    #[test]
    fn saluki_common_depends_on_no_configuration_crate() {
        assert_unreachable("saluki-common", &CONFIG_CRATES);
    }

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

    #[test]
    fn overlay_model_has_no_normal_dependents_and_only_configuration_system_tests_use_it() {
        let workspace = workspace();
        let links: Vec<String> = workspace
            .member(OVERLAY_MODEL)
            .unwrap()
            .reverse_direct_links()
            .filter(|link| {
                link.normal().is_present() || (link.dev().is_present() && link.from().name() != CONFIG_SYSTEM)
            })
            .map(|link| format!("{} -[{}]-> {OVERLAY_MODEL}", link.from().name(), LinkKinds(&link)))
            .collect();
        assert!(
            links.is_empty(),
            "`{OVERLAY_MODEL}` allows build links, and a dev link from `{CONFIG_SYSTEM}`, only:\n{}",
            links.join("\n")
        );
    }

    #[test]
    fn datadog_configuration_source_normal_links_reach_only_go_duration() {
        let workspace = workspace();
        let reach = Reach::new(workspace.member(DATADOG_CONFIG).unwrap(), Links::Normal);
        let routes: Vec<String> = reach
            .workspace_members()
            .filter(|package| package.name() != "go-duration")
            .filter_map(|package| reach.route_to(&package))
            .map(|route| route.to_string())
            .collect();
        assert!(
            routes.is_empty(),
            "`{DATADOG_CONFIG}` may reach only `go-duration` through normal links:\n{}",
            routes.join("\n")
        );
    }

    #[test]
    fn bin_components_name_no_crate_or_alias_reaching_the_configuration_model() {
        let workspace = workspace();
        let bin = workspace.member(BIN).unwrap();
        let crates: BTreeSet<String> = direct_links_reaching(&bin, &workspace.member(MODEL).unwrap())
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
