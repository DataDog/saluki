//! Artifact roles name the images, binaries and installs that participants and backends run.
//! Each role declares the reference used when a run binds none.

/// What kind of artifact a role names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactKind {
    /// A container image reference.
    Image,
    /// A path to an executable file.
    Binary,
    /// A path to an installed directory tree.
    Install,
}

impl ArtifactKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Binary => "binary",
            Self::Install => "install",
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
