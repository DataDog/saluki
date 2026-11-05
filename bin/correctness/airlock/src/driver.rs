use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    fmt,
    future::Future,
    hash::{Hash as _, Hasher as _},
    path::Path,
    time::Duration,
};

use bollard::{
    container::NetworkingConfig as ContainerNetworkingConfig,
    errors::Error,
    models::{ContainerCreateBody, EndpointSettings, HostConfig, HostConfigCgroupnsModeEnum, PortBinding, PortMap},
    query_parameters::CreateContainerOptionsBuilder,
    Docker,
};
use saluki_error::{generic_error, GenericError};
use tokio::time::sleep;
use tracing::{debug, warn};

use crate::{
    exec::{self, ExecOutput, ExecRequest, ExecStream},
    files::{self, ContainerFile, FileStat},
    group::SHARED_ROOT,
    observe::{self, ContainerExit, LogStream},
    removal,
};

// Test clients use loopback; binding there keeps published ports off other interfaces.
const PUBLISHED_HOST_IP: &str = "127.0.0.1";

/// Configuration for transient error retries.
///
/// Transient error retries are a class of retries that are applied to a select few operations that
/// have known error cases where retrying is safe and likely to fix the issue.
const TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS: u32 = 3;
const TRANSIENT_ERROR_RETRY_BASE_BACKOFF: Duration = Duration::from_millis(250);
const TRANSIENT_ERROR_RETRY_MAX_JITTER: Duration = Duration::from_millis(500);

/// Container operating system for a driver target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerOs {
    /// Linux container defaults.
    Linux,
    /// Windows container defaults.
    Windows,
}

/// Transport protocol for a container port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protocol {
    /// TCP transport.
    Tcp,
    /// UDP transport.
    Udp,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        })
    }
}

#[derive(Clone)]
struct ContainerPort {
    protocol: Protocol,
    internal_port: u16,
    host_port: u16,
}

/// Driver configuration.
///
/// This is the basic set of configuration options needed to spawn the container for a given driver.
#[derive(Clone)]
pub struct DriverConfig {
    pub(crate) driver_id: String,
    image: String,
    entrypoint: Option<Vec<String>>,
    command: Option<Vec<String>>,
    env: Vec<String>,
    binds: Vec<String>,
    ports: Vec<ContainerPort>,
    pub(crate) container_os: ContainerOs,
    host_cgroup_namespace: bool,
    host_pid_namespace: bool,
}

impl DriverConfig {
    /// Creates a new `DriverConfig` from the given driver identifier and container image reference.
    pub fn from_image(driver_id: impl Into<String>, image: String) -> Self {
        Self {
            driver_id: driver_id.into(),
            image,
            entrypoint: None,
            command: None,
            env: vec![],
            binds: vec![],
            ports: vec![],
            container_os: ContainerOs::Linux,
            host_cgroup_namespace: false,
            host_pid_namespace: false,
        }
    }

    /// Sets the entrypoint for the container.
    ///
    /// If `entrypoint` is empty, the default entrypoint will be used.
    pub fn with_entrypoint(mut self, entrypoint: Vec<String>) -> Self {
        if !entrypoint.is_empty() {
            self.entrypoint = Some(entrypoint);
        }
        self
    }

    /// Sets the command for the container.
    ///
    /// If `command` is empty, the default command will be used.
    pub fn with_command(mut self, command: Vec<String>) -> Self {
        if !command.is_empty() {
            self.command = Some(command);
        }
        self
    }

    /// Adds an environment variable to the container.
    pub fn with_env_var<K, V>(mut self, key: K, value: V) -> Self
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.env.push(format!("{}={}", key.as_ref(), value.as_ref()));
        self
    }

    /// Adds a bind mount to the container.
    ///
    /// `host_path` represents the path on the host to mount, while `container_path` represents the path on the
    /// container side to mount it to. Bind mounts can be either files or directories.
    pub fn with_bind_mount<HP, CP>(mut self, host_path: HP, container_path: CP) -> Self
    where
        HP: AsRef<Path>,
        CP: AsRef<Path>,
    {
        let bind_mount = format!("{}:{}", host_path.as_ref().display(), container_path.as_ref().display());
        self.binds.push(bind_mount);
        self
    }

    /// Adds a read-only bind mount to the container.
    ///
    /// Same as [`with_bind_mount`][Self::with_bind_mount] but the container can't modify the mounted path.
    pub fn with_readonly_bind_mount<HP, CP>(mut self, host_path: HP, container_path: CP) -> Self
    where
        HP: AsRef<Path>,
        CP: AsRef<Path>,
    {
        let bind_mount = format!(
            "{}:{}:ro",
            host_path.as_ref().display(),
            container_path.as_ref().display()
        );
        self.binds.push(bind_mount);
        self
    }

    /// Publishes a container port on `127.0.0.1:<host_port>` of the Docker host.
    ///
    /// The caller chooses a free host port. A port already held fails [`Driver::start_container`], not
    /// [`Driver::create`]. Windows containers reject published ports at create.
    pub fn with_published_port(mut self, protocol: Protocol, internal_port: u16, host_port: u16) -> Self {
        self.ports.push(ContainerPort {
            protocol,
            internal_port,
            host_port,
        });
        self
    }

    /// Sets the operating system this container will run as.
    ///
    /// Must match the isolation group's OS. Selects mount, port publication, and container path conventions.
    pub fn with_container_os(mut self, container_os: ContainerOs) -> Self {
        self.container_os = container_os;
        self
    }

    /// Configures whether a Linux target joins the Docker host's cgroup namespace.
    pub fn with_host_cgroup_namespace(mut self, host_cgroup_namespace: bool) -> Self {
        self.host_cgroup_namespace = host_cgroup_namespace;
        self
    }

    /// Sets whether a Linux container shares the Docker host's PID namespace; defaults to `false`.
    /// Windows containers ignore it.
    pub fn with_host_pid_namespace(mut self, host_pid_namespace: bool) -> Self {
        self.host_pid_namespace = host_pid_namespace;
        self
    }

    fn port_publication(&self) -> Result<(Option<Vec<String>>, Option<PortMap>), GenericError> {
        if self.ports.is_empty() {
            return Ok((None, None));
        }

        let mut exposed_ports = Vec::new();
        let mut port_bindings = match self.container_os {
            ContainerOs::Linux => Some(PortMap::new()),
            ContainerOs::Windows => None,
        };
        for port in &self.ports {
            let key = format!("{}/{}", port.internal_port, port.protocol);
            if !exposed_ports.contains(&key) {
                exposed_ports.push(key.clone());
            }
            if let Some(port_bindings) = &mut port_bindings {
                let binding = PortBinding {
                    host_ip: Some(PUBLISHED_HOST_IP.to_string()),
                    host_port: Some(port.host_port.to_string()),
                };
                port_bindings
                    .entry(key)
                    .or_default()
                    .get_or_insert_with(Vec::new)
                    .push(binding);
            } else {
                return Err(generic_error!(
                    "Participant '{}' publishes {} to host port {}, but Windows containers don't publish ports",
                    self.driver_id,
                    key,
                    port.host_port
                ));
            }
        }

        Ok((Some(exposed_ports), port_bindings))
    }

    fn container_binds(&self, isolation_group_name: &str) -> Vec<String> {
        let mut binds = self.binds.clone();
        if self.container_os == ContainerOs::Linux {
            binds.push(format!("{isolation_group_name}:{SHARED_ROOT}:z"));
        }
        binds
    }
}

/// Detailed information about the spawned container.
#[derive(Debug)]
pub struct DriverDetails {
    container_ip: Option<String>,
}

impl DriverDetails {
    /// Returns the container IP address on its primary Docker network, if known.
    pub fn container_ip(&self) -> Option<&str> {
        self.container_ip.as_deref()
    }
}

/// A created container that can be stopped and removed without its driver.
#[derive(Clone)]
pub struct ContainerRef {
    docker: Docker,
    name: String,
}

impl fmt::Debug for ContainerRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContainerRef").field("name", &self.name).finish()
    }
}

impl ContainerRef {
    /// Stops the container with a grace period, then force-removes it and its anonymous volumes.
    ///
    /// Docker sends SIGKILL after `grace`, cut to whole seconds that end at least two seconds before `deadline`.
    /// A stop still pending two seconds before `deadline` is abandoned, and the container is force-removed.
    /// Run this future under `tokio::time::timeout_at(deadline, ...)` to bound the entire operation.
    ///
    /// This future is cancel-safe: once Docker receives the stop, dropping it leaves the container stopping or
    /// removed. A later [`IsolationGroup::remove`](crate::group::IsolationGroup::remove) force-removes any remaining container.
    ///
    /// # Errors
    ///
    /// Returns an error if removal fails. A missing container counts as removed; stop failures still lead to removal.
    pub async fn stop_and_remove(&self, grace: Duration, deadline: tokio::time::Instant) -> Result<(), GenericError> {
        removal::stop_and_remove(&self.docker, &self.name, grace, deadline).await
    }
}

/// Container driver.
pub struct Driver {
    isolation_group_id: String,
    isolation_group_name: String,
    container_name: String,
    config: DriverConfig,
    docker: Docker,
}

impl Driver {
    /// Builds a driver that uses a prepared group's resources and a local image.
    pub(crate) fn in_group(docker: Docker, isolation_group_id: String, config: DriverConfig) -> Self {
        let isolation_group_name = group_resource_name(&isolation_group_id);
        Self {
            container_name: format!("{}-{}", isolation_group_name, config.driver_id),
            isolation_group_name,
            isolation_group_id,
            config,
            docker,
        }
    }

    /// Returns a reference to this driver's container, by name, that outlives the driver.
    ///
    /// It can be taken before or after [`Self::create`].
    pub fn container_ref(&self) -> ContainerRef {
        ContainerRef {
            docker: self.docker.clone(),
            name: self.container_name.clone(),
        }
    }

    /// Creates the container using its group's prepared resources and a local image.
    ///
    /// Returns the full container ID from Docker. Call [`Self::start_container`] to start it.
    /// Linux containers mount the group volume; Windows containers use explicitly configured mounts.
    ///
    /// # Errors
    ///
    /// Returns an error if the image is missing, port publication is invalid, or Docker cannot create the container.
    pub async fn create(&self) -> Result<String, GenericError> {
        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Creating container '{}'...",
            self.container_name
        );
        let (exposed_ports, port_bindings) = self.config.port_publication()?;
        let pid_mode = (self.config.container_os == ContainerOs::Linux && self.config.host_pid_namespace)
            .then(|| "host".to_string());
        let cgroupns_mode = (self.config.container_os == ContainerOs::Linux && self.config.host_cgroup_namespace)
            .then_some(HostConfigCgroupnsModeEnum::HOST);
        let container_config = ContainerCreateBody {
            hostname: Some(self.config.driver_id.clone()),
            env: Some(self.config.env.clone()),
            image: Some(self.config.image.clone()),
            entrypoint: self.config.entrypoint.clone(),
            cmd: self.config.command.clone(),
            host_config: Some(HostConfig {
                binds: Some(self.config.container_binds(&self.isolation_group_name)),
                network_mode: Some(self.isolation_group_name.clone()),
                port_bindings,
                pid_mode,
                cgroupns_mode,
                ..Default::default()
            }),
            exposed_ports,
            labels: Some(get_default_airlock_labels(&self.isolation_group_id)),
            networking_config: Some(
                ContainerNetworkingConfig {
                    endpoints_config: HashMap::from([(
                        self.isolation_group_name.clone(),
                        EndpointSettings {
                            aliases: Some(vec![self.config.driver_id.clone()]),
                            ..Default::default()
                        },
                    )]),
                }
                .into(),
            ),
            ..Default::default()
        };
        let options = CreateContainerOptionsBuilder::default()
            .name(&self.container_name)
            .build();
        let response = self.docker.create_container(Some(options), container_config).await?;
        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Created container '{}' (ID: {}).",
            self.container_name,
            response.id
        );
        Ok(response.id)
    }

    /// Starts the container that [`Self::create`] created.
    ///
    /// # Errors
    ///
    /// Returns an error if starting or inspecting the container fails.
    pub async fn start_container(&self) -> Result<DriverDetails, GenericError> {
        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Starting container '{}'...",
            self.container_name
        );
        // Starting an already-running container is a no-op, so retrying an HNS collision is safe.
        with_transient_error_retry("container start", &self.isolation_group_id, || async {
            self.docker
                .start_container(&self.container_name, None)
                .await
                .map_err(GenericError::from)
        })
        .await?;
        let response = self.docker.inspect_container(&self.container_name, None).await?;
        let container_ip = response
            .network_settings
            .and_then(|settings| settings.networks)
            .and_then(|networks| {
                networks
                    .get(&self.isolation_group_name)
                    .and_then(|settings| settings.ip_address.clone())
            })
            .filter(|address| !address.is_empty());
        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Started container '{}'.",
            self.container_name
        );
        Ok(DriverDetails { container_ip })
    }

    /// Returns stdout and stderr from the container's start, followed until it stops.
    ///
    /// The container must have been started. Output replays even after exit. The stream also ends when the
    /// container is removed; use [`Self::wait_exit`] to observe its exit separately. The stream owns its read
    /// and sends the logs request when first polled. Dropping it cancels only the read, not the container.
    ///
    /// # Errors
    ///
    /// Returns an error if the container has not started, has no state or status, is missing, or inspection fails.
    /// The stream yields one error and ends if the container is missing, reading logs fails, or Docker sends a
    /// stdin or console frame. Errors name the container and omit log bytes.
    pub async fn logs(&self) -> Result<LogStream, GenericError> {
        observe::logs(&self.docker, &self.container_name).await
    }

    /// Waits for the container's exit code and Docker error message, including nonzero codes as data.
    ///
    /// The container must have been started. Waiting after exit reports the same exit again. The future owns
    /// its read and sends no request until polled. Dropping it cancels only the read, not the container.
    ///
    /// # Errors
    ///
    /// Returns an error if the container has not started, has no state or status, is missing, inspection or
    /// waiting fails, or the wait ends without an exit status. A Docker exit error message is data, even for code 0.
    pub fn wait_exit(&self) -> impl Future<Output = Result<ContainerExit, GenericError>> + Send + 'static {
        observe::wait(&self.docker, &self.container_name)
    }

    /// Places files and directories in a container between create and start.
    ///
    /// Linux files are owned by root and keep their mode. Missing parent directories are created with mode `0755`,
    /// owned by root. Existing files are replaced, but a file cannot replace a directory. Paths resolve inside
    /// the container, including through symlinks. Windows containers take `C:\`-rooted paths with backslash
    /// separators, ignore modes, and create missing parent directories. An empty slice sends no request.
    ///
    /// # Errors
    ///
    /// Returns an error for paths invalid for the container's OS (see [`ContainerFile`]), duplicate paths, modes
    /// beyond `07777`, archive construction failures, or Docker API failures. Every file is validated before
    /// contacting Docker.
    pub async fn place_files(&self, files: &[ContainerFile]) -> Result<(), GenericError> {
        files::place(&self.docker, self.config.container_os, &self.container_name, files).await
    }

    /// Reads a regular file from a created, running, or exited container, including its mounted volumes.
    ///
    /// Returns `None` if the path is missing and the container exists. Paths follow [`ContainerFile`]'s rules for
    /// the container's OS. The entire file is read into memory without a size cap. Callers can impose their own
    /// timeout.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid paths, a missing container, Docker API failures, empty or malformed archives,
    /// or a path naming a directory, symbolic link, or other non-regular file.
    pub async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, GenericError> {
        files::read(&self.docker, self.config.container_os, &self.container_name, path).await
    }

    /// Reports a path's status in a created, running, or exited container, including mounted volumes.
    ///
    /// Reports a final symlink itself. Returns `None` for a missing path in an existing container.
    /// Paths follow [`ContainerFile`]'s rules for the container's OS. Paths on a Linux container's own `tmpfs` or
    /// `proc` mounts, such as `/dev` and `/proc`, can appear missing.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid paths, a missing container, a path through a non-directory, Docker API
    /// failures, negative file sizes, or header-decoding failures.
    pub async fn stat(&self, path: &str) -> Result<Option<FileStat>, GenericError> {
        files::stat(&self.docker, self.config.container_os, &self.container_name, path).await
    }

    /// Executes a command and collects stdout and stderr bytes with its exit code.
    ///
    /// Every exit code is data, including a missing program (126 or 127, depending on the Docker
    /// release) or a killed command (137). See
    /// [`Self::exec_stream`] for command settings, stdin, completion, cancellation, and timeout behavior.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty command, an empty environment key or one containing `=`, Docker API failures
    /// (including a container that isn't running), output read or framing errors, or an exit inspection that reports
    /// a running command or omits its state or code. A command closing stdin early is handled through its output
    /// and exit code.
    pub async fn exec(&self, request: ExecRequest) -> Result<ExecOutput, GenericError> {
        self.exec_stream(request).await?.collect_output().await
    }

    /// Executes a command and returns stdout and stderr frames as they arrive.
    ///
    /// The command uses the container's default user and working directory, without a TTY. Request environment
    /// variables extend the container's environment. Polling the stream writes stdin and requests a write-side
    /// shutdown after the supplied bytes; see [`ExecRequest::with_stdin`] for transport limits.
    /// Use [`Self::exec`] to collect output with its exit code.
    ///
    /// The stream ends when output closes; a background child holding stdout or stderr open keeps it open. Dropping
    /// this future or the stream stops reading and writing, but the command runs until it exits or the container
    /// stops. Callers can impose their own timeout.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty command, an empty environment key or one containing `=`, or Docker creation or
    /// start failures (including a container that isn't running). The stream yields output read or framing errors.
    /// A command closing stdin early is handled through its output.
    pub async fn exec_stream(&self, request: ExecRequest) -> Result<ExecStream, GenericError> {
        exec::start(&self.docker, &self.container_name, request).await
    }
}

/// Returns whether Docker reported a missing resource.
pub(crate) fn is_not_found(error: &Error) -> bool {
    matches!(error, Error::DockerResponseServerError { status_code: 404, .. })
}

/// Returns `true` if `error` is a transient error that can be retried.
fn is_transient_error(error: &GenericError) -> bool {
    is_transient_hns_error(error)
}

/// Returns `true` if `error` is a transient Windows HNS collision.
///
/// Windows containers are networked through the Host Network Service, and HNS serializes poorly: when several
/// containers create a NAT network or attach an endpoint at the same moment, the losing caller gets a 500 from the
/// daemon carrying `hnsCall failed in Win32: The object already exists. (0x1392)`. The object in question is internal
/// to HNS -- airlock names every network after a unique isolation group, so this is never a name conflict of ours --
/// and the conflict clears once the winning call completes.
///
/// The predicate matches on the message rather than on [`ContainerOs`] because `hnsCall` has no analogue on Linux,
/// which makes it inert there. It deliberately requires the `0x1392` code as well as `hnsCall`: other HNS failures
/// indicate real breakage and should fail fast rather than be papered over by a retry.
fn is_transient_hns_error(error: &GenericError) -> bool {
    error.downcast_ref::<Error>().is_some_and(|error| match error {
        Error::DockerResponseServerError { message, .. } => message.contains("hnsCall") && message.contains("0x1392"),
        _ => false,
    })
}

/// Returns how long to wait before the given transient error retry attempt.
///
/// `seed` keeps concurrent callers out of step with each other. Isolation group IDs are unique per
/// test, so two drivers that collided at the same instant draw different jitter and don't line
/// back up on the retry.
fn transient_error_retry_backoff(attempt: u32, seed: &str) -> Duration {
    let mut hasher = DefaultHasher::new();
    seed.hash(&mut hasher);
    attempt.hash(&mut hasher);

    let jitter_ms = hasher.finish() % (TRANSIENT_ERROR_RETRY_MAX_JITTER.as_millis() as u64 + 1);

    TRANSIENT_ERROR_RETRY_BASE_BACKOFF * attempt + Duration::from_millis(jitter_ms)
}

/// Runs `run_attempt`, retrying it when it fails with a known transient error.
///
/// Any other error, and the error from the final attempt, is returned to the caller untouched. "Known" transient errors
/// are hard-coded/categorized by hand, so this function mostly represents known areas where we've seen transient issues
/// that can generally be solved by simply retrying.
///
/// `run_attempt` must be safe to re-run: every call site either re-checks for the resource it is creating, or targets a
/// Docker endpoint that is idempotent.
///
/// Each retry is logged at warning level so that runs which only passed because of a retry stay visible in CI output,
/// rather than the mitigation quietly hiding how often the collision fires.
pub(crate) async fn with_transient_error_retry<F, Fut, T>(
    operation: &str, seed: &str, mut run_attempt: F,
) -> Result<T, GenericError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, GenericError>>,
{
    let mut attempt = 1;
    loop {
        match run_attempt().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                if attempt >= TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS || !is_transient_error(&e) {
                    return Err(e);
                }

                let backoff = transient_error_retry_backoff(attempt, seed);
                warn!(
                    isolation_group = seed,
                    error = %e,
                    "Transient error during {}. Retrying in {:?} (attempt {} of {}).",
                    operation,
                    backoff,
                    attempt + 1,
                    TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS
                );

                sleep(backoff).await;
                attempt += 1;
            }
        }
    }
}

pub(crate) fn group_resource_name(id: &str) -> String {
    format!("airlock-{id}")
}

pub(crate) fn get_default_airlock_labels(isolation_group_id: &str) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    labels.insert("created_by".to_string(), "airlock".to_string());
    labels.insert("airlock-isolation-group".to_string(), isolation_group_id.to_string());
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_daemon::FakeDaemon;

    const GROUP: &str = "ab12cd34";
    const BOUND: Duration = Duration::from_secs(5);

    fn docker_server_error(status_code: u16, message: &str) -> GenericError {
        Error::DockerResponseServerError {
            status_code,
            message: message.to_string(),
        }
        .into()
    }

    #[test]
    fn hns_object_already_exists_is_treated_as_transient() {
        // Verbatim message from a failed `test-integration-windows-amd64` run.
        let error = docker_server_error(
            500,
            "failed during hnsCallRawResponse: hnsCall failed in Win32: The object already exists. (0x1392)",
        );

        assert!(is_transient_hns_error(&error));
    }

    #[test]
    fn transient_hns_error_is_still_detected_through_added_context() {
        let error = docker_server_error(
            500,
            "failed during hnsCallRawResponse: hnsCall failed in Win32: The object already exists. (0x1392)",
        )
        .context("Failed to start container 'airlock-q9183x1a-target'");

        assert!(is_transient_hns_error(&error));
    }

    #[test]
    fn other_failures_are_not_treated_as_transient_hns_errors() {
        // A different HNS failure mode: real breakage, not a collision worth papering over.
        assert!(!is_transient_hns_error(&docker_server_error(
            500,
            "failed during hnsCallRawResponse: hnsCall failed in Win32: The system cannot find the file specified. (0x2)",
        )));

        // A name conflict of ours, which a retry would never clear.
        assert!(!is_transient_hns_error(&docker_server_error(
            409,
            "Conflict. The container name \"/airlock-q9183x1a-target\" is already in use",
        )));

        assert!(!is_transient_hns_error(&generic_error!("something else went wrong")));
    }

    #[test]
    fn transient_error_retry_backoff_grows_per_attempt_and_stays_within_bounds() {
        for attempt in 1..TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS {
            let backoff = transient_error_retry_backoff(attempt, "q9183x1a");

            assert!(backoff >= TRANSIENT_ERROR_RETRY_BASE_BACKOFF * attempt);
            assert!(backoff <= TRANSIENT_ERROR_RETRY_BASE_BACKOFF * attempt + TRANSIENT_ERROR_RETRY_MAX_JITTER);
        }
    }

    #[test]
    fn transient_error_retry_backoff_differs_between_isolation_groups() {
        // Racers that collided at the same instant must not retry on the same schedule, or they
        // just collide again. Isolation group IDs are taken from real failing runs.
        assert_ne!(
            transient_error_retry_backoff(1, "q9183x1a"),
            transient_error_retry_backoff(1, "fajekrvm")
        );
    }

    #[tokio::test]
    async fn with_transient_error_retry_retries_transient_failures_until_one_succeeds() {
        let attempts = std::cell::Cell::new(0);

        let result = with_transient_error_retry("test operation", "q9183x1a", || async {
            attempts.set(attempts.get() + 1);
            if attempts.get() < TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS {
                Err(docker_server_error(
                    500,
                    "failed during hnsCallRawResponse: hnsCall failed in Win32: The object already exists. (0x1392)",
                ))
            } else {
                Ok(attempts.get())
            }
        })
        .await;

        assert_eq!(result.unwrap(), TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS);
        assert_eq!(attempts.get(), TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn with_transient_error_retryy_gives_up_after_the_attempt_budget() {
        let attempts = std::cell::Cell::new(0);

        let result: Result<(), GenericError> = with_transient_error_retry("test operation", "q9183x1a", || async {
            attempts.set(attempts.get() + 1);
            Err(docker_server_error(
                500,
                "failed during hnsCallRawResponse: hnsCall failed in Win32: The object already exists. (0x1392)",
            ))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.get(), TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn with_transient_error_retry_does_not_retry_unrelated_failures() {
        let attempts = std::cell::Cell::new(0);

        let result: Result<(), GenericError> = with_transient_error_retry("test operation", "q9183x1a", || async {
            attempts.set(attempts.get() + 1);
            Err(generic_error!("something else went wrong"))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.get(), 1);
    }

    #[test]
    fn prepared_linux_container_binds_include_only_airlock_and_configured_mounts() {
        let config = DriverConfig::from_image("target", "example:latest".to_string());
        assert_eq!(config.container_binds("airlock-test"), ["airlock-test:/airlock:z"]);

        let config = config.with_readonly_bind_mount("/host/config", "/etc/config");
        assert_eq!(
            config.container_binds("airlock-test"),
            ["/host/config:/etc/config:ro", "airlock-test:/airlock:z"]
        );
    }

    #[test]
    fn prepared_windows_container_binds_include_only_configured_mounts() {
        let config =
            DriverConfig::from_image("target", "example:latest".to_string()).with_container_os(ContainerOs::Windows);
        assert!(config.container_binds("airlock-test").is_empty());

        let config = config.with_readonly_bind_mount("C:\\host", "C:\\guest");
        assert_eq!(config.container_binds("airlock-test"), ["C:\\host:C:\\guest:ro"]);
    }

    #[test]
    fn a_container_without_ports_exposes_and_publishes_nothing() {
        for os in [ContainerOs::Linux, ContainerOs::Windows] {
            let config = DriverConfig::from_image("target", "example:latest".to_string()).with_container_os(os);

            assert_eq!(config.port_publication().unwrap(), (None, None));
        }
    }

    #[test]
    fn a_linux_published_port_is_bound_to_its_host_port_on_loopback() {
        let config = DriverConfig::from_image("target", "example:latest".to_string()).with_published_port(
            Protocol::Udp,
            8125,
            24100,
        );

        let (exposed_ports, port_bindings) = config.port_publication().unwrap();

        assert_eq!(exposed_ports, Some(vec!["8125/udp".to_string()]));
        assert_eq!(
            port_bindings,
            Some(HashMap::from([(
                "8125/udp".to_string(),
                Some(vec![PortBinding {
                    host_ip: Some("127.0.0.1".to_string()),
                    host_port: Some("24100".to_string())
                }]),
            )]))
        );
    }

    #[test]
    fn two_host_ports_for_one_container_port_are_both_bound_on_loopback() {
        let config = DriverConfig::from_image("server", "example:latest".into())
            .with_published_port(Protocol::Tcp, 9000, 20000)
            .with_published_port(Protocol::Tcp, 9000, 20001)
            .with_published_port(Protocol::Udp, 9001, 20002);
        assert_eq!(
            config.port_publication().unwrap(),
            (
                Some(vec!["9000/tcp".into(), "9001/udp".into()]),
                Some(HashMap::from([
                    (
                        "9000/tcp".into(),
                        Some(vec![
                            PortBinding {
                                host_ip: Some("127.0.0.1".into()),
                                host_port: Some("20000".into())
                            },
                            PortBinding {
                                host_ip: Some("127.0.0.1".into()),
                                host_port: Some("20001".into())
                            },
                        ])
                    ),
                    (
                        "9001/udp".into(),
                        Some(vec![PortBinding {
                            host_ip: Some("127.0.0.1".into()),
                            host_port: Some("20002".into())
                        },])
                    ),
                ])),
            )
        );
    }

    #[test]
    fn a_windows_container_rejects_a_published_port() {
        let config = DriverConfig::from_image("target", "example:latest".to_string())
            .with_container_os(ContainerOs::Windows)
            .with_published_port(Protocol::Tcp, 9000, 24100);

        let error = config.port_publication().unwrap_err().to_string();
        assert_eq!(
            error,
            "Participant 'target' publishes 9000/tcp to host port 24100, but Windows containers don't publish ports"
        );
    }

    #[test]
    fn container_ports_are_exposed_once_in_first_seen_order() {
        let config = DriverConfig::from_image("target", "example:latest".to_string())
            .with_published_port(Protocol::Udp, 9001, 24001)
            .with_published_port(Protocol::Tcp, 9000, 24000)
            .with_published_port(Protocol::Udp, 9001, 24001);

        let (exposed_ports, _) = config.port_publication().unwrap();

        assert_eq!(
            exposed_ports,
            Some(vec!["9001/udp".to_string(), "9000/tcp".to_string()])
        );
    }

    #[tokio::test]
    async fn creating_a_container_does_not_start_it() {
        // Callers treat only a cancelled `create` as able to leave resources behind, so creating must not start the
        // container and starting must create nothing.
        let daemon = FakeDaemon::start(
            |method, path| match (method, path.split('?').next().unwrap_or_default()) {
                ("POST", "/containers/create") => (201, r#"{"Id": "c1", "Warnings": []}"#),
                ("POST", "/containers/airlock-ab12cd34-target/start") => (204, ""),
                ("GET", "/containers/airlock-ab12cd34-target/json") => (200, "{}"),
                _ => (500, r#"{"message": "unexpected request"}"#),
            },
        )
        .await;
        let driver = Driver::in_group(
            daemon.docker.clone(),
            GROUP.to_string(),
            DriverConfig::from_image("target", "example:latest".to_string()),
        );
        let paths = || -> Vec<String> {
            let requests = daemon.requests.lock().unwrap();
            requests
                .iter()
                .map(|request| request.split('?').next().unwrap_or_default().to_string())
                .collect()
        };

        tokio::time::timeout(BOUND, driver.create())
            .await
            .expect("creation should finish within the bound")
            .expect("creation should succeed");

        let created = paths();
        assert_eq!(created, ["POST /containers/create"], "{created:?}");
        assert!(!created.iter().any(|path| path.ends_with("/start")), "{created:?}");

        let details = tokio::time::timeout(BOUND, driver.start_container())
            .await
            .expect("starting should finish within the bound")
            .expect("starting should succeed");

        assert_eq!(details.container_ip(), None);
        assert_eq!(
            paths()[created.len()..],
            [
                "POST /containers/airlock-ab12cd34-target/start",
                "GET /containers/airlock-ab12cd34-target/json",
            ]
        );
    }

    #[tokio::test]
    async fn a_windows_driver_places_reads_and_stats_files_through_the_archive_api() {
        let daemon = FakeDaemon::start(
            |method, path| match (method, path.split('?').next().unwrap_or_default()) {
                ("PUT", "/containers/airlock-ab12cd34-subject/archive") => (200, ""),
                ("GET" | "HEAD", "/containers/airlock-ab12cd34-subject/archive") => (404, r#"{"message":"missing"}"#),
                ("GET", "/containers/airlock-ab12cd34-subject/json") => (200, "{}"),
                _ => (500, r#"{"message":"unexpected request"}"#),
            },
        )
        .await;
        let driver = Driver::in_group(
            daemon.docker.clone(),
            GROUP.to_string(),
            DriverConfig::from_image("subject", "example:latest".to_string()).with_container_os(ContainerOs::Windows),
        );
        driver.place_files(&[]).await.unwrap();
        driver
            .place_files(&[
                ContainerFile::directory("C:\\airlock\\subject\\exit"),
                ContainerFile::new("C:\\panoramic\\courier.exe", "MZ").with_mode(0o755),
            ])
            .await
            .unwrap();
        assert_eq!(driver.read_file("C:\\airlock\\subject\\exit\\adp").await.unwrap(), None);
        assert_eq!(driver.stat("C:\\airlock\\subject\\exit").await.unwrap(), None);
        let error = driver
            .place_files(&[ContainerFile::new("/file", "secret")])
            .await
            .unwrap_err();
        assert!(!format!("{error:?}").contains("secret"));
        assert!(driver.read_file("/file").await.is_err());
        assert!(driver.stat("/file").await.is_err());

        let requests = daemon.requests.lock().unwrap();
        assert_eq!(requests.len(), 5, "{requests:?}");
        assert!(requests[0].starts_with("PUT /containers/airlock-ab12cd34-subject/archive?"));
        assert!(requests[0].contains("path=C%3A%5C"), "{requests:?}");
        assert!(
            requests[1].contains("path=C%3A%5Cairlock%5Csubject%5Cexit%5Cadp"),
            "{requests:?}"
        );
        assert!(requests[3].starts_with("HEAD "), "{requests:?}");
        assert!(
            requests[3].contains("path=C%3A%5Cairlock%5Csubject%5Cexit"),
            "{requests:?}"
        );
    }

    #[tokio::test]
    async fn a_prepared_windows_driver_mounts_nothing_and_publishes_nothing() {
        let daemon = FakeDaemon::start(
            |method, path| match (method, path.split('?').next().unwrap_or_default()) {
                ("POST", "/containers/create") => (201, r#"{"Id":"c1","Warnings":[]}"#),
                _ => (500, r#"{"message":"unexpected request"}"#),
            },
        )
        .await;
        let config = DriverConfig::from_image("subject", "example:latest".to_string())
            .with_container_os(ContainerOs::Windows)
            .with_host_pid_namespace(true)
            .with_host_cgroup_namespace(true);
        let driver = Driver::in_group(daemon.docker.clone(), GROUP.to_string(), config.clone());

        driver.create().await.unwrap();

        {
            let bodies = daemon.bodies.lock().unwrap();
            assert_eq!(bodies.len(), 1);
            let body = &bodies[0];
            assert!(body.contains(r#""NetworkMode":"airlock-ab12cd34""#), "{body}");
            assert!(body.contains(r#""Aliases":["subject"]"#), "{body}");
            assert!(body.contains(r#""Binds":[]"#), "{body}");
            for absent in ["PortBindings", "ExposedPorts", "PidMode", "CgroupnsMode", "Mounts"] {
                assert!(!body.contains(absent), "{absent}: {body}");
            }
        }

        let driver = Driver::in_group(
            daemon.docker.clone(),
            GROUP.to_string(),
            config.with_published_port(Protocol::Tcp, 9000, 24100),
        );
        let error = driver.create().await.unwrap_err().to_string();
        assert!(error.contains("Windows containers don't publish ports"), "{error}");
        assert_eq!(daemon.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_prepared_driver_shares_the_host_pid_namespace_only_when_asked() {
        let daemon = FakeDaemon::start(
            |method, path| match (method, path.split('?').next().unwrap_or_default()) {
                ("POST", "/containers/create") => (201, r#"{"Id":"c1","Warnings":[]}"#),
                _ => (500, r#"{"message":"unexpected request"}"#),
            },
        )
        .await;
        for (os, host_pid_namespace, expected) in [
            (ContainerOs::Linux, true, true),
            (ContainerOs::Linux, false, false),
            (ContainerOs::Windows, true, false),
        ] {
            let driver = Driver::in_group(
                daemon.docker.clone(),
                GROUP.to_string(),
                DriverConfig::from_image("alpha", "local:latest".to_string())
                    .with_container_os(os)
                    .with_host_pid_namespace(host_pid_namespace),
            );
            driver.create().await.unwrap();
            let bodies = daemon.bodies.lock().unwrap();
            let body = bodies.last().unwrap();
            assert_eq!(body.contains(r#""PidMode":"host""#), expected, "{body}");
            if !expected {
                assert!(!body.contains("\"PidMode\""), "{body}");
            }
        }
        let requests = daemon.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests
            .iter()
            .all(|request| request.starts_with("POST /containers/create?")));
    }

    #[tokio::test]
    async fn a_missing_image_fails_creation_without_pulling_or_starting() {
        let daemon = FakeDaemon::start(|method, path| match (method, path.split('?').next().unwrap()) {
            ("POST", "/containers/create") => (404, r#"{"message":"No such image: missing:latest"}"#),
            _ => (500, r#"{"message":"unexpected request"}"#),
        })
        .await;
        for os in [ContainerOs::Linux, ContainerOs::Windows] {
            let driver = Driver::in_group(
                daemon.docker.clone(),
                GROUP.to_string(),
                DriverConfig::from_image("subject", "missing:latest".to_string()).with_container_os(os),
            );
            let error = driver.create().await.unwrap_err().to_string();
            assert!(error.contains("No such image: missing:latest"), "{error}");
        }
        let requests = daemon.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "{requests:?}");
        for request in requests.iter() {
            assert_eq!(request.split('?').next(), Some("POST /containers/create"));
        }
    }

    #[tokio::test]
    async fn a_driver_in_a_prepared_group_creates_only_its_container_without_pulling() {
        let daemon = FakeDaemon::start(
            |method, path| match (method, path.split('?').next().unwrap_or_default()) {
                ("POST", "/containers/create") => (201, r#"{"Id": "c1", "Warnings": []}"#),
                _ => (500, r#"{"message": "unexpected request"}"#),
            },
        )
        .await;
        let driver = Driver::in_group(
            daemon.docker.clone(),
            GROUP.to_string(),
            DriverConfig::from_image("alpha", "local:latest".to_string()),
        );

        driver.create().await.expect("prepared creation should succeed");

        let requests = daemon.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].split('?').next(), Some("POST /containers/create"));
        assert!(requests[0].contains("name=airlock-ab12cd34-alpha"), "{requests:?}");
        let bodies = daemon.bodies.lock().unwrap();
        let body = &bodies[0];
        assert!(body.contains(r#""Binds":["airlock-ab12cd34:/airlock:z"]"#), "{body}");
        assert!(!body.contains("\"PidMode\""), "{body}");
    }
}
