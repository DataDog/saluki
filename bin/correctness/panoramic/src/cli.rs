use std::path::{Path, PathBuf};

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
    /// path to a test cases directory (can be specified multiple times)
    #[argh(option, short = 'd')]
    pub test_dirs: Vec<PathBuf>,

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

    /// directory to write container logs to (default: auto-generated temp dir)
    #[argh(option, short = 'l')]
    pub log_dir: Option<PathBuf>,

    /// skip writing container logs to disk
    #[argh(switch)]
    pub no_logs: bool,

    /// directory whose contents are read-only bind mounted into every target container
    /// panoramic launches (not millstone or datadog-intake). The directory is treated as
    /// the container root: `<mounts-dir>/etc/foo` maps to `/etc/foo`. Defaults to
    /// `bin/correctness/panoramic/mounts/` in the Saluki workspace where this binary was
    /// compiled.
    #[argh(option, default = "default_mounts_dir()")]
    pub mounts_dir: PathBuf,
}

fn default_mounts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("mounts")
}

/// List available integration tests.
#[derive(FromArgs)]
#[argh(subcommand, name = "list")]
pub struct ListCommand {
    /// path to a test cases directory (can be specified multiple times)
    #[argh(option, short = 'd')]
    pub test_dirs: Vec<PathBuf>,
}
