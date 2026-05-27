use std::path::{Path, PathBuf};

use argh::FromArgs;
use chrono::Local;

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

    /// run only specific tests by name (comma-separated)
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

    /// directory to write container logs to (default: auto-generated temp dir). Can be set (or
    /// overridden) by environment variable PANORAMIC_LOG_DIR
    #[argh(option, short = 'l')]
    log_dir: Option<PathBuf>,

    /// directory whose contents are read-only bind mounted into every target container
    /// panoramic launches (not millstone or datadog-intake). The directory is treated as
    /// the container root: `<mounts-dir>/etc/foo` maps to `/etc/foo`. Defaults to
    /// `bin/correctness/panoramic/mounts/` in the Saluki workspace where this binary was
    /// compiled.
    #[argh(option, default = "default_mounts_dir()")]
    pub mounts_dir: PathBuf,

    /// name of the kind cluster to create or reuse for kind-runtime tests
    #[argh(option, default = "crate::kind::DEFAULT_CLUSTER_NAME.to_string()")]
    pub kind_cluster_name: String,

    /// don't delete the kind cluster after kind-runtime tests complete (useful for local iteration)
    #[argh(switch)]
    pub no_delete_kind_cluster: bool,
}

impl RunCommand {
    /// Gets the user-supplied `log_dir` from CLI arguments or environment variable, or else returns a temporary directory.
    pub fn log_dir(&self) -> PathBuf {
        let base = std::env::var("PANORAMIC_LOG_DIR")
            .ok()
            .map(PathBuf::from)
            .or(self.log_dir.clone())
            .unwrap_or_else(std::env::temp_dir);

        // Always append a timestamped subdirectory, even when the user provides a base dir.
        // TODO: consider not adding a subdirectory when the user provides a desired log dir.
        let timestamp = Local::now().format("%Y%m%d-%H%M%S");
        base.join(format!("panoramic-{}", timestamp))
    }
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

    /// output the discovered tests as json along with their image dependencies. a `ci` script depends on this for dynamic
    /// pipeline creation.
    #[argh(switch)]
    pub json: bool,
}
