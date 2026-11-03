//! Files, environment and host mounts supplied by a participant type.

use std::collections::BTreeMap;

use crate::artifact::ArtifactRole;

/// A participant type's recipe for one supported realization.
pub enum Recipe {
    /// A container created from a declared image role.
    Container(ContainerRecipe),
}

/// Image role, entrypoint, command, environment, files and host mounts for one container.
pub struct ContainerRecipe {
    pub(crate) image: ArtifactRole,
    pub(crate) entrypoint: Option<Vec<String>>,
    pub(crate) command: Option<Vec<String>>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) files: Vec<RecipeFile>,
    pub(crate) mounts: Vec<HostMount>,
}

/// A file to place at an absolute container path.
pub(crate) struct RecipeFile {
    pub(crate) path: String,
    /// Permission bits applied when the file is placed.
    pub(crate) mode: u32,
    pub(crate) contents: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostMount {
    /// A path on the Docker daemon's host.
    pub(crate) host_path: String,
    pub(crate) container_path: String,
    pub(crate) read_only: bool,
}
