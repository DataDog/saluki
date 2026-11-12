//! The run-time half of the container backend: the scope of each case, and the container of each participant.
//!
//! Per case, the backend prepares an airlock case scope: a network every container joins under its participant name,
//! and, where the container OS has one, a volume mounted at the shared root in every container, which holds each
//! participant's own directories. Per participant, it creates a container from the planned participant alone, places
//! the participant's files and the courier as one archive, starts the container, and reads back the address and the
//! host port of each published endpoint. Every resource is entered in the ledger before it is made, and the ledger
//! reclaims the scope: its containers, then its volume, then its network.
//!
//! What differs between container OSes is data: the planning facts of the enclosure, and a [`ContainerOs`] row
//! for the rest.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use airlock::container::{Container, ContainerSpec, LINUX_EPHEMERAL_PORT_RANGE};
use airlock::exec::ExecRequest;
use airlock::files::Archive;
use airlock::scope::{CaseScope, ScopePlatform};
use async_trait::async_trait;
use bollard::Docker;
use bytes::Bytes;
use saluki_error::{generic_error, GenericError};
use tokio::io::AsyncWrite;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::backend::{Exec, Facts, Realized, EXIT_DIR_ENV, LOG_DIR_ENV};
use super::ledger::{random_id, ResourceLedger};
use super::participant::Enclosure;
use super::plan::{Launch, PlannedParticipant};
use super::reference::Values;
use super::spec::Contents;
use super::step::HandleError;
use super::template::Template;
use super::wiring::PlannedAddress;

/// The mode of each of a participant's own directories. The case volume is root-owned, so a process that does not
/// run as root needs them writable to create sockets, exit records and log files there.
const OWN_DIR_MODE: u32 = 0o777;
/// The mode of a placed file that the plan gives none, as on Windows.
const DEFAULT_FILE_MODE: u32 = 0o644;
const COURIER_MODE: u32 = 0o755;
/// How long the backend waits for Docker to say whether a container is running.
const STATUS_LIMIT: Duration = Duration::from_secs(5);

/// What the container backend does differently on one container OS, beside the planning facts of its
/// enclosure.
pub(crate) struct ContainerOs {
    enclosure: Enclosure,
    /// The Docker network driver of the case network.
    network_driver: &'static str,
    /// Whether the case has a volume, mounted at the shared root in every container.
    volume: bool,
    /// The directory the archive of files is placed at.
    archive_root: &'static str,
    /// Kernel parameters set in every container.
    sysctls: &'static [(&'static str, &'static str)],
    /// The courier's target triple, without its architecture, and file name, as `make build-courier` builds it.
    courier_target: &'static str,
    courier_file: &'static str,
}

/// Linux containers.
// TODO: add the PID and cgroup namespace options with the first case that needs the host's.
pub(crate) const LINUX: ContainerOs = ContainerOs {
    enclosure: Enclosure::LinuxContainer,
    network_driver: "bridge",
    volume: true,
    archive_root: "/",
    sysctls: &[("net.ipv4.ip_local_port_range", LINUX_EPHEMERAL_PORT_RANGE)],
    courier_target: "unknown-linux-musl",
    courier_file: "courier",
};

impl ContainerOs {
    fn facts(&self) -> &'static Facts {
        self.enclosure.facts()
    }

    /// The courier binary for this OS, relative to the repository root.
    pub(crate) fn courier_binary(&self) -> PathBuf {
        PathBuf::from(format!(
            "target/{}-{}/release/{}",
            std::env::consts::ARCH,
            self.courier_target,
            self.courier_file
        ))
    }

    fn platform(&self) -> ScopePlatform {
        ScopePlatform {
            network_driver: self.network_driver,
            volume_path: self.facts().root.filter(|_| self.volume),
        }
    }

    /// `path` in the participant, as a path in the archive placed at [`Self::archive_root`].
    fn archive_path(&self, path: &str) -> Result<String, GenericError> {
        let relative = path
            .strip_prefix(self.archive_root)
            .ok_or_else(|| generic_error!("'{}' is not under '{}'.", path, self.archive_root))?;
        Ok(relative.replace(self.facts().separator, "/"))
    }
}

/// Creates the containers of cases on one container OS.
pub(crate) struct ContainerBackend {
    docker: Docker,
    os: &'static ContainerOs,
    /// The courier binary, copied into every container.
    courier: Bytes,
}

impl ContainerBackend {
    pub(crate) fn new(docker: Docker, os: &'static ContainerOs, courier: Bytes) -> Self {
        Self { docker, os, courier }
    }

    /// Prepares the scope of one case, entered in `ledger` before anything is made.
    ///
    /// # Errors
    ///
    /// Returns an error if Docker cannot make the scope, if `cancel` fires first, or if `deadline` passes first.
    pub(crate) async fn prepare(
        &self, ledger: &ResourceLedger, cancel: &CancellationToken, deadline: Instant,
    ) -> Result<CaseContainers, GenericError> {
        let scope = CaseScope::new(self.docker.clone(), random_id(), self.os.platform());
        ledger.case_scope(scope.clone());
        let name = scope.resource_name();
        ledger
            .create_by(&name, cancel, deadline, scope.prepare(deadline))
            .await
            .ok_or_else(|| generic_error!("Cancelled while preparing case scope '{}'.", name))??;
        Ok(CaseContainers {
            scope,
            os: self.os,
            courier: self.courier.clone(),
            ledger: ledger.clone(),
        })
    }
}

/// The containers of one case.
pub(crate) struct CaseContainers {
    scope: CaseScope,
    os: &'static ContainerOs,
    courier: Bytes,
    ledger: ResourceLedger,
}

impl CaseContainers {
    /// Creates the container of participant `name` from its plan, and places its files and the courier. Nothing runs
    /// yet.
    ///
    /// `values` fills every reference left in the plan, so each participant whose container ID `name` refers to must
    /// be created, and recorded in `values`, first.
    ///
    /// # Errors
    ///
    /// Returns an error if the plan holds something this backend cannot honor or a reference with no value yet, if
    /// Docker refuses the container or its files, for example because the image is missing, if `cancel` fires first, or
    /// if `deadline` passes first.
    pub(crate) async fn create(
        &self, name: &str, planned: &PlannedParticipant, values: &Values, cancel: &CancellationToken, deadline: Instant,
    ) -> Result<RealizedContainer, GenericError> {
        if planned.realization.enclosure != self.os.enclosure {
            return Err(generic_error!(
                "Participant '{}' is a {}, and this backend creates {}s.",
                name,
                planned.realization.enclosure,
                self.os.enclosure
            ));
        }
        let (spec, published) = self.spec(name, planned, values)?;
        // Filled before the container exists, so a reference with no value makes nothing.
        let archive = self.archive(name, planned, values)?;

        let scope = self.scope.resource_name();
        let cancelled = || {
            generic_error!(
                "Cancelled while creating participant '{}' in case scope '{}'.",
                name,
                scope
            )
        };
        let container = self
            .ledger
            .create_by(
                &scope,
                cancel,
                deadline,
                self.scope.create_container(name, spec, deadline),
            )
            .await
            .ok_or_else(cancelled)??;
        cancel
            .run_until_cancelled(container.place(self.os.archive_root, archive, deadline))
            .await
            .ok_or_else(cancelled)??;
        Ok(RealizedContainer {
            container,
            published,
            host_ports: BTreeMap::new(),
            ip: None,
        })
    }

    /// What the container of participant `name` runs, and the container port of each endpoint it publishes.
    fn spec(
        &self, name: &str, planned: &PlannedParticipant, values: &Values,
    ) -> Result<(ContainerSpec, BTreeMap<String, u16>), GenericError> {
        let Launch::Image { image, command, args } = &planned.launch else {
            return Err(generic_error!(
                "Participant '{}' runs host programs, and a container runs an image.",
                name
            ));
        };
        let fill_all = |templates: &[Template]| {
            templates
                .iter()
                .map(|template| fill(values, name, template))
                .collect::<Result<Vec<_>, _>>()
        };
        let mut spec = ContainerSpec::new(image);
        if let Some(command) = command {
            spec = spec.with_entrypoint(fill_all(command)?);
        }
        if let Some(args) = args {
            spec = spec.with_command(fill_all(args)?);
        }
        for (key, value) in &planned.env {
            spec = spec.with_env_var(key, fill(values, name, value)?);
        }
        let facts = self.os.facts();
        let (Some(exit_dir), Some(log_dir)) = (facts.exit_dir(name), facts.log_dir(name)) else {
            return Err(generic_error!("A {} has no directories of its own.", self.os.enclosure));
        };
        spec = spec
            .with_env_var(EXIT_DIR_ENV, exit_dir)
            .with_env_var(LOG_DIR_ENV, log_dir);
        for mount in &planned.mounts {
            spec = spec.with_bind(&mount.host, &mount.path, mount.read_only);
        }
        let mut published = BTreeMap::new();
        for (endpoint_name, endpoint) in planned.endpoints.iter().filter(|(_, endpoint)| endpoint.published) {
            let PlannedAddress::Port(port) = endpoint.address else {
                return Err(generic_error!(
                    "Participant '{}' publishes endpoint '{}', which has no port.",
                    name,
                    endpoint_name
                ));
            };
            spec = spec.with_published_port(port);
            published.insert(endpoint_name.clone(), port);
        }
        for (key, value) in self.os.sysctls {
            spec = spec.with_sysctl(*key, *value);
        }
        Ok((spec, published))
    }

    /// The archive placed in the container of participant `name`: its own directories, its files and the courier.
    fn archive(&self, name: &str, planned: &PlannedParticipant, values: &Values) -> Result<Archive, GenericError> {
        let facts = self.os.facts();
        let mut archive = Archive::new();
        for dir in [facts.dir(name), facts.exit_dir(name), facts.log_dir(name)] {
            let dir = dir.ok_or_else(|| generic_error!("A {} has no directories of its own.", self.os.enclosure))?;
            archive.add_dir(&self.os.archive_path(&dir)?, OWN_DIR_MODE)?;
        }
        for file in &planned.files {
            let contents: Cow<'_, [u8]> = match &file.contents {
                Contents::Text(text) => Cow::Borrowed(text.as_bytes()),
                Contents::Bytes(bytes) => Cow::Borrowed(bytes),
                Contents::Template(template) => Cow::Owned(fill(values, name, template)?.into_bytes()),
            };
            let mode = file.mode.unwrap_or(DEFAULT_FILE_MODE);
            archive.add_file(&self.os.archive_path(&file.destination)?, &contents, mode)?;
        }
        if let Some(courier) = facts.courier {
            archive.add_file(&self.os.archive_path(courier)?, &self.courier, COURIER_MODE)?;
        }
        Ok(archive)
    }
}

/// Fills every reference of `template`, held by participant `name`.
fn fill(values: &Values, name: &str, template: &Template) -> Result<String, GenericError> {
    values
        .fill(name, template)
        .map_err(|e| generic_error!("Participant '{}': {}", name, e))
}

/// A participant's container.
#[derive(Debug)]
pub(crate) struct RealizedContainer {
    pub(crate) container: Container,
    /// The container port of each published endpoint.
    published: BTreeMap<String, u16>,
    /// The host port of each published endpoint, once the container has started.
    host_ports: BTreeMap<String, u16>,
    /// The container's address on the case network, once it has started.
    pub(crate) ip: Option<String>,
}

impl RealizedContainer {
    /// Starts the container, then reads back its address and the host port of each published endpoint. A container
    /// that has already exited by then has neither.
    ///
    /// # Errors
    ///
    /// Returns an error if Docker cannot start the container, for example because its command does not exist, if
    /// Docker reports no host port for a published endpoint of a running container, if `cancel` fires first, or if
    /// `deadline` passes first.
    pub(crate) async fn start(&mut self, cancel: &CancellationToken, deadline: Instant) -> Result<(), GenericError> {
        let cancelled = || generic_error!("Cancelled while starting container '{}'.", self.container.name());
        cancel
            .run_until_cancelled(self.container.start(deadline))
            .await
            .ok_or_else(cancelled)??;
        let status = cancel
            .run_until_cancelled(self.container.status(deadline))
            .await
            .ok_or_else(cancelled)??;
        if !status.running {
            return Ok(());
        }
        for (endpoint, port) in &self.published {
            let host_port = status.published_ports.get(port).ok_or_else(|| {
                generic_error!(
                    "Docker reports no host port for endpoint '{}' (port {}) of container '{}'.",
                    endpoint,
                    port,
                    self.container.name()
                )
            })?;
            self.host_ports.insert(endpoint.clone(), *host_port);
        }
        self.ip = status.ip;
        Ok(())
    }

    /// Stops the container, killing it if it is still running after `grace`, then removes it.
    ///
    /// # Errors
    ///
    /// Returns an error if Docker fails to stop or remove the container, or if `deadline` passes first.
    pub(crate) async fn stop(&self, grace: Duration, deadline: Instant) -> Result<(), GenericError> {
        self.container.stop(grace, deadline).await?;
        self.container.remove(deadline).await
    }
}

#[async_trait]
impl Realized for RealizedContainer {
    async fn exec(
        &self, exec: Exec, stdout: &mut (dyn AsyncWrite + Send + Unpin), stderr: &mut (dyn AsyncWrite + Send + Unpin),
        deadline: Instant,
    ) -> Result<i64, HandleError> {
        let mut request = ExecRequest::new(exec.argv);
        for (key, value) in &exec.env {
            request = request.with_env_var(key, value);
        }
        if let Some(stdin) = exec.stdin {
            request = request.with_stdin(stdin);
        }
        let mut stdout = Watched::new(stdout);
        let mut stderr = Watched::new(stderr);
        let error = match self
            .container
            .exec_into(request, &mut stdout, &mut stderr, deadline)
            .await
        {
            Ok(code) => return Ok(code),
            Err(e) => format!("{:#}", e),
        };
        if stdout.failed || stderr.failed {
            return Err(HandleError::Harness(error));
        }
        // Docker runs no command in a stopped container, and a participant that exited is the software's fault.
        if self.has_exited().await {
            Err(HandleError::Software(format!(
                "container '{}' has exited: {}",
                self.container.name(),
                error
            )))
        } else {
            Err(HandleError::Harness(error))
        }
    }

    async fn has_exited(&self) -> bool {
        let deadline = Instant::now() + STATUS_LIMIT;
        matches!(self.container.status(deadline).await, Ok(status) if !status.running)
    }

    async fn read_file(&self, path: &str, deadline: Instant) -> Result<Option<Vec<u8>>, HandleError> {
        self.container
            .read_file(path, deadline)
            .await
            .map_err(|e| HandleError::Harness(format!("{:#}", e)))
    }

    fn host_port(&self, endpoint: &str) -> Option<u16> {
        self.host_ports.get(endpoint).copied()
    }
}

/// A writer of the runner's that remembers whether a write to it failed, so that failure is not blamed on the
/// participant.
struct Watched<'a> {
    inner: &'a mut (dyn AsyncWrite + Send + Unpin),
    failed: bool,
}

impl<'a> Watched<'a> {
    fn new(inner: &'a mut (dyn AsyncWrite + Send + Unpin)) -> Self {
        Self { inner, failed: false }
    }

    fn note<T>(&mut self, poll: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        if let Poll::Ready(Err(_)) = poll {
            self.failed = true;
        }
        poll
    }
}

impl AsyncWrite for Watched<'_> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut *self.inner).poll_write(cx, buf);
        self.note(poll)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut *self.inner).poll_flush(cx);
        self.note(poll)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut *self.inner).poll_shutdown(cx);
        self.note(poll)
    }
}
