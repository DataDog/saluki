use std::path::PathBuf;

use airlock::config::{ADPConfig, DSDConfig, MetricsIntakeConfig, MillstoneConfig};
use clap::{ArgAction, Parser};
use tracing::level_filters::LevelFilter;

#[derive(Clone, Parser)]
#[command(about)]
pub struct Cli {
    /// Enable verbose output. (Specify twice for more verbosity.)
    #[arg(global = true, short = 'v', long, action = ArgAction::Count, default_value_t = 0)]
    verbose: u8,

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

    /// Path to the millstone configuration file to use.
    ///
    /// This file is mapped into the millstone container and so it must exist on the system where this command is run
    /// from.
    #[arg(long)]
    pub millstone_config_path: PathBuf,

    /// Container image to use for metrics-intake.
    ///
    /// This must be a valid image reference -- `metrics-intake:x.y.z`,
    /// `registry.ddbuild.io/saluki/metrics-intake:x.y.z`, etc -- to an image containing the `metrics-intake` binary.
    ///
    /// The `metrics-intake` binary must exist at `/usr/local/bin/metrics-intake` in the image. Otherwise, the binary
    /// path can be overridden with the `binary-path` argument.
    #[arg(long)]
    pub metrics_intake_image: String,

    /// Path to the metrics-intake binary.
    #[arg(long, default_value = "/usr/local/bin/metrics-intake")]
    pub metrics_intake_binary_path: String,

    /// Path to the metrics-intake configuration file to use.
    ///
    /// This file is mapped into the metrics-intake container and so it must exist on the system where this command is run
    /// from.
    #[arg(long)]
    pub metrics_intake_config_path: PathBuf,

    /// Container image to use for DogStatsD.
    ///
    /// This must be a valid image reference -- `dogstatsd:x.y.z`, `docker.io/datadog/dogstatsd:x.y.z`, etc -- to an
    /// image containing the `dogstatsd` binary.
    ///
    /// The `dogstatsd` binary must exist at `/dogstatsd` in the image. Otherwise, the binary path can be overridden
    /// with the `binary-path` argument.
    #[arg(long)]
    pub dsd_image: String,

    /// Path to the DogStatsD binary.
    #[arg(short = 'b', long, default_value = "/dogstatsd")]
    pub binary_path: String,

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

    /// Path to the Agent Data Plane binary.
    #[arg(long, default_value = "/usr/local/bin/agent-data-plane")]
    pub adp_binary_path: String,

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
    /// Gets the configured log level based on the user-supplied verbosity level.
    pub fn log_level(&self) -> LevelFilter {
        match self.verbose {
            0 => LevelFilter::INFO,
            1 => LevelFilter::DEBUG,
            _ => LevelFilter::TRACE,
        }
    }

    pub fn millstone_config(&self) -> MillstoneConfig {
        MillstoneConfig {
            image: self.millstone_image.clone(),
            binary_path: self.millstone_binary_path.clone(),
            config_path: self.millstone_config_path.clone(),
        }
    }

    pub fn metrics_intake_config(&self) -> MetricsIntakeConfig {
        MetricsIntakeConfig {
            image: self.metrics_intake_image.clone(),
            binary_path: self.metrics_intake_binary_path.clone(),
            config_path: self.metrics_intake_config_path.clone(),
        }
    }

    pub fn dsd_config(&self) -> DSDConfig {
        DSDConfig {
            image: self.dsd_image.clone(),
            binary_path: self.binary_path.clone(),
            config_path: self.dsd_config_path.clone(),
            additional_env_args: self.dsd_additional_env_args.clone(),
        }
    }

    pub fn adp_config(&self) -> ADPConfig {
        ADPConfig {
            image: self.adp_image.clone(),
            binary_path: self.adp_binary_path.clone(),
            config_path: self.adp_config_path.clone(),
            additional_env_args: self.adp_additional_env_args.clone(),
        }
    }
}
