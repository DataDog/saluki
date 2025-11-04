use std::path::PathBuf;

use airlock::config::{ADPConfig, DSDConfig, DatadogIntakeConfig, MillstoneConfig};
use clap::Parser;

#[derive(Clone, Parser)]
#[command(about)]
pub struct Cli {
    /// Container image to use for millstone.
    ///
    /// This must be a valid image reference -- `millstone:x.y.z`,
    /// `registry.ddbuild.io/saluki/millstone:x.y.z`, etc -- to an image containing the `millstone` binary.
    ///
    /// The `millstone` binary must exist at `/usr/local/bin/millstone` in the image. Otherwise, the binary
    /// path can be overridden with the `binary-path` argument.
    #[arg(long)]
    pub millstone_image: String,

    /// Path to the millstone binary.
    #[arg(long, default_value = "/usr/local/bin/millstone")]
    pub millstone_binary_path: String,

    /// Path to the millstone configuration file to use for the non ADP group.
    ///
    /// This file is mapped into the DSD group's millstone container and so it must exist on the system where this
    /// command is run from.
    #[arg(long)]
    pub dsd_millstone_config_path: PathBuf,

    /// Optional path to the millstone configuration file to use for the ADP group.
    ///
    /// If not specified, uses the same config as the non ADP group.
    ///
    /// This file is mapped into the ADP group's millstone container and so it must exist on the system where this
    /// command is run from.
    #[arg(long)]
    pub adp_millstone_config_path: Option<PathBuf>,

    /// Container image to use for datadog-intake.
    ///
    /// This must be a valid image reference -- `datadog-intake:x.y.z`,
    /// `registry.ddbuild.io/saluki/datadog-intake:x.y.z`, etc -- to an image containing the `datadog-intake` binary.
    ///
    /// The `datadog-intake` binary must exist at `/usr/local/bin/datadog-intake` in the image. Otherwise, the binary
    /// path can be overridden with the `binary-path` argument.
    #[arg(long)]
    pub datadog_intake_image: String,

    /// Path to the datadog-intake binary.
    #[arg(long, default_value = "/usr/local/bin/datadog-intake")]
    pub datadog_intake_binary_path: String,

    /// Path to the datadog-intake configuration file to use.
    ///
    /// This file is mapped into the datadog-intake container and so it must exist on the system where this command is run
    /// from.
    #[arg(long)]
    pub datadog_intake_config_path: PathBuf,

    /// Container image to use for DogStatsD.
    ///
    /// This must be a valid image reference -- `dogstatsd:x.y.z`, `docker.io/datadog/dogstatsd:x.y.z`, etc -- to an
    /// image containing the `dogstatsd` binary.
    ///
    /// The `dogstatsd` binary must exist at `/dogstatsd` in the image. Otherwise, the binary path can be overridden
    /// with the `binary-path` argument.
    #[arg(long)]
    pub dsd_image: String,

    /// Entrypoint for the DogStatsD container.
    #[arg(long, default_values_t = vec!["/entrypoint.sh".to_string()])]
    pub dsd_entrypoint: Vec<String>,

    /// Command to run in the container to start DogStatsD.
    #[arg(long, default_values_t = vec!["/dogstatsd".to_string(), "start".to_string(), "--cfgpath".to_string(), "/etc/datadog-agent".to_string()])]
    pub dsd_command: Vec<String>,

    /// Path to the DogStatsD configuration file to use.
    ///
    /// This file is mapped into the DogStatsD container and so it must exist on the system where this command is
    /// run from.
    #[arg(long)]
    pub dsd_config_path: PathBuf,

    /// Additional environment variables to be passed into the DogStatsD container.
    ///
    /// These should be in the form of `KEY=VALUE`.
    #[arg(long = "dsd-env-arg")]
    pub dsd_additional_env_args: Vec<String>,

    /// Container image to use for Agent Data Plane.
    ///
    /// This must be a valid image reference -- `agent-data-plane:x.y.z`,
    /// `registry.ddbuild.io/saluki/agent-data-plane:x.y.z`, etc -- to an image containing the `agent-data-plane`
    /// binary.
    ///
    /// The `agent-data-plane` binary must exist at `/usr/bin/agent-data-plane` in the image. Otherwise, the binary
    /// path can be overridden with the `binary-path` argument.
    #[arg(long)]
    pub adp_image: String,

    /// Entrypoint for the Agent Data Plane container.
    #[arg(long, default_values_t = vec!["/entrypoint.sh".to_string()])]
    pub adp_entrypoint: Vec<String>,

    /// Command to run in the container to start Agent Data Plane.
    #[arg(long, default_values_t = vec!["/usr/local/bin/agent-data-plane".to_string(), "run".to_string(), "--config".to_string(), "/etc/datadog-agent/datadog.yaml".to_string()])]
    pub adp_command: Vec<String>,

    /// Path to the Agent Data Plane configuration file to use.
    ///
    /// This file is mapped into the ADP container and so it must exist on the system where this command is
    /// run from.
    #[arg(long)]
    pub adp_config_path: PathBuf,

    /// Additional environment variables to be passed into the Agent Data Plane container.
    ///
    /// These should be in the form of `KEY=VALUE`.
    #[arg(long = "adp-env-arg")]
    pub adp_additional_env_args: Vec<String>,
}

impl Cli {
    pub fn dsd_millstone_config(&self) -> MillstoneConfig {
        MillstoneConfig {
            image: self.millstone_image.clone(),
            binary_path: self.millstone_binary_path.clone(),
            config_path: self.dsd_millstone_config_path.clone(),
        }
    }

    pub fn adp_millstone_config(&self) -> MillstoneConfig {
        MillstoneConfig {
            image: self.millstone_image.clone(),
            binary_path: self.millstone_binary_path.clone(),
            config_path: self
                .adp_millstone_config_path
                .clone()
                .unwrap_or_else(|| self.dsd_millstone_config_path.clone()),
        }
    }

    pub fn datadog_intake_config(&self) -> DatadogIntakeConfig {
        DatadogIntakeConfig {
            image: self.datadog_intake_image.clone(),
            binary_path: self.datadog_intake_binary_path.clone(),
            config_path: self.datadog_intake_config_path.clone(),
        }
    }

    pub fn dsd_config(&self) -> DSDConfig {
        DSDConfig {
            image: self.dsd_image.clone(),
            entrypoint: self.dsd_entrypoint.clone(),
            command: self.dsd_command.clone(),
            config_path: self.dsd_config_path.clone(),
            additional_env_args: self.dsd_additional_env_args.clone(),
        }
    }

    pub fn adp_config(&self) -> ADPConfig {
        ADPConfig {
            image: self.adp_image.clone(),
            entrypoint: self.adp_entrypoint.clone(),
            command: self.adp_command.clone(),
            config_path: self.adp_config_path.clone(),
            additional_env_args: self.adp_additional_env_args.clone(),
        }
    }
}
