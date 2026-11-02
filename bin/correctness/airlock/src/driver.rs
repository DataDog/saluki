use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    fmt,
    future::Future,
    hash::{Hash as _, Hasher as _},
    path::{Path, PathBuf},
    time::Duration,
};

use bollard::{
    container::{LogOutput, NetworkingConfig as ContainerNetworkingConfig},
    errors::Error,
    exec::{CreateExecOptions, StartExecResults},
    models::{
        ContainerCreateBody, ContainerStateStatusEnum, EndpointSettings, HealthConfig, HealthStatusEnum, HostConfig,
        HostConfigCgroupnsModeEnum, Ipam, NetworkConnectRequest, NetworkCreateRequest, PortBinding, PortMap,
        VolumeCreateRequest,
    },
    query_parameters::{CreateContainerOptionsBuilder, CreateImageOptions, LogsOptions},
    Docker,
};
use futures::{StreamExt as _, TryStreamExt as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{
    io::{AsyncWriteExt as _, BufWriter},
    time::sleep,
};
use tracing::{debug, error, trace, warn};

use crate::{
    config::{DatadogIntakeConfig, MillstoneConfig, TargetConfig},
    exec::{self, ExecOutput, ExecRequest, ExecStream},
    files::{self, ContainerFile},
    group::IsolationGroup,
    observe::{self, ContainerExit, LogStream},
    removal,
};

const MILLSTONE_CONFIG_PATH_INTERNAL: &str = "/etc/millstone/config.toml";
// Test clients use loopback; binding there keeps published ports off other interfaces.
const PUBLISHED_HOST_IP: &str = "127.0.0.1";

/// Default image for the shared-volume permission fix-up container.
pub const DEFAULT_ALPINE_IMAGE: &str = "alpine:latest";
const DATADOG_INTAKE_HEALTHCHECK_INTERVAL: Duration = Duration::from_secs(1);
const DATADOG_INTAKE_HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(1);
const DATADOG_INTAKE_HEALTHCHECK_RETRIES: i64 = 30;
const DATADOG_INTAKE_HEALTHCHECK_START_PERIOD: Duration = Duration::from_secs(1);
const DATADOG_INTAKE_HEALTHCHECK_START_INTERVAL: Duration = Duration::from_secs(1);
const DATADOG_INTAKE_HEALTHCHECK_COMMAND: &str = concat!(
    "exec 3<>/dev/tcp/127.0.0.1/2049 && ",
    "printf 'GET /ready HTTP/1.1\\r\\nHost: localhost\\r\\nConnection: close\\r\\n\\r\\n' >&3 && ",
    "grep -q '200 OK' <&3"
);

/// Configuration for transient error retries.
///
/// Transient error retries are a class of retries that are applied to a select few operations that
/// have known error cases where retrying is safe and likely to fix the issue.
const TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS: u32 = 3;
const TRANSIENT_ERROR_RETRY_BASE_BACKOFF: Duration = Duration::from_millis(250);
const TRANSIENT_ERROR_RETRY_MAX_JITTER: Duration = Duration::from_millis(500);

pub enum ExitStatus {
    Success,
    Failed { code: i64, error: String },
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExitStatus::Success => write!(f, "success (0)"),
            ExitStatus::Failed { code, error } => write!(f, "failed (exit code: {}, error: {})", code, error),
        }
    }
}

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
    /// Linux host port on loopback; `None` uses an ephemeral port on every interface.
    host_port: Option<u16>,
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
    healthcheck: Option<HealthConfig>,
    ports: Vec<ContainerPort>,
    pub(crate) container_os: ContainerOs,
    host_cgroup_namespace: bool,
    /// Additional named Docker volume mounts, in `volume_name:/container/path` format.
    ///
    /// Unlike bind mounts specified via [`with_bind_mount`][Self::with_bind_mount], these reference
    /// existing named Docker volumes rather than host filesystem paths. Used to mount volumes that
    /// belong to other isolation groups (for example, a shared millstone mounting both the baseline
    /// and comparison agent volumes).
    additional_volume_mounts: Vec<String>,

    /// DNS aliases for this container on its primary network.
    ///
    /// Set via `NetworkingConfig.EndpointsConfig` at container creation time. Other containers on
    /// the same network can reach this container using any of these aliases in addition to its
    /// hostname. Used to give agent containers unambiguous names (for example, `"baseline"`, `"comparison"`)
    /// that the shared millstone can use to address each one independently.
    network_aliases: Vec<String>,

    /// Additional Docker networks to connect this container to after creation.
    ///
    /// The primary network is set via `HostConfig.NetworkMode`. Each network listed here is joined
    /// via a separate `docker network connect` call after the container is created but before it's
    /// started. Used to connect the shared millstone container to both agent networks so it can
    /// reach `baseline` and `comparison` by hostname.
    additional_networks: Vec<String>,

    /// Image used for the shared-volume permission fix-up container.
    ///
    /// Defaults to [`DEFAULT_ALPINE_IMAGE`].
    alpine_image: String,
}

impl DriverConfig {
    pub async fn millstone(config: MillstoneConfig) -> Result<Self, GenericError> {
        // Ensure the given configuration file path actually exists.
        match tokio::fs::metadata(&config.config_path).await {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(generic_error!(
                    "Specified millstone configuration path ({}) does not point to a file.",
                    config.config_path.display()
                ))
            }
            Err(e) => {
                return Err(generic_error!(
                    "Failed to ensure specified millstone configuration ({}) exists locally: {}",
                    config.config_path.display(),
                    e
                ))
            }
        }

        let millstone_binary_path = config
            .binary_path
            .unwrap_or_else(|| "/usr/local/bin/millstone".to_string());
        let entrypoint = vec![millstone_binary_path, MILLSTONE_CONFIG_PATH_INTERNAL.to_string()];

        let driver_config = Self::from_image("millstone", config.image)
            .with_entrypoint(entrypoint)
            .with_bind_mount(config.config_path, MILLSTONE_CONFIG_PATH_INTERNAL);

        Ok(driver_config)
    }

    pub async fn datadog_intake(config: DatadogIntakeConfig) -> Result<Self, GenericError> {
        let datadog_intake_binary_path = config
            .binary_path
            .unwrap_or_else(|| "/usr/local/bin/datadog-intake".to_string());
        let entrypoint = vec![datadog_intake_binary_path];

        let driver_config = DriverConfig::from_image("datadog-intake", config.image)
            .with_entrypoint(entrypoint)
            .with_healthcheck(
                vec![
                    "/bin/bash".to_string(),
                    "-c".to_string(),
                    DATADOG_INTAKE_HEALTHCHECK_COMMAND.to_string(),
                ],
                DATADOG_INTAKE_HEALTHCHECK_INTERVAL,
                DATADOG_INTAKE_HEALTHCHECK_TIMEOUT,
                DATADOG_INTAKE_HEALTHCHECK_RETRIES,
                DATADOG_INTAKE_HEALTHCHECK_START_PERIOD,
                DATADOG_INTAKE_HEALTHCHECK_START_INTERVAL,
            )
            // The intake gets an ephemeral host port; callers read it from DriverDetails after start.
            .with_exposed_port(Protocol::Tcp, 2049);

        Ok(driver_config)
    }

    pub async fn target(target_id: impl Into<String>, config: TargetConfig) -> Result<Self, GenericError> {
        let driver_config = DriverConfig::from_image(target_id, config.image)
            .with_entrypoint(config.entrypoint)
            .with_command(config.command)
            .with_env_vars(config.additional_env_vars)
            .with_container_os(config.container_os)
            .with_host_cgroup_namespace(config.host_cgroup_namespace);

        Ok(driver_config)
    }

    /// Creates a new `DriverConfig` from the given driver identifier and container image reference.
    pub fn from_image(driver_id: impl Into<String>, image: String) -> Self {
        Self {
            driver_id: driver_id.into(),
            image,
            entrypoint: None,
            command: None,
            env: vec![],
            binds: vec![],
            healthcheck: None,
            ports: vec![],
            container_os: ContainerOs::Linux,
            host_cgroup_namespace: false,
            additional_volume_mounts: vec![],
            network_aliases: vec![],
            additional_networks: vec![],
            alpine_image: DEFAULT_ALPINE_IMAGE.to_string(),
        }
    }

    /// Sets the shared-volume fix-up image for drivers built with [`Driver::from_config`].
    ///
    /// Prepared groups use the image passed to [`IsolationGroup::prepare`](crate::group::IsolationGroup::prepare).
    pub fn with_alpine_image(mut self, alpine_image: impl Into<String>) -> Self {
        self.alpine_image = alpine_image.into();
        self
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

    /// Adds environment variables to the container.
    pub fn with_env_vars(mut self, env: Vec<String>) -> Self {
        self.env.extend(env);
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

    /// Sets the healthcheck for the container.
    pub fn with_healthcheck(
        mut self, mut test_command: Vec<String>, interval: Duration, timeout: Duration, retries: i64,
        start_period: Duration, start_interval: Duration,
    ) -> Self {
        // We manually insert "CMD" as the first value in the command array, so that it doesn't have to be done by the
        // caller, since it's some goofy ass syntax to have to know about.
        test_command.insert(0, "CMD".to_string());

        self.healthcheck = Some(HealthConfig {
            test: Some(test_command),
            interval: Some(interval.as_nanos() as i64),
            timeout: Some(timeout.as_nanos() as i64),
            retries: Some(retries),
            start_period: Some(start_period.as_nanos() as i64),
            start_interval: Some(start_interval.as_nanos() as i64),
        });
        self
    }

    /// Adds a DNS alias for this container on its primary network.
    ///
    /// Prepared participants already resolve by participant name; this adds another name.
    /// Call this before creating the container.
    pub fn with_network_alias(mut self, alias: impl Into<String>) -> Self {
        self.network_aliases.push(alias.into());
        self
    }

    /// Connects this container to an additional Docker network after creation.
    ///
    /// The primary network is always the container's isolation group network. Each network added
    /// here is joined via `docker network connect` after the container is created but before it
    /// is started, so the container is reachable on all listed networks from the moment it runs.
    pub fn with_network(mut self, network: impl Into<String>) -> Self {
        self.additional_networks.push(network.into());
        self
    }

    /// Mounts a named Docker volume into the container at the given path.
    ///
    /// Unlike [`with_bind_mount`][Self::with_bind_mount], this references a named Docker volume
    /// rather than a host filesystem path. The volume must already exist when the container starts.
    /// This is useful for mounting volumes that belong to other isolation groups: for example,
    /// a shared millstone container that needs to reach the DogStatsD sockets of both the baseline
    /// and comparison agent containers.
    pub fn with_volume_mount(mut self, volume_name: impl Into<String>, container_path: impl AsRef<Path>) -> Self {
        self.additional_volume_mounts
            .push(format!("{}:{}", volume_name.into(), container_path.as_ref().display()));
        self
    }

    /// Adds an exposed port to the container.
    ///
    /// Linux containers publish it to an ephemeral host port on every interface, readable from [`DriverDetails`]
    /// after start. Windows containers expose it without publishing.
    pub fn with_exposed_port(mut self, protocol: Protocol, internal_port: u16) -> Self {
        self.ports.push(ContainerPort {
            protocol,
            internal_port,
            host_port: None,
        });
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
            host_port: Some(host_port),
        });
        self
    }

    /// Sets the operating system this container will run as.
    ///
    /// The OS choice drives several non-portable defaults (network driver, default binds, host
    /// resources to share, container path conventions) that the rest of the driver applies
    /// automatically through the helpers below. Callers should set this before any binds or
    /// health checks are added so OS-specific defaults are appended consistently.
    pub fn with_container_os(mut self, container_os: ContainerOs) -> Self {
        self.container_os = container_os;
        self
    }

    /// Configures whether a Linux target joins the Docker host's cgroup namespace.
    pub fn with_host_cgroup_namespace(mut self, host_cgroup_namespace: bool) -> Self {
        self.host_cgroup_namespace = host_cgroup_namespace;
        self
    }

    /// Whether the shared `/airlock` volume needs a one-shot world-writable chmod fix-up.
    ///
    /// Linux Docker volumes default to root-owned with restrictive permissions, so containers
    /// running as non-root users (the Datadog Agent image, in particular) cannot write to
    /// `/airlock` without an out-of-band chmod. We do that fix-up by spawning a short-lived
    /// Alpine container that owns the volume mount and runs `chmod -R 777 /airlock`. Windows
    /// containers do not have the same UID/permission model and the fix-up is unnecessary
    /// (and unsupported, since Alpine is a Linux image).
    fn needs_shared_volume_permission_fixup(&self) -> bool {
        self.container_os == ContainerOs::Linux
    }

    /// Docker network driver to use for the isolation group network on this container's OS.
    ///
    /// Linux containers use the `bridge` driver; Windows containers use `nat` (the only
    /// driver that supports container-to-container traffic on a single Windows host).
    fn network_driver(&self) -> &'static str {
        match self.container_os {
            ContainerOs::Linux => "bridge",
            ContainerOs::Windows => "nat",
        }
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
                let binding = match port.host_port {
                    Some(host_port) => PortBinding {
                        host_ip: Some(PUBLISHED_HOST_IP.to_string()),
                        host_port: Some(host_port.to_string()),
                    },
                    None => PortBinding {
                        host_ip: Some(String::new()),
                        host_port: Some(String::new()),
                    },
                };
                port_bindings
                    .entry(key)
                    .or_default()
                    .get_or_insert_with(Vec::new)
                    .push(binding);
            } else if let Some(host_port) = port.host_port {
                return Err(generic_error!(
                    "Participant '{}' publishes {} to host port {}, but Windows containers don't publish ports",
                    self.driver_id,
                    key,
                    host_port
                ));
            }
        }

        Ok((Some(exposed_ports), port_bindings))
    }

    /// Adds default mounts and configured named volumes to the supplied binds.
    ///
    /// Prepared Linux drivers mount only the shared `/airlock` volume by default. Linux drivers built with
    /// [`Driver::from_config`] also receive read-only host mounts for origin detection (`/proc`, `/sys/fs/cgroup`,
    /// the Docker socket). Windows containers use the shared `C:\airlock` volume.
    fn container_binds_from(
        &self, isolation_group_name: &str, mut binds: Vec<String>, host_resources: bool,
    ) -> Vec<String> {
        match self.container_os {
            ContainerOs::Linux => {
                binds.push(format!("{}:/airlock:z", isolation_group_name));
                if host_resources {
                    binds.push("/proc:/host/proc:ro".to_string());
                    binds.push("/sys/fs/cgroup:/host/sys/fs/cgroup:ro".to_string());
                    binds.push("/var/run/docker.sock:/var/run/docker.sock:ro".to_string());
                }
            }
            ContainerOs::Windows => {
                binds.push(format!("{}:C:\\airlock", isolation_group_name));
            }
        }

        binds.extend(self.additional_volume_mounts.clone());
        binds
    }
}

/// Detailed information about the spawned container.
#[derive(Debug, Default)]
pub struct DriverDetails {
    container_name: String,
    container_ip: Option<String>,
    port_mappings: Option<HashMap<String, u16>>,
}

/// Inserts an `internal_port` -> host port mapping when `host_port` parses as a valid `u16`.
///
/// Docker reports each binding's host port as a string, and we treat values that don't parse as
/// "no mapping available" rather than failing the whole inspect call. `internal_port` is the
/// existing key (already including the protocol suffix, for example `"58125/udp"`).
fn insert_port_mapping_if_parseable(
    port_mappings: &mut HashMap<String, u16>, internal_port: impl Into<String>, host_port: Option<&str>,
) {
    if let Some(host_port) = host_port.and_then(|value| value.parse::<u16>().ok()) {
        port_mappings.insert(internal_port.into(), host_port);
    }
}

impl DriverDetails {
    /// Returns the name of the container.
    pub fn container_name(&self) -> &str {
        &self.container_name
    }

    /// Returns the container IP address on its primary Docker network, if known.
    pub fn container_ip(&self) -> Option<&str> {
        self.container_ip.as_deref()
    }

    /// Returns the host port of an exposed or published port, if mapped.
    ///
    /// Use the same protocol and internal port as in the configuration. Published ports listen on `127.0.0.1`;
    /// ephemeral ports listen on every interface.
    pub fn try_get_exposed_port(&self, protocol: Protocol, internal_port: u16) -> Option<u16> {
        self.port_mappings
            .as_ref()
            .and_then(|port_mappings| port_mappings.get(&format!("{}/{}", internal_port, protocol)).copied())
    }
}

/// What `Driver::create` makes besides the container.
#[derive(Eq, PartialEq)]
enum Provisioning {
    /// Creates shared resources if missing, fixes volume permissions, and pulls images.
    OnCreate,
    /// Uses an [`IsolationGroup`](crate::group::IsolationGroup)'s resources and local images.
    Prepared,
}

/// Container driver.
pub struct Driver {
    isolation_group_id: String,
    isolation_group_name: String,
    container_name: String,
    config: DriverConfig,
    docker: Docker,
    log_dir: Option<PathBuf>,
    provisioning: Provisioning,
}

impl Driver {
    /// Creates a new `Driver` from the given isolation group ID and configuration.
    ///
    /// # Isolation group
    ///
    /// The isolation group ID serves as a unique identifier to be used for both the name of the container as well as
    /// the shared resources that are created and attached to the container. If two drivers share the same isolation
    /// group ID, the containers they spawn will be located in the same network namespace, have access to the same
    /// shared Airlock volume, etc.
    ///
    /// # Shared volume
    ///
    /// The container will have a volume bind-mounted at `/airlock` that's shared between all containers in the same
    /// isolation group. This volume is mounted as world writeable (777) so all containers can freely read and write to
    /// it. This makes it easier for containers to share data between one another, but also means that care should be
    /// taken to avoid conflicts between trying to write to the same file, etc.
    ///
    /// # Errors
    ///
    /// If the Docker client can't be created/configured, an error will be returned.
    pub fn from_config(isolation_group_id: String, config: DriverConfig) -> Result<Self, GenericError> {
        let docker = crate::docker::connect()?;

        Ok(Self {
            provisioning: Provisioning::OnCreate,
            ..Self::in_group(docker, isolation_group_id, config)
        })
    }

    /// Builds a driver that uses a prepared group's resources and a local image.
    pub(crate) fn in_group(docker: Docker, isolation_group_id: String, config: DriverConfig) -> Self {
        let isolation_group_name = group_resource_name(&isolation_group_id);
        Self {
            container_name: format!("{}-{}", isolation_group_name, config.driver_id),
            isolation_group_name,
            isolation_group_id,
            config,
            docker,
            log_dir: None,
            provisioning: Provisioning::Prepared,
        }
    }

    /// Configures the driver to capture container logs.
    ///
    /// The logs will be stored in the given directory, under a subdirectory named after the isolation group ID. Each
    /// container will get a log for standard output and standard error, following the pattern of `<container
    /// name>.[stdout|stderr].log`.
    pub fn with_logging(mut self, log_dir: PathBuf) -> Self {
        self.log_dir = Some(log_dir);
        self
    }

    /// Returns the string identifier of the driver.
    ///
    /// For a prepared isolation group, this is the participant name.
    pub fn driver_id(&self) -> &str {
        &self.config.driver_id
    }

    /// Removes a group's containers and their anonymous volumes, shared volume, and network.
    ///
    /// Force-removes containers, killing any that are running. Attempts every removal despite failures and treats
    /// missing resources as removed. Callers with a Docker connection should use [`IsolationGroup::remove`].
    ///
    /// # Errors
    ///
    /// Returns a Docker client creation error, or a combined error listing all resource-listing and removal failures.
    pub async fn clean_related_resources(isolation_group_id: String) -> Result<(), GenericError> {
        let docker = crate::docker::connect()?;
        IsolationGroup::remove(&docker, &isolation_group_id).await
    }

    async fn create_network_if_missing(&self) -> Result<(), GenericError> {
        with_transient_error_retry("network creation", &self.isolation_group_id, move || {
            self.create_network_if_missing_inner()
        })
        .await
    }

    async fn create_network_if_missing_inner(&self) -> Result<(), GenericError> {
        // See if the network already exists or not.
        let networks = self.docker.list_networks(None).await?;
        if networks
            .iter()
            .any(|network| network.name.as_deref() == Some(self.isolation_group_name.as_str()))
        {
            debug!("Network '{}' already exists.", self.isolation_group_name);
            return Ok(());
        }

        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Network '{}' does not exist. Creating...",
            self.isolation_group_name
        );

        // Create the network since it doesn't yet exist.
        let network_options = NetworkCreateRequest {
            name: self.isolation_group_name.clone(),
            driver: Some(self.config.network_driver().to_string()),
            ipam: Some(Ipam::default()),
            enable_ipv6: Some(false),
            labels: Some(get_default_airlock_labels(self.isolation_group_id.as_str())),
            ..Default::default()
        };
        let response = self.docker.create_network(network_options).await?;
        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Created network '{}' (ID: {:?}).",
            self.isolation_group_name,
            response.id
        );

        Ok(())
    }

    async fn create_image_if_missing_inner(&self, image: &str) -> Result<(), GenericError> {
        let image_options = CreateImageOptions {
            from_image: Some(image.to_string()),
            ..Default::default()
        };

        let mut create_stream = self.docker.create_image(Some(image_options), None, None);
        while let Some(info) = create_stream.next().await {
            trace!(
                driver_id = self.config.driver_id,
                isolation_group = self.isolation_group_id,
                image,
                "Received image pull update: {:?}",
                info
            );
        }

        Ok(())
    }

    async fn create_image_if_missing(&self) -> Result<(), GenericError> {
        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Pulling image '{}'...",
            self.config.image
        );

        self.create_image_if_missing_inner(self.config.image.as_str()).await?;

        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Pulled image '{}'.",
            self.config.image
        );

        Ok(())
    }

    async fn create_volume_if_missing(&self) -> Result<(), GenericError> {
        // Check to see if the shared volume already exists.
        let volumes = self
            .docker
            .list_volumes(None::<bollard::query_parameters::ListVolumesOptions>)
            .await?;
        if volumes
            .volumes
            .iter()
            .flatten()
            .any(|volume| volume.name == self.isolation_group_name.as_str())
        {
            debug!("Shared volume '{}' already exists.", self.isolation_group_name);
            return Ok(());
        }

        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Shared volume '{}' does not exist. Creating...",
            self.isolation_group_name
        );

        let volume_options = VolumeCreateRequest {
            name: Some(self.isolation_group_name.clone()),
            driver: Some("local".to_string()),
            labels: Some(get_default_airlock_labels(self.isolation_group_id.as_str())),
            ..Default::default()
        };
        self.docker.create_volume(volume_options).await?;

        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Created shared volume '{}'.",
            self.isolation_group_name
        );

        Ok(())
    }

    async fn adjust_shared_volume_permissions(&self) -> Result<(), GenericError> {
        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Adjusting permissions on shared volume '{}'...",
            self.container_name
        );

        // We spin up a minimal Alpine container, chmod the directory bind-mounted to the shared volume, and that's it.
        let image = self.config.alpine_image.clone();
        self.create_image_if_missing_inner(&image).await?;

        let container_name = format!("airlock-{}-volume-fix-up", self.isolation_group_id);
        let entrypoint = vec![
            "chmod".to_string(),
            "-R".to_string(),
            "777".to_string(),
            "/airlock".to_string(),
        ];
        let _ = self
            .create_container_inner(
                container_name.clone(),
                image,
                Some(entrypoint),
                None,
                vec![],
                None,
                false,
            )
            .await?;

        self.start_container_inner(&container_name).await?;
        // The legacy permission fix-up ignores the exit code.
        let _exit = observe::wait(&self.docker, &container_name).await?;
        self.cleanup_inner(&container_name).await?;

        Ok(())
    }

    /// Returns the DNS aliases for this container on its isolation group network.
    fn network_aliases(&self) -> Vec<String> {
        match self.provisioning {
            Provisioning::OnCreate => self.config.network_aliases.clone(),
            Provisioning::Prepared => {
                let mut aliases = vec![self.config.driver_id.clone()];
                aliases.extend(
                    self.config
                        .network_aliases
                        .iter()
                        .filter(|alias| *alias != &self.config.driver_id)
                        .cloned(),
                );
                aliases
            }
        }
    }

    async fn create_container_inner(
        &self, container_name: String, image: String, entrypoint: Option<Vec<String>>, cmd: Option<Vec<String>>,
        binds: Vec<String>, env: Option<Vec<String>>, with_ports: bool,
    ) -> Result<String, GenericError> {
        let binds = self.config.container_binds_from(
            &self.isolation_group_name,
            binds,
            self.provisioning == Provisioning::OnCreate,
        );

        // Apply DNS aliases on the primary network.
        let aliases = self.network_aliases();
        let networking_config = if !aliases.is_empty() {
            let mut endpoints = HashMap::new();
            endpoints.insert(
                self.isolation_group_name.clone(),
                EndpointSettings {
                    aliases: Some(aliases),
                    ..Default::default()
                },
            );
            Some(
                ContainerNetworkingConfig {
                    endpoints_config: endpoints,
                }
                .into(),
            )
        } else {
            None
        };

        let (exposed_ports, port_bindings) = if with_ports {
            self.config.port_publication()?
        } else {
            (None, None)
        };

        // Linux drivers built with Driver::from_config share host PIDs for origin detection.
        // Drivers from a prepared group keep their own PID namespace.
        let pid_mode = (self.config.container_os == ContainerOs::Linux && self.provisioning == Provisioning::OnCreate)
            .then(|| "host".to_string());
        let cgroupns_mode = (self.config.container_os == ContainerOs::Linux && self.config.host_cgroup_namespace)
            .then_some(HostConfigCgroupnsModeEnum::HOST);

        let container_config = ContainerCreateBody {
            hostname: Some(self.config.driver_id.clone()),
            env,
            image: Some(image),
            entrypoint,
            cmd,
            host_config: Some(HostConfig {
                binds: Some(binds),
                network_mode: Some(self.isolation_group_name.clone()),
                port_bindings,
                pid_mode,
                cgroupns_mode,
                ..Default::default()
            }),
            healthcheck: self.config.healthcheck.clone(),
            exposed_ports,
            labels: Some(get_default_airlock_labels(self.isolation_group_id.as_str())),
            networking_config,
            ..Default::default()
        };

        let create_options = CreateContainerOptionsBuilder::default().name(&container_name).build();

        let response = self
            .docker
            .create_container(Some(create_options), container_config)
            .await?;

        Ok(response.id)
    }

    async fn create_container(&self) -> Result<String, GenericError> {
        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Creating container '{}'...",
            self.container_name
        );

        let container_id = self
            .create_container_inner(
                self.container_name.clone(),
                self.config.image.clone(),
                self.config.entrypoint.clone(),
                self.config.command.clone(),
                self.config.binds.clone(),
                Some(self.config.env.clone()),
                true,
            )
            .await?;

        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Created container '{}' (ID: {}).",
            self.container_name,
            container_id
        );

        Ok(container_id)
    }

    async fn start_container_inner(&self, container_name: &str) -> Result<DriverDetails, GenericError> {
        // Retrying is safe: the daemon treats starting an already-running container as a no-op rather than an error.
        with_transient_error_retry("container start", &self.isolation_group_id, move || async move {
            self.docker
                .start_container(container_name, None)
                .await
                .map_err(GenericError::from)
        })
        .await?;

        let mut details = DriverDetails {
            container_name: container_name.to_string(),
            ..Default::default()
        };

        let response = self.docker.inspect_container(container_name, None).await?;
        if let Some(network_settings) = response.network_settings {
            // Look up the IP only on the primary isolation-group network. Falling back to
            // "any other network's IP" would be non-deterministic and effectively wrong for
            // assertion targeting.
            if let Some(networks) = network_settings.networks.as_ref() {
                details.container_ip = networks
                    .get(&self.isolation_group_name)
                    .and_then(|settings| settings.ip_address.clone())
                    .filter(|address| !address.is_empty());
            }

            if let Some(ports) = network_settings.ports {
                let port_mappings = details.port_mappings.get_or_insert_with(HashMap::new);
                for (internal_port, bindings) in ports {
                    if let Some(bindings) = bindings {
                        for binding in bindings {
                            insert_port_mapping_if_parseable(
                                port_mappings,
                                internal_port.clone(),
                                binding.host_port.as_deref(),
                            );
                            if port_mappings.contains_key(&internal_port) {
                                break;
                            }
                        }
                    }
                }
            }
        }

        Ok(details)
    }

    /// Starts the container that [`Driver::create`] created.
    ///
    /// # Errors
    ///
    /// If there is an error while starting or inspecting the container, or while preparing its log files, it will be
    /// returned.
    pub async fn start_container(&self) -> Result<DriverDetails, GenericError> {
        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Starting container '{}'...",
            self.container_name
        );

        let details = self.start_container_inner(&self.container_name).await?;

        if let Some(log_dir) = self.log_dir.clone() {
            debug!(
                "Capturing logs for container '{}' to {}...",
                self.container_name,
                log_dir.display()
            );

            self.capture_container_logs(log_dir, &self.config.driver_id, &self.container_name)
                .await?;
        }

        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Started container '{}'.",
            self.container_name
        );

        Ok(details)
    }

    /// Connects this container to each network listed in `additional_networks`.
    ///
    /// Called after container creation but before start, so the container is already reachable
    /// on all configured networks from the moment it begins running.
    async fn connect_to_additional_networks(&self) -> Result<(), GenericError> {
        for network in &self.config.additional_networks {
            self.docker
                .connect_network(
                    network,
                    NetworkConnectRequest {
                        container: self.container_name.clone(),
                        endpoint_config: None,
                    },
                )
                .await
                .map_err(|e| {
                    generic_error!(
                        "Failed to connect container '{}' to network '{}': {}",
                        self.container_name,
                        network,
                        e
                    )
                })?;
        }
        Ok(())
    }

    /// Creates and starts the container, provisioning resources as described by [`Self::create`].
    ///
    /// # Errors
    ///
    /// Returns any error from creation, starting, inspection, or log setup.
    pub async fn start(&mut self) -> Result<DriverDetails, GenericError> {
        self.create().await?;
        self.start_container().await
    }

    /// Creates the container and connects its networks; call [`Self::start_container`] to start it.
    ///
    /// Returns the full container ID from Docker.
    ///
    /// Drivers built with [`Self::from_config`] also provision shared resources and pull images. On Linux, they
    /// mount host resources for origin detection and share the host PID namespace. Drivers from an
    /// [`IsolationGroup`](crate::group::IsolationGroup) reuse its prepared resources, require a local image, and
    /// keep their own PID namespace. Their only default mount is the group volume.
    ///
    /// # Errors
    ///
    /// Returns any error from provisioning, container creation, or network connection, even if the container
    /// was created before the network connection failed.
    pub async fn create(&self) -> Result<String, GenericError> {
        match self.provisioning {
            Provisioning::OnCreate => {
                self.create_network_if_missing().await?;
                self.create_image_if_missing().await?;
                self.create_volume_if_missing().await?;
                if self.config.needs_shared_volume_permission_fixup() {
                    self.adjust_shared_volume_permissions().await?;
                }
            }
            Provisioning::Prepared => {}
        }

        let container_id = self.create_container().await?;
        self.connect_to_additional_networks().await?;
        Ok(container_id)
    }

    /// Waits until the container is marked as healthy.
    ///
    /// If the container has no health checks defined, this returns early and does no waiting.
    ///
    /// # Errors
    ///
    /// If there is an error while inspecting the container, it will be returned.
    pub async fn wait_for_container_healthy(&mut self) -> Result<(), GenericError> {
        loop {
            // Inspect the container, and see if it even has any health checks defined. If not, then we can return early.
            let response = self.docker.inspect_container(&self.container_name, None).await?;
            let state = response
                .state
                .ok_or_else(|| generic_error!("Container state should be present."))?;

            // Make sure the container is actually running.
            let status = state
                .status
                .ok_or_else(|| generic_error!("Container status should be present."))?;
            if status != ContainerStateStatusEnum::RUNNING {
                return Err(generic_error!(
                    "Container exited unexpectedly (driver_id: {}, container: {}). Check logs in the test run directory.",
                    self.config.driver_id,
                    self.container_name
                ));
            }

            if let Some(health_status) = state.health.and_then(|h| h.status) {
                match health_status {
                    // No healthcheck defined, or healthy, so we're good to go.
                    HealthStatusEnum::EMPTY | HealthStatusEnum::NONE | HealthStatusEnum::HEALTHY => {
                        debug!(
                            driver_id = self.config.driver_id,
                            "Container '{}' healthy or no healthcheck defined. Proceeding.", &self.container_name
                        );
                        return Ok(());
                    }

                    // Not healthy yet, so we'll keep waiting.
                    HealthStatusEnum::STARTING => {
                        debug!(
                            driver_id = self.config.driver_id,
                            "Container '{}' not yet healthy. Waiting...", &self.container_name
                        );
                    }

                    HealthStatusEnum::UNHEALTHY => {
                        return Err(generic_error!(
                            "Container became unhealthy (driver_id: {}, container: {}). Check logs in the test run directory.",
                            self.config.driver_id,
                            self.container_name
                        ));
                    }
                }
            } else {
                debug!(
                    driver_id = self.config.driver_id,
                    "Container '{}' has no healthcheck defined. Proceeding.", &self.container_name
                );
                return Ok(());
            }

            // Wait for a second and then check again.
            sleep(Duration::from_secs(1)).await;
        }
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

    /// Waits for the container to finish and returns its success or failure status.
    ///
    /// Success requires exit code 0 and no Docker error message; every other exit is a failed status.
    ///
    /// # Errors
    ///
    /// Returns any observation error from [`Self::wait_exit`], including a container that has not started.
    pub async fn wait_for_container_exit(&self) -> Result<ExitStatus, GenericError> {
        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Waiting for container '{}' to finish...",
            &self.container_name
        );

        let exit = self.wait_exit().await?;
        let exit_status = if exit.code == 0 && exit.error.is_none() {
            ExitStatus::Success
        } else {
            ExitStatus::Failed {
                code: exit.code,
                error: exit.error.unwrap_or_else(|| "<no error message provided>".to_string()),
            }
        };

        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Container '{}' finished: {exit_status}",
            &self.container_name
        );

        Ok(exit_status)
    }

    /// Places files in a Linux container between create and start.
    ///
    /// Files are owned by root and keep their mode. Missing parent directories are created with mode `0755`,
    /// owned by root. Existing files are replaced, but a file cannot replace a directory. Paths resolve inside
    /// the container, including through symlinks. An empty slice sends no request.
    ///
    /// # Errors
    ///
    /// Returns an error for Windows containers, invalid or duplicate paths, modes beyond `07777`, archive
    /// construction failures, or Docker API failures. Every file is validated before contacting Docker.
    pub async fn place_files(&self, files: &[ContainerFile]) -> Result<(), GenericError> {
        self.ensure_linux_for_files()?;
        files::place(&self.docker, &self.container_name, files).await
    }

    /// Reads a regular file from a created, running, or exited Linux container, including its mounted volumes.
    ///
    /// Returns `None` if the path is missing and the container exists. Paths follow [`ContainerFile`]'s rules.
    /// The entire file is read into memory without a size cap. Callers can impose their own timeout.
    ///
    /// # Errors
    ///
    /// Returns an error for Windows containers, invalid paths, a missing container, Docker API failures,
    /// empty or malformed archives, or a path naming a directory, symbolic link, or other non-regular file.
    pub async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, GenericError> {
        self.ensure_linux_for_files()?;
        files::read(&self.docker, &self.container_name, path).await
    }

    fn ensure_linux_for_files(&self) -> Result<(), GenericError> {
        if self.config.container_os == ContainerOs::Windows {
            return Err(generic_error!(
                "Participant '{}' runs in a Windows container; airlock places and reads files only in Linux containers",
                self.config.driver_id
            ));
        }
        Ok(())
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
    /// variables extend the container's environment. Polling the stream also writes stdin and closes it after the
    /// supplied bytes. Every exit code is data; read it with [`ExecStream::exit_code`] once the stream has ended.
    ///
    /// The stream ends when output closes; a background child holding stdout or stderr open keeps it open. Dropping
    /// this future or the stream stops reading and writing, but the command runs until it exits or the container
    /// stops. Callers can impose their own timeout.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty command, an empty environment key or one containing `=`, or Docker creation or
    /// start failures (including a container that isn't running). The stream yields output read or framing errors;
    /// exit inspection errors are returned by [`ExecStream::exit_code`]. A command closing stdin early is handled
    /// through its output and exit code.
    pub async fn exec_stream(&self, request: ExecRequest) -> Result<ExecStream, GenericError> {
        exec::start(&self.docker, &self.container_name, request).await
    }

    /// Executes a command inside the running container and returns its stdout.
    ///
    /// The command runs as root with no TTY. Stderr is discarded, and only stdout is returned. If the command exits with a
    /// nonzero status, an error is returned.
    ///
    /// # Errors
    ///
    /// If the exec creation, start, output collection, or command exit code indicates failure, an error is returned.
    pub async fn exec_in_container(&self, cmd: Vec<String>) -> Result<String, GenericError> {
        let exec_opts = CreateExecOptions {
            attach_stdout: Some(true),
            attach_stderr: Some(false),
            cmd: Some(cmd.clone()),
            ..Default::default()
        };

        let exec = self
            .docker
            .create_exec(&self.container_name, exec_opts)
            .await
            .with_error_context(|| format!("Failed to create exec instance for container {}.", self.container_name))?;

        let exec_id = exec.id.clone();

        let output = self
            .docker
            .start_exec(&exec.id, None)
            .await
            .with_error_context(|| format!("Failed to start exec for container {}.", self.container_name))?;

        let mut stdout = String::new();
        if let StartExecResults::Attached { mut output, .. } = output {
            while let Some(chunk) = output.try_next().await? {
                if let LogOutput::StdOut { message } = chunk {
                    stdout.push_str(&String::from_utf8_lossy(&message));
                }
            }
        }

        // Check the command's exit code.
        let inspect = self
            .docker
            .inspect_exec(&exec_id)
            .await
            .error_context("Failed to inspect exec result.")?;

        if let Some(code) = inspect.exit_code {
            if code != 0 {
                return Err(generic_error!(
                    "Command {:?} exited with code {} in container {}.",
                    cmd,
                    code,
                    self.container_name
                ));
            }
        }

        Ok(stdout)
    }

    async fn cleanup_inner(&self, container_name: &str) -> Result<(), GenericError> {
        self.docker.stop_container(container_name, None).await?;
        self.docker.remove_container(container_name, None).await?;

        Ok(())
    }

    /// Stops the container with a grace period, then force-removes it and its anonymous volumes.
    ///
    /// Docker sends SIGKILL after `grace`, cut to whole seconds that end at least two seconds before `deadline`.
    /// A stop still pending two seconds before `deadline` is abandoned, and the container is force-removed.
    /// Run this future under `tokio::time::timeout_at(deadline, ...)` to bound the entire operation.
    ///
    /// This future is cancel-safe: once Docker receives the stop, dropping it leaves the container stopping or
    /// removed. A later [`IsolationGroup::remove`] force-removes any remaining container.
    ///
    /// # Errors
    ///
    /// Returns an error if removal fails. A missing container counts as removed; stop failures still lead to removal.
    pub async fn stop_and_remove(&self, grace: Duration, deadline: tokio::time::Instant) -> Result<(), GenericError> {
        removal::stop_and_remove(&self.docker, &self.container_name, grace, deadline).await
    }

    async fn capture_container_logs(
        &self, container_log_dir: PathBuf, log_name: &str, container_name: &str,
    ) -> Result<(), GenericError> {
        // Make sure the directories exist first and prepare the files, just to get any permissions issues out of the
        // way up front before we spawn our background task.
        tokio::fs::create_dir_all(&container_log_dir)
            .await
            .error_context("Failed to create logs directory. Possible permissions issue.")?;

        let stdout_log_path = container_log_dir.join(format!("{}.stdout.log", log_name));
        let stderr_log_path = container_log_dir.join(format!("{}.stderr.log", log_name));

        let mut stdout_file = tokio::fs::File::create(&stdout_log_path)
            .await
            .map(BufWriter::new)
            .error_context("Failed to create standard output log file. Possible permissions issue.")?;
        let mut stderr_file = tokio::fs::File::create(&stderr_log_path)
            .await
            .map(BufWriter::new)
            .error_context("Failed to create standard error log file. Possible permissions issue.")?;

        // Spawn a background task to capture the logs.
        let logs_config = LogsOptions {
            follow: true,
            stdout: true,
            stderr: true,
            ..Default::default()
        };
        let mut log_stream = self.docker.logs(container_name, Some(logs_config));

        tokio::spawn(async move {
            while let Some(log_result) = log_stream.next().await {
                match log_result {
                    Ok(log) => match log {
                        LogOutput::StdErr { message } => {
                            if let Err(e) = stderr_file.write_all(&strip_ansi_codes(&message)).await {
                                error!(error = %e, "Failed to write log line to standard error log file.");
                                break;
                            }
                            if let Err(e) = stderr_file.flush().await {
                                error!(error = %e, "Failed to flush standard error log file.");
                                break;
                            }
                        }
                        LogOutput::StdOut { message } => {
                            if let Err(e) = stdout_file.write_all(&strip_ansi_codes(&message)).await {
                                error!(error = %e, "Failed to write log line to standard output log file.");
                                break;
                            }
                            if let Err(e) = stdout_file.flush().await {
                                error!(error = %e, "Failed to flush standard output log file.");
                                break;
                            }
                        }
                        LogOutput::StdIn { .. } | LogOutput::Console { .. } => {}
                    },
                    Err(e) => {
                        error!(error = %e, "Failed to read log line from container.");
                        break;
                    }
                }
            }

            // One final fsync to ensure the logs are fully written to disk.
            if let Err(e) = stdout_file.get_mut().sync_all().await {
                error!(error = %e, "Failed to fsync standard output log file.");
            }

            if let Err(e) = stderr_file.get_mut().sync_all().await {
                error!(error = %e, "Failed to fsync standard error log file.");
            }
        });

        Ok(())
    }
}

/// Removes ANSI escape sequences (`ESC[...letter`) from a byte slice.
fn strip_ansi_codes(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b && input.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < input.len() && !input[i].is_ascii_alphabetic() {
                i += 1;
            }
            i += 1;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
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
async fn with_transient_error_retry<F, Fut, T>(
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
    fn alpine_image_defaults_to_docker_hub_and_can_be_overridden() {
        let config = DriverConfig::from_image("target", "example:latest".to_string());
        assert_eq!(config.alpine_image, "alpine:latest");

        let config = config.with_alpine_image("registry.example/alpine:3.20");
        assert_eq!(config.alpine_image, "registry.example/alpine:3.20");
    }

    #[test]
    fn legacy_linux_container_binds_include_airlock_and_linux_host_resources() {
        let config = DriverConfig::from_image("target", "example:latest".to_string());

        let binds = config.container_binds_from("airlock-test", config.binds.clone(), true);

        assert!(binds.contains(&"airlock-test:/airlock:z".to_string()));
        assert!(binds.contains(&"/proc:/host/proc:ro".to_string()));
        assert!(binds.contains(&"/sys/fs/cgroup:/host/sys/fs/cgroup:ro".to_string()));
        assert!(binds.contains(&"/var/run/docker.sock:/var/run/docker.sock:ro".to_string()));
    }

    #[test]
    fn prepared_linux_container_binds_include_only_airlock_and_configured_mounts() {
        let config = DriverConfig::from_image("target", "example:latest".to_string());
        assert_eq!(
            config.container_binds_from("airlock-test", config.binds.clone(), false),
            ["airlock-test:/airlock:z"]
        );

        let config = config
            .with_readonly_bind_mount("/host/config", "/etc/config")
            .with_volume_mount("extra", "/extra");
        assert_eq!(
            config.container_binds_from("airlock-test", config.binds.clone(), false),
            ["/host/config:/etc/config:ro", "airlock-test:/airlock:z", "extra:/extra"]
        );
    }

    #[test]
    fn windows_container_binds_use_windows_airlock_and_skip_linux_host_resources() {
        let config =
            DriverConfig::from_image("target", "example:latest".to_string()).with_container_os(ContainerOs::Windows);

        // Host-resource selection applies only to Linux containers.
        let binds = config.container_binds_from("airlock-test", config.binds.clone(), false);

        assert!(binds.contains(&"airlock-test:C:\\airlock".to_string()));
        assert!(!binds.iter().any(|bind| bind.contains("/proc")));
        assert!(!binds.iter().any(|bind| bind.contains("/sys/fs/cgroup")));
        assert!(!binds.iter().any(|bind| bind.contains("/var/run/docker.sock")));
        assert!(!binds.iter().any(|bind| bind.ends_with(":z")));
    }

    #[tokio::test]
    async fn target_config_preserves_windows_container_os() {
        let target = TargetConfig {
            image: "example:latest".to_string(),
            entrypoint: vec![],
            command: vec![],
            additional_env_vars: vec![],
            container_os: ContainerOs::Windows,
            host_cgroup_namespace: false,
        };

        let config = DriverConfig::target("target", target).await.unwrap();

        assert_eq!(config.container_os, ContainerOs::Windows);
        assert!(!config.host_cgroup_namespace);
    }

    #[tokio::test]
    async fn target_config_preserves_host_cgroup_namespace() {
        let target = TargetConfig {
            image: "example:latest".to_string(),
            entrypoint: vec![],
            command: vec![],
            additional_env_vars: vec![],
            container_os: ContainerOs::Linux,
            host_cgroup_namespace: true,
        };

        let config = DriverConfig::target("target", target).await.unwrap();

        assert!(config.host_cgroup_namespace);
    }

    #[test]
    fn port_mapping_inserts_parseable_host_port() {
        let mut mappings = HashMap::new();

        insert_port_mapping_if_parseable(&mut mappings, "55100/tcp", Some("49152"));

        assert_eq!(mappings.get("55100/tcp"), Some(&49152));
    }

    #[test]
    fn port_mapping_ignores_invalid_host_port() {
        let mut mappings = HashMap::new();

        insert_port_mapping_if_parseable(&mut mappings, "55100/tcp", Some("not-a-port"));

        assert!(!mappings.contains_key("55100/tcp"));
    }

    #[test]
    fn windows_container_skips_shared_volume_permission_fixup() {
        let config =
            DriverConfig::from_image("target", "example:latest".to_string()).with_container_os(ContainerOs::Windows);

        assert!(!config.needs_shared_volume_permission_fixup());
    }

    #[test]
    fn windows_container_uses_nat_network_driver() {
        let config =
            DriverConfig::from_image("target", "example:latest".to_string()).with_container_os(ContainerOs::Windows);

        assert_eq!(config.network_driver(), "nat");
    }

    #[test]
    fn a_container_without_ports_exposes_and_publishes_nothing() {
        for os in [ContainerOs::Linux, ContainerOs::Windows] {
            let config = DriverConfig::from_image("target", "example:latest".to_string()).with_container_os(os);

            assert_eq!(config.port_publication().unwrap(), (None, None));
        }
    }

    #[test]
    fn a_linux_exposed_port_is_published_to_an_ephemeral_host_port() {
        let config =
            DriverConfig::from_image("target", "example:latest".to_string()).with_exposed_port(Protocol::Tcp, 55100);

        let (exposed_ports, port_bindings) = config.port_publication().unwrap();

        assert_eq!(exposed_ports, Some(vec!["55100/tcp".to_string()]));
        assert_eq!(
            port_bindings,
            Some(HashMap::from([(
                "55100/tcp".to_string(),
                Some(vec![PortBinding {
                    host_ip: Some(String::new()),
                    host_port: Some(String::new())
                }]),
            )]))
        );
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
    fn windows_container_exposes_ports_without_publishing_to_host() {
        let config = DriverConfig::from_image("target", "example:latest".to_string())
            .with_container_os(ContainerOs::Windows)
            .with_exposed_port(Protocol::Udp, 58125);

        assert_eq!(
            config.port_publication().unwrap(),
            (Some(vec!["58125/udp".to_string()]), None)
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
            .with_exposed_port(Protocol::Udp, 9001)
            .with_exposed_port(Protocol::Tcp, 9000)
            .with_exposed_port(Protocol::Udp, 9001);

        let (exposed_ports, _) = config.port_publication().unwrap();

        assert_eq!(
            exposed_ports,
            Some(vec!["9001/udp".to_string(), "9000/tcp".to_string()])
        );
    }

    #[test]
    fn linux_container_uses_bridge_network_driver() {
        let config = DriverConfig::from_image("target", "example:latest".to_string());

        assert_eq!(config.network_driver(), "bridge");
    }

    /// Isolation group ID used against the fake daemon.
    const GROUP: &str = "ab12cd34";

    /// Bounds requests to the fake daemon.
    const BOUND: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn creating_a_container_does_not_start_it() {
        // Callers treat only a cancelled `create` as able to leave resources behind, so creating must not start the
        // container and starting must create nothing.
        let daemon = FakeDaemon::start(
            |method, path| match (method, path.split('?').next().unwrap_or_default()) {
                ("GET", "/networks") => (200, r#"[{"Name": "airlock-ab12cd34"}]"#),
                ("POST", "/images/create") => (200, "{}"),
                ("GET", "/volumes") => (200, r#"{"Volumes": [], "Warnings": []}"#),
                ("POST", "/volumes/create") => (
                    201,
                    r#"{"Name": "airlock-ab12cd34", "Driver": "local", "Mountpoint": "/x", "Labels": {},
                        "Options": {}, "Scope": "local"}"#,
                ),
                ("POST", "/containers/create") => (201, r#"{"Id": "c1", "Warnings": []}"#),
                ("POST", "/containers/airlock-ab12cd34-target/start") => (204, ""),
                ("GET", "/containers/airlock-ab12cd34-target/json") => (200, "{}"),
                _ => (500, r#"{"message": "unexpected request"}"#),
            },
        )
        .await;
        // A Windows container skips the shared volume fix-up, which starts a helper container of its own.
        let driver = Driver {
            isolation_group_id: GROUP.to_string(),
            isolation_group_name: format!("airlock-{GROUP}"),
            container_name: format!("airlock-{GROUP}-target"),
            config: DriverConfig::from_image("target", "example:latest".to_string())
                .with_container_os(ContainerOs::Windows),
            docker: daemon.docker.clone(),
            log_dir: None,
            provisioning: Provisioning::OnCreate,
        };
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
        assert_eq!(
            created.last().map(String::as_str),
            Some("POST /containers/create"),
            "{created:?}"
        );
        assert!(!created.iter().any(|path| path.ends_with("/start")), "{created:?}");

        let details = tokio::time::timeout(BOUND, driver.start_container())
            .await
            .expect("starting should finish within the bound")
            .expect("starting should succeed");

        assert_eq!(details.container_name(), "airlock-ab12cd34-target");
        assert_eq!(
            paths()[created.len()..],
            [
                "POST /containers/airlock-ab12cd34-target/start",
                "GET /containers/airlock-ab12cd34-target/json",
            ]
        );
    }

    #[tokio::test]
    async fn a_windows_container_neither_places_nor_reads_files() {
        let daemon = FakeDaemon::start(|_, _| (500, r#"{"message":"unexpected request"}"#)).await;
        let driver = Driver::in_group(
            daemon.docker.clone(),
            GROUP.to_string(),
            DriverConfig::from_image("subject", "example:latest".to_string()).with_container_os(ContainerOs::Windows),
        );
        let placement = driver.place_files(&[ContainerFile::new("/file", "secret")]).await;
        let empty_placement = driver.place_files(&[]).await;
        let read = driver.read_file("/file").await;
        for error in [placement.unwrap_err(), empty_placement.unwrap_err(), read.unwrap_err()] {
            let message = error.to_string();
            assert!(message.contains("Participant 'subject'"), "{message}");
            assert!(message.contains("Windows"), "{message}");
            assert!(!format!("{error:?}").contains("secret"));
        }
        assert!(daemon.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_legacy_driver_shares_host_resources_and_assigns_ports_only_to_its_participant() {
        // The volume-permission helper shares the driver's configuration; host-port bindings belong to the participant.
        let daemon = FakeDaemon::start(
            |method, path| match (method, path.split('?').next().unwrap_or_default()) {
                ("GET", "/networks") => (200, r#"[{"Name": "airlock-ab12cd34"}]"#),
                ("POST", "/images/create") => (200, "{}"),
                ("GET", "/volumes") => (
                    200,
                    r#"{"Volumes": [{"Name": "airlock-ab12cd34", "Driver": "local", "Mountpoint": "/x",
                        "Labels": {}, "Options": {}, "Scope": "local"}]}"#,
                ),
                ("POST", "/containers/create") => (201, r#"{"Id": "c1", "Warnings": []}"#),
                ("POST", "/containers/airlock-ab12cd34-volume-fix-up/start")
                | ("POST", "/containers/airlock-ab12cd34-volume-fix-up/stop")
                | ("DELETE", "/containers/airlock-ab12cd34-volume-fix-up") => (204, ""),
                ("GET", "/containers/airlock-ab12cd34-volume-fix-up/json") => (200, r#"{"State":{"Status":"exited"}}"#),
                ("POST", "/containers/airlock-ab12cd34-volume-fix-up/wait") => (200, r#"{"StatusCode": 0}"#),
                _ => (500, r#"{"message": "unexpected request"}"#),
            },
        )
        .await;
        let driver = Driver {
            provisioning: Provisioning::OnCreate,
            ..Driver::in_group(
                daemon.docker.clone(),
                GROUP.to_string(),
                DriverConfig::from_image("target", "example:latest".to_string())
                    .with_published_port(Protocol::Tcp, 9000, 24100)
                    .with_exposed_port(Protocol::Udp, 9001),
            )
        };

        tokio::time::timeout(BOUND, driver.create())
            .await
            .expect("creation should finish within the bound")
            .expect("creation should succeed");

        let requests = daemon.requests.lock().unwrap();
        let bodies = daemon.bodies.lock().unwrap();
        let creates: Vec<_> = requests
            .iter()
            .zip(bodies.iter())
            .filter(|(request, _)| request.starts_with("POST /containers/create?"))
            .collect();
        assert_eq!(creates.len(), 2, "{requests:?}");
        let (fix_up_request, fix_up_body) = creates[0];
        assert!(fix_up_request.contains("name=airlock-ab12cd34-volume-fix-up"));
        assert!(!fix_up_body.contains("\"ExposedPorts\""), "{fix_up_body}");
        assert!(!fix_up_body.contains("\"PortBindings\""), "{fix_up_body}");
        let (participant_request, participant_body) = creates[1];
        assert!(participant_request.contains("name=airlock-ab12cd34-target"));
        for body in [fix_up_body, participant_body] {
            assert!(body.contains(r#""PidMode":"host""#), "{body}");
            assert!(
                body.contains(concat!(
                    r#""Binds":["airlock-ab12cd34:/airlock:z","#,
                    r#""/proc:/host/proc:ro","/sys/fs/cgroup:/host/sys/fs/cgroup:ro","#,
                    r#""/var/run/docker.sock:/var/run/docker.sock:ro"]"#,
                )),
                "{body}"
            );
        }
        for expected in [
            "\"ExposedPorts\"",
            "\"PortBindings\"",
            "\"9000/tcp\"",
            "\"9001/udp\"",
            "\"24100\"",
        ] {
            assert!(participant_body.contains(expected), "{participant_body}");
        }
    }

    #[tokio::test]
    async fn a_prepared_driver_answers_to_its_participant_name_and_a_legacy_driver_gains_no_alias() {
        let daemon = FakeDaemon::start(|_, _| (500, r#"{"message":"unexpected request"}"#)).await;
        let config = DriverConfig::from_image("alpha", "local:latest".to_string());
        for (provisioning, config, expected) in [
            (Provisioning::Prepared, config.clone(), vec!["alpha"]),
            (
                Provisioning::Prepared,
                config.clone().with_network_alias("extra"),
                vec!["alpha", "extra"],
            ),
            (
                Provisioning::Prepared,
                config.clone().with_network_alias("alpha"),
                vec!["alpha"],
            ),
            (
                Provisioning::OnCreate,
                config.clone().with_network_alias("extra"),
                vec!["extra"],
            ),
            (Provisioning::OnCreate, config, vec![]),
        ] {
            let driver = Driver {
                provisioning,
                ..Driver::in_group(daemon.docker.clone(), GROUP.to_string(), config)
            };
            assert_eq!(driver.network_aliases(), expected);
        }
        assert!(daemon.requests.lock().unwrap().is_empty());
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
