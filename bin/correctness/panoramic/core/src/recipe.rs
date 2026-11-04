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

impl ContainerRecipe {
    /// Creates a recipe using the image's entrypoint, command and environment defaults.
    pub fn new(image: ArtifactRole) -> Self {
        Self {
            image,
            entrypoint: None,
            command: None,
            env: BTreeMap::new(),
            files: Vec::new(),
            mounts: Vec::new(),
        }
    }

    /// Replaces the image's entrypoint.
    pub fn with_entrypoint(mut self, entrypoint: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.entrypoint = Some(entrypoint.into_iter().map(Into::into).collect());
        self
    }

    /// Sets the rendered environment, overriding matching image defaults.
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }
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

/// Recipes for test participants that exercise lifecycle callers outside this crate.
#[cfg(any(test, feature = "test-util"))]
pub mod test_support {
    use super::*;

    /// Checks a recipe that overrides only the image entrypoint and environment.
    ///
    /// # Panics
    ///
    /// Panics if the recipe differs from the expected declarations or adds files, mounts or a command.
    #[track_caller]
    pub fn assert_container(recipe: &Recipe, image: ArtifactRole, entrypoint: &[&str], env: &BTreeMap<String, String>) {
        let Recipe::Container(recipe) = recipe;
        assert_eq!(recipe.image, image);
        assert_eq!(recipe.entrypoint.as_deref().unwrap(), entrypoint);
        assert_eq!(&recipe.env, env);
        assert!(recipe.command.is_none());
        assert!(recipe.files.is_empty());
        assert!(recipe.mounts.is_empty());
    }

    /// Builds rendered configuration with the supplied environment and empty files and parameters.
    pub fn rendered_env(env: BTreeMap<String, String>) -> crate::participant::RenderedConfig {
        crate::participant::RenderedConfig {
            env,
            params: BTreeMap::new(),
            files: BTreeMap::new(),
        }
    }

    /// Builds a container recipe with the supplied image, entrypoint and command.
    pub fn container(image: ArtifactRole, entrypoint: &[&str], command: &[&str]) -> Recipe {
        Recipe::Container(ContainerRecipe {
            image,
            entrypoint: Some(entrypoint.iter().map(|arg| (*arg).into()).collect()),
            command: Some(command.iter().map(|arg| (*arg).into()).collect()),
            env: BTreeMap::new(),
            files: vec![],
            mounts: vec![],
        })
    }

    /// Builds a shell fixture from `SCRIPT`, with optional `ENTRYPOINT` and `HOST_MOUNT` fault injection.
    pub fn shell(image: ArtifactRole, config: &crate::participant::RenderedConfig) -> Recipe {
        let script = config.env.get("SCRIPT").map_or("exec sleep 60", String::as_str);
        let Recipe::Container(mut recipe) = container(image, &["sh", "-c"], &[script]);
        recipe.env = config.env.clone();
        if let Some(entrypoint) = config.env.get("ENTRYPOINT") {
            recipe.entrypoint = Some(vec![entrypoint.clone()]);
        }
        if let Some(host_path) = config.env.get("HOST_MOUNT") {
            recipe.mounts.push(HostMount {
                host_path: host_path.clone(),
                container_path: "/fixture".into(),
                read_only: true,
            });
        }
        Recipe::Container(recipe)
    }
}
