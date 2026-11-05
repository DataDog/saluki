//! Creates and reaches participants in a prepared Windows container isolation scope.
//!
//! Exec and courier calls have their own limits. Other backend calls are safe to drop; callers bound them.

use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use airlock::{
    driver::{ContainerOs, Driver, DriverConfig},
    exec::ExecRequest,
    files::ContainerFile,
    group::IsolationGroup,
};
use async_trait::async_trait;
use bytes::Bytes;
use panoramic_courier::{Command, Invocation};
use saluki_error::{generic_error, ErrorContext as _, GenericError};

use super::{
    container::{exec_request, ContainerObserver},
    courier, LateFact, ParticipantRequest, RunBackends, RuntimeParticipant, ScopeRuntime,
};
use crate::{
    artifact::ArtifactKind,
    endpoint::{ResolvedAddress, RunnerHostPort},
    handle::{ExecCommand, ExecOutput, FileStatus, HandleError, HttpResponse, Primitives},
    layout::{Os, RealizationKind},
    ledger::ParticipantRemoval,
    observe::ParticipantObservation,
    participant::ProcessSpec,
    plan::ScopeLayout,
    probe::{AttemptLimits, ProbeAnswer},
    recipe::{ContainerRecipe, Recipe, RecipeFile},
    tasks::CaseTasks,
};

const COURIER_PATH: &str = r"C:\panoramic\courier.exe";
pub(super) const SHARED_ROOT: &str = r"C:\airlock";

/// A prepared isolation scope and the courier bytes placed in each participant.
pub(crate) struct WindowsScope {
    group: IsolationGroup,
    courier: ContainerFile,
}

impl RunBackends {
    /// Prepares a NAT network and retains the verified courier bytes for placement in each participant.
    ///
    /// Register `scope.id()` for removal before calling: failure or cancellation can leave partial resources.
    pub(crate) async fn prepare_windows_scope(
        &self, scope: &ScopeLayout, participants: &[&str], courier: Bytes,
    ) -> Result<WindowsScope, GenericError> {
        let client = self
            .docker
            .client(Os::Windows)
            .await
            .map_err(|error| generic_error!(error))?;
        let group = IsolationGroup::prepare_windows(client, scope.id(), participants).await?;
        Ok(WindowsScope {
            group,
            courier: ContainerFile::new(COURIER_PATH, courier),
        })
    }
}

impl ScopeRuntime for WindowsScope {
    fn participant(&self, request: ParticipantRequest<'_>) -> Result<Arc<dyn RuntimeParticipant>, String> {
        let ParticipantRequest {
            name,
            realization,
            recipe,
            runner_host_ports,
            options: _,
            inputs,
            specs,
        } = request;
        debug_assert_eq!(realization.kind, RealizationKind::WindowsContainer);
        let Recipe::Container(recipe) = recipe else {
            return Err(format!(
                "recipe of participant '{name}' is not a container recipe for {} on {}",
                realization.kind.name(),
                realization.os.name(),
            ));
        };
        let image = inputs.image(recipe.image)?;
        self.container(name, image, &recipe, &runner_host_ports, specs)
            .map(|participant| Arc::new(participant) as Arc<dyn RuntimeParticipant>)
            .map_err(|error| format!("participant '{name}': {error:#}"))
    }
}

impl WindowsScope {
    fn container(
        &self, name: &str, image: &str, recipe: &ContainerRecipe, ports: &[RunnerHostPort],
        specs: &'static [ProcessSpec],
    ) -> Result<WindowsParticipant, GenericError> {
        let config = container_config(name, image, recipe, ports)?;
        let driver = self.group.driver(config)?;
        let files = container_files(name, &recipe.files, &self.courier)
            .with_error_context(|| format!("Invalid files for participant '{name}'"))?;
        Ok(WindowsParticipant {
            name: name.into(),
            driver: Arc::new(driver),
            files,
            specs,
            #[cfg(any(test, feature = "test-util"))]
            output_line_bytes: recipe.output_line_bytes,
        })
    }
}

fn container_config(
    name: &str, image: &str, recipe: &ContainerRecipe, ports: &[RunnerHostPort],
) -> Result<DriverConfig, GenericError> {
    if !matches!(recipe.image.kind, ArtifactKind::Image { os: Os::Windows }) {
        return Err(generic_error!(
            "recipe of participant '{name}' names role '{}', which is not a Windows image",
            recipe.image.name
        ));
    }
    if !ports.is_empty() {
        return Err(generic_error!(
            "Windows participant '{name}' cannot publish runner-host ports; use courier routes"
        ));
    }
    if !recipe.mounts.is_empty() {
        return Err(generic_error!(
            "Windows participant '{name}' cannot mount runner-host paths"
        ));
    }
    let mut config = DriverConfig::from_image(name, image.to_owned()).with_container_os(ContainerOs::Windows);
    if let Some(entrypoint) = &recipe.entrypoint {
        config = config.with_entrypoint(entrypoint.clone());
    }
    if let Some(command) = &recipe.command {
        config = config.with_command(command.clone());
    }
    for (key, value) in container_env(name, &recipe.env)? {
        config = config.with_env_var(key, value);
    }
    Ok(config)
}

fn container_env(name: &str, recipe_env: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>, GenericError> {
    let mut env = recipe_env.clone();
    for (key, value) in [
        ("PANORAMIC_PARTICIPANT", name.to_owned()),
        ("PANORAMIC_EXIT_DIR", format!(r"{SHARED_ROOT}\{name}\exit")),
        ("PANORAMIC_LOG_DIR", format!(r"{SHARED_ROOT}\{name}\log")),
    ] {
        if env.keys().any(|name| name.eq_ignore_ascii_case(key)) {
            return Err(generic_error!(
                "recipe of participant '{name}' sets reserved environment variable '{key}'"
            ));
        }
        env.insert(key.into(), value);
    }
    Ok(env)
}

/// A scope participant whose removal reference is available before container creation.
pub(crate) struct WindowsParticipant {
    name: String,
    driver: Arc<Driver>,
    files: Vec<ContainerFile>,
    specs: &'static [ProcessSpec],
    #[cfg(any(test, feature = "test-util"))]
    output_line_bytes: Option<usize>,
}

#[async_trait]
impl RuntimeParticipant for WindowsParticipant {
    fn removal(&self) -> ParticipantRemoval {
        ParticipantRemoval::Container(self.driver.container_ref())
    }

    async fn create(&self) -> Result<Vec<(LateFact, String)>, GenericError> {
        let id = self
            .driver
            .create()
            .await
            .with_error_context(|| format!("Failed to create participant '{}'", self.name))?;
        self.driver
            .place_files(&self.files)
            .await
            .with_error_context(|| format!("Failed to place files in participant '{}'", self.name))?;
        Ok(vec![(LateFact::ContainerId, id)])
    }

    async fn start(&self) -> Result<Vec<(LateFact, String)>, GenericError> {
        let details = self
            .driver
            .start_container()
            .await
            .with_error_context(|| format!("Failed to start participant '{}'", self.name))?;
        Ok(details
            .container_ip()
            .map(|ip| (LateFact::Ip, ip.to_owned()))
            .into_iter()
            .collect())
    }

    async fn observe(self: Arc<Self>, tasks: &CaseTasks, output_dir: &Path) -> ParticipantObservation {
        ContainerObserver {
            driver: self.driver.clone(),
            name: self.name.clone(),
            specs: self.specs,
            exit_dir: format!(r"{SHARED_ROOT}\{}\exit\", self.name),
            log_dir: format!(r"{SHARED_ROOT}\{}\log\", self.name),
            follower: |path| {
                let invocation = Invocation {
                    command: Command::Follow { path: path.into() },
                    timeout: None,
                };
                let mut argv = vec![COURIER_PATH.into()];
                argv.extend(invocation.args());
                ExecRequest::new(argv)
            },
            #[cfg(any(test, feature = "test-util"))]
            output_line_bytes: self.output_line_bytes,
        }
        .observe(tasks, output_dir)
        .await
    }
}

#[async_trait]
impl Primitives for WindowsParticipant {
    async fn exec(&self, command: ExecCommand, limit: Duration) -> Result<ExecOutput, HandleError> {
        tokio::time::timeout(limit, self.driver.exec(exec_request(command)))
            .await
            .map_err(|_| HandleError::Harness {
                message: format!(
                    "Abandoned the exec in participant '{}' after {limit:?} without an exit; it may still be running",
                    self.name,
                ),
            })?
            .map(ExecOutput::from)
            .map_err(|error| HandleError::Harness {
                message: format!("Exec in participant '{}' failed: {error:#}", self.name),
            })
    }

    async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, HandleError> {
        self.driver.read_file(path).await.map_err(|error| HandleError::Harness {
            message: format!("Failed to read '{path}' in participant '{}': {error:#}", self.name),
        })
    }

    async fn stat(&self, path: &str) -> Result<Option<FileStatus>, HandleError> {
        self.driver
            .stat(path)
            .await
            .map(|status| status.map(FileStatus::from))
            .map_err(|error| HandleError::Harness {
                message: format!("Failed to stat '{path}' in participant '{}': {error:#}", self.name),
            })
    }

    async fn probe(&self, address: &ResolvedAddress, limits: &AttemptLimits) -> Result<ProbeAnswer, HandleError> {
        courier::probe(
            |command| async { self.driver.exec(exec_request(command)).await.map(ExecOutput::from) },
            COURIER_PATH,
            address,
            limits,
        )
        .await
    }

    async fn send(
        &self, address: &ResolvedAddress, payload: &[u8], limits: &AttemptLimits,
    ) -> Result<String, HandleError> {
        courier::send(
            |command| async { self.driver.exec(exec_request(command)).await.map(ExecOutput::from) },
            COURIER_PATH,
            address,
            payload,
            limits,
        )
        .await
    }

    async fn http(
        &self, address: &ResolvedAddress, path: &str, headers_only: bool, limits: &AttemptLimits,
    ) -> Result<HttpResponse, HandleError> {
        courier::http(
            |command| async { self.driver.exec(exec_request(command)).await.map(ExecOutput::from) },
            COURIER_PATH,
            address,
            path,
            headers_only,
            limits,
        )
        .await
    }
}

fn container_files(
    name: &str, files: &[RecipeFile], courier: &ContainerFile,
) -> Result<Vec<ContainerFile>, GenericError> {
    for file in files {
        if file.path.eq_ignore_ascii_case(COURIER_PATH) {
            return Err(generic_error!(
                "Participant file '{}' uses a path the backend reserves",
                file.path
            ));
        }
    }
    let mut files: Vec<_> = files
        .iter()
        .map(|file| ContainerFile::new(&file.path, file.contents.clone()).with_mode(file.mode))
        .collect();
    files.extend(["exit", "log", "shared"].map(|dir| ContainerFile::directory(format!(r"{SHARED_ROOT}\{name}\{dir}"))));
    files.push(courier.clone());
    Ok(files)
}

#[cfg(test)]
mod daemon_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{artifact::ArtifactRole, endpoint::Transport, recipe::HostMount};

    const IMAGE: ArtifactRole = ArtifactRole {
        name: "windows-image",
        kind: ArtifactKind::Image { os: Os::Windows },
        local_default: "windows:local",
    };

    #[test]
    fn each_participant_gets_its_name_and_own_observation_directories() {
        for name in ["first", "second"] {
            assert_eq!(
                container_env(name, &BTreeMap::from([("CASE_VALUE".into(), "kept".into())])).unwrap(),
                BTreeMap::from([
                    ("CASE_VALUE".into(), "kept".into()),
                    ("PANORAMIC_PARTICIPANT".into(), name.into()),
                    ("PANORAMIC_EXIT_DIR".into(), format!(r"C:\airlock\{name}\exit")),
                    ("PANORAMIC_LOG_DIR".into(), format!(r"C:\airlock\{name}\log")),
                ])
            );
            let courier = ContainerFile::new(COURIER_PATH, b"courier".to_vec());
            let files = [RecipeFile {
                path: r"C:\config\settings".into(),
                mode: 0o600,
                contents: b"private".to_vec(),
            }];
            assert_eq!(
                container_files(name, &files, &courier).unwrap(),
                vec![
                    ContainerFile::new(r"C:\config\settings", b"private".to_vec()).with_mode(0o600),
                    ContainerFile::directory(format!(r"C:\airlock\{name}\exit")),
                    ContainerFile::directory(format!(r"C:\airlock\{name}\log")),
                    ContainerFile::directory(format!(r"C:\airlock\{name}\shared")),
                    courier,
                ]
            );
        }
    }

    #[test]
    fn reserved_environment_and_courier_paths_are_case_insensitive() {
        for key in ["PANORAMIC_PARTICIPANT", "PANORAMIC_EXIT_DIR", "PANORAMIC_LOG_DIR"] {
            for key in [key.to_string(), key.to_lowercase()] {
                let error = container_env("first", &BTreeMap::from([(key.clone(), String::new())]))
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains("reserved environment variable") && error.contains("first"),
                    "{error}"
                );
            }
        }
        for path in [COURIER_PATH.to_string(), COURIER_PATH.to_lowercase()] {
            let courier = ContainerFile::new(COURIER_PATH, vec![]);
            let error = container_files(
                "first",
                &[RecipeFile {
                    path: path.clone(),
                    mode: 0o600,
                    contents: vec![],
                }],
                &courier,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(&path) && error.contains("backend reserves"), "{error}");
        }
    }

    #[test]
    fn windows_participants_reject_runner_host_ports_mounts_and_non_windows_images() {
        let recipe = ContainerRecipe::new(IMAGE);
        assert!(container_config("first", "local", &recipe, &[]).is_ok());
        let ports = [RunnerHostPort {
            transport: Transport::Tcp,
            container_port: 8080,
            host_port: 20000,
        }];
        let error = container_config("first", "local", &recipe, &ports)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("runner-host ports"), "{error}");
        let mut mounted = ContainerRecipe::new(IMAGE);
        mounted.mounts.push(HostMount {
            host_path: "/host".into(),
            container_path: r"C:\mounted".into(),
            read_only: true,
        });
        let error = container_config("first", "local", &mounted, &[])
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("mount runner-host paths"), "{error}");
        for kind in [
            ArtifactKind::Image { os: Os::Linux },
            ArtifactKind::Binary,
            ArtifactKind::ContainerBinary { os: Os::Windows },
        ] {
            let recipe = ContainerRecipe::new(ArtifactRole { kind, ..IMAGE });
            let error = container_config("first", "local", &recipe, &[])
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("not a Windows image"), "{error}");
        }
    }
}
