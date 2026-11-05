//! Files, environment and host mounts supplied by a participant type.

use std::collections::BTreeMap;

use crate::artifact::ArtifactRole;

/// A participant type's recipe for one supported realization.
pub enum Recipe {
    /// A container created from a declared image role.
    Container(ContainerRecipe),
    /// Processes sharing a host directory and environment.
    HostProcessGroup(HostRecipe),
}

impl Recipe {
    pub(crate) fn roles(&self) -> Vec<ArtifactRole> {
        match self {
            Self::Container(recipe) => vec![recipe.image],
            Self::HostProcessGroup(recipe) => recipe.processes.iter().map(|process| process.program).collect(),
        }
    }
}

/// Environment, relative files and processes for a host process group.
pub struct HostRecipe {
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) files: Vec<RecipeFile>,
    pub(crate) processes: Vec<HostProcess>,
}

impl HostRecipe {
    /// Creates a host process group with processes in startup order.
    pub fn new(processes: Vec<HostProcess>) -> Self {
        Self {
            env: BTreeMap::new(),
            files: Vec::new(),
            processes,
        }
    }

    /// Sets the environment shared by a host process group.
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    /// Places a file relative to a host process group's participant directory before startup.
    pub fn with_file(mut self, path: impl Into<String>, mode: u32, contents: impl Into<Vec<u8>>) -> Self {
        self.files.push(RecipeFile {
            path: path.into(),
            mode,
            contents: contents.into(),
        });
        self
    }
}

/// One named process in a host process group.
#[cfg_attr(
    not(any(unix, test, feature = "test-util")),
    expect(dead_code, reason = "Unix host runtimes spawn the declared processes.")
)]
pub struct HostProcess {
    pub(crate) name: String,
    pub(crate) program: ArtifactRole,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
}

impl HostProcess {
    /// Declares a host process group's executable and arguments.
    pub fn new(name: impl Into<String>, program: ArtifactRole, args: Vec<String>) -> Self {
        Self {
            name: name.into(),
            program,
            args,
            env: BTreeMap::new(),
        }
    }

    /// Sets process-specific environment, overriding the host process group's environment.
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }
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

/// A file to place at an absolute container path or a path relative to a host participant directory.
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
        let Recipe::Container(recipe) = recipe else {
            panic!("expected a container recipe")
        };
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
        let Recipe::Container(recipe) = recipe else {
            panic!("expected a container recipe")
        };
        assert_eq!(recipe.image, image);
        assert_eq!(&recipe.env, env);
        assert!(recipe.entrypoint.is_none());
        assert!(recipe.command.is_none());
        assert!(recipe.mounts.is_empty());
    }

    /// Checks a container recipe's image, entrypoint, command and environment, with no host mounts.
    ///
    /// # Panics
    ///
    /// Panics if any setting differs; placed files can be checked with [`files`].
    #[track_caller]
    pub fn assert_container_command(
        recipe: &Recipe, image: ArtifactRole, entrypoint: &[&str], command: &[&str], env: &BTreeMap<String, String>,
    ) {
        let Recipe::Container(recipe) = recipe else {
            panic!("expected a container recipe")
        };
        assert_eq!(recipe.image, image);
        assert_eq!(recipe.entrypoint.as_deref().unwrap(), entrypoint);
        assert_eq!(recipe.command.as_deref().unwrap(), command);
        assert_eq!(&recipe.env, env);
        assert!(recipe.mounts.is_empty());
    }

    /// Returns each placed file's path, permission bits and contents.
    pub fn files(recipe: &Recipe) -> Vec<(&str, u32, &[u8])> {
        let files = match recipe {
            Recipe::Container(recipe) => &recipe.files,
            Recipe::HostProcessGroup(recipe) => &recipe.files,
        };
        files
            .iter()
            .map(|file| (file.path.as_str(), file.mode, file.contents.as_slice()))
            .collect()
    }

    /// Builds rendered configuration with the supplied environment and files and empty parameters.
    pub fn rendered(
        env: BTreeMap<String, String>, files: BTreeMap<String, Vec<u8>>,
    ) -> crate::participant::RenderedConfig {
        rendered_in("/participant", env, files)
    }

    /// Builds rendered configuration with the supplied environment, parameters and files.
    pub fn rendered_with_params(
        env: BTreeMap<String, String>, params: BTreeMap<String, serde_json::Value>, files: BTreeMap<String, Vec<u8>>,
    ) -> crate::participant::RenderedConfig {
        crate::participant::RenderedConfig {
            directory: "/participant".into(),
            env,
            params,
            files,
        }
    }

    /// Builds rendered configuration for a participant directory.
    pub fn rendered_in(
        directory: &str, env: BTreeMap<String, String>, files: BTreeMap<String, Vec<u8>>,
    ) -> crate::participant::RenderedConfig {
        crate::participant::RenderedConfig {
            directory: directory.into(),
            env,
            params: BTreeMap::new(),
            files,
        }
    }

    /// A host process's name, program, arguments and effective environment.
    pub type HostProcessView<'a> = (&'a str, ArtifactRole, &'a [String], BTreeMap<String, String>);

    /// Returns each host process's name, program, arguments and effective environment.
    ///
    /// # Panics
    ///
    /// Panics if the recipe is not a host process group.
    pub fn host_processes(recipe: &Recipe) -> Vec<HostProcessView<'_>> {
        let Recipe::HostProcessGroup(recipe) = recipe else {
            panic!("expected a host process group")
        };
        recipe
            .processes
            .iter()
            .map(|process| {
                let mut env = recipe.env.clone();
                env.extend(process.env.clone());
                (process.name.as_str(), process.program, process.args.as_slice(), env)
            })
            .collect()
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
        let Recipe::Container(container) = &mut recipe else {
            panic!("expected a container recipe")
        };
        container.output_line_bytes = Some(limit);
        recipe
    }

    /// Builds a shell fixture from `SCRIPT`, with optional `ENTRYPOINT` and `HOST_MOUNT` fault injection.
    pub fn shell(image: ArtifactRole, config: &crate::participant::RenderedConfig) -> Recipe {
        let script = config.env.get("SCRIPT").map_or("exec sleep 60", String::as_str);
        let Recipe::Container(mut recipe) = container(image, &["sh", "-c"], &[script]) else {
            unreachable!("container builds a container recipe")
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::ArtifactKind;

    #[test]
    fn host_recipe_builders_keep_env_files_and_process_env() {
        let role = ArtifactRole {
            name: "shell",
            kind: ArtifactKind::Binary,
            local_default: "/bin/sh",
        };
        let shared = BTreeMap::from([("KEY".into(), "shared".into())]);
        let process_env = BTreeMap::from([("KEY".into(), "process".into())]);
        let recipe = HostRecipe::new(vec![
            HostProcess::new("main", role, vec!["script".into()]).with_env(process_env.clone())
        ])
        .with_env(shared.clone())
        .with_file("settings", 0o644, b"data".to_vec());
        assert_eq!(recipe.env, shared);
        assert_eq!(recipe.files.len(), 1);
        assert_eq!(recipe.files[0].path, "settings");
        assert_eq!(recipe.files[0].mode, 0o644);
        assert_eq!(recipe.files[0].contents, b"data");
        assert_eq!(recipe.processes.len(), 1);
        assert_eq!(recipe.processes[0].name, "main");
        assert_eq!(recipe.processes[0].program, role);
        assert_eq!(recipe.processes[0].args, ["script"]);
        assert_eq!(recipe.processes[0].env, process_env);
    }
}
