//! Artifact roles name the images, binaries and installs that participants and backends run.
//! Manifests bind roles to references for a run; omitted roles use their local defaults.
//! Plans carry resolved bindings with the source of each reference.
//! Verification checks availability and records identities.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use saluki_error::{generic_error, GenericError};
use serde::Deserialize;

mod verify;
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

/// Role bindings for one run, read from an artifact manifest. The default binds nothing.
#[derive(Clone, Debug, Default)]
pub struct ArtifactManifest {
    /// File the roles were read from; set whenever roles is non-empty.
    pub(crate) source: Option<PathBuf>,
    pub(crate) roles: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    schema: u32,
    #[serde(default)]
    roles: BTreeMap<String, String>,
}

impl ArtifactManifest {
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
        for (name, reference) in &file.roles {
            if reference.trim().is_empty() {
                return Err(generic_error!(
                    "artifact manifest {}: role '{name}' has an empty reference",
                    source.display()
                ));
            }
        }
        Ok(Self {
            source: Some(source.to_path_buf()),
            roles: file.roles,
        })
    }
}

/// Where a binding's reference came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
        assert_eq!(manifest.source.as_deref(), Some(Path::new("artifacts.toml")));
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
        assert_eq!(manifest.source, Some(path));
        assert_eq!(manifest.roles, BTreeMap::from([("tool".into(), "bin/tool".into())]));
    }
}
