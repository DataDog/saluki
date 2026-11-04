//! Workspace package graph and dependency queries.

use std::{collections::HashMap, fmt, path::Path};

use guppy::{
    graph::{DependencyDirection, PackageGraph, PackageLink, PackageMetadata},
    MetadataCommand, PackageId,
};

/// The package graph of a Cargo workspace.
///
/// The graph holds every package for every feature and every platform: optional, feature-gated, and
/// `[target.'cfg(..)']` links are all present.
pub struct Workspace {
    graph: PackageGraph,
}

impl Workspace {
    /// Loads the package graph of the workspace whose root manifest is at `manifest_path`.
    ///
    /// # Errors
    ///
    /// Returns an error if `cargo metadata` fails, including when `Cargo.lock` needs an update.
    pub fn load(manifest_path: &Path) -> Result<Self, guppy::Error> {
        let mut command = MetadataCommand::new();
        command.manifest_path(manifest_path).other_options(["--locked"]);
        Ok(Self {
            graph: command.build_graph()?,
        })
    }

    /// Returns the workspace root directory.
    pub fn root(&self) -> &Path {
        self.graph.workspace().root().as_std_path()
    }

    /// Returns the workspace member named `name`.
    ///
    /// # Errors
    ///
    /// Returns an error if no workspace member has that package name.
    pub fn member(&self, name: &str) -> Result<PackageMetadata<'_>, guppy::Error> {
        self.graph.workspace().member_by_name(name)
    }
}

/// Dependency kinds included by [`Dependencies`], without feature or platform filtering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Links {
    /// Any link out of the root, including dev-dependencies, then [`Links::AsDependency`].
    Built,

    /// Normal or build entries at every hop. Cargo never builds a dependency's dev-dependencies.
    AsDependency,

    /// Links with a normal entry, at every hop.
    Normal,
}

/// A package's direct and transitive dependencies, with one route to each.
pub struct Dependencies<'g> {
    via: HashMap<&'g PackageId, PackageLink<'g>>,
}

impl<'g> Dependencies<'g> {
    /// Computes the packages `root` depends on through `links`.
    ///
    /// For [`Links::Built`], a package reachable without a root dev-dependency is reported through such a route,
    /// so a failing guard names the dependency that has to change.
    pub fn new(root: PackageMetadata<'g>, links: Links) -> Self {
        let mut via = match links {
            Links::Built => Self::new(root, Links::AsDependency).via,
            Links::AsDependency | Links::Normal => HashMap::new(),
        };
        let root_id = root.id();
        let query = root.to_package_query(DependencyDirection::Forward);
        query.resolve_with_fn(|cx, link| {
            let follow = match links {
                Links::Built => cx.starts_from_initial(&link) || !link.dev_only(),
                Links::AsDependency => !link.dev_only(),
                Links::Normal => link.normal().is_present(),
            };
            if follow && link.to().id() != root_id {
                via.entry(link.to().id()).or_insert(link);
            }
            follow
        });
        Self { via }
    }

    /// Returns the dependency route to `package`, or `None` for the root or a non-dependency.
    pub fn route_to(&self, package: &PackageMetadata<'g>) -> Option<Route<'g>> {
        let mut links = Vec::new();
        let mut id = package.id();
        while let Some(link) = self.via.get(id) {
            links.push(*link);
            id = link.from().id();
        }
        if links.is_empty() {
            return None;
        }
        links.reverse();
        Some(Route { links })
    }

    /// Returns workspace members the root depends on, excluding the root itself.
    pub fn workspace_members(&self) -> impl Iterator<Item = PackageMetadata<'g>> + '_ {
        self.via
            .values()
            .map(|link| link.to())
            .filter(|package| package.in_workspace())
    }
}

/// A chain of package links, displayed as `a -[normal]-> b -[build]-> c`.
pub struct Route<'g> {
    links: Vec<PackageLink<'g>>,
}

impl fmt::Display for Route<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(first) = self.links.first() else {
            return Ok(());
        };
        f.write_str(first.from().name())?;
        for link in &self.links {
            write!(f, " -[{}]-> {}", LinkKinds(link), link.to().name())?;
        }
        Ok(())
    }
}

/// The dependency sections a link appears in, displayed as `normal+dev`.
pub struct LinkKinds<'a, 'g>(pub &'a PackageLink<'g>);

impl fmt::Display for LinkKinds<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let link = self.0;
        let kinds = [
            ("normal", link.normal().is_present()),
            ("dev", link.dev().is_present()),
            ("build", link.build().is_present()),
        ];
        let mut separator = "";
        for (name, _) in kinds.iter().filter(|(_, present)| *present) {
            write!(f, "{separator}{name}")?;
            separator = "+";
        }
        Ok(())
    }
}

/// Returns direct links of any kind from `from` to `target` or to packages that depend on `target`
/// through [`Links::AsDependency`].
pub fn direct_links_to_target_or_its_dependents<'g>(
    from: &PackageMetadata<'g>, target: &PackageMetadata<'g>,
) -> Vec<PackageLink<'g>> {
    from.direct_links()
        .filter(|link| {
            let to = link.to();
            to.id() == target.id() || Dependencies::new(to, Links::AsDependency).route_to(target).is_some()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, process::Command};

    use super::*;

    /// Exercises renames, optional target-specific links, root-only dev-dependency traversal, and a package reached
    /// both through a root dev-dependency and through a build chain.
    struct Fixture {
        dir: tempfile::TempDir,
    }

    const MANIFESTS: &[(&str, &str)] = &[
        (
            "a",
            r#"
            [dependencies]
            renamed = { package = "b", path = "../b" }

            [dev-dependencies]
            c = { path = "../c" }

            [build-dependencies]
            d = { path = "../d" }

            [target.'cfg(windows)'.dependencies]
            e = { path = "../e", optional = true }
            "#,
        ),
        ("b", ""),
        (
            "c",
            r#"
            [dependencies]
            g = { path = "../g" }

            [dev-dependencies]
            f = { path = "../f" }
            "#,
        ),
        (
            "d",
            r#"
            [dependencies]
            g = { path = "../g" }
            "#,
        ),
        ("e", ""),
        ("f", ""),
        ("g", ""),
    ];

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("create fixture directory");
            let members: Vec<String> = MANIFESTS.iter().map(|(name, _)| format!("{name:?}")).collect();
            fs::write(
                dir.path().join("Cargo.toml"),
                format!("[workspace]\nmembers = [{}]\nresolver = \"2\"\n", members.join(", ")),
            )
            .expect("write workspace manifest");
            for (name, dependencies) in MANIFESTS {
                let package = dir.path().join(name);
                fs::create_dir_all(package.join("src")).expect("create package directory");
                fs::write(package.join("src/lib.rs"), "").expect("write package source");
                fs::write(
                    package.join("Cargo.toml"),
                    format!("[package]\nname = {name:?}\nversion = \"0.1.0\"\nedition = \"2021\"\n{dependencies}"),
                )
                .expect("write package manifest");
            }
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let status = Command::new(cargo)
                .args(["generate-lockfile", "--offline", "--manifest-path"])
                .arg(dir.path().join("Cargo.toml"))
                .status()
                .expect("run cargo generate-lockfile");
            assert!(status.success(), "cargo generate-lockfile failed");
            Self { dir }
        }

        fn load(&self) -> Workspace {
            Workspace::load(&self.dir.path().join("Cargo.toml")).expect("load fixture workspace")
        }
    }

    fn member_names<'g>(packages: impl Iterator<Item = PackageMetadata<'g>>) -> BTreeSet<&'g str> {
        packages.map(|package| package.name()).collect()
    }

    #[test]
    fn built_dependencies_include_root_dev_links_but_not_dependency_dev_links() {
        let fixture = Fixture::new();
        let workspace = fixture.load();
        let a = workspace.member("a").unwrap();
        let dependencies = Dependencies::new(a, Links::Built);

        assert_eq!(
            member_names(dependencies.workspace_members()),
            BTreeSet::from(["b", "c", "d", "e", "g"])
        );
        assert!(dependencies.route_to(&workspace.member("f").unwrap()).is_none());
        assert_eq!(
            dependencies
                .route_to(&workspace.member("g").unwrap())
                .unwrap()
                .to_string(),
            "a -[build]-> d -[normal]-> g"
        );
        assert_eq!(
            dependencies
                .route_to(&workspace.member("e").unwrap())
                .unwrap()
                .to_string(),
            "a -[normal]-> e"
        );

        let c_dependencies = Dependencies::new(workspace.member("c").unwrap(), Links::Built);
        assert_eq!(
            c_dependencies
                .route_to(&workspace.member("f").unwrap())
                .unwrap()
                .to_string(),
            "c -[dev]-> f"
        );
    }

    #[test]
    fn normal_dependencies_follow_only_normal_links() {
        let fixture = Fixture::new();
        let workspace = fixture.load();
        let dependencies = Dependencies::new(workspace.member("a").unwrap(), Links::Normal);

        assert_eq!(
            member_names(dependencies.workspace_members()),
            BTreeSet::from(["b", "e"])
        );
    }

    #[test]
    fn direct_links_include_target_and_packages_depending_on_it() {
        let fixture = Fixture::new();
        let workspace = fixture.load();
        let a = workspace.member("a").unwrap();

        let to_b: Vec<_> = direct_links_to_target_or_its_dependents(&a, &workspace.member("b").unwrap())
            .iter()
            .map(|link| (link.resolved_name(), link.to().name()))
            .collect();
        assert_eq!(to_b, [("renamed", "b")]);

        let mut to_g: Vec<_> = direct_links_to_target_or_its_dependents(&a, &workspace.member("g").unwrap())
            .iter()
            .map(|link| (link.resolved_name(), LinkKinds(link).to_string()))
            .collect();
        to_g.sort();
        assert_eq!(to_g, [("c", "dev".to_string()), ("d", "build".to_string())]);

        assert!(direct_links_to_target_or_its_dependents(&a, &workspace.member("f").unwrap()).is_empty());
    }

    #[test]
    fn unknown_member_name_is_an_error() {
        let fixture = Fixture::new();
        let workspace = fixture.load();

        assert!(matches!(
            workspace.member("missing"),
            Err(guppy::Error::UnknownWorkspaceName(name)) if name == "missing"
        ));
    }

    #[test]
    fn load_rejects_a_stale_lockfile() {
        let fixture = Fixture::new();
        fs::write(fixture.dir.path().join("Cargo.lock"), "version = 4\n").expect("truncate lockfile");

        assert!(matches!(
            Workspace::load(&fixture.dir.path().join("Cargo.toml")),
            Err(guppy::Error::CommandError(_))
        ));
    }
}
