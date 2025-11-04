use std::path::{Path, PathBuf};

use airlock::config::{
    DatadogIntakeConfig as AirlockDatadogIntakeConfig, MillstoneConfig as AirlockMillstoneConfig, TargetConfig,
};
use saluki_config::ConfigurationLoader;
use saluki_error::{ErrorContext as _, GenericError};
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
    pub baseline: BaselineTargetConfig,

    /// Comparison target configuration.
    pub comparison: ComparisonTargetConfig,

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

    /// Path to the millstone configuration file to use for the baseline target.
    ///
    /// This file is mapped into the baseline target's `millstone` container and so it must exist on the system where
    /// this command is run from.
    pub baseline_config_path: PathBuf,

    /// Optional path to the millstone configuration file to use for the comparison target.
    ///
    /// This file is mapped into the comparison target's `millstone` container and so it must exist on the system where
    /// this command is run from.
    pub comparison_config_path: PathBuf,
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
pub struct BaselineTargetConfig {
    /// Container image to use for baseline target.
    ///
    /// This must be a valid image reference: `name:x.y.z`, `docker.io/datadog/name:x.y.z`, etc.
    pub image: String,

    /// Entrypoint for the baseline target container.
    #[serde(default = "Vec::new")]
    pub entrypoint: Vec<String>,

    /// Command to run in the container to start the baseline target.
    #[serde(default = "Vec::new")]
    pub command: Vec<String>,

    /// Path to the configuration file to supply to the baseline target.
    ///
    /// This must be a valid path to a file on the host system, which will then be mapped into the baseline target container
    /// at `/etc/target/<filename>`, where `<filename>` is the basename of the file on the host system.
    pub config_path: PathBuf,

    /// Additional environment variables to be passed into the baseline target container.
    ///
    /// These should be in the form of `KEY=VALUE`.
    #[serde(default = "Vec::new")]
    pub additional_env_vars: Vec<String>,
}

#[derive(Clone, Deserialize)]
pub struct ComparisonTargetConfig {
    /// Container image to use for comparison target.
    ///
    /// This must be a valid image reference: `name:x.y.z`, `docker.io/datadog/name:x.y.z`, etc.
    pub image: String,

    /// Entrypoint for the comparison target container.
    #[serde(default = "Vec::new")]
    pub entrypoint: Vec<String>,

    /// Command to run in the container to start the comparison target.
    #[serde(default = "Vec::new")]
    pub command: Vec<String>,

    /// Path to the configuration file to supply to the comparison target.
    ///
    /// This must be a valid path to a file on the host system, which will then be mapped into the comparison target container
    /// at `/etc/target/<filename>`, where `<filename>` is the basename of the file on the host system.
    pub config_path: PathBuf,

    /// Additional environment variables to be passed into the comparison target container.
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

    fn get_canonicalized_config_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_config_path.join(path)
        }
    }

    pub fn baseline_millstone_config(&self) -> AirlockMillstoneConfig {
        AirlockMillstoneConfig {
            image: self.millstone.image.clone(),
            binary_path: Some(self.millstone.binary_path.clone()),
            config_path: self.get_canonicalized_config_path(&self.millstone.baseline_config_path),
        }
    }

    pub fn comparison_millstone_config(&self) -> AirlockMillstoneConfig {
        AirlockMillstoneConfig {
            image: self.millstone.image.clone(),
            binary_path: Some(self.millstone.binary_path.clone()),
            config_path: self.get_canonicalized_config_path(&self.millstone.comparison_config_path),
        }
    }

    pub fn datadog_intake_config(&self) -> AirlockDatadogIntakeConfig {
        AirlockDatadogIntakeConfig {
            image: self.datadog_intake.image.clone(),
            binary_path: Some(self.datadog_intake.binary_path.clone()),
            config_path: self.get_canonicalized_config_path(&self.datadog_intake.config_path),
        }
    }

    pub fn baseline_target_config(&self) -> TargetConfig {
        TargetConfig {
            image: self.baseline.image.clone(),
            entrypoint: self.baseline.entrypoint.clone(),
            command: self.baseline.command.clone(),
            additional_env_vars: self.baseline.additional_env_vars.clone(),
        }
    }

    pub fn comparison_target_config(&self) -> TargetConfig {
        TargetConfig {
            image: self.comparison.image.clone(),
            entrypoint: self.comparison.entrypoint.clone(),
            command: self.comparison.command.clone(),
            additional_env_vars: self.comparison.additional_env_vars.clone(),
        }
    }

    pub fn baseline_target_config_path(&self) -> PathBuf {
        self.get_canonicalized_config_path(&self.baseline.config_path)
    }

    pub fn comparison_target_config_path(&self) -> PathBuf {
        self.get_canonicalized_config_path(&self.comparison.config_path)
    }
}
