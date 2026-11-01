//! Artifact roles name the images, binaries and installs that participants and backends run.
//! Manifests bind roles to references for a run and can be rendered from bindings; omitted roles use their local defaults.
//! Plans carry resolved bindings with the source of each reference.
//! Verification checks availability and records identities; pulling fetches images and records their IDs.

use std::{collections::BTreeMap, path::Path};

use saluki_error::{generic_error, GenericError};
use serde::{Deserialize, Serialize};

mod image_store;
mod pull;
mod verify;
pub use pull::pull;
pub use verify::{verify, ArtifactVerification};

#[cfg(any(test, feature = "test-util"))]
pub mod test_util;

/// What kind of artifact a role names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactKind {
    /// A container image reference.
    Image,
    /// A path to an executable file.
    Binary,
    /// A path to an installed directory tree.
    Install {
        /// Executable path relative to the install directory, used to identify the install.
        main_binary: &'static str,
    },
}

impl ArtifactKind {
    /// Returns the kind's display name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Binary => "binary",
            Self::Install { .. } => "install",
        }
    }
}

/// A stable name for an artifact a realization runs, with its local default.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactRole {
    /// Role name. Every declaration of one name, by any participant type or backend, names the same artifact.
    pub name: &'static str,
    /// What the role's reference names.
    pub kind: ArtifactKind,
    /// Reference used when a run binds nothing to this role.
    pub local_default: &'static str,
}

impl ArtifactRole {
    pub(crate) fn describe(self) -> String {
        let description = format!("{} '{}'", self.kind.name(), self.local_default);
        match self.kind {
            ArtifactKind::Install { main_binary } => format!("{description} with main binary '{main_binary}'"),
            _ => description,
        }
    }
}

/// Role bindings for one run. The default binds nothing.
#[derive(Clone, Debug, Default)]
pub struct ArtifactManifest {
    /// Label naming where the bindings came from in errors.
    pub(crate) origin: String,
    pub(crate) roles: BTreeMap<String, String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    schema: u32,
    #[serde(default)]
    roles: BTreeMap<String, String>,
}

impl ArtifactManifest {
    /// Creates a manifest from role bindings, preserving references as written.
    /// `origin` labels the bindings in errors.
    ///
    /// # Errors
    ///
    /// Returns an error if a reference is empty or whitespace-only.
    pub fn new(origin: impl Into<String>, roles: BTreeMap<String, String>) -> Result<Self, GenericError> {
        let origin = origin.into();
        for (name, reference) in &roles {
            if reference.trim().is_empty() {
                return Err(generic_error!("{origin}: role '{name}' has an empty reference"));
            }
        }
        Ok(Self { origin, roles })
    }

    /// Reads and parses a manifest file.
    ///
    /// # Errors
    ///
    /// Returns an error naming the file if it cannot be read or parsed as a supported manifest.
    pub fn load(path: &Path) -> Result<Self, GenericError> {
        let content = std::fs::read_to_string(path)
            .map_err(|error| generic_error!("failed to read artifact manifest {}: {error}", path.display()))?;
        Self::parse(&content, path)
    }

    /// Parses manifest text, preserving references as written. `source` names the file in errors.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid TOML, missing or unsupported schema, unknown top-level fields,
    /// or references that are empty, whitespace-only or not strings.
    pub fn parse(content: &str, source: &Path) -> Result<Self, GenericError> {
        let file: ManifestFile = toml::from_str(content)
            .map_err(|error| generic_error!("artifact manifest {}: {error}", source.display()))?;
        if file.schema != 1 {
            return Err(generic_error!(
                "artifact manifest {}: schema {} is not supported; expected schema = 1",
                source.display(),
                file.schema
            ));
        }
        Self::new(format!("artifact manifest {}", source.display()), file.roles)
    }

    /// Renders the schema and bindings as TOML, with roles in name order.
    pub fn render(&self) -> String {
        toml::to_string(&ManifestFile {
            schema: 1,
            roles: self.roles.clone(),
        })
        .expect("a u32 schema and a string-to-string map always serialize as TOML")
    }
}

/// Where a binding's reference came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingSource {
    /// A reference supplied by the run's artifact manifest.
    Manifest,
    /// The role's declared local default.
    LocalDefault,
}

impl BindingSource {
    /// Returns the source's display name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::LocalDefault => "local default",
        }
    }
}

/// An artifact role a plan needs, with the reference it resolves to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactBinding {
    /// The role needed by the selected layout.
    pub role: ArtifactRole,
    /// The resolved reference, preserved as written.
    pub reference: String,
    /// Where the resolved reference came from.
    pub source: BindingSource,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_sources_serialize_as_snake_case_names() {
        assert_eq!(serde_json::to_value(BindingSource::Manifest).unwrap(), "manifest");
        assert_eq!(
            serde_json::to_value(BindingSource::LocalDefault).unwrap(),
            "local_default"
        );
    }

    #[test]
    fn roles_describe_their_kind_local_default_and_main_binary() {
        for (kind, local_default, expected) in [
            (ArtifactKind::Image, "tool:latest", "image 'tool:latest'"),
            (ArtifactKind::Binary, "bin/tool", "binary 'bin/tool'"),
            (
                ArtifactKind::Install {
                    main_binary: "bin/tool",
                },
                "share/tool",
                "install 'share/tool' with main binary 'bin/tool'",
            ),
        ] {
            let role = ArtifactRole {
                name: "tool",
                kind,
                local_default,
            };
            assert_eq!(role.describe(), expected);
        }
    }

    #[test]
    fn a_manifest_binds_each_role_to_its_reference_as_written() {
        let manifest = ArtifactManifest::parse(
            "schema = 1\n[roles]\ntool = '../bin/tool'\nresources = ' share/tool '\n",
            Path::new("artifacts.toml"),
        )
        .unwrap();
        assert_eq!(manifest.origin, "artifact manifest artifacts.toml");
        assert_eq!(
            manifest.roles,
            BTreeMap::from([
                ("tool".into(), "../bin/tool".into()),
                ("resources".into(), " share/tool ".into()),
            ])
        );
    }

    #[test]
    fn a_manifest_without_a_roles_table_binds_nothing() {
        let manifest = ArtifactManifest::parse("schema = 1", Path::new("artifacts.toml")).unwrap();
        assert!(manifest.roles.is_empty());
        assert!(ArtifactManifest::default().roles.is_empty());
    }

    #[test]
    fn a_manifest_renders_its_schema_and_roles_in_name_order_as_toml() {
        let manifest = ArtifactManifest::new(
            "bindings",
            BTreeMap::from([("b".into(), "y".into()), ("a".into(), "x".into())]),
        )
        .unwrap();
        assert_eq!(manifest.render(), "schema = 1\n\n[roles]\na = \"x\"\nb = \"y\"\n");
    }

    #[test]
    fn a_rendered_manifest_parses_back_to_the_same_bindings() {
        let manifest = ArtifactManifest::new(
            "bindings",
            BTreeMap::from([
                ("equals".into(), "a=b=c".into()),
                ("path".into(), r"C:\tools\tool.log".into()),
                ("quotes".into(), "\"double\" and 'single'".into()),
                ("spaces".into(), " reference ".into()),
                ("unicode".into(), "référence/工具".into()),
            ]),
        )
        .unwrap();
        for manifest in [manifest, ArtifactManifest::default()] {
            let parsed = ArtifactManifest::parse(&manifest.render(), Path::new("rendered.toml")).unwrap();
            assert_eq!(parsed.roles, manifest.roles);
        }
    }

    #[test]
    fn manifests_built_from_roles_reject_empty_references_naming_their_origin() {
        for reference in ["", " "] {
            let error =
                ArtifactManifest::new("--set", BTreeMap::from([("tool".into(), reference.into())])).unwrap_err();
            assert_eq!(error.to_string(), "--set: role 'tool' has an empty reference");
        }
    }

    #[test]
    fn invalid_manifests_fail_naming_the_file_and_problem() {
        for (content, expected) in [
            ("[roles]\ntool = 'bin/tool'", "missing field `schema`"),
            ("schema = 2", "schema 2 is not supported; expected schema = 1"),
            ("schema = 1\nextra = true", "unknown field `extra`"),
            ("schema = 1\n[roles]\ntool = 1", "expected a string"),
            ("schema = 1\n[roles.tool]\npath = 'bin/tool'", "expected a string"),
            ("schema = 1\n[roles]\ntool = ''", "role 'tool' has an empty reference"),
            ("schema = 1\n[roles]\ntool = ' '", "role 'tool' has an empty reference"),
            ("this is not toml", "TOML parse error"),
        ] {
            let message = ArtifactManifest::parse(content, Path::new("cases/artifacts.toml"))
                .unwrap_err()
                .to_string();
            assert!(
                message.starts_with("artifact manifest cases/artifacts.toml:"),
                "{message}"
            );
            assert!(message.contains(expected), "{content}: {message}");
        }
    }

    #[test]
    fn loading_a_missing_manifest_fails_naming_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.toml");
        let message = ArtifactManifest::load(&path).unwrap_err().to_string();
        assert!(
            message.starts_with(&format!("failed to read artifact manifest {}:", path.display())),
            "{message}"
        );
    }

    #[test]
    fn loading_a_manifest_reads_its_bindings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifacts.toml");
        std::fs::write(&path, "schema = 1\n[roles]\ntool = 'bin/tool'").unwrap();
        let manifest = ArtifactManifest::load(&path).unwrap();
        assert_eq!(manifest.origin, format!("artifact manifest {}", path.display()));
        assert_eq!(manifest.roles, BTreeMap::from([("tool".into(), "bin/tool".into())]));
    }
}
