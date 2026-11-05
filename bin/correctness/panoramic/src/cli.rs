//! Defines Panoramic's command-line interface.

use std::num::NonZeroUsize;
use std::path::PathBuf;

use airlock::driver::DEFAULT_ALPINE_IMAGE;
use chrono::Local;
use clap::{
    builder::{PossibleValuesParser, TypedValueParser},
    Args, Parser, Subcommand, ValueEnum,
};
use panoramic_core::layout::Os;

use crate::config::{
    os_for_runtime, runner_artifacts::LegacyArtifactInputs, runtime_for_os, LINUX_RUNTIME, MAC_RUNTIME, WINDOWS_RUNTIME,
};
use crate::text_output::OutputFormat;
use crate::{image_override::ImageOverride, test::RunnerSettings};

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

/// Parses `--runtime`, accepting only the runtime identifiers and returning the subject OS each selects.
fn runtime_parser() -> impl TypedValueParser<Value = Os> {
    PossibleValuesParser::new([LINUX_RUNTIME, MAC_RUNTIME, WINDOWS_RUNTIME])
        .map(|value| os_for_runtime(&value).expect("only runtime identifiers are possible values"))
}

fn subject_os_parser() -> impl TypedValueParser<Value = Os> {
    let systems = [Os::Linux, Os::MacOs, Os::Windows];
    PossibleValuesParser::new(systems.map(Os::name)).map(move |value| {
        systems
            .into_iter()
            .find(|os| os.name() == value)
            .expect("only OS names are possible values")
    })
}

/// Subject OS selection shared by `run` and `list`.
#[derive(Args, Debug)]
pub struct SubjectOsArgs {
    /// Subject OS selection using runtime names: `linux`, `mac` or `windows`;
    /// `mac` selects `macos`. Participant cases run only if their `subject_os` list
    /// contains its OS. Defaults to the host's OS. Correctness cases run only for subject OS
    /// `linux`; other selections report them as skipped.
    #[arg(long, verbatim_doc_comment, value_parser = runtime_parser())]
    pub runtime: Option<Os>,

    /// Subject OS to select cases for: `linux`, `macos` or `windows`. Defaults to the OS
    /// that --runtime selects, then the host's OS. Must agree with --runtime when both
    /// are given. Correctness cases run only for subject OS `linux`; other selections
    /// report them as skipped.
    #[arg(long, verbatim_doc_comment, value_parser = subject_os_parser())]
    pub subject_os: Option<Os>,
}

impl SubjectOsArgs {
    /// Returns the selected subject OS.
    ///
    /// # Errors
    ///
    /// Returns a message when --runtime and --subject-os select different OSes.
    pub fn resolve(&self) -> Result<Os, String> {
        if let (Some(runtime), Some(subject_os)) = (self.runtime, self.subject_os) {
            if runtime != subject_os {
                return Err(format!(
                    "--runtime {} selects subject OS '{}', which conflicts with --subject-os {}",
                    runtime_for_os(runtime),
                    runtime.name(),
                    subject_os.name()
                ));
            }
        }
        Ok(self.subject_os.or(self.runtime).unwrap_or_else(Os::host))
    }
}

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
// CLI arg: --alpine-image
// ENV var: PANORAMIC_ALPINE_IMAGE
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

    /// Manage the artifacts cases need.
    Artifacts(ArtifactsCommand),
}

#[derive(Args)]
pub struct ArtifactsCommand {
    #[command(subcommand)]
    pub command: ArtifactsSubcommand,
}

#[derive(Subcommand)]
pub enum ArtifactsSubcommand {
    /// Write an artifact manifest from role bindings.
    Render(RenderCommand),

    /// Verify needed artifacts and print their identities without pulling images.
    Verify(ArtifactCasesCommand),

    /// Pull needed images and print their identities.
    Pull(ArtifactCasesCommand),
}

#[derive(Args)]
#[command(args_override_self = true)]
pub struct RenderCommand {
    /// Binds an artifact role to a reference, as ROLE=REFERENCE (can be specified multiple times).
    /// Only the first `=` separates the role.
    #[arg(long, value_name = "ROLE=REFERENCE", value_parser = artifact_binding_parser, verbatim_doc_comment)]
    pub set: Vec<(String, String)>,

    /// File to write the manifest to; an existing file is replaced.
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: PathBuf,
}

fn artifact_binding_parser(value: &str) -> Result<(String, String), String> {
    value
        .split_once('=')
        .map(|(role, reference)| (role.to_string(), reference.to_string()))
        .ok_or_else(|| "expected ROLE=REFERENCE".to_string())
}

#[derive(Args)]
#[command(args_override_self = true)]
pub struct ArtifactCasesCommand {
    /// Path to a test cases directory (can be specified multiple times).
    #[arg(short = 'd', long)]
    pub test_dirs: Vec<PathBuf>,

    /// Subject OS to select cases for: `linux`, `macos` or `windows`. Defaults to the host's OS.
    #[arg(long, value_parser = subject_os_parser(), default_value = Os::host().name())]
    pub subject_os: Os,

    /// Artifact manifest (TOML) binding artifact roles to image references and paths.
    /// Roles it omits use their local defaults; a role no participant type, backend or
    /// typed case schema declares is an error. Each needed role is printed with the
    /// reference it resolves to.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    pub artifacts: Option<PathBuf>,
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

    #[command(flatten)]
    pub subject_os: SubjectOsArgs,

    /// Artifact manifest (TOML) binding artifact roles to image references and paths.
    /// Roles it omits use their local defaults; a role no participant type, backend or
    /// typed case schema declares is an error. Every needed artifact is verified
    /// before any case starts. Explicit --image-override and --alpine-image values,
    /// including their PANORAMIC_*
    /// environment variables, must name the same reference as the role they feed,
    /// or the run exits 2.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    pub artifacts: Option<PathBuf>,

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

    /// Name of the kind cluster to create or reuse for kind-runtime tests.
    #[arg(long, default_value = crate::kind::DEFAULT_CLUSTER_NAME)]
    pub kind_cluster_name: String,

    /// Don't delete the kind cluster after kind-runtime tests complete (useful for
    /// local iteration)
    #[arg(long, verbatim_doc_comment)]
    pub no_delete_kind_cluster: bool,

    #[arg(long, env = "PANORAMIC_ALPINE_IMAGE", help = format!(
        "Alpine image for the short-lived container that fixes up shared-volume\npermissions. CI points this at an internal registry [default: {}]",
        DEFAULT_ALPINE_IMAGE
    ))]
    alpine_image: Option<String>,

    /// Image to run for one of a test's containers, as `<name>=<image>`, replacing
    /// what the test case declares. Repeat the flag to name more than one container; the
    /// environment variable takes them as one comma-separated list. CI uses this to run
    /// the cases against the images built for the commit. `panoramic list --json`
    /// reports the container names each test declares, and a name no test in the run
    /// declares is an error
    #[arg(
        long = "image-override",
        value_name = "NAME=IMAGE",
        env = "PANORAMIC_IMAGE_OVERRIDES",
        value_delimiter = ',',
        verbatim_doc_comment
    )]
    pub image_overrides: Vec<ImageOverride>,
}

impl RunCommand {
    /// Collects the settings that the runner hands to every test.
    pub(crate) fn runner_settings(&self) -> RunnerSettings {
        RunnerSettings {
            alpine_image: self.alpine_image.clone().unwrap_or_else(|| DEFAULT_ALPINE_IMAGE.into()),
        }
    }

    /// Collects artifact inputs explicitly given by flags or environment variables.
    pub(crate) fn legacy_artifact_inputs(&self) -> LegacyArtifactInputs<'_> {
        LegacyArtifactInputs {
            image_overrides: &self.image_overrides,
            alpine_image: self.alpine_image.as_deref(),
        }
    }

    /// Gets the log directory for this run.
    ///
    /// The base comes from `--log-dir`/`PANORAMIC_LOG_DIR`, falling back to a temporary directory.
    pub fn log_dir(&self) -> PathBuf {
        let base = self.log_dir.clone().unwrap_or_else(std::env::temp_dir);

        // Always append a timestamped subdirectory so artifacts from separate runs never mix. The
        // reports name the directory they landed in.
        let timestamp = Local::now().format("%Y%m%d-%H%M%S");
        base.join(format!("panoramic-{}", timestamp))
    }
}

/// List available integration tests.
#[derive(Args)]
#[command(args_override_self = true)]
pub struct ListCommand {
    /// Path to a test cases directory (can be specified multiple times).
    #[arg(short = 'd', long)]
    pub test_dirs: Vec<PathBuf>,

    #[command(flatten)]
    pub subject_os: SubjectOsArgs,

    /// Artifact manifest (TOML) binding artifact roles to image references and paths.
    /// Roles it omits use their local defaults; a role no participant type, backend or
    /// typed case schema declares is an error. The text listing shows the reference
    /// each role resolves to.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    pub artifacts: Option<PathBuf>,

    /// Output the discovered tests as json along with their image dependencies. A
    /// `ci` script depends on this for dynamic pipeline creation
    #[arg(long, verbatim_doc_comment)]
    pub json: bool,
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
    const PANORAMIC_ENV_FALLBACKS: &[(&str, &str)] = &[
        ("alpine_image", "PANORAMIC_ALPINE_IMAGE"),
        ("log_dir", "PANORAMIC_LOG_DIR"),
        ("image_overrides", "PANORAMIC_IMAGE_OVERRIDES"),
    ];

    fn run_command_of(cli: &Cli) -> &RunCommand {
        match &cli.command {
            Command::Run(cmd) => cmd,
            Command::List(_) | Command::Artifacts(_) => panic!("expected a `run` invocation"),
        }
    }

    #[test]
    fn run_and_list_reject_an_unknown_runtime() {
        for command in ["run", "list"] {
            let error = Cli::try_parse_from(["panoramic", command, "--runtime", "docker"])
                .err()
                .unwrap();
            assert_eq!(error.exit_code(), 2);
            let message = error.to_string();
            assert!(message.contains("docker"), "{message}");
            assert!(message.contains("[possible values: linux, mac, windows]"), "{message}");
            for (runtime, expected) in [("linux", Os::Linux), ("mac", Os::MacOs), ("windows", Os::Windows)] {
                let cli = Cli::try_parse_from(["panoramic", command, "--runtime", runtime]).unwrap();
                let os = match cli.command {
                    Command::Run(cmd) => cmd.subject_os.runtime,
                    Command::List(cmd) => cmd.subject_os.runtime,
                    Command::Artifacts(_) => panic!("expected run or list"),
                };
                assert_eq!(os, Some(expected));
            }
            for runtime in ["macos", "Linux", "unknown"] {
                assert!(Cli::try_parse_from(["panoramic", command, "--runtime", runtime]).is_err());
            }
        }
    }

    fn os_args(cli: Cli) -> SubjectOsArgs {
        match cli.command {
            Command::Run(cmd) => cmd.subject_os,
            Command::List(cmd) => cmd.subject_os,
            Command::Artifacts(_) => panic!("expected run or list"),
        }
    }

    #[test]
    fn subject_os_accepts_only_case_file_os_names_on_run_and_list() {
        for command in ["run", "list"] {
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                let args = os_args(Cli::try_parse_from(["panoramic", command, "--subject-os", os.name()]).unwrap());
                assert_eq!(args.resolve(), Ok(os));
            }
            for name in ["mac", "docker", "Linux"] {
                let error = Cli::try_parse_from(["panoramic", command, "--subject-os", name])
                    .err()
                    .unwrap();
                assert_eq!(error.exit_code(), 2);
            }
        }
    }

    #[test]
    fn the_subject_os_defaults_to_the_runtime_then_the_host() {
        for command in ["run", "list"] {
            assert_eq!(
                os_args(Cli::try_parse_from(["panoramic", command]).unwrap()).resolve(),
                Ok(Os::host())
            );
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                let args =
                    os_args(Cli::try_parse_from(["panoramic", command, "--runtime", runtime_for_os(os)]).unwrap());
                assert_eq!(args.resolve(), Ok(os));
            }
        }
    }

    #[test]
    fn a_subject_os_that_conflicts_with_the_runtime_is_an_error_and_an_equal_one_is_not() {
        for command in ["run", "list"] {
            for runtime in [Os::Linux, Os::MacOs, Os::Windows] {
                for subject_os in [Os::Linux, Os::MacOs, Os::Windows] {
                    let args = os_args(
                        Cli::try_parse_from([
                            "panoramic",
                            command,
                            "--runtime",
                            runtime_for_os(runtime),
                            "--subject-os",
                            subject_os.name(),
                        ])
                        .unwrap(),
                    );
                    let expected = if runtime == subject_os {
                        Ok(subject_os)
                    } else {
                        Err(format!(
                            "--runtime {} selects subject OS '{}', which conflicts with --subject-os {}",
                            runtime_for_os(runtime),
                            runtime.name(),
                            subject_os.name()
                        ))
                    };
                    assert_eq!(args.resolve(), expected);
                }
            }
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
        assert!(cmd.subject_os.runtime.is_none());
        assert!(cmd.tests.is_none());
        assert!(!cmd.fail_fast);
        assert!(!cmd.verbose);
        assert!(!cmd.no_tui);
        assert!(!cmd.no_delete_kind_cluster);
        assert!(cmd.image_overrides.is_empty());
    }

    #[test]
    fn image_overrides_accept_a_repeated_flag_and_a_delimited_list() {
        let repeated = Cli::try_parse_from([
            "panoramic",
            "run",
            "-d",
            "cases",
            "--image-override",
            "millstone=registry.example.com/tools:abc123",
            "--image-override",
            "baseline=registry.example.com/agent:abc123",
        ])
        .expect("repeated flags should parse");

        // The same pairs as one delimited value, which is the shape the environment variable takes.
        let delimited = Cli::try_parse_from([
            "panoramic",
            "run",
            "-d",
            "cases",
            "--image-override",
            "millstone=registry.example.com/tools:abc123,baseline=registry.example.com/agent:abc123",
        ])
        .expect("a delimited value should parse");

        for cmd in [run_command_of(&repeated), run_command_of(&delimited)] {
            let pairs: Vec<(&str, &str)> = cmd
                .image_overrides
                .iter()
                .map(|entry| (entry.name.as_str(), entry.image.as_str()))
                .collect();
            assert_eq!(
                pairs,
                vec![
                    ("millstone", "registry.example.com/tools:abc123"),
                    ("baseline", "registry.example.com/agent:abc123")
                ]
            );
        }

        // A value that isn't a `name=image` pair fails at parse time rather than mid-run.
        assert!(Cli::try_parse_from(["panoramic", "run", "-d", "cases", "--image-override", "millstone"]).is_err());
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
        assert_eq!(cmd.subject_os.runtime, Some(Os::MacOs));
        assert_eq!(cmd.kind_cluster_name, "custom");
        assert!(cmd.no_delete_kind_cluster);
    }

    #[test]
    fn runner_settings_fall_back_to_the_documented_defaults() {
        let cli = Cli::try_parse_from(["panoramic", "run", "-d", "cases"]).expect("minimal run should parse");
        let settings = run_command_of(&cli).runner_settings();

        assert_eq!(settings.alpine_image, DEFAULT_ALPINE_IMAGE);
    }

    #[test]
    fn explicit_run_flags_plumb_into_runner_settings() {
        let cli = Cli::try_parse_from([
            "panoramic",
            "run",
            "-d",
            "cases",
            "--alpine-image",
            "registry.example/alpine:3.20",
        ])
        .expect("run with explicit paths should parse");
        let settings = run_command_of(&cli).runner_settings();

        assert_eq!(settings.alpine_image, "registry.example/alpine:3.20");
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
    fn list_and_run_accept_an_artifacts_file_without_an_environment_fallback() {
        for name in ["list", "run"] {
            let command = Cli::try_parse_from(["panoramic", name, "--artifacts", "artifacts.toml"])
                .unwrap()
                .command;
            let artifacts = match command {
                Command::List(cmd) => cmd.artifacts,
                Command::Run(cmd) => cmd.artifacts,
                _ => panic!("expected list or run"),
            };
            assert_eq!(artifacts, Some(PathBuf::from("artifacts.toml")));
            let command = Cli::command();
            let argument = command
                .find_subcommand(name)
                .unwrap()
                .get_arguments()
                .find(|argument| argument.get_id() == "artifacts")
                .unwrap();
            assert_eq!(argument.get_env(), None);
        }
    }

    #[test]
    fn legacy_artifact_inputs_contain_only_explicitly_given_values() {
        let cli = Cli::try_parse_from(["panoramic", "run"]).unwrap();
        let inputs = run_command_of(&cli).legacy_artifact_inputs();
        assert!(inputs.image_overrides.is_empty());
        assert_eq!(inputs.alpine_image, None);

        let cli = Cli::try_parse_from([
            "panoramic",
            "run",
            "--alpine-image",
            "registry.example/alpine:latest",
            "--image-override",
            "comparison=agent:bound,datadog-intake=tools:bound",
        ])
        .unwrap();
        let inputs = run_command_of(&cli).legacy_artifact_inputs();
        assert_eq!(inputs.alpine_image, Some("registry.example/alpine:latest"));
        assert_eq!(
            inputs
                .image_overrides
                .iter()
                .map(|entry| (entry.name.as_str(), entry.image.as_str()))
                .collect::<Vec<_>>(),
            [("comparison", "agent:bound"), ("datadog-intake", "tools:bound")]
        );
    }

    #[test]
    fn run_artifacts_combines_with_non_artifact_run_flags() {
        let cli = Cli::try_parse_from([
            "panoramic",
            "run",
            "--artifacts",
            "artifacts.toml",
            "--kind-cluster-name",
            "cluster",
            "-l",
            "logs",
            "--no-tui",
        ])
        .unwrap();
        let cmd = run_command_of(&cli);
        assert_eq!(cmd.artifacts.as_deref(), Some(Path::new("artifacts.toml")));
        assert_eq!(cmd.kind_cluster_name, "cluster");
        assert_eq!(cmd.log_dir.as_deref(), Some(Path::new("logs")));
        assert!(cmd.no_tui);
    }

    #[test]
    fn artifact_help_names_typed_case_schemas_as_role_declarers() {
        for path in [
            vec!["list"],
            vec!["run"],
            vec!["artifacts", "verify"],
            vec!["artifacts", "pull"],
        ] {
            let mut command = Cli::command();
            command.build();
            let mut subcommand = &mut command;
            for name in path {
                subcommand = subcommand.find_subcommand_mut(name).unwrap();
            }
            let help = subcommand.render_long_help().to_string();
            assert!(help.contains("typed case schema declares is an error."), "{help}");
        }
    }

    #[test]
    fn artifacts_render_accepts_repeated_set_pairs_and_an_output_file() {
        let Command::Artifacts(ArtifactsCommand {
            command: ArtifactsSubcommand::Render(cmd),
        }) = Cli::try_parse_from([
            "panoramic",
            "artifacts",
            "render",
            "--set",
            "b=y",
            "-o",
            "first.toml",
            "--set",
            "a=x",
            "-o",
            "last.toml",
        ])
        .unwrap()
        .command
        else {
            panic!("expected artifacts render")
        };
        assert_eq!(cmd.set, vec![("b".into(), "y".into()), ("a".into(), "x".into())]);
        assert_eq!(cmd.output, PathBuf::from("last.toml"));
    }

    #[test]
    fn artifacts_render_splits_each_pair_at_its_first_equals_sign() {
        for (pair, expected) in [("a=b=c", ("a", "b=c")), ("a=", ("a", "")), ("=b", ("", "b"))] {
            let Command::Artifacts(ArtifactsCommand {
                command: ArtifactsSubcommand::Render(cmd),
            }) = Cli::try_parse_from([
                "panoramic",
                "artifacts",
                "render",
                "--set",
                pair,
                "--output",
                "out.toml",
            ])
            .unwrap()
            .command
            else {
                panic!("expected artifacts render")
            };
            assert_eq!(cmd.set, vec![(expected.0.into(), expected.1.into())]);
            assert_eq!(cmd.output, PathBuf::from("out.toml"));
        }
    }

    #[test]
    fn artifacts_render_rejects_pairs_without_equals_and_requires_an_output_file() {
        let error = Cli::try_parse_from(["panoramic", "artifacts", "render", "--set", "role", "-o", "out.toml"])
            .err()
            .unwrap();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("expected ROLE=REFERENCE"), "{error}");
        let error = Cli::try_parse_from(["panoramic", "artifacts", "render"]).err().unwrap();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("--output <FILE>"), "{error}");
    }

    #[test]
    fn artifacts_render_takes_no_test_directories_subject_os_or_manifest() {
        for (flag, value) in [
            ("-d", "cases"),
            ("--test-dirs", "cases"),
            ("--subject-os", "linux"),
            ("--artifacts", "artifacts.toml"),
        ] {
            let error = Cli::try_parse_from(["panoramic", "artifacts", "render", "-o", "out.toml", flag, value])
                .err()
                .unwrap();
            assert_eq!(error.exit_code(), 2);
            assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        }
    }

    #[test]
    fn artifacts_verify_and_pull_accept_directories_subject_os_and_manifest_but_not_runtime() {
        for subcommand in ["verify", "pull"] {
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                let Command::Artifacts(ArtifactsCommand { command }) = Cli::try_parse_from([
                    "panoramic",
                    "artifacts",
                    subcommand,
                    "-d",
                    "cases",
                    "--test-dirs",
                    "other",
                    "--subject-os",
                    os.name(),
                    "--artifacts",
                    "artifacts.toml",
                ])
                .unwrap()
                .command
                else {
                    panic!("expected artifacts {subcommand}")
                };
                let cmd = match (subcommand, command) {
                    ("verify", ArtifactsSubcommand::Verify(cmd)) | ("pull", ArtifactsSubcommand::Pull(cmd)) => cmd,
                    _ => panic!("expected artifacts {subcommand}"),
                };
                assert_eq!(cmd.test_dirs, vec![PathBuf::from("cases"), PathBuf::from("other")]);
                assert_eq!(cmd.subject_os, os);
                assert_eq!(cmd.artifacts, Some(PathBuf::from("artifacts.toml")));
            }
            let command = Cli::command();
            let cases_command = command
                .find_subcommand("artifacts")
                .unwrap()
                .find_subcommand(subcommand)
                .unwrap();
            assert_eq!(
                cases_command
                    .get_arguments()
                    .find(|argument| argument.get_id() == "artifacts")
                    .unwrap()
                    .get_env(),
                None
            );
            for (flag, value) in [
                ("--runtime", "linux"),
                ("--subject-os", "mac"),
                ("--subject-os", "Linux"),
            ] {
                let error = Cli::try_parse_from(["panoramic", "artifacts", subcommand, flag, value])
                    .err()
                    .unwrap();
                assert_eq!(error.exit_code(), 2);
            }
        }
    }

    #[test]
    fn artifacts_verify_and_pull_default_to_the_host_subject_os() {
        for subcommand in ["verify", "pull"] {
            let Command::Artifacts(ArtifactsCommand { command }) =
                Cli::try_parse_from(["panoramic", "artifacts", subcommand])
                    .unwrap()
                    .command
            else {
                panic!("expected artifacts {subcommand}")
            };
            let cmd = match (subcommand, command) {
                ("verify", ArtifactsSubcommand::Verify(cmd)) | ("pull", ArtifactsSubcommand::Pull(cmd)) => cmd,
                _ => panic!("expected artifacts {subcommand}"),
            };
            assert_eq!(cmd.subject_os, Os::host());
            assert!(cmd.test_dirs.is_empty());
            assert!(cmd.artifacts.is_none());
        }
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
            Command::Run(_) | Command::Artifacts(_) => panic!("expected a `list` invocation"),
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
