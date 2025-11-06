use std::path::{Path, PathBuf};

use airlock::{
    config::{
        DatadogIntakeConfig as AirlockDatadogIntakeConfig, MillstoneConfig as AirlockMillstoneConfig,
        TargetConfig as AirlockTargetConfig,
    },
    driver::DriverConfig,
};
use saluki_config::ConfigurationLoader;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;

fn default_millstone_binary_path() -> String {
    "/usr/local/bin/millstone".to_string()
}

fn default_datadog_intake_binary_path() -> String {
    "/usr/local/bin/datadog-intake".to_string()
}

#[derive(Clone, Deserialize)]
pub struct Config {
    /// Millstone configuration.
    pub millstone: MillstoneConfig,

    /// Datadog intake configuration.
    pub datadog_intake: DatadogIntakeConfig,

    /// Baseline target configuration.
    pub baseline: TargetConfig,

    /// Comparison target configuration.
    pub comparison: TargetConfig,

    #[serde(skip, default = "PathBuf::new")]
    base_config_path: PathBuf,
}

#[derive(Clone, Deserialize)]
pub struct MillstoneConfig {
    /// Container image to use for millstone.
    ///
    /// This must be a valid image reference: `millstone:x.y.z`, `registry.ddbuild.io/saluki/millstone:x.y.z`, etc.
    pub image: String,

    /// Path to the millstone binary.
    ///
    /// Defaults to `/usr/local/bin/millstone`.
    #[serde(default = "default_millstone_binary_path")]
    pub binary_path: String,

    /// Path to the millstone configuration file to use.
    ///
    /// This file is mapped into the baseline target's `millstone` container and so it must exist on the system where
    /// this command is run from.
    pub config_path: PathBuf,
}

#[derive(Clone, Deserialize)]
pub struct DatadogIntakeConfig {
    /// Container image to use for datadog-intake.
    ///
    /// This must be a valid image reference: `datadog-intake:x.y.z`, `registry.ddbuild.io/saluki/datadog-intake:x.y.z`, etc.
    pub image: String,

    /// Path to the datadog-intake binary.
    ///
    /// Defaults to `/usr/local/bin/datadog-intake`.
    #[serde(default = "default_datadog_intake_binary_path")]
    pub binary_path: String,

    /// Path to the datadog-intake configuration file to use.
    ///
    /// This must be a valid path to a file on the host system.
    pub config_path: PathBuf,
}

#[derive(Clone, Deserialize)]
pub struct TargetConfig {
    /// Container image to use for target.
    ///
    /// This must be a valid image reference: `name:x.y.z`, `docker.io/datadog/name:x.y.z`, etc.
    pub image: String,

    /// Entrypoint for the target container.
    #[serde(default = "Vec::new")]
    pub entrypoint: Vec<String>,

    /// Command to run in the container to start the target.
    #[serde(default = "Vec::new")]
    pub command: Vec<String>,

    /// Files to be mapped into the target container.
    ///
    /// Entries must be in the form of `host_path:container_path`.
    #[serde(default = "Vec::new")]
    pub files: Vec<String>,

    /// Additional environment variables to be passed into the target container.
    ///
    /// These should be in the form of `KEY=VALUE`.
    #[serde(default = "Vec::new")]
    pub additional_env_vars: Vec<String>,
}

impl Config {
    pub fn from_yaml(config_path: &str) -> Result<Self, GenericError> {
        let config_path = PathBuf::from(config_path)
            .canonicalize()
            .error_context("Failed to canonicalize configuration file path.")?;

        // We load the configuration file from the given path, and also environment variables, and then deserialize.
        let mut config = ConfigurationLoader::default()
            .from_yaml(&config_path)
            .error_context("Failed to load configuration file.")?
            .from_environment("GROUND_TRUTH")
            .expect("Environment variable prefix should not be empty.")
            .into_typed::<Config>()
            .error_context("Failed to deserialize configuration file.")?;

        // Now that we've deserialized things, calculate the base path of the configuration file we loaded, which we
        // then use as the base path for any configuration fields which also specify paths to files. We only use the
        // base path if those paths aren't already absolute.
        config.base_config_path = config_path
            .parent()
            .expect("Configuration file path must be an absolute file path.")
            .to_path_buf();

        Ok(config)
    }

    fn get_canonicalized_config_path<P: AsRef<Path>>(&self, path: P) -> PathBuf {
        let path = path.as_ref();
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_config_path.join(path)
        }
    }

    pub fn millstone_config(&self) -> AirlockMillstoneConfig {
        AirlockMillstoneConfig {
            image: self.millstone.image.clone(),
            binary_path: Some(self.millstone.binary_path.clone()),
            config_path: self.get_canonicalized_config_path(&self.millstone.config_path),
        }
    }

    pub fn datadog_intake_config(&self) -> AirlockDatadogIntakeConfig {
        AirlockDatadogIntakeConfig {
            image: self.datadog_intake.image.clone(),
            binary_path: Some(self.datadog_intake.binary_path.clone()),
            config_path: self.get_canonicalized_config_path(&self.datadog_intake.config_path),
        }
    }

    async fn target_driver_config(&self, target_config: &TargetConfig) -> Result<DriverConfig, GenericError> {
        let airlock_target_config = AirlockTargetConfig {
            image: target_config.image.clone(),
            entrypoint: target_config.entrypoint.clone(),
            command: target_config.command.clone(),
            additional_env_vars: target_config.additional_env_vars.clone(),
        };

        let mut driver_config = DriverConfig::target("target", airlock_target_config).await?;

        for file in &target_config.files {
            // Parse the two file paths -- host path and container path -- from the entry,
            // and canonicalize the host path. The container path must be absolute.
            match file.split_once(':') {
                Some((host_path, container_path)) => {
                    let host_path = self.get_canonicalized_config_path(host_path);
                    let container_path = Path::new(container_path);
                    if !container_path.is_absolute() {
                        return Err(generic_error!(
                            "Container path '{}' must be absolute.",
                            container_path.display()
                        ));
                    }

                    driver_config = driver_config.with_bind_mount(host_path, container_path)
                }
                None => {
                    return Err(generic_error!(
                        "Invalid file entry format (expected 'host_path:container_path', got '{}')",
                        file,
                    ))
                }
            };
        }

        Ok(driver_config)
    }

    pub async fn baseline_target_driver_config(&self) -> Result<DriverConfig, GenericError> {
        self.target_driver_config(&self.baseline).await
    }

    pub async fn comparison_target_driver_config(&self) -> Result<DriverConfig, GenericError> {
        self.target_driver_config(&self.comparison).await
    }
}
