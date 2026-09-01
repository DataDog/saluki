use std::path::{Path, PathBuf};

use chrono::Local;
use clap::{Args, Parser, Subcommand};

use crate::reporter::OutputFormat;

/// Environment inputs the runner reads on its own, rendered at the bottom of the help output.
///
/// Only `PANORAMIC_LOG_DIR` has a flag behind it, so the rest can't be described by an `#[arg(env =
/// ...)]` attribute. Anything the runner starts reading has to be added here by hand; the tests
/// below check the list against the variables we know about.
const ENV_HELP: &str = "\
Environment variables:
  ADP_BINARY_PATH           `agent-data-plane` binary that host-process (`runtime: mac`) tests
                            spawn. Defaults to `target/release/agent-data-plane`, resolved
                            against the current directory.
  CORE_AGENT_BINARY_PATH    Datadog Agent binary that converged host-process tests spawn.
                            Defaults to the sandbox install that `make provision-macos-test-env`
                            writes.
  DOCKER_HOST               Docker endpoint to talk to. When unset, the standard socket path and
                            common non-standard locations are probed.
  PANORAMIC_ALPINE_IMAGE    Alpine image used for the short-lived container that fixes up
                            shared-volume permissions. Defaults to `alpine:latest`; CI points
                            this at an internal registry.
  PANORAMIC_LOG_DIR         Base directory for container logs. `--log-dir` wins when both are
                            set. Defaults to the system temporary directory.
  RUST_LOG                  Tracing filter for non-TUI output. Defaults to `info`.";

/// Panoramic: Integration test runner for Agent Data Plane.
#[derive(Parser)]
#[command(
    name = "panoramic",
    args_override_self = true,
    disable_help_flag = true,
    after_help = ENV_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    // Replaces clap's built-in help flag, which renders an abbreviated `-h` and a fuller `--help`.
    // Both forms should show everything, so both are wired to the long renderer. `global` carries
    // the flag into the subcommands, whose own built-in flags `disable_help_flag` suppresses.
    /// Print help
    #[arg(short = 'h', long = "help", global = true, display_order = 1000, action = clap::ArgAction::HelpLong)]
    _help: Option<bool>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run integration tests.
    Run(RunCommand),

    /// List available integration tests.
    List(ListCommand),
}

// We build clap without its `wrap_help` feature, so help text is never reflowed. Descriptions that
// need more than one line carry `verbatim_doc_comment` and are wrapped where they're written.
/// Run integration tests.
#[derive(Args)]
#[command(args_override_self = true, after_help = ENV_HELP)]
pub struct RunCommand {
    /// Path to a test cases directory (can be specified multiple times).
    #[arg(short = 'd', long)]
    pub test_dirs: Vec<PathBuf>,

    /// Run only specific tests by name (comma-separated).
    #[arg(short = 't', long)]
    pub tests: Option<String>,

    /// Integration-test runtime to scope discovery to (for example, `linux`, `mac`,
    /// or `windows`). Only integration tests whose `runtimes:` list contains this
    /// value are eligible to run. Defaults to `mac` on macOS hosts and `docker`
    /// everywhere else. Correctness tests are unaffected by this flag
    #[arg(long, verbatim_doc_comment)]
    pub runtime: Option<String>,

    /// Number of tests to run in parallel.
    #[arg(short = 'p', long, default_value = "4")]
    pub parallelism: usize,

    /// Output format.
    #[arg(short = 'o', long, value_enum, ignore_case = true, default_value = "text")]
    pub output: OutputFormat,

    /// Stop on first failure.
    #[arg(short = 'f', long)]
    pub fail_fast: bool,

    /// Show verbose output including all assertion details.
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Disable interactive TUI (use plain text output).
    #[arg(long)]
    pub no_tui: bool,

    /// Base directory to write container logs to (default: auto-generated temp dir).
    /// Each run gets its own timestamped subdirectory underneath
    #[arg(short = 'l', long, env = "PANORAMIC_LOG_DIR", verbatim_doc_comment)]
    log_dir: Option<PathBuf>,

    /// Directory whose contents are read-only bind mounted into every target
    /// container panoramic launches (not millstone or datadog-intake). The directory
    /// is treated as the container root: `<mounts-dir>/etc/foo` maps to `/etc/foo`.
    /// Defaults to `bin/correctness/panoramic/mounts/` in the Saluki workspace where
    /// this binary was compiled
    #[arg(long, default_value_os_t = default_mounts_dir(), verbatim_doc_comment)]
    pub mounts_dir: PathBuf,

    /// Name of the kind cluster to create or reuse for kind-runtime tests.
    #[arg(long, default_value = crate::kind::DEFAULT_CLUSTER_NAME)]
    pub kind_cluster_name: String,

    /// Don't delete the kind cluster after kind-runtime tests complete (useful for
    /// local iteration)
    #[arg(long, verbatim_doc_comment)]
    pub no_delete_kind_cluster: bool,
}

impl RunCommand {
    /// Gets the log directory for this run.
    ///
    /// The base comes from `--log-dir`/`PANORAMIC_LOG_DIR`, falling back to a temporary directory.
    pub fn log_dir(&self) -> PathBuf {
        let base = self.log_dir.clone().unwrap_or_else(std::env::temp_dir);

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
#[derive(Args)]
#[command(args_override_self = true)]
pub struct ListCommand {
    /// Path to a test cases directory (can be specified multiple times).
    #[arg(short = 'd', long)]
    pub test_dirs: Vec<PathBuf>,

    /// Integration-test runtime to scope discovery to. Same semantics as on `run`:
    /// defaults to `mac` on macOS, `docker` everywhere else. Correctness tests are
    /// unaffected
    #[arg(long, verbatim_doc_comment)]
    pub runtime: Option<String>,

    /// Output the discovered tests as json along with their image dependencies. A
    /// `ci` script depends on this for dynamic pipeline creation
    #[arg(long, verbatim_doc_comment)]
    pub json: bool,
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use clap::CommandFactory as _;

    use super::*;

    /// Every environment variable the runner reads, and therefore every variable the help output
    /// has to mention. Grep for `env::var` under `bin/correctness/` when this list changes.
    const RUNNER_ENV_VARS: &[&str] = &[
        "ADP_BINARY_PATH",
        "CORE_AGENT_BINARY_PATH",
        "DOCKER_HOST",
        "PANORAMIC_ALPINE_IMAGE",
        "PANORAMIC_LOG_DIR",
        "RUST_LOG",
    ];

    fn run_command_of(cli: &Cli) -> &RunCommand {
        match &cli.command {
            Command::Run(cmd) => cmd,
            Command::List(_) => panic!("expected a `run` invocation"),
        }
    }

    #[test]
    fn argument_definitions_are_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_command_routes_both_help_flags_to_the_long_renderer() {
        // Out of the box, clap gives `-h` an abbreviated help and `--help` the full one. Both forms
        // have to show everything, so every command gets the one replacement flag. Building is what
        // propagates that flag down into the subcommands.
        let mut command = Cli::command();
        command.build();
        let commands = std::iter::once(&command).chain(command.get_subcommands());

        for subject in commands {
            if subject.get_name() == "help" {
                continue;
            }

            let help_args: Vec<_> = subject
                .get_arguments()
                .filter(|arg| arg.get_long() == Some("help"))
                .collect();

            assert_eq!(help_args.len(), 1, "'{}' should have one help flag", subject.get_name());
            let help = help_args[0];
            assert_eq!(help.get_short(), Some('h'));
            assert!(
                matches!(help.get_action(), clap::ArgAction::HelpLong),
                "'{}' renders an abbreviated help for one of the flags",
                subject.get_name()
            );
        }
    }

    #[test]
    fn help_documents_every_runner_environment_variable() {
        let mut command = Cli::command();
        let top_level = command.render_long_help().to_string();
        let run = command
            .find_subcommand_mut("run")
            .expect("run subcommand should exist")
            .render_long_help()
            .to_string();

        for var in RUNNER_ENV_VARS {
            assert!(top_level.contains(var), "`panoramic --help` does not mention {}", var);
            assert!(run.contains(var), "`panoramic run --help` does not mention {}", var);
        }
    }

    #[test]
    fn log_dir_reads_its_environment_fallback() {
        // Asserted against the argument model rather than by mutating the process environment,
        // which would race with the other tests in this binary.
        let command = Cli::command();
        let log_dir = command
            .find_subcommand("run")
            .expect("run subcommand should exist")
            .get_arguments()
            .find(|arg| arg.get_id() == "log_dir")
            .expect("run should have a log-dir argument");

        assert_eq!(log_dir.get_env(), Some(OsStr::new("PANORAMIC_LOG_DIR")));
    }

    #[test]
    fn output_format_lists_and_validates_its_values() {
        let help = Cli::command()
            .find_subcommand_mut("run")
            .expect("run subcommand should exist")
            .render_long_help()
            .to_string();
        assert!(help.contains("[possible values: text, json]"), "help was:\n{}", help);

        let cli = Cli::try_parse_from(["panoramic", "run", "-d", "cases", "-o", "json"]).expect("json is valid");
        assert!(matches!(run_command_of(&cli).output, OutputFormat::Json));

        // The pre-clap parser lowercased the value before matching, so mixed case still has to work.
        let cli = Cli::try_parse_from(["panoramic", "run", "-d", "cases", "-o", "JSON"]).expect("JSON is valid");
        assert!(matches!(run_command_of(&cli).output, OutputFormat::Json));

        // Rejecting this at parse time is the point: the old parser only noticed once the run was
        // already underway.
        assert!(Cli::try_parse_from(["panoramic", "run", "-d", "cases", "-o", "yaml"]).is_err());
    }

    #[test]
    fn defaults_match_the_pre_clap_parser() {
        let cli = Cli::try_parse_from(["panoramic", "run", "-d", "cases"]).expect("minimal run should parse");
        let cmd = run_command_of(&cli);

        assert_eq!(cmd.parallelism, 4);
        assert!(matches!(cmd.output, OutputFormat::Text));
        assert_eq!(cmd.mounts_dir, default_mounts_dir());
        assert_eq!(cmd.kind_cluster_name, crate::kind::DEFAULT_CLUSTER_NAME);
        assert!(cmd.runtime.is_none());
        assert!(cmd.tests.is_none());
        assert!(!cmd.fail_fast);
        assert!(!cmd.verbose);
        assert!(!cmd.no_tui);
        assert!(!cmd.no_delete_kind_cluster);
    }

    #[test]
    fn known_run_invocations_parse() {
        // These mirror the invocations in the Makefile, `.gitlab/`, and `AGENTS.md`.
        let cli = Cli::try_parse_from([
            "panoramic",
            "run",
            "-d",
            "test/correctness/cases",
            "--no-tui",
            "-p",
            "1",
            "-l",
            "integration-logs",
        ])
        .expect("makefile-style invocation should parse");
        let cmd = run_command_of(&cli);
        assert_eq!(cmd.test_dirs, vec![PathBuf::from("test/correctness/cases")]);
        assert!(cmd.no_tui);
        assert_eq!(cmd.parallelism, 1);
        assert!(cmd.log_dir().starts_with("integration-logs"));

        // `-d` accumulates; `-t` takes the last value, matching the previous parser.
        let cli = Cli::try_parse_from([
            "panoramic",
            "run",
            "-d",
            "test/correctness/cases",
            "-d",
            "test/integration/cases",
            "-t",
            "one",
            "-t",
            "two",
            "-f",
            "-v",
            "--runtime",
            "mac",
            "--kind-cluster-name",
            "custom",
            "--no-delete-kind-cluster",
        ])
        .expect("full invocation should parse");
        let cmd = run_command_of(&cli);
        assert_eq!(
            cmd.test_dirs,
            vec![
                PathBuf::from("test/correctness/cases"),
                PathBuf::from("test/integration/cases")
            ]
        );
        assert_eq!(cmd.tests.as_deref(), Some("two"));
        assert!(cmd.fail_fast);
        assert!(cmd.verbose);
        assert_eq!(cmd.runtime.as_deref(), Some("mac"));
        assert_eq!(cmd.kind_cluster_name, "custom");
        assert!(cmd.no_delete_kind_cluster);
    }

    #[test]
    fn known_list_invocations_parse() {
        let cli = Cli::try_parse_from(["panoramic", "list", "-d", "test/integration/cases", "--json"])
            .expect("list --json should parse");
        match cli.command {
            Command::List(cmd) => {
                assert_eq!(cmd.test_dirs, vec![PathBuf::from("test/integration/cases")]);
                assert!(cmd.json);
            }
            Command::Run(_) => panic!("expected a `list` invocation"),
        }
    }

    #[test]
    fn log_dir_appends_a_timestamped_subdirectory() {
        let cli = Cli::try_parse_from(["panoramic", "run", "-d", "cases", "-l", "/tmp/panoramic-test"])
            .expect("run with a log dir should parse");
        let log_dir = run_command_of(&cli).log_dir();

        let parent = log_dir.parent().expect("log dir should have a parent");
        assert_eq!(parent, Path::new("/tmp/panoramic-test"));
        let leaf = log_dir
            .file_name()
            .and_then(OsStr::to_str)
            .expect("log dir should have a name");
        assert!(leaf.starts_with("panoramic-"), "leaf was '{}'", leaf);
    }
}
