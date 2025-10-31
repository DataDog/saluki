use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};
use tracing::level_filters::LevelFilter;

#[derive(Parser)]
#[command(about)]
pub struct Cli {
    /// Enable verbose output. (Specify twice for more verbosity.)
    #[arg(global = true, short = 'v', long, action = ArgAction::Count, default_value_t = 0)]
    verbose: u8,

    /// Isolation group identifier.
    ///
    /// This identifier is used to group related containers together, in terms of networking. The identifier should be
    /// unique between different test runs to avoid conflicts, but the same between different containers that need to
    /// interact with one another in a given test run.
    #[arg(long)]
    pub isolation_group_id: String,

    /// Subcommand to run.
    #[command(subcommand)]
    pub action: Action,
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
}

#[derive(Subcommand)]
pub enum Action {
    /// Run a millstone container to generate input metrics.
    #[command(name = "run-millstone")]
    Millstone(MillstoneConfig),

    /// Run a datadog-intake container to receive output metrics.
    #[command(name = "run-datadog-intake")]
    DatadogIntake(DatadogIntakeConfig),

    /// Run a DogStatsD container to receive input metrics.
    #[command(name = "run-dogstatsd")]
    Dogstatsd(DSDConfig),

    /// Run an Agent Data Plane container to receive input metrics.
    #[command(name = "run-adp")]
    AgentDataPlane(ADPConfig),
}

/// Millstone configuration.
#[derive(Args, Clone)]
pub struct MillstoneConfig {
    /// Container image to use.
    ///
    /// This must be a valid image reference -- `millstone:x.y.z`, `registry.ddbuild.io/saluki/millstone:x.y.z`, etc --
    /// to an image containing the `millstone` binary.
    ///
    /// The `millstone` binary must exist at `/usr/local/bin/millstone` in the image. Otherwise, the binary path can be
    /// overridden with the `binary-path` argument.
    #[arg(short = 'i', long)]
    pub image: String,

    /// Path to the millstone binary.
    #[arg(short = 'b', long, default_value = "/usr/local/bin/millstone")]
    pub binary_path: String,

    /// Path to the millstone configuration file to use.
    ///
    /// This file is mapped into the millstone container and so it must exist on the system where this command is run
    /// from.
    #[arg(short = 'c', long)]
    pub config_path: PathBuf,
}

/// datadog-intake configuration.
#[derive(Args, Clone)]
pub struct DatadogIntakeConfig {
    /// Container image to use.
    ///
    /// This must be a valid image reference -- `datadog-intake:x.y.z`,
    /// `registry.ddbuild.io/saluki/datadog-intake:x.y.z`, etc -- to an image containing the `datadog-intake` binary.
    ///
    /// The `datadog-intake` binary must exist at `/usr/local/bin/datadog-intake` in the image. Otherwise, the binary
    /// path can be overridden with the `binary-path` argument.
    #[arg(short = 'i', long)]
    pub image: String,

    /// Path to the datadog-intake binary.
    #[arg(short = 'b', long, default_value = "/usr/local/bin/datadog-intake")]
    pub binary_path: String,

    /// Path to the datadog-intake configuration file to use.
    ///
    /// This file is mapped into the datadog-intake container and so it must exist on the system where this command is run
    /// from.
    #[arg(short = 'c', long)]
    pub config_path: PathBuf,
}

/// DogStatsD configuration.
#[derive(Args, Clone)]
pub struct DSDConfig {
    /// Container image to use.
    ///
    /// This must be a valid image reference -- `dogstatsd:x.y.z`, `docker.io/datadog/dogstatsd:x.y.z`, etc -- to an
    /// image containing the `dogstatsd` binary.
    ///
    /// The `dogstatsd` binary must exist at `/dogstatsd` in the image. Otherwise, the binary path can be overridden
    /// with the `binary-path` argument.
    #[arg(long)]
    pub image: String,

    /// Entrypoint to execute..
    #[arg(short = 'e', long, default_values_t = vec!["/entrypoint.sh".to_string()])]
    pub entrypoint: Vec<String>,

    /// Command to run DogStatsD with.
    #[arg(short = 'b', long, default_values_t = vec!["/dogstatsd".to_string(), "start".to_string(), "--cfgpath".to_string(), "/etc/datadog-agent".to_string()])]
    pub command: Vec<String>,

    /// Path to the DogStatsD configuration file to use.
    ///
    /// This file is mapped into the DogStatsD container and so it must exist on the system where this command is
    /// run from.
    #[arg(short = 'c', long)]
    pub config_path: PathBuf,

    /// Additional environment variables to be passed into the DogStatsD container.
    ///
    /// These should be in the form of `KEY=VALUE`.
    #[arg(short = 'e', long = "env-arg")]
    pub additional_env_args: Vec<String>,
}

/// Agent Data Plane configuration.
#[derive(Args, Clone)]
pub struct ADPConfig {
    /// Container image to use.
    ///
    /// This must be a valid image reference -- `agent-data-plane:x.y.z`,
    /// `registry.ddbuild.io/saluki/agent-data-plane:x.y.z`, etc -- to an image containing the `agent-data-plane`
    /// binary.
    ///
    /// The `agent-data-plane` binary must exist at `/usr/bin/agent-data-plane` in the image. Otherwise, the binary
    /// path can be overridden with the `binary-path` argument.
    #[arg(short = 'i', long)]
    pub image: String,

    /// Entrypoint to execute.
    #[arg(short = 'e', long, default_values_t = vec!["/entrypoint.sh".to_string()])]
    pub entrypoint: Vec<String>,

    /// Command to run Agent Data Plane with.
    #[arg(short = 'b', long, default_values_t = vec!["/usr/local/bin/agent-data-plane".to_string(), "run".to_string(), "--config".to_string(), "/etc/datadog-agent/datadog.yaml".to_string()])]
    pub command: Vec<String>,

    /// Path to the Agent Data Plane configuration file to use.
    ///
    /// This file is mapped into the ADP container and so it must exist on the system where this command is
    /// run from.
    #[arg(short = 'c', long)]
    pub config_path: PathBuf,

    /// Additional environment variables to be passed into the Agent Data Plane container.
    ///
    /// These should be in the form of `KEY=VALUE`.
    #[arg(short = 'e', long = "env-arg")]
    pub additional_env_args: Vec<String>,
}
