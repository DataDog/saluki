//! CLI parsing for the check-runner binary.

use std::path::PathBuf;

use argh::FromArgs;

/// Runs a single Datadog check and forwards results to the Datadog platform.
#[derive(FromArgs, Debug)]
#[argh(
    description = "Runs a single Datadog check and forwards results.",
    help_triggers("-h", "--help", "help")
)]
pub struct Cli {
    /// path to the Datadog Agent configuration file
    #[argh(option, short = 'c', long = "config")]
    pub config_file: Option<PathBuf>,
}
