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
}

/// Run subcommand configuration.
#[derive(Args, Debug)]
pub struct RunConfig {
    /// Path to the configuration file.
    #[arg(short = 'c', long = "config", default_value = "/etc/datadog-agent/datadog.yaml")]
    pub config: PathBuf,
}
