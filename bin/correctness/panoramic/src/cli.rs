//! Defines Panoramic's command-line interface.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use saluki_common::time::{TimeZone, Timestamp, TimestampFormat};

use crate::core::participant::Os;
use crate::participant_case::Replacement;
use crate::reporter::OutputFormat;

/// Environment variables Panoramic honors but doesn't own, rendered at the bottom of the help output.
///
/// Panoramic-owned environment settings are arguments carrying a `PANORAMIC_*` fallback, which clap renders
/// next to the flag they belong to. Bollard reads `DOCKER_HOST` and the logging setup reads `RUST_LOG`.
/// `DOCKER_HOST` is not a flag, and `RUST_LOG` is not a clap environment fallback, so both are described here by hand.
const ENV_HELP: &str = "\
Environment variables:
  DOCKER_HOST    Docker endpoint to talk to. When unset, the standard socket path and common
                 non-standard locations are probed.
  RUST_LOG       tracing-subscriber filter, for expert use. When set, it takes precedence over
                 --log-level and applies to every crate, including external dependencies.";

/// Crates that `--log-level` applies to: Panoramic itself and the first-party libraries it links.
///
/// These are tracing target names, which are crate names with underscores rather than dashes.
const FIRST_PARTY_LOG_TARGETS: &[&str] = &["panoramic", "airlock", "stele", "saluki_common", "saluki_error"];

/// Verbosity selected by `--log-level`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    /// Builds the tracing filter directives that scope this level to Panoramic and its first-party libraries.
    ///
    /// The leading `off` is the default for everything else, which keeps external dependencies silent.
    pub fn filter_directives(self) -> String {
        let scoped: Vec<String> = FIRST_PARTY_LOG_TARGETS
            .iter()
            .map(|target| format!("{}={}", target, self.as_str()))
            .collect();

        format!("off,{}", scoped.join(","))
    }
}

// CLI Interface Doctrine: human-authored rules for the public interface of this binary.
//
// Panoramic-owned settings should preferably be passed by command-line argument. If an
// environment variable is desired, it should be prefixed with PANORAMIC_. For example:
// CLI arg: --log-dir
// ENV var: PANORAMIC_LOG_DIR
//
// Saluki-repo-owned settings need not have a PANORAMIC_ prefix if they are shared with other
// processes, build or CI environments. These SHOULD have an addressable command-line argument.
// For example:
// CLI arg: --some-shared-saluki-ci-thing
// ENV var: SOME_SHARED_SALUKI_CI_THING
//
// Environment variables MUST be parsed by the clap command-line argument interface and documented
// in its help text. Downstream procedural code must not do its own discovery of environment
// variables; doing so makes the interface diffuse and hard to discover. The only exceptions are
// canonical environment variables, such as DOCKER_HOST, that downstream libraries discover on their
// own, but even these MUST be documented in the help text manually.
//
// Logging is a special exception. `--log-level` provides a convenient filter for Panoramic and its
// first-party libraries. The canonical RUST_LOG overrides this and feeds the logging library
// directly.
//
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

    /// Log level for panoramic and the first-party libraries it links (airlock,
    /// stele, saluki-common, saluki-error). External dependencies stay silent.
    /// RUST_LOG, when set, takes precedence over this flag
    #[arg(
        long,
        global = true,
        value_enum,
        ignore_case = true,
        default_value = "info",
        verbatim_doc_comment
    )]
    pub log_level: LogLevel,

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
    Run(Box<RunCommand>),

    /// List available integration tests.
    List(ListCommand),

    /// Print what a participant case does on one subject OS, without running anything.
    Plan(PlanCommand),
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

    /// Subject OS to run participant cases for: `linux`, `macos` or `windows`. A
    /// participant case that does not declare it is skipped. Defaults to the host's
    /// OS
    #[arg(long, value_parser = str::parse::<Os>, default_value_t = Os::host(), verbatim_doc_comment)]
    pub subject_os: Os,

    /// Image or host program a participant case runs in place of one it names, as
    /// `<name>=<reference>`, where `<name>` is the image or program path exactly as the
    /// case or its participant type writes it. Repeat the flag for more than one; the
    /// environment variable takes them as one comma-separated list. A name given twice,
    /// or one no discovered case uses, is an error. CI uses this to run the cases
    /// against the images and binaries built for the commit
    #[arg(
        long = "set",
        value_name = "NAME=REFERENCE",
        env = "PANORAMIC_SET",
        value_delimiter = ',',
        verbatim_doc_comment
    )]
    pub set: Vec<Replacement>,

    /// Number of tests to run in parallel.
    #[arg(short = 'p', long, default_value = "4")]
    pub parallelism: NonZeroUsize,

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

    /// Name of the kind cluster to create or reuse for pod-backend tests.
    #[arg(long, default_value = crate::kind::DEFAULT_CLUSTER_NAME)]
    pub kind_cluster_name: String,

    /// Don't delete the kind cluster after the run completes (useful for local
    /// iteration)
    #[arg(long, verbatim_doc_comment)]
    pub no_delete_kind_cluster: bool,

    /// How long setting up the kind cluster may take before the first case:
    /// creating it, pulling the images, and loading them. Written with a unit, as in
    /// `30m` or `1h30m`
    #[arg(long, default_value = "30m", value_parser = parse_kind_setup_timeout, verbatim_doc_comment)]
    pub kind_setup_timeout: Duration,
}

/// Parses `--kind-setup-timeout`, a duration above zero.
fn parse_kind_setup_timeout(text: &str) -> Result<Duration, String> {
    let timeout = go_duration::parse_duration(text).map_err(|e| e.to_string())?;
    if timeout.is_zero() {
        return Err("the timeout must be greater than zero".to_string());
    }
    Ok(timeout)
}

impl RunCommand {
    /// Gets the log directory for this run.
    ///
    /// The base comes from `--log-dir`/`PANORAMIC_LOG_DIR`, falling back to a temporary directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the system clock is out of range, since the directory is named after the current time.
    pub fn log_dir(&self) -> Result<PathBuf, String> {
        let base = self.log_dir.clone().unwrap_or_else(std::env::temp_dir);

        // Always append a timestamped subdirectory so artifacts from separate runs never mix. The
        // reports name the directory they landed in.
        let now = Timestamp::now().map_err(|e| e.to_string())?;
        let timestamp = now.format(TimestampFormat::CompactDateTime(TimeZone::system()));
        Ok(base.join(format!("panoramic-{}", timestamp)))
    }
}

/// List available integration tests.
#[derive(Args)]
#[command(args_override_self = true)]
pub struct ListCommand {
    /// Path to a test cases directory (can be specified multiple times).
    #[arg(short = 'd', long)]
    pub test_dirs: Vec<PathBuf>,

    /// Subject OS to list participant cases for. Same semantics as on `run`: a
    /// participant case that does not declare it is left out
    #[arg(long, value_parser = str::parse::<Os>, default_value_t = Os::host(), verbatim_doc_comment)]
    pub subject_os: Os,

    /// Output the discovered tests as json. A `ci` script depends on this for
    /// dynamic pipeline creation
    #[arg(long, verbatim_doc_comment)]
    pub json: bool,
}

/// Print what a participant case does on one subject OS, without running anything.
///
/// Loads the case and plans it for every OS it declares, so a mistake for any OS is an error on any host, then prints
/// the plan for the selected one as JSON. Needs no Docker daemon.
#[derive(Args)]
#[command(args_override_self = true)]
pub struct PlanCommand {
    /// The case directory, holding its `config.yaml`.
    pub case_dir: PathBuf,

    /// The subject OS to print the plan for: `linux`, `macos` or `windows`.
    /// Defaults to the host's OS
    #[arg(long, value_parser = str::parse::<Os>, default_value_t = Os::host(), verbatim_doc_comment)]
    pub subject_os: Os,

    /// Print the plan in a format for people instead of JSON.
    #[arg(long)]
    pub text: bool,

    /// Image or host program the case runs in place of one it names, as
    /// `<name>=<reference>`. Same semantics as on `run`: the plan shows both, and a
    /// name the case does not use is an error
    #[arg(
        long = "set",
        value_name = "NAME=REFERENCE",
        env = "PANORAMIC_SET",
        value_delimiter = ',',
        verbatim_doc_comment
    )]
    pub set: Vec<Replacement>,
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::Path;

    use clap::CommandFactory as _;

    use super::*;

    /// Environment variables a dependency reads on its own, and therefore the ones the help footer
    /// has to spell out. Grep for `env::var` under `bin/correctness/` when this list changes.
    const EXTERNAL_ENV_VARS: &[&str] = &["DOCKER_HOST", "RUST_LOG"];

    /// Panoramic's own settings, each an argument on `run` with a `PANORAMIC_*` fallback that clap
    /// renders next to the flag.
    const PANORAMIC_ENV_FALLBACKS: &[(&str, &str)] = &[("log_dir", "PANORAMIC_LOG_DIR"), ("set", "PANORAMIC_SET")];

    fn run_command_of(cli: &Cli) -> &RunCommand {
        match &cli.command {
            Command::Run(cmd) => cmd,
            Command::List(_) | Command::Plan(_) => panic!("expected a `run` invocation"),
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
    fn help_documents_every_external_environment_variable() {
        let mut command = Cli::command();
        let top_level = command.render_long_help().to_string();
        let run = command
            .find_subcommand_mut("run")
            .expect("run subcommand should exist")
            .render_long_help()
            .to_string();

        for var in EXTERNAL_ENV_VARS {
            assert!(top_level.contains(var), "`panoramic --help` does not mention {}", var);
            assert!(run.contains(var), "`panoramic run --help` does not mention {}", var);
        }
    }

    #[test]
    fn help_footer_covers_only_the_external_environment_variables() {
        // The footer is hand-written prose, so it's the one place a Panoramic-owned setting could be
        // documented twice: once by clap next to its flag, once here. Keep it to the exceptions.
        for (_, var) in PANORAMIC_ENV_FALLBACKS {
            assert!(
                !ENV_HELP.contains(var),
                "the help footer should not re-document {}",
                var
            );
        }

        for var in EXTERNAL_ENV_VARS {
            assert!(ENV_HELP.contains(var), "the help footer does not mention {}", var);
        }
    }

    #[test]
    fn help_explains_that_rust_log_outranks_the_log_level_flag() {
        let help = Cli::command().render_long_help().to_string();

        assert!(help.contains("--log-level"), "help was:\n{}", help);
        assert!(help.contains("takes precedence over"), "help was:\n{}", help);
        assert!(help.contains("External dependencies"), "help was:\n{}", help);
    }

    #[test]
    fn panoramic_settings_read_their_environment_fallbacks() {
        // Asserted against the argument model rather than by mutating the process environment,
        // which would race with the other tests in this binary.
        let command = Cli::command();
        let run = command.find_subcommand("run").expect("run subcommand should exist");

        for (id, var) in PANORAMIC_ENV_FALLBACKS {
            let arg = run
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .unwrap_or_else(|| panic!("run should have a '{}' argument", id));

            assert_eq!(arg.get_env(), Some(OsStr::new(var)), "'{}' has the wrong fallback", id);
        }
    }

    #[test]
    fn panoramic_settings_are_scoped_to_the_run_subcommand() {
        let command = Cli::command();
        let list = command.find_subcommand("list").expect("list subcommand should exist");

        for (id, _) in PANORAMIC_ENV_FALLBACKS {
            assert!(
                !list.get_arguments().any(|arg| arg.get_id() == id),
                "'{}' should not be defined on `list`",
                id
            );
        }
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
    fn zero_parallelism_is_rejected() {
        // The runner would raise zero to one and run anyway, leaving the run report to describe a
        // parallelism the run never used.
        assert!(Cli::try_parse_from(["panoramic", "run", "-d", "cases", "-p", "0"]).is_err());
        assert!(Cli::try_parse_from(["panoramic", "run", "-d", "cases", "-p", "1"]).is_ok());
    }

    #[test]
    fn defaults_match_the_pre_clap_parser() {
        let cli = Cli::try_parse_from(["panoramic", "run", "-d", "cases"]).expect("minimal run should parse");
        let cmd = run_command_of(&cli);

        assert_eq!(cmd.parallelism.get(), 4);
        assert!(matches!(cmd.output, OutputFormat::Text));
        assert_eq!(cmd.kind_cluster_name, crate::kind::DEFAULT_CLUSTER_NAME);
        assert_eq!(cmd.subject_os, Os::host());
        assert!(cmd.tests.is_none());
        assert!(!cmd.fail_fast);
        assert!(!cmd.verbose);
        assert!(!cmd.no_tui);
        assert!(!cmd.no_delete_kind_cluster);
        assert!(cmd.set.is_empty());
    }

    #[test]
    fn known_run_invocations_parse() {
        // These mirror the invocations in the Makefile, `.gitlab/`, and the `panoramic` skill.
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
        assert_eq!(cmd.parallelism.get(), 1);
        assert!(cmd
            .log_dir()
            .expect("log dir should resolve")
            .starts_with("integration-logs"));

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
            "--subject-os",
            "macos",
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
        assert_eq!(cmd.subject_os, Os::Macos);
        assert_eq!(cmd.kind_cluster_name, "custom");
        assert!(cmd.no_delete_kind_cluster);
    }

    #[test]
    fn set_accepts_a_repeated_flag_and_a_delimited_list() {
        let repeated = Cli::try_parse_from([
            "panoramic",
            "run",
            "-d",
            "cases",
            "--set",
            "saluki-images/correctness-tools:latest=registry.example.com/tools:abc123",
            "--set",
            "saluki-images/datadog-agent:testing-devel=registry.example.com/agent:abc123",
        ])
        .expect("repeated flags should parse");

        // The same pairs as one delimited value, which is the shape the environment variable takes.
        let delimited = Cli::try_parse_from([
            "panoramic",
            "run",
            "-d",
            "cases",
            "--set",
            "saluki-images/correctness-tools:latest=registry.example.com/tools:abc123,\
             saluki-images/datadog-agent:testing-devel=registry.example.com/agent:abc123",
        ])
        .expect("a delimited value should parse");

        for cmd in [run_command_of(&repeated), run_command_of(&delimited)] {
            let pairs: Vec<(&str, &str)> = cmd
                .set
                .iter()
                .map(|entry| (entry.name.as_str(), entry.reference.as_str()))
                .collect();
            assert_eq!(
                pairs,
                vec![
                    (
                        "saluki-images/correctness-tools:latest",
                        "registry.example.com/tools:abc123"
                    ),
                    (
                        "saluki-images/datadog-agent:testing-devel",
                        "registry.example.com/agent:abc123"
                    )
                ]
            );
        }

        // A value that isn't a `name=reference` pair fails at parse time rather than mid-run.
        assert!(Cli::try_parse_from(["panoramic", "run", "-d", "cases", "--set", "millstone"]).is_err());
    }

    #[test]
    fn log_level_is_global_and_defaults_to_info() {
        let cli = Cli::try_parse_from(["panoramic", "list", "-d", "cases"]).expect("minimal list should parse");
        assert_eq!(cli.log_level, LogLevel::Info);

        // `--log-level` is global, so it parses on either side of the subcommand.
        let cli = Cli::try_parse_from(["panoramic", "--log-level", "trace", "run", "-d", "cases"])
            .expect("a leading --log-level should parse");
        assert_eq!(cli.log_level, LogLevel::Trace);

        let cli = Cli::try_parse_from(["panoramic", "run", "-d", "cases", "--log-level", "DEBUG"])
            .expect("a trailing, mixed-case --log-level should parse");
        assert_eq!(cli.log_level, LogLevel::Debug);

        assert!(Cli::try_parse_from(["panoramic", "run", "-d", "cases", "--log-level", "verbose"]).is_err());
    }

    #[test]
    fn log_level_lists_every_accepted_value() {
        let help = Cli::command().render_long_help().to_string();
        assert!(
            help.contains("[possible values: off, error, warn, info, debug, trace]"),
            "help was:\n{}",
            help
        );
    }

    #[test]
    fn log_level_filters_scope_the_level_to_first_party_crates() {
        let directives = LogLevel::Debug.filter_directives();

        assert!(directives.starts_with("off,"), "directives were '{}'", directives);
        for target in FIRST_PARTY_LOG_TARGETS {
            assert!(
                directives.contains(&format!("{}=debug", target)),
                "'{}' is missing from '{}'",
                target,
                directives
            );
        }

        // `off` is a level like any other: the scoped crates go quiet, they don't fall back to a
        // default.
        assert!(LogLevel::Off.filter_directives().contains("panoramic=off"));
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
            Command::Run(_) | Command::Plan(_) => panic!("expected a `list` invocation"),
        }
    }

    #[test]
    fn log_dir_appends_a_timestamped_subdirectory() {
        let cli = Cli::try_parse_from(["panoramic", "run", "-d", "cases", "-l", "/tmp/panoramic-test"])
            .expect("run with a log dir should parse");
        let log_dir = run_command_of(&cli).log_dir().expect("log dir should resolve");

        let parent = log_dir.parent().expect("log dir should have a parent");
        assert_eq!(parent, Path::new("/tmp/panoramic-test"));
        let leaf = log_dir
            .file_name()
            .and_then(OsStr::to_str)
            .expect("log dir should have a name");
        assert!(leaf.starts_with("panoramic-"), "leaf was '{}'", leaf);
    }
}
