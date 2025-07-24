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

    /// Various debugging commands.
    #[command(subcommand)]
    Debug(DebugConfig),

    /// Prints the current configuration.
    #[command(name = "config")]
    Config,
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

    /// Overrides the current log level.
    #[command(name = "set-log-level")]
    SetLogLevel(SetLogLevelConfig),
}

/// Set log level configuration.
#[derive(Args, Debug)]
pub struct SetLogLevelConfig {
    /// Filter directives to apply (e.g. `INFO`, `DEBUG`, `TRACE`, `WARN`, `ERROR`).
    #[arg(required = true)]
    pub filter_directives: String,

    /// Amount of time to apply the log level override, in seconds.
    #[arg(required = true)]
    pub duration_secs: u64,
}
