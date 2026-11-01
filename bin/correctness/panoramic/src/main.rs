//! Test runner for running integration tests.

#![deny(warnings)]
#![deny(missing_docs)]

use std::{io::IsTerminal, process::ExitCode};

use clap::Parser as _;
use panoramic_core::case::{CaseLoader, Registries};
use saluki_common::logging::{filter_from_env, parse_filter_directives};
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

use crate::{list_command::list_tests, run_command::run_tests};

mod kind;

mod actions;
mod artifact_commands;
mod assertions;
mod cli;
mod collector;

mod correctness;
use self::cli::{Cli, Command, LogLevel};

mod config;
mod dynamic_vars;
mod integration;
mod list_command;
mod mounts;
use self::config::typed_case::TypedConfigLoader;

mod image_override;
mod intake;

#[cfg(test)]
mod case_loader_tests;
#[cfg(test)]
mod crate_dependencies_tests;
#[cfg(test)]
mod source_boundary_tests;

/// Participant types, steps and typed-schema artifact roles available to loading and compilation.
pub(crate) const REGISTRIES: Registries<'static> = Registries {
    participant_types: panoramic_participants::REGISTERED_TYPES,
    steps: panoramic_steps::REGISTERED_STEPS,
    typed_case_roles: &[panoramic_participants::TOOLS_LINUX_IMAGE],
};

/// Loads case files with the participant types and steps this binary registers.
pub(crate) const CASE_LOADER: CaseLoader<'static, TypedConfigLoader> =
    CaseLoader::new(REGISTRIES, TypedConfigLoader, test::TestSuite::Integration.name());

mod text_output;
use self::text_output::OutputFormat;

mod providability;
mod run_command;
mod runner;
mod test;
mod test_env;
mod traffic;
mod tui;
mod unix_runner;
mod utils;

#[cfg(not(windows))]
fn default_crypto_provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::aws_lc_rs::default_provider()
}

#[cfg(windows)]
fn default_crypto_provider() -> rustls::crypto::CryptoProvider {
    rustls_cng_crypto::default_provider()
}

#[tokio::main]
async fn main() -> ExitCode {
    // Install the rustls crypto provider once at startup. reqwest is built without selecting a provider, so the
    // process-wide provider must be installed before any Rustls client configuration is built.
    let _ = default_crypto_provider().install_default();

    let cli = Cli::parse();

    // See if we should use TUI mode.
    //
    // This influences how we configure things since some output gets redirected/rendered differently in TUI mode.
    let (use_tui, is_test_run) = match &cli.command {
        Command::Run(cmd) => (
            !cmd.no_tui && matches!(cmd.output, OutputFormat::Text) && std::io::stdout().is_terminal(),
            true,
        ),
        Command::List(_) | Command::Artifacts(_) => (false, false),
    };

    if !use_tui {
        initialize_logging(cli.log_level);
        if is_test_run {
            info!("Panoramic starting...");
        }
    }

    let result = match cli.command {
        Command::Run(cmd) => run_tests(cmd, use_tui).await,
        Command::List(cmd) => list_tests(cmd).await,
        Command::Artifacts(cmd) => match cmd.command {
            cli::ArtifactsSubcommand::Render(cmd) => artifact_commands::render(cmd),
            cli::ArtifactsSubcommand::Verify(cmd) => artifact_commands::verify(cmd, &mut std::io::stdout()).await,
            cli::ArtifactsSubcommand::Pull(cmd) => artifact_commands::pull(cmd, &mut std::io::stdout()).await,
        },
    };

    if !use_tui {
        match result {
            ExitCode::SUCCESS => {
                if is_test_run {
                    info!("Panoramic stopped.")
                }
            }
            _ => error!("Panoramic stopped with errors."),
        }
    }

    result
}

fn initialize_logging(log_level: LogLevel) {
    let default_filter = parse_filter_directives(&log_level.filter_directives())
        .expect("first-party log filter directives should always be valid");

    tracing_subscriber::registry()
        .with(filter_from_env(default_filter))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(std::io::stderr().is_terminal())
                .with_target(false)
                .with_thread_ids(false)
                .compact(),
        )
        .init();
}

#[cfg(test)]
mod tests {
    use panoramic_core::layout::Os;

    use super::*;
    use crate::{
        collector::EXIT_HARNESS_ERROR,
        config::{
            discover_in_scope, runtime_for_os,
            tests::{planner, write_case, PARTICIPANT_FIELDS},
        },
    };

    #[tokio::test]
    async fn conflicting_runtime_and_subject_os_exit_2_for_run_list_and_list_json_before_discovery_or_logs() {
        use tracing::instrument::WithSubscriber as _;

        let dir = tempfile::tempdir().unwrap();
        let cases = dir.path().join("cases");
        write_case(
            &cases,
            "case",
            "type: integration\nname: case\ntimeout: 1s\nruntimes: [linux, windows]\nprocedure: []\n",
        );
        write_case(&cases, "bad", "type: bogus\n");
        let logs = dir.path().join("logs");
        for (command, extra, use_tui) in [
            ("run", vec!["-t", "not-selected", "-l", logs.to_str().unwrap()], false),
            ("run", vec!["-t", "not-selected", "-l", logs.to_str().unwrap()], true),
            ("list", vec![], false),
            ("list", vec!["--json"], false),
        ] {
            let mut args = vec![
                "panoramic",
                command,
                "-d",
                cases.to_str().unwrap(),
                "--runtime",
                "linux",
                "--subject-os",
                "windows",
            ];
            args.extend(extra);
            let output = tempfile::NamedTempFile::new().unwrap();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
                .finish();
            let exit = async {
                match Cli::try_parse_from(args).unwrap().command {
                    Command::Run(cmd) => run_tests(cmd, use_tui).await,
                    Command::List(cmd) => list_tests(cmd).await,
                    Command::Artifacts(_) => panic!("expected run or list"),
                }
            }
            .with_subscriber(subscriber)
            .await;
            assert_eq!(exit, ExitCode::from(2));
            assert!(!logs.exists());
            if !use_tui {
                let diagnostic = std::fs::read_to_string(output.path()).unwrap();
                assert!(
                    diagnostic.contains(
                        "--runtime linux selects subject OS 'linux', which conflicts with --subject-os windows"
                    ),
                    "{diagnostic}"
                );
                assert!(!diagnostic.contains("Failed to discover"), "{diagnostic}");
            }
        }
    }

    #[test]
    fn log_level_scopes_the_selected_level_to_first_party_crates() {
        let directives = parse_filter_directives(&LogLevel::Debug.filter_directives())
            .expect("valid directives")
            .to_string();

        // Parsing reorders directives, so check membership rather than the whole string.
        assert!(
            directives.contains("panoramic=debug"),
            "directives were '{}'",
            directives
        );
        assert!(directives.contains("airlock=debug"), "directives were '{}'", directives);
        assert!(directives.contains("off"), "directives were '{}'", directives);
        assert!(!directives.contains("hyper"), "directives were '{}'", directives);
    }

    #[tokio::test]
    async fn list_shows_a_participant_case_the_integration_runner_cannot_run_but_list_json_and_run_fail() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "participant",
            &PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: linux-container}"),
        );
        write_case(
            dir.path(),
            "good",
            "type: integration\nname: good\ntimeout: 1s\nruntimes: [linux]\nprocedure: []\n",
        );
        let cases = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], Os::Linux)
            .unwrap()
            .plans;
        assert_eq!(
            cases.iter().map(|plan| plan.case().name()).collect::<Vec<_>>(),
            ["good", "participant"]
        );
        for json in [false, true] {
            let mut args = vec![
                "panoramic",
                "list",
                "-d",
                dir.path().to_str().unwrap(),
                "--runtime",
                "linux",
            ];
            if json {
                args.push("--json");
            }
            let cli = Cli::try_parse_from(args).unwrap();
            let Command::List(cmd) = cli.command else {
                panic!("expected list");
            };
            let expected = if json {
                ExitCode::from(EXIT_HARNESS_ERROR)
            } else {
                ExitCode::SUCCESS
            };
            assert_eq!(list_tests(cmd).await, expected, "json: {json}");
        }
        let logs = dir.path().join("logs");
        for selection in [Some("good"), None] {
            let mut args = vec![
                "panoramic",
                "run",
                "-d",
                dir.path().to_str().unwrap(),
                "--runtime",
                "linux",
                "--no-tui",
                "-l",
                logs.to_str().unwrap(),
            ];
            if let Some(name) = selection {
                args.extend(["-t", name]);
            }
            let cli = Cli::try_parse_from(args).unwrap();
            let Command::Run(cmd) = cli.command else {
                panic!("expected run");
            };
            assert_eq!(run_tests(cmd, false).await, ExitCode::from(EXIT_HARNESS_ERROR));
            assert!(
                !logs.exists(),
                "runner construction must fail before creating the log directory"
            );
        }
    }

    #[tokio::test]
    async fn a_case_with_an_unsupported_realization_fails_list_list_json_and_run_with_exit_2() {
        let dir = tempfile::tempdir().unwrap();
        let pod = write_case(
            dir.path(),
            "b-pod",
            &PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: kubernetes-pod}"),
        );
        let windows = write_case(
            dir.path(),
            "a-windows",
            &PARTICIPANT_FIELDS
                .replace("[linux]", "[linux, windows]")
                .replace("subject: true", "subject: true, realization: {kind: linux-container}"),
        );
        let logs = dir.path().join("logs");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let message = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], os)
                .err()
                .unwrap()
                .to_string();
            assert_eq!(
                message,
                format!(
                    "Failed to plan test cases:\n  - case 'a-windows' in {}: on subject OS 'windows', \
                     participant 'adp' of type 'agent-adp' cannot run as linux-container on windows; \
                     it supports linux-container on linux, host-process-group on macos, \
                     windows-container on windows\n  - case 'b-pod' in {}: on subject OS 'linux', \
                     participant 'adp' of type 'agent-adp' cannot run as kubernetes-pod on linux; \
                     it supports linux-container on linux, host-process-group on macos, windows-container on windows",
                    windows.canonicalize().unwrap().display(),
                    pod.canonicalize().unwrap().display()
                )
            );
            for (command, extra) in [
                ("list", vec![]),
                ("list", vec!["--json"]),
                ("run", vec!["--no-tui", "-l", logs.to_str().unwrap()]),
            ] {
                let mut args = vec![
                    "panoramic",
                    command,
                    "-d",
                    dir.path().to_str().unwrap(),
                    "--runtime",
                    runtime_for_os(os),
                ];
                args.extend(extra);
                let cli = Cli::try_parse_from(args).unwrap();
                let exit = match cli.command {
                    Command::Run(cmd) => run_tests(cmd, false).await,
                    Command::List(cmd) => list_tests(cmd).await,
                    Command::Artifacts(_) => panic!("expected run or list"),
                };
                assert_eq!(exit, ExitCode::from(2), "{command} under {os:?}");
                assert!(!logs.exists(), "layout errors must precede log directory creation");
            }
        }
    }
}
