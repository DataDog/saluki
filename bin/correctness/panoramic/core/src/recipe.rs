//! Files, environment and host mounts supplied by a participant type.

use std::collections::BTreeMap;

use crate::{artifact::ArtifactRole, participant::RenderedConfig};

/// A participant type's recipe for one supported realization.
pub enum Recipe {
    /// A container created from a declared image role.
    Container(ContainerRecipe),
    /// Processes sharing a host directory and environment.
    HostProcessGroup(HostRecipe),
    /// Named containers sharing a pod's network and case storage.
    Pod(PodRecipe),
}

impl Recipe {
    pub(crate) fn roles(&self) -> Vec<ArtifactRole> {
        match self {
            Self::Container(recipe) => vec![recipe.image],
            Self::HostProcessGroup(recipe) => recipe.processes.iter().map(|process| process.program).collect(),
            Self::Pod(recipe) => recipe
                .containers
                .iter()
                .map(|container| container.recipe.image)
                .collect(),
        }
    }
}

/// Containers sharing a pod. The first container supplies exec and the container-ID late fact.
/// Each container must set an entrypoint and command arguments (possibly empty), wrapped by a file-ready gate.
pub struct PodRecipe {
    pub(crate) containers: Vec<PodContainer>,
    /// Placed paths mapped to their source configuration names, including transformed files.
    pub(crate) late_files: BTreeMap<String, String>,
}

impl PodRecipe {
    /// Creates a pod with one named container and its explicit entrypoint and command.
    pub fn new(container: impl Into<String>, recipe: ContainerRecipe) -> Self {
        Self {
            containers: vec![PodContainer {
                name: container.into(),
                recipe,
                ports: Vec::new(),
            }],
            late_files: BTreeMap::new(),
        }
    }

    /// Defers a placed file derived from `config_name` until late references resolve after startup.
    pub fn with_late_file(mut self, path: impl Into<String>, config_name: impl Into<String>) -> Self {
        self.late_files.insert(path.into(), config_name.into());
        self
    }

    pub(crate) fn check_late_files(&self, config: &RenderedConfig) -> Result<(), String> {
        for (path, source) in &self.late_files {
            if !config.files.contains_key(source) {
                return Err(format!(
                    "late recipe file '{path}' names missing configuration file '{source}'"
                ));
            }
            if !self
                .containers
                .iter()
                .any(|container| container.recipe.files.iter().any(|file| &file.path == path))
            {
                return Err(format!(
                    "configuration file '{source}' names unplaced late recipe file '{path}'"
                ));
            }
        }
        for name in &config.late_files {
            let placed = self.late_files.values().any(|source| source == name);
            if !placed {
                return Err(format!(
                    "configuration file '{name}' has deferred late references but is not placed as a late recipe file"
                ));
            }
        }
        Ok(())
    }
}

pub(crate) struct PodContainer {
    pub(crate) name: String,
    pub(crate) recipe: ContainerRecipe,
    pub(crate) ports: Vec<(crate::endpoint::Transport, u16)>,
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

    /// Mounts a path from the runtime host at an absolute container path, with the requested access mode.
    pub fn with_host_mount(
        mut self, host_path: impl Into<String>, container_path: impl Into<String>, read_only: bool,
    ) -> Self {
        self.mounts.push(HostMount {
            host_path: host_path.into(),
            container_path: container_path.into(),
            read_only,
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
    /// A path on the runtime host, including the kind node for pods.
    pub(crate) host_path: String,
    pub(crate) container_path: String,
    pub(crate) read_only: bool,
}

/// Recipes for test participants that exercise lifecycle callers outside this crate.
#[cfg(any(test, feature = "test-util"))]
pub mod test_support {
    use std::collections::BTreeSet;

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

    /// Checks a single-container pod's name, image, arguments and environment, with no host mounts or declared ports.
    ///
    /// # Panics
    ///
    /// Panics if any setting differs; placed files can be checked with [`files`].
    #[track_caller]
    pub fn assert_pod(
        recipe: &Recipe, name: &str, image: ArtifactRole, entrypoint: &[&str], command: &[&str],
        env: &BTreeMap<String, String>,
    ) {
        let Recipe::Pod(recipe) = recipe else {
            panic!("expected a pod recipe")
        };
        let [container] = recipe.containers.as_slice() else {
            panic!("expected exactly one pod container")
        };
        assert_eq!(container.name, name);
        assert!(container.ports.is_empty());
        let recipe = &container.recipe;
        assert_eq!(recipe.image, image);
        assert_eq!(recipe.entrypoint.as_deref().unwrap(), entrypoint);
        assert_eq!(recipe.command.as_deref().unwrap(), command);
        assert_eq!(&recipe.env, env);
        assert!(recipe.mounts.is_empty());
    }

    /// Returns each mount's host path, container path and read-only setting, in recipe order.
    pub fn host_mounts(recipe: &Recipe) -> Vec<(&str, &str, bool)> {
        let containers: Vec<_> = match recipe {
            Recipe::Container(container) => vec![container],
            Recipe::Pod(pod) => pod.containers.iter().map(|container| &container.recipe).collect(),
            Recipe::HostProcessGroup(_) => Vec::new(),
        };
        containers
            .into_iter()
            .flat_map(|container| {
                container
                    .mounts
                    .iter()
                    .map(|mount| (mount.host_path.as_str(), mount.container_path.as_str(), mount.read_only))
            })
            .collect()
    }

    /// Marks named files as deferred in a rendered-config fixture.
    pub fn with_deferred_files(mut config: RenderedConfig, names: &[&str]) -> RenderedConfig {
        config.late_files.extend(names.iter().map(|name| (*name).to_owned()));
        config
    }

    /// Returns the sorted paths written after the pod starts.
    pub fn late_files(recipe: &Recipe) -> Vec<&str> {
        match recipe {
            Recipe::Pod(pod) => pod.late_files.keys().map(String::as_str).collect(),
            _ => Vec::new(),
        }
    }

    /// Returns each placed file's path, permission bits and contents.
    pub fn files(recipe: &Recipe) -> Vec<(&str, u32, &[u8])> {
        let files = match recipe {
            Recipe::Container(recipe) => &recipe.files,
            Recipe::HostProcessGroup(recipe) => &recipe.files,
            Recipe::Pod(recipe) => {
                return recipe
                    .containers
                    .iter()
                    .flat_map(|container| {
                        container
                            .recipe
                            .files
                            .iter()
                            .map(|file| (file.path.as_str(), file.mode, file.contents.as_slice()))
                    })
                    .collect();
            }
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
            late_files: BTreeSet::new(),
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
            late_files: BTreeSet::new(),
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
    fn pod_recipe_wraps_one_named_container_and_marks_its_late_files() {
        let role = ArtifactRole {
            name: "tool",
            kind: ArtifactKind::Image {
                os: crate::layout::Os::Linux,
            },
            local_default: "tool:local",
        };
        let env = BTreeMap::from([("KEY".into(), "value".into())]);
        let pod = PodRecipe::new(
            "worker",
            ContainerRecipe::new(role)
                .with_entrypoint(["/tool"])
                .with_command(["--config", "/settings"])
                .with_env(env.clone())
                .with_file("/settings", 0o640, b"id={{ self.late.container_id }}".to_vec()),
        );
        assert!(pod.late_files.is_empty());
        let pod = pod
            .with_late_file("/settings", "settings.yaml")
            .with_late_file("/settings", "settings.yaml");
        assert_eq!(
            pod.late_files,
            BTreeMap::from([("/settings".into(), "settings.yaml".into())])
        );
        let recipe = Recipe::Pod(pod);
        test_support::assert_pod(&recipe, "worker", role, &["/tool"], &["--config", "/settings"], &env);
        assert_eq!(
            test_support::files(&recipe),
            [("/settings", 0o640, b"id={{ self.late.container_id }}".as_slice())]
        );
        assert_eq!(recipe.roles(), [role]);
    }

    #[test]
    fn every_deferred_config_file_needs_a_named_source_at_a_marked_path() {
        let first = b"id={{ self.late.container_id }}";
        let second = b"uid={{ self.late.pod_uid }}";
        let mut config = test_support::rendered(
            BTreeMap::new(),
            BTreeMap::from([
                ("first.yaml".into(), first.to_vec()),
                ("second.yaml".into(), second.to_vec()),
            ]),
        );
        config.late_files.extend(config.files.keys().cloned());
        let mut pod = PodRecipe::new(
            "main",
            ContainerRecipe::new(crate::backend::FIXUP_IMAGE)
                .with_file("/etc/first", 0o644, first.as_slice())
                .with_file("/etc/second", 0o644, second.as_slice()),
        );
        for (name, path) in [("first.yaml", "/etc/first"), ("second.yaml", "/etc/second")] {
            assert_eq!(
                pod.check_late_files(&config),
                Err(format!(
                    "configuration file '{name}' has deferred late references but is not placed as a late recipe file"
                ))
            );
            pod = pod.with_late_file(path, name);
        }
        assert_eq!(pod.check_late_files(&config), Ok(()));
        pod.containers[0].recipe.files.pop();
        assert_eq!(
            pod.check_late_files(&config),
            Err("configuration file 'second.yaml' names unplaced late recipe file '/etc/second'".into())
        );
    }

    #[test]
    fn transformed_late_files_retain_their_config_source_and_require_placement() {
        let mut config = test_support::rendered(
            BTreeMap::new(),
            BTreeMap::from([
                ("settings.yaml".into(), b"id: '{{ self.late.container_id }}'".to_vec()),
                ("other.yaml".into(), b"other: '{{ self.late.pod_uid }}'".to_vec()),
            ]),
        );
        config.late_files.insert("settings.yaml".into());
        for (source, path, expected) in [
            ("settings.yaml", "/etc/settings", None),
            (
                "missing.yaml",
                "/etc/settings",
                Some("names missing configuration file 'missing.yaml'"),
            ),
            (
                "settings.yaml",
                "/etc/missing",
                Some("names unplaced late recipe file '/etc/missing'"),
            ),
            (
                "other.yaml",
                "/etc/settings",
                Some("configuration file 'settings.yaml' has deferred late references"),
            ),
        ] {
            let pod = PodRecipe::new(
                "worker",
                ContainerRecipe::new(crate::backend::FIXUP_IMAGE).with_file(
                    "/etc/settings",
                    0o644,
                    b"{\"id\": \"{{ self.late.container_id }}\"}".as_slice(),
                ),
            )
            .with_late_file(path, source);
            let result = pod.check_late_files(&config);
            match expected {
                None => assert_eq!(result, Ok(())),
                Some(expected) => assert!(result.unwrap_err().contains(expected), "{source}: {path}"),
            }
        }
    }

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
