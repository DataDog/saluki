use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(about)]
pub struct Cli {
    /// Subcommand to run.   
    #[command(subcommand)]
    pub action: Option<Action>,
}

#[derive(Subcommand)]
pub enum Action {
    /// Runs the data plane.
    #[command(name = "run")]
    Run(RunConfig),

    /// Configures the log level of the data plane.
    #[command(subcommand)]
    Debug(DebugConfig),
}

/// Run subcommand configuration.
#[derive(Args, Debug)]
pub struct RunConfig {
    /// Path to the configuration file.
    #[arg(short = 'c', long = "config", default_value = "/etc/datadog-agent/datadog.yaml")]
    pub config: PathBuf,
}

/// Debug subcommand configuration.
#[derive(Subcommand, Debug)]
pub enum DebugConfig {
    /// Resets log level.
    #[command(name = "reset-log-level")]
    ResetLogLevel,

    /// Overrides the default log filtering directives.
    #[command(name = "set-log-level")]
    SetLogLevel(SetLogLevelConfig),
}

/// Set log level configuration.
#[derive(Args, Debug)]
pub struct SetLogLevelConfig {
    /// Filter directives to apply.
    #[arg(required = true)]
    pub filter_directives: String,

    /// Duration in seconds for which the override will be applied.
    #[arg(required = true)]
    pub duration_secs: u64,
}
