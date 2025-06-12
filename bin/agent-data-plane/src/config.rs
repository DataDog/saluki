use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};

#[derive(Parser)]
#[command(about)]
pub struct Cli {
    /// Enable verbose output. (Specify twice for more verbosity.)
    #[arg(global = true, short = 'v', long, action = ArgAction::Count, default_value_t = 0)]
    verbose: u8,

    /// Subcommand to run.    
    #[command(subcommand)]
    pub action: Option<Action>, // Makes subcommands optional
}

#[derive(Subcommand)]
pub enum Action {
    /// Run subcommand for the ADP binary
    #[command(name = "run")]
    Run(RunConfig),
}

/// Run subcommand configuration
#[derive(Args, Debug)]
pub struct RunConfig {
    /// Path to the configuration file
    #[arg(long = "config", default_value = "/etc/datadog-agent/datadog.yaml")]
    pub config: PathBuf,
}
