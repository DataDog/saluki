use std::path::PathBuf;

use argh::FromArgs;

mod config;
pub use self::config::handle_config_command;
use self::config::ConfigCommand;

mod debug;
pub use self::debug::handle_debug_command;
use self::debug::DebugCommand;

mod dogstatsd;
pub use self::dogstatsd::handle_dogstatsd_command;
use self::dogstatsd::DogstatsdCommand;

mod run;
pub use self::run::handle_run_command;
use self::run::RunCommand;

mod utils;

#[derive(FromArgs)]
#[argh(description = "Agent Data Plane", help_triggers("-h", "--help", "help"))]
pub struct Cli {
    /// path to the configuration file.
    #[argh(option, short = 'c', long = "config")]
    pub config_file: Option<PathBuf>,

    /// subcommand to run.
    #[argh(subcommand)]
    pub action: Action,
}

#[derive(FromArgs)]
#[argh(subcommand)]
pub enum Action {
    /// Runs the data plane.
    Run(RunCommand),

    /// Various debugging commands.
    Debug(DebugCommand),

    /// Prints the current configuration.
    Config(ConfigCommand),

    /// Various dogstatsd commands.
    Dogstatsd(DogstatsdCommand),
}
