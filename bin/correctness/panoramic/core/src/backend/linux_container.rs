//! Creates and reaches participants in a prepared Linux container isolation scope.
//!
//! Exec and courier calls have their own limits. Other backend calls are safe to drop; callers bound them.

use std::{path::Path, sync::Arc, time::Duration};

use airlock::{
    driver::{ContainerRef, Driver, DriverConfig, Protocol},
    exec::{ExecOutput, ExecRequest},
    files::{ContainerFile, FileStat},
    group::{IsolationGroup, SHARED_ROOT},
};
use async_trait::async_trait;
use bytes::Bytes;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::sync::watch;

use super::{courier, RunBackends};
use crate::{
    endpoint::{ResolvedAddress, RunnerHostPort, Transport},
    handle::{ExecCommand, HandleError, Primitives},
    layout::Os,
    observe::{
        exit_record::watch_record,
        follow_output,
        output::{host_output_file, FileOutput, FILE_OUTPUT_START},
        watch_exit, Capture, ExitObservation, ParticipantExit, ParticipantObservation, ProcessObservation,
        ProcessOutput,
    },
    participant::{ExitChannel, OutputChannel, ProcessSpec},
    plan::ScopeLayout,
    probe::{AttemptLimits, ProbeAnswer},
    recipe::{ContainerRecipe, RecipeFile},
    tasks::CaseTasks,
};

/// A per-participant courier path in the container's own filesystem, separate from the shared volume.
const COURIER_PATH: &str = "/panoramic/courier";

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

    /// Removes all scope resources, including partial preparation. Cancel-safe; the caller bounds it.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Used by daemon tests to check scope removal through the shared connection."
        )
    )]
    pub(crate) async fn remove_linux_scope(&self, scope: &ScopeLayout) -> Result<(), GenericError> {
        let client = self
            .docker
            .client(Os::Linux)
            .await
            .map_err(|error| generic_error!(error))?;
        IsolationGroup::remove(&client, &scope.id()).await
    }
}

impl LinuxScope {
    /// Builds a participant from its image and recipe, publishing each runner-host port on `127.0.0.1`.
    pub(crate) fn participant(
        &self, name: &str, image: &str, recipe: &ContainerRecipe, ports: &[RunnerHostPort],
    ) -> Result<LinuxParticipant, GenericError> {
        let mut config = DriverConfig::from_image(name, image.to_owned());
        if let Some(entrypoint) = &recipe.entrypoint {
            config = config.with_entrypoint(entrypoint.clone());
        }
        if let Some(command) = &recipe.command {
            config = config.with_command(command.clone());
        }
        for (key, value) in &recipe.env {
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
        // Linux container participants share the host PID namespace by default.
        config = config.with_host_pid_namespace(true);
        let driver = self.group.driver(config)?;
        let files = container_files(&recipe.files, &self.courier)
            .with_error_context(|| format!("Invalid files for participant '{name}'"))?;
        Ok(LinuxParticipant {
            name: name.into(),
            driver,
            files,
            ports: ports.to_vec(),
        })
    }
}

/// A scope participant whose removal reference is available before container creation.
pub(crate) struct LinuxParticipant {
    name: String,
    driver: Driver,
    files: Vec<ContainerFile>,
    ports: Vec<RunnerHostPort>,
}

impl LinuxParticipant {
    /// Returns a removal reference before creation, so reclaim can be registered first.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Used by daemon tests to remove individual participants.")
    )]
    pub(crate) fn container_ref(&self) -> ContainerRef {
        self.driver.container_ref()
    }

    /// Creates the container and places its files, returning the full container ID.
    /// On placement failure the container exists; reclaim it through `container_ref` or scope removal.
    pub(crate) async fn create(&self) -> Result<String, GenericError> {
        let id = self
            .driver
            .create()
            .await
            .with_error_context(|| format!("Failed to create participant '{}'", self.name))?;
        self.driver
            .place_files(&self.files)
            .await
            .with_error_context(|| format!("Failed to place files in participant '{}'", self.name))?;
        Ok(id)
    }

    /// Starts the participant and returns its IP if still attached to the network.
    /// Returns `None` if an immediate exit has already detached the container.
    pub(crate) async fn start(&self) -> Result<Option<String>, GenericError> {
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
        Ok(details.container_ip().map(str::to_owned))
    }

    /// Follows a started container's exit and output, and each record-channel process's exit,
    /// until the case stops its tasks.
    pub(crate) async fn observe(
        self: &Arc<Self>, specs: &[ProcessSpec], tasks: &CaseTasks, output_dir: &Path,
    ) -> ParticipantObservation {
        let (exit_tx, exit_rx) = watch::channel(ParticipantExit::Running);
        let (capture_tx, capture_rx) = watch::channel(Capture::Following);
        let mut observation = ParticipantObservation::new(
            exit_rx.clone(),
            capture_rx.clone(),
            output_dir.join("container.stdout.log"),
            output_dir.join("container.stderr.log"),
        );
        tasks.spawn(watch_exit(self.driver.wait_exit(), exit_tx));
        let mut container_output = None;
        for spec in specs {
            let process = match spec.exit {
                ExitChannel::Container => ProcessObservation::Container(exit_rx.clone()),
                ExitChannel::Record { file } => {
                    let path = format!("{SHARED_ROOT}/{}/exit/{file}", self.name);
                    let participant = self.clone();
                    let name = spec.name;
                    let exit = exit_rx.clone();
                    let (tx, rx) = watch::channel(ExitObservation::Running);
                    tasks.spawn(async move {
                        watch_record(
                            || participant.driver.read_file(&path),
                            exit,
                            tx,
                            &participant.name,
                            name,
                            &path,
                        )
                        .await;
                    });
                    ProcessObservation::Record(rx)
                }
            };
            observation.processes.insert(spec.name, process);
            let output = match spec.output {
                OutputChannel::Container => {
                    container_output = Some(spec.name);
                    ProcessOutput {
                        capture: capture_rx.clone(),
                        paths: vec![observation.stdout_path.clone(), observation.stderr_path.clone()],
                    }
                }
                OutputChannel::File { file, stream } => {
                    let path = format!("{SHARED_ROOT}/{}/log/{file}", self.name);
                    let filename = host_output_file(spec.name, spec.output).expect("file output has a host file name");
                    let host_path = output_dir.join(filename);
                    let output = FileOutput {
                        participant: self.name.clone().into(),
                        process: spec.name,
                        stream,
                        source_path: path.clone(),
                        host_path: host_path.clone(),
                        lines: observation.lines.clone(),
                    };
                    let participant = self.clone();
                    let exit = exit_rx.clone();
                    let (tx, rx) = watch::channel(Capture::Following);
                    tasks.spawn(async move {
                        output
                            .follow(
                                participant.driver.exec_stream(ExecRequest::new(vec![
                                    "sh".into(),
                                    "-c".into(),
                                    "printf '%s' \"$1\"; exec tail -n +1 -F \"$2\"".into(),
                                    "sh".into(),
                                    FILE_OUTPUT_START.into(),
                                    path.clone(),
                                ])),
                                || participant.driver.read_file(&path),
                                exit,
                                tx,
                            )
                            .await;
                    });
                    ProcessOutput {
                        capture: rx,
                        paths: vec![host_path],
                    }
                }
            };
            observation.outputs.insert(spec.name, output);
        }
        match self.driver.logs().await {
            Ok(stream) => {
                tasks.spawn(follow_output(
                    stream,
                    observation.stdout_path.clone(),
                    observation.stderr_path.clone(),
                    capture_tx,
                    observation.lines.clone(),
                    self.name.clone().into(),
                    container_output,
                ));
            }
            Err(error) => {
                capture_tx.send_replace(Capture::Failed {
                    reason: format!("{error:#}"),
                });
            }
        }
        observation
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
            .map_err(|error| HandleError::Harness {
                message: format!("Exec in participant '{}' failed: {error:#}", self.name),
            })
    }

    async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, HandleError> {
        self.driver.read_file(path).await.map_err(|error| HandleError::Harness {
            message: format!("Failed to read '{path}' in participant '{}': {error:#}", self.name),
        })
    }

    async fn stat(&self, path: &str) -> Result<Option<FileStat>, HandleError> {
        self.driver.stat(path).await.map_err(|error| HandleError::Harness {
            message: format!("Failed to stat '{path}' in participant '{}': {error:#}", self.name),
        })
    }

    async fn probe(&self, address: &ResolvedAddress, limits: &AttemptLimits) -> Result<ProbeAnswer, HandleError> {
        courier::probe(
            |command| self.driver.exec(exec_request(command)),
            COURIER_PATH,
            address,
            limits,
        )
        .await
    }

    async fn send(&self, address: &ResolvedAddress, payload: &[u8], limits: &AttemptLimits) -> Result<(), HandleError> {
        courier::send(
            |command| self.driver.exec(exec_request(command)),
            COURIER_PATH,
            address,
            payload,
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

fn exec_request(command: ExecCommand) -> ExecRequest {
    let mut request = ExecRequest::new(command.argv);
    for (key, value) in command.env {
        request = request.with_env_var(key, value);
    }
    if let Some(stdin) = command.stdin {
        request = request.with_stdin(stdin);
    }
    request
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
