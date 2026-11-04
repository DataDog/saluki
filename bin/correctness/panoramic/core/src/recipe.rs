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
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) output_line_bytes: Option<usize>,
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
            #[cfg(any(test, feature = "test-util"))]
            output_line_bytes: None,
        }
    }

    /// Replaces the image's entrypoint.
    pub fn with_entrypoint(mut self, entrypoint: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.entrypoint = Some(entrypoint.into_iter().map(Into::into).collect());
        self
    }

    /// Replaces the image's command arguments.
    pub fn with_command(mut self, command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.command = Some(command.into_iter().map(Into::into).collect());
        self
    }

    /// Places a file at an absolute container path with the given permission bits before startup.
    pub fn with_file(mut self, path: impl Into<String>, mode: u32, contents: impl Into<Vec<u8>>) -> Self {
        self.files.push(RecipeFile {
            path: path.into(),
            mode,
            contents: contents.into(),
        });
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

    /// Checks an image recipe's image, environment and unmodified container defaults.
    ///
    /// # Panics
    ///
    /// Panics if the image, environment or image defaults differ.
    #[track_caller]
    pub fn assert_image_recipe(recipe: &Recipe, image: ArtifactRole, env: &BTreeMap<String, String>) {
        let Recipe::Container(recipe) = recipe;
        assert_eq!(recipe.image, image);
        assert_eq!(&recipe.env, env);
        assert!(recipe.entrypoint.is_none());
        assert!(recipe.command.is_none());
        assert!(recipe.mounts.is_empty());
    }

    /// Returns each placed file's path, permission bits and contents.
    pub fn files(recipe: &Recipe) -> Vec<(&str, u32, &[u8])> {
        let Recipe::Container(recipe) = recipe;
        recipe
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.mode, file.contents.as_slice()))
            .collect()
    }

    /// Builds rendered configuration with the supplied environment and files and empty parameters.
    pub fn rendered(
        env: BTreeMap<String, String>, files: BTreeMap<String, Vec<u8>>,
    ) -> crate::participant::RenderedConfig {
        crate::participant::RenderedConfig {
            env,
            params: BTreeMap::new(),
            files,
        }
    }

    /// Builds a container recipe with the supplied image, entrypoint and command.
    pub fn container(image: ArtifactRole, entrypoint: &[&str], command: &[&str]) -> Recipe {
        Recipe::Container(
            ContainerRecipe::new(image)
                .with_entrypoint(entrypoint.iter().copied())
                .with_command(command.iter().copied()),
        )
    }

    /// Sets a test participant's in-memory output bound while its files retain the complete output.
    pub fn with_output_line_bytes(mut recipe: Recipe, limit: usize) -> Recipe {
        let Recipe::Container(container) = &mut recipe;
        container.output_line_bytes = Some(limit);
        recipe
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
