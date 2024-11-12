use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use bollard::{
    container::{Config, CreateContainerOptions, ListContainersOptions, LogOutput, LogsOptions},
    errors::Error,
    image::CreateImageOptions,
    models::{HealthConfig, HealthStatusEnum, HostConfig, Ipam},
    network::CreateNetworkOptions,
    volume::CreateVolumeOptions,
    Docker,
};
use futures::StreamExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{
    io::{AsyncWriteExt as _, BufWriter},
    time::sleep,
};
use tracing::{debug, error, trace};

use crate::config::{ADPConfig, DSDConfig, MetricsIntakeConfig, MillstoneConfig};

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

/// Driver configuration.
///
/// This is the basic set of configuration options needed to spawn the container for a given driver.
pub struct DriverConfig {
    driver_id: &'static str,
    image: String,
    entrypoint: Option<Vec<String>>,
    command: Option<Vec<String>>,
    env: Vec<String>,
    binds: Vec<String>,
    healthcheck: Option<HealthConfig>,
    exposed_ports: Vec<(&'static str, u16)>,
}

impl DriverConfig {
    pub async fn millstone(config: MillstoneConfig) -> Result<Self, GenericError> {
        // Ensure the given configuration file path actually exists.
        if let Err(e) = tokio::fs::try_exists(&config.config_path).await {
            return Err(generic_error!(
                "Failed to ensure specified millstone configuration exists locally: {}",
                e
            ));
        }

        let entrypoint = vec![config.binary_path, "/etc/millstone/config.yaml".to_string()];

        let driver_config = Self::from_image("millstone", config.image)
            .with_entrypoint(entrypoint)
            .with_bind_mount(config.config_path, "/etc/millstone/config.yaml");

        Ok(driver_config)
    }

    pub async fn metrics_intake(config: MetricsIntakeConfig) -> Result<Self, GenericError> {
        // Ensure the given configuration file path actually exists.
        if let Err(e) = tokio::fs::try_exists(&config.config_path).await {
            return Err(generic_error!(
                "Failed to ensure specified metrics-intake configuration exists locally: {}",
                e
            ));
        }

        let entrypoint = vec![config.binary_path, "/etc/metrics-intake/config.yaml".to_string()];

        let driver_config = DriverConfig::from_image("metrics-intake", config.image)
            .with_entrypoint(entrypoint)
            .with_bind_mount(config.config_path, "/etc/metrics-intake/config.yaml")
            // Map our intake port to an ephemeral port on the host side, which we'll query once the container has been
            // started so that we can connect to it.
            .with_exposed_port("tcp", 2049);

        Ok(driver_config)
    }

    pub async fn dogstatsd(config: DSDConfig) -> Result<Self, GenericError> {
        // Ensure the given configuration file path actually exists.
        if let Err(e) = tokio::fs::try_exists(&config.config_path).await {
            return Err(generic_error!(
                "Failed to ensure specified DogStatsD configuration exists locally: {}",
                e
            ));
        }

        // We run the regular entrypoint bundled in the DogStatsD container because it contains the necessary logic to
        // ensure the DogStatsD binary is executable, and we simply put the things we care about the `command` field,
        // which the `entrypoint.sh` script will dutifully pass through to `exec`.
        let entrypoint = vec!["/entrypoint.sh".to_string()];
        let command = vec![
            config.binary_path,
            "start".to_string(),
            "--cfgpath".to_string(),
            "/etc/datadog-agent".to_string(),
        ];

        let driver_config = DriverConfig::from_image("dogstatsd", config.image)
            .with_entrypoint(entrypoint)
            .with_command(command)
            .with_bind_mount(config.config_path, "/etc/datadog-agent/dogstatsd.yaml")
            // We override the default health check baked into the image, which is egregiously long in my opinion. It
            // has an interval of 60 seconds, with no startup allowance... which means it takes a full minute before the
            // first healthcheck is even triggered, even if the Agent is healthy long before that. Very dumb.
            //
            // We're specifying a startup period here so that we rapidly check the Agent's health (once a second) during
            // the "startup" period (20 seconds), which should lead to detecting the Agent becoming healthy almost as
            // soon as that transition happens.
            .with_healthcheck(
                vec!["/probe.sh".to_string()],
                Duration::from_secs(60),
                Duration::from_secs(5),
                2,
                Duration::from_secs(20),
                Duration::from_secs(1),
            )
            .with_env_vars(config.additional_env_args);

        Ok(driver_config)
    }

    pub async fn agent_data_plane(config: ADPConfig) -> Result<Self, GenericError> {
        // Ensure the given configuration file path actually exists.
        if let Err(e) = tokio::fs::try_exists(&config.config_path).await {
            return Err(generic_error!(
                "Failed to ensure specified ADP configuration exists locally: {}",
                e
            ));
        }

        let entrypoint = vec![config.binary_path, "/etc/datadog-agent/datadog.yaml".to_string()];

        let driver_config = DriverConfig::from_image("agent-data-plane", config.image)
            .with_entrypoint(entrypoint)
            .with_bind_mount(config.config_path, "/etc/datadog-agent/datadog.yaml")
            .with_env_vars(config.additional_env_args);

        Ok(driver_config)
    }

    /// Creates a new `DriverConfig` from the given driver identifier and container image reference.
    fn from_image(driver_id: &'static str, image: String) -> Self {
        Self {
            driver_id,
            image,
            entrypoint: None,
            command: None,
            env: vec![],
            binds: vec![],
            healthcheck: None,
            exposed_ports: vec![],
        }
    }

    /// Sets the entrypoint for the container.
    pub fn with_entrypoint(mut self, entrypoint: Vec<String>) -> Self {
        self.entrypoint = Some(entrypoint);
        self
    }

    /// Sets the command for the container.
    pub fn with_command(mut self, command: Vec<String>) -> Self {
        self.command = Some(command);
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

    /// Adds an exposed port to the container.
    ///
    /// Exposed ports are ports mapped from inside the container to an ephemeral port on the host. The `protocol` should
    /// be either `tcp` or `udp`. A port on the host side is picked from the "local" port range. For example, on Linux
    /// the range is defined by `/proc/sys/net/ipv4/ip_local_port_range`.
    ///
    /// When starting the driver via [`Driver::start`][crate::driver::Driver::start], the ephemeral port mappings will
    /// be returned in [`DriverDetails`][crate::driver::DriverDetails].
    pub fn with_exposed_port(mut self, protocol: &'static str, internal_port: u16) -> Self {
        self.exposed_ports.push((protocol, internal_port));
        self
    }
}

/// Detailed information about the spawned container.
#[derive(Debug, Default)]
pub struct DriverDetails {
    container_name: String,
    port_mappings: Option<HashMap<String, u16>>,
}

impl DriverDetails {
    /// Returns the name of the container.
    pub fn container_name(&self) -> &str {
        &self.container_name
    }

    /// Attempts to look up a mapped ephemeral port for the given exposed port.
    ///
    /// The same `protocol` and internal port values used to expose the port must be used here. If the given
    /// protocol/port combination was not exposed, `None` is returned. Otherwise, the mapped ephemeral port is returned.
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
    /// The container will have a volume bind-mounted at `/airlock` that is shared between all containers in the same
    /// isolation group. This volume is mounted as world writeable (777) so all containers can freely read and write to
    /// it. This makes it easier for containers to share data between one another, but also means that care should be
    /// taken to avoid conflicts between trying to write to the same file, etc.
    ///
    /// # Errors
    ///
    /// If the Docker client cannot be created/configured, an error will be returned.
    pub fn from_config(isolation_group_id: String, config: DriverConfig) -> Result<Self, GenericError> {
        let docker = Docker::connect_with_defaults()?;

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
    /// If the Docker client cannot be created/configured, or there is an error when finding or removing any of the
    /// related resources, an error will be returned.
    pub async fn clean_related_resources(isolation_group_id: String) -> Result<(), GenericError> {
        let docker = Docker::connect_with_defaults()?;

        let isolation_group_name = format!("airlock-{}", isolation_group_id);
        let isolation_group_label = format!("airlock-isolation-group={}", isolation_group_id);

        // Remove any containers related to the isolation group. We do so forcefully.
        let list_options = Some(ListContainersOptions {
            all: true,
            filters: vec![("label", vec!["created_by=airlock", isolation_group_label.as_str()])]
                .into_iter()
                .collect(),
            ..Default::default()
        });
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
        if let Err(e) = docker.remove_volume(isolation_group_name.as_str(), None).await {
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
        let networks = self.docker.list_networks::<String>(None).await?;
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
        let network_options = CreateNetworkOptions {
            name: self.isolation_group_name.clone(),
            check_duplicate: true,
            driver: "bridge".to_string(),
            ipam: Ipam::default(),
            enable_ipv6: false,
            labels: get_default_airlock_labels(self.isolation_group_id.as_str()),
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
            from_image: image,
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
        let volumes = self.docker.list_volumes::<String>(None).await?;
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

        let volume_options = CreateVolumeOptions {
            name: self.isolation_group_name.clone(),
            driver: "local".to_string(),
            labels: get_default_airlock_labels(self.isolation_group_id.as_str()),
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
        let image = "alpine:3.20".to_string();
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
        mut binds: Vec<String>, env: Option<Vec<String>>,
    ) -> Result<String, GenericError> {
        // Take the configured binds for the container and add in our shared volume that all containers in this
        // isolation group will use.
        binds.push(format!("{}:/airlock:z", self.isolation_group_name));

        let (publish_all_ports, exposed_ports) = if self.config.exposed_ports.is_empty() {
            (None, None)
        } else {
            let mut exposed_ports = HashMap::new();
            for (protocol, internal_port) in &self.config.exposed_ports {
                exposed_ports.insert(format!("{}/{}", internal_port, protocol), HashMap::new());
            }

            (Some(true), Some(exposed_ports))
        };

        let container_config = Config {
            hostname: Some(self.config.driver_id.to_string()),
            env,
            image: Some(image),
            entrypoint,
            cmd,
            host_config: Some(HostConfig {
                binds: Some(binds),
                network_mode: Some(self.isolation_group_name.clone()),
                publish_all_ports,
                ..Default::default()
            }),
            healthcheck: self.config.healthcheck.clone(),
            exposed_ports,
            labels: Some(get_default_airlock_labels(self.isolation_group_id.as_str())),
            ..Default::default()
        };

        let create_options = CreateContainerOptions {
            name: container_name,
            ..Default::default()
        };

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
        self.docker.start_container::<String>(container_name, None).await?;

        let mut details = DriverDetails {
            container_name: container_name.to_string(),
            ..Default::default()
        };

        let response = self.docker.inspect_container(container_name, None).await?;
        if let Some(network_settings) = response.network_settings {
            if let Some(ports) = network_settings.ports {
                let port_mappings = details.port_mappings.get_or_insert_with(HashMap::new);
                for (internal_port, bindings) in ports {
                    if let Some(bindings) = bindings {
                        for binding in bindings {
                            if let Some(host_ip) = binding.host_ip.as_deref() {
                                if host_ip == "0.0.0.0" {
                                    let maybe_host_port =
                                        binding.host_port.as_ref().and_then(|value| value.parse::<u16>().ok());

                                    if let Some(host_port) = maybe_host_port {
                                        port_mappings.insert(internal_port, host_port);
                                        break;
                                    }
                                }
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
        self.adjust_shared_volume_permissions().await?;

        self.create_container().await?;
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
            if let Some(health_status) = response.state.and_then(|s| s.health).and_then(|h| h.status) {
                match health_status {
                    // No healthcheck defined, or healthy, so we're good to go.
                    HealthStatusEnum::EMPTY | HealthStatusEnum::NONE | HealthStatusEnum::HEALTHY => {
                        debug!(
                            driver_id = self.config.driver_id,
                            isolation_group = self.isolation_group_id,
                            "Container '{}' healthy or no healthcheck defined. Proceeding.",
                            &self.container_name
                        );
                        return Ok(());
                    }

                    // Not healthy yet, so we'll keep waiting.
                    HealthStatusEnum::STARTING | HealthStatusEnum::UNHEALTHY => {
                        debug!(
                            driver_id = self.config.driver_id,
                            isolation_group = self.isolation_group_id,
                            "Container '{}' not yet healthy. Waiting...",
                            &self.container_name
                        );
                    }
                }
            } else {
                debug!(
                    driver_id = self.config.driver_id,
                    isolation_group = self.isolation_group_id,
                    "Container '{}' has no healthcheck defined. Proceeding.",
                    &self.container_name
                );
                return Ok(());
            }

            // Wait for a second and then check again.
            sleep(Duration::from_secs(1)).await;
        }
    }

    async fn wait_for_container_exit_inner(&self, container_name: &str) -> Result<ExitStatus, GenericError> {
        let mut wait_stream = self.docker.wait_container::<String>(container_name, None);
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
        let mut log_stream = self.docker.logs::<String>(container_name, Some(logs_config));

        tokio::spawn(async move {
            while let Some(log_result) = log_stream.next().await {
                match log_result {
                    Ok(log) => match log {
                        LogOutput::StdErr { message } => {
                            if let Err(e) = stderr_file.write_all(&message[..]).await {
                                error!(error = %e, "Failed to write log line to standard error log file.");
                                break;
                            }
                            if let Err(e) = stderr_file.flush().await {
                                error!(error = %e, "Failed to flush standard error log file.");
                                break;
                            }
                        }
                        LogOutput::StdOut { message } => {
                            if let Err(e) = stdout_file.write_all(&message[..]).await {
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

fn get_default_airlock_labels(isolation_group_id: &str) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    labels.insert("created_by".to_string(), "airlock".to_string());
    labels.insert("airlock-isolation-group".to_string(), isolation_group_id.to_string());
    labels
}
