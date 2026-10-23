//! Test runner for running integration tests.

#![deny(warnings)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::{io::IsTerminal, process::ExitCode};

use clap::Parser as _;
use saluki_common::logging::{filter_from_env, parse_filter_directives};
use tokio::sync::{watch, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

use crate::runner::Runner;

mod kind;
use self::kind::KindLifecycle;

mod actions;
mod assertions;
mod cli;
mod collector;
use self::collector::{
    consume_run, run_teardown, RunCollector, RunDisplay, RunEnd, EXIT_HARNESS_ERROR, EXIT_NO_TESTS_SELECTED,
};

mod correctness;
use self::cli::{Cli, Command, LogLevel};

mod config;
mod dynamic_vars;
mod integration;
mod mounts;
use self::config::{default_host_runtime, discover_tests};

mod events;
use self::events::create_event_channel;

mod image_override;
mod intake;
mod interrupts;
use self::interrupts::Interrupts;

mod machine_output;
use self::machine_output::RunReport;

mod reporter;
use self::reporter::{OutputFormat, Reporter};

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
        Command::List(_) => (false, false),
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

/// Splits the `-t` selection into the tests that matched and the names that matched nothing.
fn resolve_selection(requested: &str, discovered: &[Box<dyn test::Test>]) -> (Vec<String>, Vec<String>) {
    let discovered: Vec<String> = discovered.iter().map(|t| t.name()).collect();
    let mut selected = Vec::new();
    let mut unmatched = Vec::new();

    for name in requested.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if discovered.iter().any(|d| d == name) {
            selected.push(name.to_string());
        } else {
            unmatched.push(name.to_string());
        }
    }

    (selected, unmatched)
}

async fn run_tests(cmd: cli::RunCommand, use_tui: bool) -> ExitCode {
    let settings = cmd.runner_settings();
    debug!(
        adp_binary_path = %settings.adp_binary_path.display(),
        core_agent_binary_path = %settings.core_agent_binary_path.display(),
        mounts_dir = %settings.mounts_dir.display(),
        alpine_image = %settings.alpine_image,
        "Resolved runner settings."
    );

    if cmd.test_dirs.is_empty() {
        let msg = "No test directories specified. Use -d <path> to specify one or more directories.";
        if use_tui {
            eprintln!("{}", msg);
        } else {
            error!("{}", msg);
        }
        return ExitCode::from(2);
    }

    let integration_runtime = cmd
        .runtime
        .clone()
        .unwrap_or_else(|| default_host_runtime().to_string());
    let test_cases = match discover_tests(&cmd.test_dirs, &integration_runtime, &cmd.image_overrides) {
        Ok(tests) => tests,
        Err(e) => {
            if use_tui {
                eprintln!("Failed to discover tests: {}", e);
            } else {
                error!("Failed to discover tests: {}", e);
            }
            return ExitCode::from(2);
        }
    };

    if test_cases.is_empty() {
        let dirs_str: Vec<_> = cmd.test_dirs.iter().map(|d| d.display().to_string()).collect();
        let msg = format!("No test cases found in: {}", dirs_str.join(", "));
        if use_tui {
            eprintln!("{}", msg);
        } else {
            error!("{}", msg);
        }
        return ExitCode::from(2);
    }

    // Resolve the `-t` selection before doing any work: a name that matches nothing, or a
    // selection that leaves nothing to run, is an error rather than a green run of zero tests.
    let selected_tests = match cmd.tests.as_deref() {
        Some(requested) => {
            let (selected, unmatched) = resolve_selection(requested, &test_cases);
            if !unmatched.is_empty() {
                let msg = format!(
                    "No test matches: {}. Run 'panoramic list' to see the tests in scope for runtime '{}'.",
                    unmatched.join(", "),
                    integration_runtime
                );
                if use_tui {
                    eprintln!("{}", msg);
                } else {
                    error!("{}", msg);
                }
                return ExitCode::from(EXIT_NO_TESTS_SELECTED);
            }
            if selected.is_empty() {
                let msg = "No tests selected. Drop -t, or name at least one test.";
                if use_tui {
                    eprintln!("{}", msg);
                } else {
                    error!("{}", msg);
                }
                return ExitCode::from(EXIT_NO_TESTS_SELECTED);
            }
            Some(selected)
        }
        None => None,
    };
    // Register only the selected tests so the runner and `run.json` share one selection.
    let test_cases: Vec<Box<dyn test::Test>> = match selected_tests {
        Some(names) => test_cases.into_iter().filter(|t| names.contains(&t.name())).collect(),
        None => test_cases,
    };

    // Create log directory.
    let log_dir = match std::path::absolute(cmd.log_dir()) {
        Ok(path) => path,
        Err(e) => {
            error!("Failed to resolve log directory: {}", e);
            return ExitCode::from(2);
        }
    };
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        if use_tui {
            eprintln!("Failed to create log directory: {}", e);
        } else {
            error!("Failed to create log directory: {}", e);
        }
        return ExitCode::from(2);
    }

    // Install interrupt handlers before starting background work.
    let mut interrupts = Interrupts::install();
    let mut collector = RunCollector::start(
        RunReport::new(
            cmd.test_dirs.clone(),
            integration_runtime.clone(),
            cmd.parallelism.get(),
            log_dir.clone(),
            test_cases.iter().map(|t| t.name()).collect(),
        ),
        cmd.fail_fast,
    );

    // Create the event channel early so the kind setup task can emit status messages.
    let (tx, rx) = create_event_channel();

    // Spawn kind cluster setup in the background so non-kind tests start immediately.
    // Kind tests will wait on `kind_rx` before doing any work.
    let kind_images = collect_kind_images(&test_cases);
    let kind_lifecycle_slot = std::sync::Arc::new(Mutex::new(None::<KindLifecycle>));
    let kind_rx = if kind_images.is_empty() {
        None
    } else {
        let (kind_tx, kind_rx) = watch::channel::<Option<Result<(), String>>>(None);
        let slot = kind_lifecycle_slot.clone();
        let cluster_name = cmd.kind_cluster_name.clone();
        let event_tx = tx.clone();
        tokio::spawn(async move {
            match KindLifecycle::ensure(cluster_name, kind_images, event_tx).await {
                Ok(lc) => {
                    *slot.lock().await = Some(lc);
                    let _ = kind_tx.send(Some(Ok(())));
                }
                Err(e) => {
                    let _ = kind_tx.send(Some(Err(format!("{:?}", e))));
                }
            }
        });
        Some(kind_rx)
    };

    // Inject runtime config and build the test registry.
    let mut registry = Runner::new(log_dir.clone(), settings);
    if let Some(ref rx) = kind_rx {
        registry = registry.with_kind_ready(rx.clone());
    }
    for tc in test_cases {
        registry.register(tc).expect("failure to register test");
    }

    // The consume loop cancels this on the first interrupt.
    let cancel_all = CancellationToken::new();

    // Build run args.
    let args = runner::RunArgs::new(cancel_all.clone(), tx)
        .with_parallelism(cmd.parallelism)
        .with_fail_fast(cmd.fail_fast);

    // Spawn the test runner task (same code path for both modes).
    let runner_handle = tokio::spawn(async move { registry.run_tests(args).await });

    let mut display_error = None;
    let display = if use_tui {
        match tui::Tui::new(&log_dir) {
            Ok(tui) => Some(RunDisplay::Tui(tui)),
            Err(e) => {
                display_error = Some(e);
                None
            }
        }
    } else {
        info!("Starting test run with parallelism of {}...", cmd.parallelism);
        info!("Container logs will be written to '{}'.", log_dir.display());
        if cmd.fail_fast {
            info!("Fail-fast mode enabled; will stop on first failure.");
        }
        Some(RunDisplay::Plain(Reporter::new(
            cmd.output,
            cmd.verbose,
            Some(log_dir.clone()),
        )))
    };

    let consumed = consume_run(rx, runner_handle, &mut collector, display, &mut interrupts, &cancel_all).await;
    let display_error = display_error.or(consumed.display_error);
    if let Some(e) = &display_error {
        eprintln!("The TUI failed: {}", e);
    }

    // Tear down the kind cluster unless the caller asked to keep it.
    if kind_rx.is_some() {
        // The TUI has shut down by now, so its notices go to stderr directly.
        let notice = |message: String| {
            if use_tui {
                eprintln!("{}", message);
            } else {
                warn!("{}", message);
            }
        };
        let cluster_name = &cmd.kind_cluster_name;

        if consumed.end == RunEnd::Abandoned {
            notice(format!(
                "Skipping kind cluster teardown after a second interrupt. Cluster '{}' may still be running; \
                 delete it with 'kind delete cluster --name {}'.",
                cluster_name, cluster_name
            ));
        } else {
            let lifecycle: Option<KindLifecycle> = kind_lifecycle_slot.lock().await.take();
            if let Some(lifecycle) = lifecycle {
                if cmd.no_delete_kind_cluster {
                    info!(
                        "Skipping kind cluster teardown (--no-delete-kind-cluster). \
                         Cluster '{}' is still running.",
                        cluster_name
                    );
                } else if let Some(source) = run_teardown(lifecycle.teardown(), &mut collector, &mut interrupts).await {
                    notice(format!(
                        "Received {}; abandoned kind cluster teardown. A 'kind delete' already started may still \
                         finish. If cluster '{}' is still running, delete it with 'kind delete cluster --name {}'.",
                        source, cluster_name, cluster_name
                    ));
                }
            } else {
                // Failed or unfinished setup can leave a cluster behind.
                notice(format!(
                    "Kind cluster setup did not complete successfully. \
                     A kind cluster named '{}' may still be running — run 'kind delete cluster --name {}' to clean it up.",
                    cluster_name, cluster_name
                ));
            }
        }
    }

    // Print after teardown so the report includes an interrupt during teardown.
    let mut json_output_failed = false;
    if matches!(cmd.output, OutputFormat::Json) {
        match serde_json::to_string_pretty(collector.report()) {
            Ok(json) => println!("{}", json),
            Err(e) => {
                error!("Failed to serialize the machine-readable run report: {}", e);
                json_output_failed = true;
            }
        }
    }

    // Report output failures as harness errors, even when the tests passed.
    if json_output_failed || display_error.is_some() {
        return ExitCode::from(EXIT_HARNESS_ERROR);
    }

    collector::exit_code(collector.report())
}

/// Collects the unique set of images required by all kind-runtime tests in the given list.
fn collect_kind_images(tests: &[Box<dyn test::Test>]) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut images = BTreeSet::new();
    for test in tests {
        if test.runtime() == "kubernetes_in_docker" {
            for (_, image) in test.images() {
                images.insert(image);
            }
        }
    }
    images.into_iter().collect()
}

async fn list_tests(cmd: cli::ListCommand) -> ExitCode {
    if cmd.test_dirs.is_empty() {
        error!("No test directories specified. Use -d <path> to specify one or more directories.");
        return ExitCode::from(2);
    }

    if !cmd.json {
        let dirs_str: Vec<_> = cmd.test_dirs.iter().map(|d| d.display().to_string()).collect();
        info!("Discovering test cases from: {}...", dirs_str.join(", "));
    }

    let integration_runtime = cmd
        .runtime
        .clone()
        .unwrap_or_else(|| default_host_runtime().to_string());
    let test_cases = match discover_tests(&cmd.test_dirs, &integration_runtime, &[]) {
        Ok(tests) => tests,
        Err(e) => {
            error!("Failed to discover tests: {}", e);
            return ExitCode::from(2);
        }
    };

    if cmd.json {
        let mut test_map = BTreeMap::new();
        for test in &test_cases {
            test_map.insert(
                test.name(),
                serde_json::json!({
                    "type": test.suite(),
                    "runtime": test.runtime(),
                    "timeout": test.timeout(),
                    "images": test.images(),
                }),
            );
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&test_map).expect("Unable to serialize a map of the tests")
        )
    } else {
        if test_cases.is_empty() {
            info!("No test cases found.");
            return ExitCode::SUCCESS;
        }

        info!("Discovered {} test case(s).", test_cases.len());

        println!();
        println!("Available tests ({}):", test_cases.len());
        println!();

        for test_case in &test_cases {
            println!("  {}", test_case.name());
            if let Some(description) = test_case.description() {
                println!("    {}", description);
            }
            println!("    Timeout: {:?}", test_case.timeout());
            println!();
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn image_override_validation_precedes_name_selection() {
        let dir = tempfile::tempdir().unwrap();
        let case_dir = dir.path().join("unselected");
        std::fs::create_dir(&case_dir).unwrap();
        std::fs::write(
            case_dir.join("config.yaml"),
            r#"
type: correctness
runtime: docker
analysis_mode: metrics
baseline: {image: base}
comparison: {image: comp}
"#,
        )
        .unwrap();
        for (entry, expected) in [
            ("millstone=tools:custom", EXIT_NO_TESTS_SELECTED),
            ("typo=tools:custom", EXIT_HARNESS_ERROR),
        ] {
            let cli = Cli::try_parse_from([
                "panoramic",
                "run",
                "-d",
                dir.path().to_str().unwrap(),
                "-t",
                "not-discovered",
                "--image-override",
                entry,
            ])
            .unwrap();
            let Command::Run(cmd) = cli.command else {
                panic!("expected run")
            };
            assert_eq!(run_tests(cmd, false).await, ExitCode::from(expected));
        }
    }

    #[test]
    fn selection_reports_names_that_match_nothing() {
        let discovered: Vec<Box<dyn test::Test>> = Vec::new();
        let (selected, unmatched) = resolve_selection("no-such-test", &discovered);
        assert!(selected.is_empty());
        assert_eq!(unmatched, vec!["no-such-test".to_string()]);

        // An empty or whitespace-only selection matches nothing and names nothing, which the caller
        // treats as "no test selected" rather than a green run.
        let (selected, unmatched) = resolve_selection(" , ", &discovered);
        assert!(selected.is_empty());
        assert!(unmatched.is_empty());
    }
}
