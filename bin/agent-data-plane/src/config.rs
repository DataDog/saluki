use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};
use tracing::level_filters::LevelFilter;

#[derive(Parser)]
#[command(about)] // Q: Is a description needed here?
pub struct Cli {
    /// Enable verbose output. (Specify twice for more verbosity.)
    #[arg(global = true, short = 'v', long, action = ArgAction::Count, default_value_t = 0)]
    verbose: u8,

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
    /// run subcommand for the ADP binary
    #[command(name = "run")]
    Run(RunConfig),
}

/// run subcommand configuration
#[derive(Args)]
pub struct RunConfig {
    /// Path to the configuration file
    #[arg(long = "config", default_value = "/etc/datadog-agent/datadog.yaml")]
    pub config: PathBuf,
}
