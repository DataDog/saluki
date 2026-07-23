use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use bollard::{
    container::{LogOutput, NetworkingConfig as ContainerNetworkingConfig},
    errors::Error,
    exec::{CreateExecOptions, StartExecResults},
    models::{
        ContainerCreateBody, ContainerStateStatusEnum, EndpointSettings, HealthConfig, HealthStatusEnum, HostConfig,
        Ipam, NetworkConnectRequest, NetworkCreateRequest, VolumeCreateRequest,
    },
    query_parameters::{CreateContainerOptionsBuilder, CreateImageOptions, ListContainersOptionsBuilder, LogsOptions},
    Docker,
};
use futures::{StreamExt as _, TryStreamExt as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{
    io::{AsyncWriteExt as _, BufWriter},
    time::sleep,
};
use tracing::{debug, error, trace};

use crate::config::{DatadogIntakeConfig, MillstoneConfig, TargetConfig};

const MILLSTONE_CONFIG_PATH_INTERNAL: &str = "/etc/millstone/config.toml";
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

/// Driver configuration.
///
/// This is the basic set of configuration options needed to spawn the container for a given driver.
#[derive(Clone)]
pub struct DriverConfig {
    driver_id: &'static str,
    image: String,
    entrypoint: Option<Vec<String>>,
    command: Option<Vec<String>>,
    env: Vec<String>,
    binds: Vec<String>,
    healthcheck: Option<HealthConfig>,
    exposed_ports: Vec<(&'static str, u16)>,
    container_os: ContainerOs,
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
            // Map our intake port to an ephemeral port on the host side, which we'll query once the container has been
            // started so that we can connect to it.
            .with_exposed_port("tcp", 2049);

        Ok(driver_config)
    }

    pub async fn target(target_id: &'static str, config: TargetConfig) -> Result<Self, GenericError> {
        let driver_config = DriverConfig::from_image(target_id, config.image)
            .with_entrypoint(config.entrypoint)
            .with_command(config.command)
            .with_env_vars(config.additional_env_vars)
            .with_container_os(config.container_os);

        Ok(driver_config)
    }

    /// Creates a new `DriverConfig` from the given driver identifier and container image reference.
    pub fn from_image(driver_id: &'static str, image: String) -> Self {
        Self {
            driver_id,
            image,
            entrypoint: None,
            command: None,
            env: vec![],
            binds: vec![],
            healthcheck: None,
            exposed_ports: vec![],
            container_os: ContainerOs::Linux,
            additional_volume_mounts: vec![],
            network_aliases: vec![],
            additional_networks: vec![],
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
        let host_path = docker_bind_mount_host_path(host_path.as_ref());
        let bind_mount = format!("{}:{}", host_path, container_path.as_ref().display());
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
    /// Other containers on the same network can resolve this container by `alias` in addition to
    /// its hostname. Call this before the container is started.
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
    /// The `protocol` should be either `tcp` or `udp`. Linux containers publish exposed ports to ephemeral host ports,
    /// which are returned in [`DriverDetails`] after starting the driver. Windows containers keep exposed ports internal
    /// to the container network because Panoramic probes them from inside the container or via the container IP.
    pub fn with_exposed_port(mut self, protocol: &'static str, internal_port: u16) -> Self {
        self.exposed_ports.push((protocol, internal_port));
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

    fn port_publishing_options(&self) -> (Option<bool>, Option<Vec<String>>) {
        if self.exposed_ports.is_empty() {
            return (None, None);
        }

        let exposed_ports = self
            .exposed_ports
            .iter()
            .map(|(protocol, internal_port)| format!("{}/{}", internal_port, protocol))
            .collect();
        let publish_all_ports = match self.container_os {
            ContainerOs::Linux => Some(true),
            ContainerOs::Windows => None,
        };

        (publish_all_ports, Some(exposed_ports))
    }

    /// Returns the full set of bind mounts to apply to this container, including OS-specific
    /// defaults and any additional named volume mounts.
    ///
    /// Linux containers receive the shared `/airlock` volume plus read-only mounts of host
    /// paths needed for origin detection (`/proc`, `/sys/fs/cgroup`, the Docker socket).
    /// Windows containers receive only the shared `C:\airlock` volume; the host-resource
    /// mounts have no Windows-container equivalent and the `:z` shared-relabel mount option is
    /// Linux-specific.
    fn container_binds_from(&self, isolation_group_name: &str, mut binds: Vec<String>) -> Vec<String> {
        match self.container_os {
            ContainerOs::Linux => {
                binds.push(format!("{}:/airlock:z", isolation_group_name));
                binds.push("/proc:/host/proc:ro".to_string());
                binds.push("/sys/fs/cgroup:/host/sys/fs/cgroup:ro".to_string());
                binds.push("/var/run/docker.sock:/var/run/docker.sock:ro".to_string());
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

    /// Attempts to look up a mapped ephemeral port for the given exposed port.
    ///
    /// The same `protocol` and internal port values used to expose the port must be used here. If the given
    /// protocol/port combination wasn't exposed, `None` is returned. Otherwise, the mapped ephemeral port is returned.
    /// This port is exposed on `0.0.0.0` on the host side.
    pub fn try_get_exposed_port(&self, protocol: &str, internal_port: u16) -> Option<u16> {
        self.port_mappings
            .as_ref()
            .and_then(|port_mappings| port_mappings.get(&format!("{}/{}", internal_port, protocol)).copied())
    }
}

/// Container driver.
pub struct Driver {
    isolation_group_id: String,
    isolation_group_name: String,
    container_name: String,
    config: DriverConfig,
    docker: Docker,
    log_dir: Option<PathBuf>,
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
            isolation_group_name: format!("airlock-{}", isolation_group_id),
            container_name: format!("airlock-{}-{}", isolation_group_id, config.driver_id),
            isolation_group_id,
            config,
            docker,
            log_dir: None,
        })
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
    /// This is generally a shorthand of the application/service, such as `dogstatsd` or `millstone`.
    pub fn driver_id(&self) -> &'static str {
        self.config.driver_id
    }

    /// Clean up any containers, networks, and volumes related to the given isolation group ID.
    ///
    /// This is a free function to facilitate cleaning up resources after a number of drivers are run.
    ///
    /// # Errors
    ///
    /// If the Docker client can't be created/configured, or there is an error when finding or removing any of the
    /// related resources, an error will be returned.
    pub async fn clean_related_resources(isolation_group_id: String) -> Result<(), GenericError> {
        let docker = crate::docker::connect()?;

        let isolation_group_name = format!("airlock-{}", isolation_group_id);
        let isolation_group_label = format!("airlock-isolation-group={}", isolation_group_id);

        // Remove any containers related to the isolation group. We do so forcefully.
        let list_filters: HashMap<&str, Vec<&str>> =
            [("label", vec!["created_by=airlock", isolation_group_label.as_str()])]
                .into_iter()
                .collect();
        let list_options = Some(
            ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&list_filters)
                .build(),
        );
        let containers = docker.list_containers(list_options).await.with_error_context(|| {
            format!(
                "Failed to list containers attached to isolation group '{}'.",
                isolation_group_id
            )
        })?;

        for container in containers {
            let container_name = match container.id {
                Some(id) => id,
                None => {
                    debug!("Listed container had no ID. Skipping removal.");
                    continue;
                }
            };

            if let Err(e) = docker.stop_container(container_name.as_str(), None).await {
                error!(error = %e, "Failed to stop container '{}'.", container_name);
                continue;
            } else {
                debug!("Stopped container '{}'.", container_name);
            }

            if let Err(e) = docker.remove_container(container_name.as_str(), None).await {
                error!(error = %e, "Failed to remove container '{}'.", container_name);
                continue;
            } else {
                debug!("Removed container '{}'.", container_name);
            }
        }

        // Remove the shared volume.
        if let Err(e) = docker
            .remove_volume(
                isolation_group_name.as_str(),
                None::<bollard::query_parameters::RemoveVolumeOptions>,
            )
            .await
        {
            error!(error = %e, "Failed to remove shared volume '{}'.", isolation_group_name);
        } else {
            debug!("Removed shared volume '{}'.", isolation_group_name);
        }

        // Remove the network.
        if let Err(e) = docker.remove_network(isolation_group_name.as_str()).await {
            error!(error = %e, "Failed to remove shared network '{}'.", isolation_group_name);
        } else {
            debug!("Removed shared network '{}'.", isolation_group_name);
        }

        Ok(())
    }

    async fn create_network_if_missing(&self) -> Result<(), GenericError> {
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
        let image = get_alpine_container_image();
        self.create_image_if_missing_inner(&image).await?;

        let container_name = format!("airlock-{}-volume-fix-up", self.isolation_group_id);
        let entrypoint = vec![
            "chmod".to_string(),
            "-R".to_string(),
            "777".to_string(),
            "/airlock".to_string(),
        ];
        let _ = self
            .create_container_inner(container_name.clone(), image, Some(entrypoint), None, vec![], None)
            .await?;

        self.start_container_inner(&container_name).await?;
        self.wait_for_container_exit_inner(&container_name).await?;
        self.cleanup_inner(&container_name).await?;

        Ok(())
    }

    async fn create_container_inner(
        &self, container_name: String, image: String, entrypoint: Option<Vec<String>>, cmd: Option<Vec<String>>,
        binds: Vec<String>, env: Option<Vec<String>>,
    ) -> Result<String, GenericError> {
        let binds = self.config.container_binds_from(&self.isolation_group_name, binds);

        // Set up NetworkingConfig to apply aliases on the primary network, if any are configured.
        let networking_config = if !self.config.network_aliases.is_empty() {
            let mut endpoints = HashMap::new();
            endpoints.insert(
                self.isolation_group_name.clone(),
                EndpointSettings {
                    aliases: Some(self.config.network_aliases.clone()),
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

        let (publish_all_ports, exposed_ports) = self.config.port_publishing_options();

        // Linux test containers run with `pid_mode=host` so origin-detection logic in ADP and
        // the Core Agent can see processes on the runner. Windows containers do not support
        // host PID mode, so we leave it unset and accept that Windows-runtime tests don't
        // exercise the host-pid origin-detection path.
        let pid_mode = match self.config.container_os {
            ContainerOs::Linux => Some("host".to_string()),
            ContainerOs::Windows => None,
        };

        let container_config = ContainerCreateBody {
            hostname: Some(self.config.driver_id.to_string()),
            env,
            image: Some(image),
            entrypoint,
            cmd,
            host_config: Some(HostConfig {
                binds: Some(binds),
                network_mode: Some(self.isolation_group_name.clone()),
                publish_all_ports,
                pid_mode,
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

    async fn create_container(&self) -> Result<(), GenericError> {
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
            )
            .await?;

        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Created container '{}' (ID: {}).",
            self.container_name,
            container_id
        );

        Ok(())
    }

    async fn start_container_inner(&self, container_name: &str) -> Result<DriverDetails, GenericError> {
        self.docker.start_container(container_name, None).await?;

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

    async fn start_container(&self) -> Result<DriverDetails, GenericError> {
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

            self.capture_container_logs(log_dir, self.config.driver_id, &self.container_name)
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

    /// Starts the container, creating any necessary resources.
    ///
    /// # Errors
    ///
    /// If there is an error while creating the network or shared volume, while pulling the container image, or while
    /// creating or starting the container, it will be returned.
    pub async fn start(&mut self) -> Result<DriverDetails, GenericError> {
        self.create_network_if_missing().await?;
        self.create_image_if_missing().await?;
        self.create_volume_if_missing().await?;
        if self.config.needs_shared_volume_permission_fixup() {
            self.adjust_shared_volume_permissions().await?;
        }

        self.create_container().await?;
        self.connect_to_additional_networks().await?;
        self.start_container().await
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

    async fn wait_for_container_exit_inner(&self, container_name: &str) -> Result<ExitStatus, GenericError> {
        let mut wait_stream = self.docker.wait_container(container_name, None);
        match wait_stream.next().await {
            Some(result) => match result {
                Ok(response) => {
                    // When the exit code is non-zero, `bollard` transforms the normal `ContainerWaitResponse` into
                    // `Error::DockerContainerWaitError`, which is why we have these asserts here to catch any scenario
                    // where there's _somehow_ an error condition being indicated without it having been transformed into
                    // `Error::DockerContainerWaitError`.
                    //
                    // Essentially, getting to this point should imply successfully exiting, but the API isn't very
                    // ergonomic in that regard, so we're just making sure.
                    assert_eq!(response.error, None);
                    assert_eq!(response.status_code, 0);

                    Ok(ExitStatus::Success)
                }

                Err(Error::DockerContainerWaitError { error, code }) => {
                    let error = if error.is_empty() {
                        String::from("<no error message provided>")
                    } else {
                        error
                    };
                    Ok(ExitStatus::Failed { code, error })
                }

                Err(e) => Err(generic_error!("Failed to wait for container to finish: {:?}", e)),
            },
            None => unreachable!("Docker wait stream ended unexpectedly."),
        }
    }

    /// Waits for the container to finish successfully.
    ///
    /// The container's exit status is returned, indicating success (exit code 0) or failure (exit code != 0), including
    /// any error message related to the failure.
    ///
    /// # Errors
    ///
    /// If an error is encountered while waiting for the container to exit, it will be returned.
    pub async fn wait_for_container_exit(&self) -> Result<ExitStatus, GenericError> {
        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Waiting for container '{}' to finish...",
            &self.container_name
        );

        let exit_status = self.wait_for_container_exit_inner(&self.container_name).await?;

        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Container '{}' finished successfully.",
            &self.container_name
        );

        Ok(exit_status)
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

    /// Cleans up the container, stopping and removing it from the system.
    ///
    /// # Errors
    ///
    /// If there is an error while stopping or removing the container, it will be returned.
    pub async fn cleanup(self) -> Result<(), GenericError> {
        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Cleaning up container '{}'...",
            self.container_name
        );

        let start = Instant::now();

        self.cleanup_inner(&self.container_name).await?;

        debug!(
            driver_id = self.config.driver_id,
            isolation_group = self.isolation_group_id,
            "Container '{}' removed after {:?}.",
            self.container_name,
            start.elapsed()
        );

        Ok(())
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

fn docker_bind_mount_host_path(path: &Path) -> String {
    // Windows canonicalization uses the verbatim namespace, which the Docker bind-mount API does not accept.
    let path = path.to_string_lossy();
    if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{}", path)
    } else if let Some(path) = path.strip_prefix(r"\\?\") {
        path.to_string()
    } else {
        path.into_owned()
    }
}

fn get_alpine_container_image() -> String {
    // Normally, we would just use `alpine:latest` and let Docker figure out the registry to pull it from (that is, Docker
    // Hub) but in CI, we don't have Docker Hub available to us, so we need to use an internal registry.
    //
    // Rather than threading through this information from the top level, we simply look for an override environment
    // variable here.. which lets us specify the right image reference to use in CI, while allowing normal users to just
    // grab it from Docker Hub when running locally.
    std::env::var("PANORAMIC_ALPINE_IMAGE").unwrap_or_else(|_| "alpine:latest".to_string())
}

fn get_default_airlock_labels(isolation_group_id: &str) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    labels.insert("created_by".to_string(), "airlock".to_string());
    labels.insert("airlock-isolation-group".to_string(), isolation_group_id.to_string());
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_mount_removes_windows_verbatim_prefix_from_host_path() {
        let config = DriverConfig::from_image("target", "example:latest".to_string())
            .with_bind_mount(r"\\?\C:\mnt\fixture.ps1", r"C:\adp\fixture.ps1");

        assert_eq!(config.binds, vec![r"C:\mnt\fixture.ps1:C:\adp\fixture.ps1"]);
    }

    #[test]
    fn bind_mount_converts_windows_verbatim_unc_host_path() {
        let config = DriverConfig::from_image("target", "example:latest".to_string())
            .with_bind_mount(r"\\?\UNC\server\fixtures\fixture.ps1", r"C:\adp\fixture.ps1");

        assert_eq!(config.binds, vec![r"\\server\fixtures\fixture.ps1:C:\adp\fixture.ps1"]);
    }

    #[test]
    fn default_linux_container_binds_include_airlock_and_linux_host_resources() {
        let config = DriverConfig::from_image("target", "example:latest".to_string());

        let binds = config.container_binds_from("airlock-test", config.binds.clone());

        assert!(binds.contains(&"airlock-test:/airlock:z".to_string()));
        assert!(binds.contains(&"/proc:/host/proc:ro".to_string()));
        assert!(binds.contains(&"/sys/fs/cgroup:/host/sys/fs/cgroup:ro".to_string()));
        assert!(binds.contains(&"/var/run/docker.sock:/var/run/docker.sock:ro".to_string()));
    }

    #[test]
    fn windows_container_binds_use_windows_airlock_and_skip_linux_host_resources() {
        let config =
            DriverConfig::from_image("target", "example:latest".to_string()).with_container_os(ContainerOs::Windows);

        let binds = config.container_binds_from("airlock-test", config.binds.clone());

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
        };

        let config = DriverConfig::target("target", target).await.unwrap();

        assert_eq!(config.container_os, ContainerOs::Windows);
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
    fn windows_container_exposes_ports_without_publishing_to_host() {
        let config = DriverConfig::from_image("target", "example:latest".to_string())
            .with_container_os(ContainerOs::Windows)
            .with_exposed_port("udp", 58125);

        let (publish_all_ports, exposed_ports) = config.port_publishing_options();

        assert_eq!(publish_all_ports, None);
        assert_eq!(exposed_ports, Some(vec!["58125/udp".to_string()]));
    }

    #[test]
    fn linux_container_exposes_ports_and_publishes_to_host() {
        let config = DriverConfig::from_image("target", "example:latest".to_string()).with_exposed_port("tcp", 55100);

        let (publish_all_ports, exposed_ports) = config.port_publishing_options();

        assert_eq!(publish_all_ports, Some(true));
        assert_eq!(exposed_ports, Some(vec!["55100/tcp".to_string()]));
    }

    #[test]
    fn linux_container_uses_bridge_network_driver() {
        let config = DriverConfig::from_image("target", "example:latest".to_string());

        assert_eq!(config.network_driver(), "bridge");
    }
}
