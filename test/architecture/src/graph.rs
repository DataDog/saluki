//! Workspace package graph and dependency reach.

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

/// Which dependency kinds a [`Reach`] follows, without feature or platform filtering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Links {
    /// Any link out of the root, including dev-dependencies, then [`Links::AsDependency`].
    Built,

    /// Normal or build entries at every hop. Cargo never builds a dependency's dev-dependencies.
    AsDependency,

    /// Links with a normal entry, at every hop.
    Normal,
}

/// The packages reachable from a root package, with one route to each.
pub struct Reach<'g> {
    via: HashMap<&'g PackageId, PackageLink<'g>>,
}

impl<'g> Reach<'g> {
    /// Computes the packages reachable from `root` through `links`.
    pub fn new(root: PackageMetadata<'g>, links: Links) -> Self {
        let mut via = HashMap::new();
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

    /// Returns the route from the root to `package`, or `None` if `package` is the root or unreachable.
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

    /// Returns the reachable workspace members, excluding the root.
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

/// Returns the direct links, of any kind, from `from` to `target` or to a package that reaches
/// `target` through [`Links::AsDependency`].
pub fn direct_links_reaching<'g>(from: &PackageMetadata<'g>, target: &PackageMetadata<'g>) -> Vec<PackageLink<'g>> {
    from.direct_links()
        .filter(|link| {
            let to = link.to();
            to.id() == target.id() || Reach::new(to, Links::AsDependency).route_to(target).is_some()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, process::Command};

    use super::*;

    /// Exercises renames, optional target-specific links, and root-only dev-dependency traversal.
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
    fn built_reach_follows_root_dev_links_but_not_dependency_dev_links() {
        let fixture = Fixture::new();
        let workspace = fixture.load();
        let a = workspace.member("a").unwrap();
        let reach = Reach::new(a, Links::Built);

        assert_eq!(
            member_names(reach.workspace_members()),
            BTreeSet::from(["b", "c", "d", "e", "g"])
        );
        assert!(reach.route_to(&workspace.member("f").unwrap()).is_none());
        assert_eq!(
            reach.route_to(&workspace.member("g").unwrap()).unwrap().to_string(),
            "a -[build]-> d -[normal]-> g"
        );
        assert_eq!(
            reach.route_to(&workspace.member("e").unwrap()).unwrap().to_string(),
            "a -[normal]-> e"
        );

        let c_reach = Reach::new(workspace.member("c").unwrap(), Links::Built);
        assert_eq!(
            c_reach.route_to(&workspace.member("f").unwrap()).unwrap().to_string(),
            "c -[dev]-> f"
        );
    }

    #[test]
    fn normal_reach_follows_only_normal_links() {
        let fixture = Fixture::new();
        let workspace = fixture.load();
        let reach = Reach::new(workspace.member("a").unwrap(), Links::Normal);

        assert_eq!(member_names(reach.workspace_members()), BTreeSet::from(["b", "e"]));
    }

    #[test]
    fn direct_links_reaching_matches_target_and_packages_reaching_it() {
        let fixture = Fixture::new();
        let workspace = fixture.load();
        let a = workspace.member("a").unwrap();

        let to_b: Vec<_> = direct_links_reaching(&a, &workspace.member("b").unwrap())
            .iter()
            .map(|link| (link.resolved_name(), link.to().name()))
            .collect();
        assert_eq!(to_b, [("renamed", "b")]);

        let to_g: Vec<_> = direct_links_reaching(&a, &workspace.member("g").unwrap())
            .iter()
            .map(|link| (link.resolved_name(), LinkKinds(link).to_string()))
            .collect();
        assert_eq!(to_g, [("d", "build".to_string())]);

        assert!(direct_links_reaching(&a, &workspace.member("f").unwrap()).is_empty());
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
