use std::path::PathBuf;

use airlock::config::{DatadogIntakeConfig, MillstoneConfig, TargetConfig};
use clap::Parser;

#[derive(Clone, Parser)]
#[command(about)]
pub struct Cli {
    /// Container image to use for millstone.
    ///
    /// This must be a valid image reference -- `millstone:x.y.z`, `registry.ddbuild.io/saluki/millstone:x.y.z`, etc --
    /// to an image containing the `millstone` binary.
    ///
    /// The `millstone` binary must exist at `/usr/local/bin/millstone` in the image. Otherwise, the binary path can be
    /// overridden with the `binary-path` argument.
    #[arg(long)]
    pub millstone_image: String,

    /// Path to the millstone binary.
    #[arg(long, default_value = "/usr/local/bin/millstone")]
    pub millstone_binary_path: String,

    /// Path to the millstone configuration file to use for the baseline target.
    ///
    /// This file is mapped into the baseline target's `millstone` container and so it must exist on the system where
    /// this command is run from.
    #[arg(long)]
    pub baseline_millstone_config_path: PathBuf,

    /// Optional path to the millstone configuration file to use for the comparison target.
    ///
    /// If not specified, uses the same config as the baseline target.
    ///
    /// This file is mapped into the comparison target's `millstone` container and so it must exist on the system where
    /// this command is run from.
    #[arg(long)]
    pub comparison_millstone_config_path: Option<PathBuf>,

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

    /// Container image to use for baseline target.
    ///
    /// This must be a valid image reference -- `dogstatsd:x.y.z`, `docker.io/datadog/dogstatsd:x.y.z`, etc -- to an
    /// image containing the `dogstatsd` binary.
    #[arg(long)]
    pub baseline_image: String,

    /// Entrypoint for the baseline target container.
    #[arg(long)]
    pub baseline_entrypoint: Vec<String>,

    /// Command to run in the container to start the baseline target.
    #[arg(long)]
    pub baseline_command: Vec<String>,

    /// Path to the configuration file to supply to the baseline target.
    ///
    /// This should be a valid path to a file on the host system, which will then be mapped into the baseline target container
    /// at `/etc/target/<filename>`, where `<filename>` is the basename of the file on the host system.
    #[arg(long)]
    pub baseline_config_path: PathBuf,

    /// Additional environment variables to be passed into the baseline target container.
    ///
    /// These should be in the form of `KEY=VALUE`.
    #[arg(long = "baseline-env-arg")]
    pub baseline_additional_env_args: Vec<String>,

    /// Container image to use for the comparison target.
    ///
    /// This must be a valid image reference -- `agent-data-plane:x.y.z`,
    /// `registry.ddbuild.io/saluki/agent-data-plane:x.y.z`, etc -- to an image containing the `agent-data-plane`
    /// binary.
    #[arg(long)]
    pub comparison_image: String,

    /// Entrypoint for the comparison target container.
    #[arg(long)]
    pub comparison_entrypoint: Vec<String>,

    /// Command to run in the container to start the comparison target.
    #[arg(long)]
    pub comparison_command: Vec<String>,

    /// Path to the configuration file to supply to the comparison target.
    ///
    /// This should be a valid path to a file on the host system, which will then be mapped into the comparison target container
    /// at `/etc/target/<filename>`, where `<filename>` is the basename of the file on the host system.
    #[arg(long)]
    pub comparison_config_path: PathBuf,

    /// Additional environment variables to be passed into the comparison target container.
    ///
    /// These should be in the form of `KEY=VALUE`.
    #[arg(long = "comparison-env-arg")]
    pub comparison_additional_env_args: Vec<String>,
}

impl Cli {
    pub fn baseline_millstone_config(&self) -> MillstoneConfig {
        MillstoneConfig {
            image: self.millstone_image.clone(),
            binary_path: Some(self.millstone_binary_path.clone()),
            config_path: self.baseline_millstone_config_path.clone(),
        }
    }

    pub fn comparison_millstone_config(&self) -> MillstoneConfig {
        MillstoneConfig {
            image: self.millstone_image.clone(),
            binary_path: Some(self.millstone_binary_path.clone()),
            config_path: self
                .comparison_millstone_config_path
                .clone()
                .unwrap_or_else(|| self.baseline_millstone_config_path.clone()),
        }
    }

    pub fn datadog_intake_config(&self) -> DatadogIntakeConfig {
        DatadogIntakeConfig {
            image: self.datadog_intake_image.clone(),
            binary_path: Some(self.datadog_intake_binary_path.clone()),
            config_path: self.datadog_intake_config_path.clone(),
        }
    }

    pub fn baseline_target_config(&self) -> TargetConfig {
        TargetConfig {
            image: self.baseline_image.clone(),
            entrypoint: self.baseline_entrypoint.clone(),
            command: self
                .baseline_command
                .iter()
                .flat_map(|s| s.split(' ').map(String::from))
                .collect(),
            additional_env_args: self.baseline_additional_env_args.clone(),
        }
    }

    pub fn comparison_target_config(&self) -> TargetConfig {
        TargetConfig {
            image: self.comparison_image.clone(),
            entrypoint: self.comparison_entrypoint.clone(),
            command: self
                .comparison_command
                .iter()
                .flat_map(|s| s.split(' ').map(String::from))
                .collect(),
            additional_env_args: self.comparison_additional_env_args.clone(),
        }
    }
}
