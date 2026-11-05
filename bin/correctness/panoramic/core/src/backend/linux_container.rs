//! Creates and reaches participants in a prepared Linux container isolation scope.
//!
//! Exec and courier calls have their own limits. Other backend calls are safe to drop; callers bound them.

use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use airlock::{
    driver::{Driver, DriverConfig, Protocol},
    exec::ExecRequest,
    files::ContainerFile,
    group::{IsolationGroup, SHARED_ROOT},
};
use async_trait::async_trait;
use bytes::Bytes;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde_json::Value;

use super::{
    container::{exec_request, ContainerObserver},
    courier, LateFact, ParticipantRequest, RunBackends, RuntimeParticipant, ScopeRuntime,
};
use crate::{
    artifact::ArtifactKind,
    endpoint::{ResolvedAddress, RunnerHostPort, Transport},
    handle::{ExecCommand, ExecOutput, FileStatus, HandleError, HttpResponse, Primitives},
    layout::{Os, RealizationKind},
    ledger::ParticipantRemoval,
    observe::{output::FILE_OUTPUT_START, ParticipantObservation},
    participant::ProcessSpec,
    plan::ScopeLayout,
    probe::{AttemptLimits, ProbeAnswer},
    recipe::{ContainerRecipe, Recipe, RecipeFile},
    tasks::CaseTasks,
};

/// A per-participant courier path in the container's own filesystem, separate from the shared volume.
const COURIER_PATH: &str = "/panoramic/courier";

pub(crate) const HOST_CGROUP_NAMESPACE: &str = "host_cgroup_namespace";

/// A prepared isolation scope and the courier bytes placed in each participant.
pub(crate) struct LinuxScope {
    group: IsolationGroup,
    courier: ContainerFile,
}

impl RunBackends {
    /// Prepares a scope using the verified fix-up image reference and the exact courier bytes the caller verified.
    ///
    /// Register `scope.id()` for removal before calling: failure or cancellation can leave partial resources.
    pub(crate) async fn prepare_linux_scope(
        &self, scope: &ScopeLayout, participants: &[&str], fixup_image: &str, courier: Bytes,
    ) -> Result<LinuxScope, GenericError> {
        let client = self
            .docker
            .client(Os::Linux)
            .await
            .map_err(|error| generic_error!(error))?;
        let group = IsolationGroup::prepare(client, scope.id(), participants, fixup_image).await?;
        Ok(LinuxScope {
            group,
            courier: ContainerFile::new(COURIER_PATH, courier).with_mode(0o755),
        })
    }
}

impl ScopeRuntime for LinuxScope {
    fn participant(&self, request: ParticipantRequest<'_>) -> Result<Arc<dyn RuntimeParticipant>, String> {
        let ParticipantRequest {
            name,
            realization,
            recipe,
            runner_host_ports,
            inputs,
            options,
            specs,
        } = request;
        debug_assert_eq!(realization.kind, RealizationKind::LinuxContainer);
        let Recipe::Container(recipe) = recipe else {
            return Err(format!(
                "recipe of participant '{name}' is not a container recipe for {} on {}",
                realization.kind.name(),
                realization.os.name(),
            ));
        };
        if !matches!(recipe.image.kind, ArtifactKind::Image { os: Os::Linux }) {
            return Err(format!(
                "recipe of participant '{name}' names role '{}', which is not a Linux image",
                recipe.image.name,
            ));
        }
        let image = inputs.image(recipe.image)?;
        self.container(name, image, &recipe, &runner_host_ports, options, specs)
            .map(|participant| Arc::new(participant) as Arc<dyn RuntimeParticipant>)
            .map_err(|error| format!("participant '{name}': {error:#}"))
    }
}

impl LinuxScope {
    /// Builds a participant from its image and recipe, publishing each runner-host port on `127.0.0.1`.
    fn container(
        &self, name: &str, image: &str, recipe: &ContainerRecipe, ports: &[RunnerHostPort],
        options: &BTreeMap<String, Value>, specs: &'static [ProcessSpec],
    ) -> Result<LinuxParticipant, GenericError> {
        let mut config = DriverConfig::from_image(name, image.to_owned());
        if let Some(entrypoint) = &recipe.entrypoint {
            config = config.with_entrypoint(entrypoint.clone());
        }
        if let Some(command) = &recipe.command {
            config = config.with_command(command.clone());
        }
        for (key, value) in container_env(name, &recipe.env)? {
            config = config.with_env_var(key, value);
        }
        for mount in &recipe.mounts {
            config = if mount.read_only {
                config.with_readonly_bind_mount(&mount.host_path, &mount.container_path)
            } else {
                config.with_bind_mount(&mount.host_path, &mount.container_path)
            };
        }
        for port in ports {
            config = config.with_published_port(protocol(port.transport), port.container_port, port.host_port);
        }
        config = config.with_host_pid_namespace(true).with_host_cgroup_namespace(
            options
                .get(HOST_CGROUP_NAMESPACE)
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
        let driver = self.group.driver(config)?;
        let files = container_files(&recipe.files, &self.courier)
            .with_error_context(|| format!("Invalid files for participant '{name}'"))?;
        Ok(LinuxParticipant {
            name: name.into(),
            driver: Arc::new(driver),
            files,
            ports: ports.to_vec(),
            specs,
            #[cfg(any(test, feature = "test-util"))]
            output_line_bytes: recipe.output_line_bytes,
        })
    }
}

fn container_env(name: &str, recipe_env: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>, GenericError> {
    let mut env = recipe_env.clone();
    for (key, value) in [
        ("PANORAMIC_PARTICIPANT", name.to_owned()),
        ("PANORAMIC_EXIT_DIR", format!("{SHARED_ROOT}/{name}/exit")),
        ("PANORAMIC_LOG_DIR", format!("{SHARED_ROOT}/{name}/log")),
    ] {
        if env.insert(key.into(), value).is_some() {
            return Err(generic_error!(
                "recipe of participant '{name}' sets reserved environment variable '{key}'"
            ));
        }
    }
    Ok(env)
}

/// A scope participant whose removal reference is available before container creation.
pub(crate) struct LinuxParticipant {
    name: String,
    driver: Arc<Driver>,
    files: Vec<ContainerFile>,
    ports: Vec<RunnerHostPort>,
    specs: &'static [ProcessSpec],
    #[cfg(any(test, feature = "test-util"))]
    output_line_bytes: Option<usize>,
}

#[async_trait]
impl RuntimeParticipant for LinuxParticipant {
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
            .with_error_context(|| {
                let context = format!("Failed to start participant '{}'", self.name);
                if self.ports.is_empty() {
                    return context;
                }
                let ports = self.ports.iter()
                    .map(|port| format!("127.0.0.1:{}/{}", port.host_port, port.transport.name()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{context}, which publishes runner-host ports {ports}; run one panoramic invocation per runner host and Docker daemon at a time")
            })?;
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
            exit_dir: format!("{SHARED_ROOT}/{}/exit/", self.name),
            log_dir: format!("{SHARED_ROOT}/{}/log/", self.name),
            follower: |path| {
                ExecRequest::new(vec![
                    "sh".into(),
                    "-c".into(),
                    "printf '%s' \"$1\"; exec tail -n +1 -F \"$2\"".into(),
                    "sh".into(),
                    FILE_OUTPUT_START.into(),
                    path.into(),
                ])
            },
            #[cfg(any(test, feature = "test-util"))]
            output_line_bytes: self.output_line_bytes,
        }
        .observe(tasks, output_dir)
        .await
    }
}

#[async_trait]
impl Primitives for LinuxParticipant {
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

    async fn send(&self, address: &ResolvedAddress, payload: &[u8], limits: &AttemptLimits) -> Result<(), HandleError> {
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

fn protocol(transport: Transport) -> Protocol {
    match transport {
        Transport::Tcp => Protocol::Tcp,
        Transport::Udp => Protocol::Udp,
    }
}

fn checked_recipe_files(files: &[RecipeFile]) -> Result<&[RecipeFile], GenericError> {
    for file in files {
        if file.path == COURIER_PATH {
            return Err(generic_error!(
                "Participant file '{}' uses a path the backend reserves",
                file.path
            ));
        }
    }
    Ok(files)
}

fn container_files(files: &[RecipeFile], courier: &ContainerFile) -> Result<Vec<ContainerFile>, GenericError> {
    let mut files: Vec<_> = checked_recipe_files(files)?
        .iter()
        .map(|file| ContainerFile::new(&file.path, file.contents.clone()).with_mode(file.mode))
        .collect();
    files.push(courier.clone());
    Ok(files)
}

#[cfg(test)]
pub(crate) mod daemon_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::FileKind;

    #[test]
    fn command_output_preserves_backend_bytes_and_exit_code() {
        let output = ExecOutput::from(airlock::exec::ExecOutput {
            stdout: vec![0, 255, b'\n'],
            stderr: vec![254, 0],
            exit_code: 17,
        });
        assert_eq!(
            output,
            ExecOutput {
                stdout: vec![0, 255, b'\n'],
                stderr: vec![254, 0],
                exit_code: 17
            }
        );
    }

    #[test]
    fn file_status_preserves_backend_kinds_modes_and_sizes() {
        use airlock::files::FileKind as BackendKind;
        for (kind, expected) in [
            (BackendKind::File, FileKind::File),
            (BackendKind::Directory, FileKind::Directory),
            (
                BackendKind::Symlink {
                    target: "/target".into(),
                },
                FileKind::Symlink {
                    target: "/target".into(),
                },
            ),
            (BackendKind::Socket, FileKind::Socket),
            (BackendKind::NamedPipe, FileKind::NamedPipe),
            (BackendKind::Other, FileKind::Other),
        ] {
            let status = FileStatus::from(airlock::files::FileStat {
                kind,
                mode: 0o4755,
                size: 123456789,
            });
            assert_eq!(
                status,
                FileStatus {
                    kind: expected,
                    mode: 0o4755,
                    size: 123456789
                }
            );
        }
    }

    #[test]
    fn every_container_gets_its_name_and_observation_directories() {
        for name in ["first", "second"] {
            let env = container_env(name, &BTreeMap::from([("CASE_VALUE".into(), "kept".into())])).unwrap();
            assert_eq!(
                env,
                BTreeMap::from([
                    ("CASE_VALUE".into(), "kept".into()),
                    ("PANORAMIC_PARTICIPANT".into(), name.into()),
                    ("PANORAMIC_EXIT_DIR".into(), format!("{SHARED_ROOT}/{name}/exit")),
                    ("PANORAMIC_LOG_DIR".into(), format!("{SHARED_ROOT}/{name}/log")),
                ])
            );
        }
    }

    #[test]
    fn a_recipe_cannot_set_a_reserved_observation_variable() {
        for key in ["PANORAMIC_PARTICIPANT", "PANORAMIC_EXIT_DIR", "PANORAMIC_LOG_DIR"] {
            let error = container_env("first", &BTreeMap::from([(key.into(), String::new())]))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("first") && error.contains(key) && error.contains("reserved"),
                "{error}"
            );
        }
    }

    #[test]
    fn runner_host_ports_map_to_published_container_ports() {
        assert_eq!(protocol(Transport::Tcp), Protocol::Tcp);
        assert_eq!(protocol(Transport::Udp), Protocol::Udp);
    }

    #[test]
    fn a_recipe_file_at_the_courier_path_is_rejected() {
        let files = [RecipeFile {
            path: COURIER_PATH.into(),
            mode: 0o600,
            contents: vec![],
        }];
        let courier = ContainerFile::new(COURIER_PATH, vec![]).with_mode(0o755);
        let error = container_files(&files, &courier).err().unwrap().to_string();
        assert!(error.contains(COURIER_PATH), "{error}");
        assert!(error.contains("backend reserves"), "{error}");
    }

    #[test]
    fn recipe_files_are_placed_with_one_courier() {
        let files = [
            RecipeFile {
                path: "/etc/settings".into(),
                mode: 0o600,
                contents: b"private".to_vec(),
            },
            RecipeFile {
                path: "/run/start".into(),
                mode: 0o700,
                contents: b"start".to_vec(),
            },
        ];
        let courier = ContainerFile::new(COURIER_PATH, b"courier".to_vec()).with_mode(0o755);
        assert_eq!(container_files(&files, &courier).unwrap().len(), files.len() + 1);
        assert_eq!(container_files(&[], &courier).unwrap().len(), 1);
    }
}
