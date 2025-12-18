use std::path::PathBuf;

use argh::FromArgs;

/// Panoramic: Integration test runner for Agent Data Plane.
#[derive(FromArgs)]
#[argh(help_triggers("-h", "--help", "help"))]
pub struct Cli {
    #[argh(subcommand)]
    pub command: Command,
}

#[derive(FromArgs)]
#[argh(subcommand)]
pub enum Command {
    Run(RunCommand),
    List(ListCommand),
}

/// Run integration tests.
#[derive(FromArgs)]
#[argh(subcommand, name = "run")]
pub struct RunCommand {
    /// path to the test cases directory
    #[argh(option, short = 'd', default = "default_test_dir()")]
    pub test_dir: PathBuf,

    /// run only specific test(s) by name (comma-separated)
    #[argh(option, short = 't')]
    pub tests: Option<String>,

    /// number of tests to run in parallel
    #[argh(option, short = 'p', default = "4")]
    pub parallelism: usize,

    /// output format (text, json)
    #[argh(option, short = 'o', default = "String::from(\"text\")")]
    pub output: String,

    /// stop on first failure
    #[argh(switch, short = 'f')]
    pub fail_fast: bool,

    /// show verbose output including all assertion details
    #[argh(switch, short = 'v')]
    pub verbose: bool,

    /// disable interactive TUI (use plain text output)
    #[argh(switch)]
    pub no_tui: bool,

    /// skip writing container logs to disk
    #[argh(switch)]
    pub no_logs: bool,
}

/// List available integration tests.
#[derive(FromArgs)]
#[argh(subcommand, name = "list")]
pub struct ListCommand {
    /// path to the test cases directory
    #[argh(option, short = 'd', default = "default_test_dir()")]
    pub test_dir: PathBuf,
}

fn default_test_dir() -> PathBuf {
    PathBuf::from("test/integration-tests/cases")
}
