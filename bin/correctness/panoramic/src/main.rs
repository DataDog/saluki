//! Test runner for running integration tests.

#![deny(warnings)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::{
    io::IsTerminal,
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant},
};

use chrono::Local;
use clap::Parser as _;
use tokio::sync::{mpsc, watch, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter};

use crate::runner::Runner;

mod kind;
use self::kind::KindLifecycle;

mod actions;
mod assertions;
mod cli;
mod correctness;
use self::cli::{Cli, Command, LogLevel};

mod config;
mod dynamic_vars;
mod mounts;
use self::config::{default_host_runtime, discover_tests};

mod events;
use self::events::{create_event_channel, TestEvent};

mod machine_output;
use self::machine_output::RunReport;

mod reporter;
use self::reporter::{ErrorKind, OutputFormat, Reporter, TestResult, TestSuiteResult};

mod runner;
mod test;
mod test_env;
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
    let env_filter = if std::env::var_os(EnvFilter::DEFAULT_ENV).is_some() {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::new(log_level.filter_directives())
    };

    tracing_subscriber::registry()
        .with(env_filter)
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

/// Exit codes callers key off. `1` means the code under test failed an assertion; `2` means the
/// harness never got to a verdict and the environment likely needs attention; `3` means the
/// selection named nothing to run.
const EXIT_ASSERTION_FAILURE: u8 = 1;
const EXIT_HARNESS_ERROR: u8 = 2;
const EXIT_NO_TESTS_SELECTED: u8 = 3;

/// Maps a finished run onto its exit code.
fn exit_code_for(suite: &TestSuiteResult) -> ExitCode {
    if suite.any_errored() {
        ExitCode::from(EXIT_HARNESS_ERROR)
    } else if !suite.all_passed() {
        ExitCode::from(EXIT_ASSERTION_FAILURE)
    } else {
        ExitCode::SUCCESS
    }
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
    let test_cases = match discover_tests(&cmd.test_dirs, &integration_runtime) {
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

    let started_at = Local::now().to_rfc3339();
    let run_started = Instant::now();
    machine_output::write_run_report(&RunReport::new(
        cmd.test_dirs.clone(),
        integration_runtime.clone(),
        cmd.parallelism.get(),
        started_at.clone(),
        Duration::ZERO,
        log_dir.clone(),
        &[],
    ));

    // Create the event channel early so the kind setup task can emit status messages.
    let (tx, rx) = create_event_channel();

    // Spawn kind cluster setup in the background so non-kind tests start immediately.
    // Kind tests will wait on `kind_rx` before doing any work.
    let kind_images = collect_kind_images(&test_cases, cmd.tests.as_deref());
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

    // Create a signal sender so that we can shut it down on ctrl-c.
    let cancel_all = CancellationToken::new();

    // Build run args.
    let mut args = runner::RunArgs::new(cancel_all.clone())
        .with_parallelism(cmd.parallelism)
        .with_fail_fast(cmd.fail_fast)
        .with_event_sender(tx);

    // The runtime scope is already applied at discovery time. The optional -t name filter
    // narrows further. When unset, every discovered test runs.
    if let Some(names) = selected_tests {
        args = args.with_filter(Box::new(move |t: &dyn test::Test| names.iter().any(|n| *n == t.name())));
    }

    // Spawn the test runner task (same code path for both modes).
    let runner_handle = tokio::spawn(async move { registry.run_tests(args).await });

    // In non-TUI mode we need to handle a SIGINT from ctrl-c and call cancel on the cancel_all token.
    if !use_tui {
        let cancel_all_clone = cancel_all.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                info!("Received Ctrl+C, cancelling test run...");
                cancel_all_clone.cancel();
            }
        });
    }

    // Spawn the appropriate consumer based on mode.
    let suite_result = if use_tui {
        run_with_tui_consumer(rx, cancel_all, Some(log_dir.clone()), runner_handle, run_started).await
    } else {
        run_with_logging_consumer(rx, &cmd, Some(log_dir.clone()), runner_handle, run_started).await
    };

    let run_report = RunReport::new(
        cmd.test_dirs.clone(),
        integration_runtime,
        cmd.parallelism.get(),
        started_at,
        suite_result.duration,
        log_dir,
        &suite_result.results,
    );
    machine_output::write_run_report(&run_report);
    let mut json_output_failed = false;
    if matches!(cmd.output, OutputFormat::Json) {
        match serde_json::to_string_pretty(&run_report) {
            Ok(json) => println!("{}", json),
            Err(e) => {
                error!("Failed to serialize the machine-readable run report: {}", e);
                json_output_failed = true;
            }
        }
    }

    // Tear down the kind cluster unless the caller asked to keep it.
    if kind_rx.is_some() {
        let lifecycle: Option<KindLifecycle> = kind_lifecycle_slot.lock().await.take();
        if let Some(lifecycle) = lifecycle {
            if cmd.no_delete_kind_cluster {
                info!(
                    "Skipping kind cluster teardown (--no-delete-kind-cluster). \
                     Cluster '{}' is still running.",
                    cmd.kind_cluster_name
                );
            } else {
                lifecycle.teardown().await;
            }
        } else {
            // lifecycle is None when setup failed after creating the cluster but before
            // completing image loading. The cluster may still be running.
            warn!(
                "Kind cluster setup did not complete successfully. \
                 A kind cluster named '{}' may still be running — run 'kind delete cluster --name {}' to clean it up.",
                cmd.kind_cluster_name, cmd.kind_cluster_name
            );
        }
    }

    // A caller that asked for JSON and got none has no verdict to read, so the run's own exit code
    // would be a claim about a report that was never printed.
    if json_output_failed {
        return ExitCode::from(EXIT_HARNESS_ERROR);
    }

    exit_code_for(&suite_result)
}

/// Collects the unique set of images required by all kind-runtime tests in the given list.
fn collect_kind_images(tests: &[Box<dyn test::Test>], filter: Option<&str>) -> Vec<String> {
    use std::collections::BTreeSet;
    let names: Vec<&str> = filter
        .map(|f| f.split(',').map(str::trim).collect())
        .unwrap_or_default();
    let mut images = BTreeSet::new();
    for test in tests {
        if !names.is_empty() && !names.contains(&test.name().as_str()) {
            continue;
        }
        if test.runtime() == "kubernetes_in_docker" {
            for (_, image) in test.images() {
                images.insert(image);
            }
        }
    }
    images.into_iter().collect()
}

/// Run with the TUI consumer.
async fn run_with_tui_consumer(
    rx: mpsc::UnboundedReceiver<TestEvent>, cancel_all: CancellationToken, log_dir: Option<PathBuf>,
    runner_handle: tokio::task::JoinHandle<Vec<TestResult>>, started: Instant,
) -> TestSuiteResult {
    let tui_error = tui::run_tui_consumer(rx, cancel_all, log_dir).await.err();
    let mut results = match runner_handle.await {
        Ok(results) => results,
        Err(e) => vec![TestResult::errored(
            "panoramic runner",
            ErrorKind::Internal,
            format!("Runner task failed: {}", e),
            started.elapsed(),
            Vec::new(),
        )],
    };
    if let Some(e) = tui_error {
        results.push(TestResult::errored(
            "panoramic TUI",
            ErrorKind::Internal,
            format!("TUI failed: {}", e),
            started.elapsed(),
            Vec::new(),
        ));
    }
    TestSuiteResult::from_results(results, started.elapsed())
}

/// Run with the logging consumer (non-TUI mode).
async fn run_with_logging_consumer(
    rx: mpsc::UnboundedReceiver<TestEvent>, cmd: &cli::RunCommand, log_dir: Option<PathBuf>,
    runner_handle: tokio::task::JoinHandle<Vec<TestResult>>, started: Instant,
) -> TestSuiteResult {
    let reporter = Reporter::new(cmd.output, cmd.verbose, log_dir.clone());

    info!("Starting test run with parallelism of {}...", cmd.parallelism);

    if let Some(ref dir) = log_dir {
        info!("Container logs will be written to '{}'.", dir.display());
    }

    if cmd.fail_fast {
        info!("Fail-fast mode enabled; will stop on first failure.");
    }

    // Run the logging consumer (blocks until AllDone).
    let mut suite_result = run_logging_consumer(rx, &reporter, started).await;

    if let Err(e) = runner_handle.await {
        suite_result.results.push(TestResult::errored(
            "panoramic runner",
            ErrorKind::Internal,
            format!("Runner task failed: {}", e),
            started.elapsed(),
            Vec::new(),
        ));
        suite_result = TestSuiteResult::from_results(suite_result.results, started.elapsed());
    }

    info!(
        "Test run complete. {} passed, {} failed, {} errored, {} total ({:.2?}).",
        suite_result.passed, suite_result.failed, suite_result.errored, suite_result.total, suite_result.duration
    );

    // Report final suite result.
    reporter.report_suite_result(&suite_result);

    suite_result
}

/// Consume test events and log via Reporter.
async fn run_logging_consumer(
    mut rx: mpsc::UnboundedReceiver<TestEvent>, reporter: &Reporter, started: Instant,
) -> TestSuiteResult {
    let mut results = Vec::new();

    loop {
        match rx.recv().await {
            Some(TestEvent::RunStarted { total_tests }) => {
                info!("Running {} test(s)...", total_tests);
            }
            Some(TestEvent::TestStarted { name }) => {
                info!("Starting test '{}'...", name);
            }
            Some(TestEvent::TestCompleted { result, log_dir }) => {
                reporter.report_test_result(&result, log_dir);
                results.push(*result);
            }
            Some(TestEvent::StatusLine { message }) => {
                info!("{}", message);
            }
            Some(TestEvent::AllDone) => {
                break;
            }
            None => {
                // Channel closed unexpectedly.
                break;
            }
        }
    }

    TestSuiteResult::from_results(results, started.elapsed())
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
    let test_cases = match discover_tests(&cmd.test_dirs, &integration_runtime) {
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
    use std::time::Duration;

    use super::*;
    use crate::assertions::AssertionResult;

    #[test]
    fn log_level_scopes_the_selected_level_to_first_party_crates() {
        let directives = EnvFilter::new(LogLevel::Debug.filter_directives()).to_string();

        // `EnvFilter` reorders directives, so check membership rather than the whole string.
        assert!(
            directives.contains("panoramic=debug"),
            "directives were '{}'",
            directives
        );
        assert!(directives.contains("airlock=debug"), "directives were '{}'", directives);
        assert!(directives.contains("off"), "directives were '{}'", directives);
        assert!(!directives.contains("hyper"), "directives were '{}'", directives);
    }

    fn passing(name: &str) -> TestResult {
        TestResult::from_assertions(
            name,
            Duration::from_secs(1),
            vec![AssertionResult {
                name: "log_contains".to_string(),
                passed: true,
                message: "found".to_string(),
                duration: Duration::from_millis(1),
            }],
            Vec::new(),
        )
    }

    fn failing(name: &str) -> TestResult {
        TestResult::from_assertions(
            name,
            Duration::from_secs(1),
            vec![AssertionResult {
                name: "log_contains".to_string(),
                passed: false,
                message: "not found".to_string(),
                duration: Duration::from_millis(1),
            }],
            Vec::new(),
        )
    }

    fn suite_of(results: Vec<TestResult>) -> TestSuiteResult {
        TestSuiteResult::from_results(results, Duration::from_secs(1))
    }

    #[test]
    fn exit_code_distinguishes_assertion_failures_from_harness_errors() {
        assert_eq!(exit_code_for(&suite_of(vec![passing("a")])), ExitCode::SUCCESS);
        assert_eq!(
            exit_code_for(&suite_of(vec![passing("a"), failing("b")])),
            ExitCode::from(EXIT_ASSERTION_FAILURE)
        );

        let errored = TestResult::errored("c", ErrorKind::Setup, "boom", Duration::from_secs(1), Vec::new());
        assert_eq!(
            exit_code_for(&suite_of(vec![errored])),
            ExitCode::from(EXIT_HARNESS_ERROR)
        );

        // A setup error outranks an assertion failure: the caller should fix the environment before
        // reading any diff.
        let errored = TestResult::errored("c", ErrorKind::Timeout, "slow", Duration::from_secs(1), Vec::new());
        assert_eq!(
            exit_code_for(&suite_of(vec![failing("b"), errored])),
            ExitCode::from(EXIT_HARNESS_ERROR)
        );
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
