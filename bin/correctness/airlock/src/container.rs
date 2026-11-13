//! Named containers in a case scope: create, start, observe, stop and remove.
//!
//! Creating and starting are separate, so files can be placed between them. A container joins the case network
//! under its participant name, mounts the case volume when its scope has one, and can publish TCP ports on the runner's
//! `127.0.0.1`. Docker chooses each host port, and [`Container::status`] reads it back after start.

use std::{collections::HashMap, pin::Pin, time::Duration};

use bollard::{
    container::LogOutput,
    errors::Error,
    models::{
        ContainerCreateBody, ContainerStateStatusEnum, EndpointSettings, HostConfig, Mount, MountType,
        NetworkingConfig, PortBinding,
    },
    query_parameters::{
        CreateContainerOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder, StopContainerOptionsBuilder,
        WaitContainerOptionsBuilder,
    },
    Docker,
};
use bytes::Bytes;
use futures::{Stream, StreamExt as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::time::{timeout_at, Instant};

use crate::{
    driver::with_transient_error_retry,
    removal::{bounded, RequestError},
    scope::CaseScope,
};

/// The host address published ports bind to.
const PUBLISH_HOST_IP: &str = "127.0.0.1";

/// The ephemeral port range of a Linux container, for `net.ipv4.ip_local_port_range`. It stays above every fixed port a
/// test process listens on, so an outbound socket cannot take one.
pub const LINUX_EPHEMERAL_PORT_RANGE: &str = "60000 65535";

/// What to run in a container, and how it joins its case scope.
#[derive(Clone, Debug, Default)]
pub struct ContainerSpec {
    image: String,
    entrypoint: Option<Vec<String>>,
    command: Option<Vec<String>>,
    env: Vec<String>,
    published_ports: Vec<u16>,
    mounts: Vec<Mount>,
    sysctls: HashMap<String, String>,
    host_pid: bool,
}

impl ContainerSpec {
    /// Creates a spec that runs `image` with its own entrypoint and command.
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            ..Default::default()
        }
    }

    /// Replaces the image's entrypoint.
    pub fn with_entrypoint(mut self, entrypoint: Vec<String>) -> Self {
        self.entrypoint = Some(entrypoint);
        self
    }

    /// Replaces the image's command.
    pub fn with_command(mut self, command: Vec<String>) -> Self {
        self.command = Some(command);
        self
    }

    /// Adds an environment variable.
    pub fn with_env_var(mut self, key: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        self.env.push(format!("{}={}", key.as_ref(), value.as_ref()));
        self
    }

    /// Publishes a TCP port of the container on the runner's `127.0.0.1`, at a host port Docker chooses.
    pub fn with_published_port(mut self, port: u16) -> Self {
        self.published_ports.push(port);
        self
    }

    /// Mounts the host path `host` at `path` in the container. Creating the container fails if `host` does not exist.
    pub fn with_bind(mut self, host: impl Into<String>, path: impl Into<String>, read_only: bool) -> Self {
        self.mounts.push(Mount {
            source: Some(host.into()),
            target: Some(path.into()),
            typ: Some(MountType::BIND),
            read_only: Some(read_only),
            ..Default::default()
        });
        self
    }

    /// Sets a kernel parameter in the container's namespaces, such as `net.ipv4.ip_local_port_range`.
    pub fn with_sysctl(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.sysctls.insert(key.into(), value.into());
        self
    }

    /// Runs the container in the host's PID namespace.
    pub fn with_host_pid(mut self) -> Self {
        self.host_pid = true;
        self
    }
}

/// One output chunk of a container or an exec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Output {
    /// Bytes written to standard output.
    Stdout(Bytes),
    /// Bytes written to standard error.
    Stderr(Bytes),
}

impl Output {
    pub(crate) fn from_log(log: LogOutput) -> Option<Self> {
        match log {
            LogOutput::StdOut { message } | LogOutput::Console { message } => Some(Self::Stdout(message)),
            LogOutput::StdErr { message } => Some(Self::Stderr(message)),
            LogOutput::StdIn { .. } => None,
        }
    }
}

/// A stream of a container's output.
pub type OutputStream = Pin<Box<dyn Stream<Item = Result<Output, GenericError>> + Send>>;

/// A container that exists in Docker, created by [`CaseScope::create_container`].
#[derive(Clone, Debug)]
pub struct Container {
    pub(crate) docker: Docker,
    pub(crate) name: String,
    pub(crate) id: String,
    /// The case network the container joined.
    pub(crate) network: String,
}

/// What Docker reports of a container after its start.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Status {
    /// Whether the container is running.
    pub running: bool,
    /// The container's address on the case network, while it runs.
    pub ip: Option<String>,
    /// The host port Docker chose for each published TCP port, while the container runs.
    pub published_ports: HashMap<u16, u16>,
}

impl CaseScope {
    /// Creates the container for `participant` from `spec`, without starting it.
    ///
    /// The container is named [`CaseScope::container_name`], joins the case network under the participant's name, which
    /// is also its host name, and carries the scope's labels, so [`CaseScope::reclaim`] removes it.
    ///
    /// # Errors
    ///
    /// Returns an error if Docker refuses the container, for example because the image is missing or the name is
    /// taken, or if `deadline` passes first.
    pub async fn create_container(
        &self, participant: &str, spec: ContainerSpec, deadline: Instant,
    ) -> Result<Container, GenericError> {
        let name = self.container_name(participant);
        let network = self.resource_name();

        let networking_config = NetworkingConfig {
            endpoints_config: Some(HashMap::from([(
                network.clone(),
                EndpointSettings {
                    aliases: Some(vec![participant.to_string()]),
                    ..Default::default()
                },
            )])),
        };

        let binds = self.platform.volume_path.map(|path| {
            // The case volume has the network's name (see `CaseScope::resource_name`).
            let volume = self.resource_name();
            vec![format!("{}:{}", volume, path)]
        });

        let exposed_ports: Vec<String> = spec
            .published_ports
            .iter()
            .map(|port| format!("{}/tcp", port))
            .collect();
        let port_bindings: HashMap<String, Option<Vec<PortBinding>>> = exposed_ports
            .iter()
            .map(|port| {
                let binding = PortBinding {
                    host_ip: Some(PUBLISH_HOST_IP.to_string()),
                    host_port: None,
                };
                (port.clone(), Some(vec![binding]))
            })
            .collect();

        let body = ContainerCreateBody {
            hostname: Some(participant.to_string()),
            image: Some(spec.image),
            entrypoint: spec.entrypoint,
            cmd: spec.command,
            env: Some(spec.env),
            labels: Some(self.labels()),
            exposed_ports: Some(exposed_ports),
            host_config: Some(HostConfig {
                binds,
                mounts: (!spec.mounts.is_empty()).then_some(spec.mounts),
                network_mode: Some(network.clone()),
                port_bindings: Some(port_bindings),
                sysctls: (!spec.sysctls.is_empty()).then_some(spec.sysctls),
                pid_mode: spec.host_pid.then(|| "host".to_string()),
                ..Default::default()
            }),
            networking_config: Some(networking_config),
            ..Default::default()
        };
        let options = CreateContainerOptionsBuilder::default().name(&name).build();
        let created = bounded(Some(deadline), self.docker.create_container(Some(options), body))
            .await
            .map_err(|e| generic_error!("Failed to create container '{}': {}", name, e))?;

        Ok(Container {
            docker: self.docker.clone(),
            name,
            id: created.id,
            network,
        })
    }
}

impl Container {
    /// Returns the container's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the ID Docker gave the container.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Starts the container.
    ///
    /// # Errors
    ///
    /// Returns an error if Docker cannot start the container, for example because its command does not exist, or if
    /// `deadline` passes first.
    pub async fn start(&self, deadline: Instant) -> Result<(), GenericError> {
        // Retrying is safe: the daemon treats starting a running container as a no-op.
        let start = with_transient_error_retry("container start", &self.name, || async {
            self.docker
                .start_container(&self.name, None)
                .await
                .with_error_context(|| format!("Failed to start container '{}'.", self.name))
        });
        // The deadline bounds the waits between attempts, too.
        timeout_at(deadline, start).await.unwrap_or_else(|_| {
            Err(generic_error!(
                "Failed to start container '{}': {}",
                self.name,
                RequestError::DeadlineExceeded
            ))
        })
    }

    /// Returns the host port Docker chose for the published TCP `port`. Call it after [`Self::start`].
    ///
    /// # Errors
    ///
    /// Returns an error if inspecting the container fails, if Docker reports no binding of `port` on `127.0.0.1`, or if
    /// `deadline` passes first.
    pub async fn published_port(&self, port: u16, deadline: Instant) -> Result<u16, GenericError> {
        self.status(deadline)
            .await?
            .published_ports
            .get(&port)
            .copied()
            .ok_or_else(|| {
                generic_error!(
                    "Docker reports no host port for port {} of container '{}'.",
                    port,
                    self.name
                )
            })
    }

    /// Returns whether the container is running, with its address and published ports. Call it after [`Self::start`].
    ///
    /// # Errors
    ///
    /// Returns an error if inspecting the container fails, or if `deadline` passes first.
    pub async fn status(&self, deadline: Instant) -> Result<Status, GenericError> {
        let inspect = bounded(Some(deadline), self.docker.inspect_container(&self.name, None))
            .await
            .map_err(|e| generic_error!("Failed to inspect container '{}': {}", self.name, e))?;
        let running = inspect.state.and_then(|state| state.running).unwrap_or(false);
        let settings = inspect.network_settings.unwrap_or_default();
        let ip = settings
            .networks
            .and_then(|mut networks| networks.remove(&self.network))
            .and_then(|endpoint| endpoint.ip_address)
            .filter(|ip| !ip.is_empty());
        let published_ports = settings
            .ports
            .into_iter()
            .flatten()
            .filter_map(|(key, bindings)| {
                let port = key.strip_suffix("/tcp")?.parse().ok()?;
                let host_port = bindings
                    .into_iter()
                    .flatten()
                    .filter(|binding| binding.host_ip.as_deref() == Some(PUBLISH_HOST_IP))
                    .find_map(|binding| binding.host_port.and_then(|host_port| host_port.parse().ok()))?;
                Some((port, host_port))
            })
            .collect();
        Ok(Status {
            running,
            ip,
            published_ports,
        })
    }

    /// Waits for the container to stop running and returns its exit code. Call it after [`Self::start`].
    ///
    /// Death by signal `n` is reported as code 128 + `n`.
    ///
    /// # Errors
    ///
    /// Returns an error if Docker cannot report the exit, for example because the container was removed, if the
    /// container never ran, because it was not started or its start failed, or if `deadline` passes before the
    /// container exits.
    pub async fn wait(&self, deadline: Instant) -> Result<i64, GenericError> {
        let options = WaitContainerOptionsBuilder::default().condition("not-running").build();
        let waited = match timeout_at(deadline, self.docker.wait_container(&self.name, Some(options)).next()).await {
            Err(_) => Err(RequestError::DeadlineExceeded.to_string()),
            Ok(Some(Ok(response))) => match response.error.and_then(|error| error.message).filter(|m| !m.is_empty()) {
                Some(message) => Err(message),
                None => Ok(response.status_code),
            },
            // `bollard` turns a nonzero exit into this error, with Docker's message if it sent one.
            Ok(Some(Err(Error::DockerContainerWaitError { error, code }))) if error.is_empty() => Ok(code),
            Ok(Some(Err(Error::DockerContainerWaitError { error, .. }))) => Err(error),
            Ok(Some(Err(e))) => Err(e.to_string()),
            Ok(None) => Err("Docker ended the wait without an exit".to_string()),
        };
        let code = waited
            .map_err(|message| generic_error!("Failed to wait for container '{}' to exit: {}", self.name, message))?;

        // Docker also ends the wait of a container that never ran, with a code that no process returned.
        let state = bounded(Some(deadline), self.docker.inspect_container(&self.name, None))
            .await
            .map_err(|e| generic_error!("Failed to inspect container '{}' after its exit: {}", self.name, e))?
            .state
            .unwrap_or_default();
        if state.status == Some(ContainerStateStatusEnum::CREATED) {
            return Err(generic_error!(
                "Container '{}' never ran: {}",
                self.name,
                state
                    .error
                    .filter(|error| !error.is_empty())
                    .as_deref()
                    .unwrap_or("it was not started")
            ));
        }
        Ok(code)
    }

    /// Follows the container's output from its start until it exits.
    pub fn logs(&self) -> OutputStream {
        let options = LogsOptionsBuilder::default()
            .follow(true)
            .stdout(true)
            .stderr(true)
            .build();
        let name = self.name.clone();
        let logs = self.docker.logs(&self.name, Some(options)).filter_map(move |log| {
            let output = match log {
                Ok(log) => Output::from_log(log).map(Ok),
                Err(e) => Some(Err(generic_error!(
                    "Failed to read the output of container '{}': {}",
                    name,
                    e
                ))),
            };
            async move { output }
        });
        Box::pin(logs)
    }

    /// Stops the container: signals its main process, then kills it if it is still running after `grace`.
    ///
    /// Docker counts the grace in whole seconds, so a fraction of a second is rounded up.
    ///
    /// A container that is already stopped or gone counts as stopped.
    ///
    /// # Errors
    ///
    /// Returns an error if Docker fails to stop the container, or if `deadline` passes first.
    pub async fn stop(&self, grace: Duration, deadline: Instant) -> Result<(), GenericError> {
        let seconds = grace.as_secs() + u64::from(grace.subsec_nanos() > 0);
        let grace = i32::try_from(seconds).unwrap_or(i32::MAX);
        let options = StopContainerOptionsBuilder::default().t(grace).build();
        match bounded(Some(deadline), self.docker.stop_container(&self.name, Some(options))).await {
            Err(e) if !e.is_not_found() => Err(generic_error!("Failed to stop container '{}': {}", self.name, e)),
            _ => Ok(()),
        }
    }

    /// Force-removes the container and its anonymous volumes, killing it if it is running. A container that is already
    /// gone counts as removed.
    ///
    /// # Errors
    ///
    /// Returns an error if Docker fails to remove the container, or if `deadline` passes first.
    pub async fn remove(&self, deadline: Instant) -> Result<(), GenericError> {
        let options = RemoveContainerOptionsBuilder::default().force(true).v(true).build();
        match bounded(Some(deadline), self.docker.remove_container(&self.name, Some(options))).await {
            Err(e) if !e.is_not_found() => Err(generic_error!("Failed to remove container '{}': {}", self.name, e)),
            _ => Ok(()),
        }
    }
}
