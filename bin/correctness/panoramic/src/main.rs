//! Test runner for running integration tests.

#![deny(warnings)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::{io::IsTerminal, path::PathBuf, process::ExitCode, time::Instant};

use tokio::sync::{mpsc, watch, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter};

use crate::runner::Runner;

mod kind;
use self::kind::KindLifecycle;

mod assertions;
mod cli;
mod correctness;
use self::cli::{Cli, Command};

mod config;
mod dynamic_vars;
mod mounts;
use self::config::discover_tests;

mod events;
use self::events::{create_event_channel, TestEvent};

mod reporter;
use self::reporter::{OutputFormat, Reporter, TestResult, TestSuiteResult};

mod runner;
mod test;
mod tui;
mod utils;

#[tokio::main]
async fn main() -> ExitCode {
    // Install the rustls crypto provider once at startup. Both reqwest and kube use rustls 0.23,
    // which requires an explicit provider install when multiple TLS-using crates are present.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli: Cli = argh::from_env();

    // See if we should use TUI mode.
    //
    // This influences how we configure things since some output gets redirected/rendered differently in TUI mode.
    let (use_tui, is_test_run) = match &cli.command {
        Command::Run(cmd) => (
            !cmd.no_tui && cmd.output == "text" && std::io::stdout().is_terminal(),
            true,
        ),
        Command::List(_) => (false, false),
    };

    if !use_tui {
        initialize_logging();
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

fn initialize_logging() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_thread_ids(false)
                .compact(),
        )
        .init();
}

async fn run_tests(cmd: cli::RunCommand, use_tui: bool) -> ExitCode {
    if cmd.test_dirs.is_empty() {
        let msg = "No test directories specified. Use -d <path> to specify one or more directories.";
        if use_tui {
            eprintln!("{}", msg);
        } else {
            error!("{}", msg);
        }
        return ExitCode::from(2);
    }

    let test_cases = match discover_tests(&cmd.test_dirs) {
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

    // Create log directory.
    let log_dir = cmd.log_dir();
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        if use_tui {
            eprintln!("Failed to create log directory: {}", e);
        } else {
            error!("Failed to create log directory: {}", e);
        }
        return ExitCode::from(2);
    }

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
        Some(std::sync::Arc::new(Mutex::new(kind_rx)))
    };

    // Inject runtime config and build the test registry.
    let mut registry = Runner::new(log_dir.clone(), cmd.mounts_dir.clone());
    if let Some(ref rx) = kind_rx {
        registry = registry.with_kind_ready(rx.clone());
    }
    for tc in test_cases {
        registry.register(tc).expect("failure to register test");
    }

    // Cancellation token for the test run.

    // Create a signal sender so that we can shut it down on ctrl-c.
    let cancel_all = CancellationToken::new();

    // Build run args.
    let mut args = runner::RunArgs::new(cancel_all.clone())
        .with_parallelism(cmd.parallelism)
        .with_fail_fast(cmd.fail_fast)
        .with_event_sender(tx);

    if let Some(ref filter_str) = cmd.tests {
        let names: Vec<String> = filter_str.split(',').map(|s| s.trim().to_string()).collect();
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
    let all_passed = if use_tui {
        run_with_tui_consumer(rx, cancel_all, Some(log_dir), runner_handle).await
    } else {
        run_with_logging_consumer(rx, &cmd, Some(log_dir), runner_handle).await
    };

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

    if all_passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
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

/// Run with the TUI consumer.
async fn run_with_tui_consumer(
    rx: mpsc::UnboundedReceiver<TestEvent>, cancel_all: CancellationToken, log_dir: Option<PathBuf>,
    runner_handle: tokio::task::JoinHandle<Vec<TestResult>>,
) -> bool {
    // Run the TUI consumer (blocks until AllDone or user cancels).
    if let Err(e) = tui::run_tui_consumer(rx, cancel_all, log_dir).await {
        eprintln!("TUI error: {}", e);
        return false;
    }

    // Wait for the runner to finish and get results.
    match runner_handle.await {
        Ok(results) => results.iter().all(|r| r.passed),
        Err(e) => {
            eprintln!("Runner task error: {}", e);
            false
        }
    }
}

/// Run with the logging consumer (non-TUI mode).
async fn run_with_logging_consumer(
    rx: mpsc::UnboundedReceiver<TestEvent>, cmd: &cli::RunCommand, log_dir: Option<PathBuf>,
    runner_handle: tokio::task::JoinHandle<Vec<TestResult>>,
) -> bool {
    let output_format = match OutputFormat::from_str(&cmd.output) {
        Some(format) => format,
        None => {
            error!("Invalid output format '{}'. Use 'text' or 'json'.", cmd.output);
            return false;
        }
    };

    let reporter = Reporter::new(output_format, cmd.verbose);

    info!("Starting test run with parallelism of {}...", cmd.parallelism);

    if let Some(ref dir) = log_dir {
        info!("Container logs will be written to '{}'.", dir.display());
    }

    if cmd.fail_fast {
        info!("Fail-fast mode enabled; will stop on first failure.");
    }

    let started = Instant::now();

    // Run the logging consumer (blocks until AllDone).
    let suite_result = run_logging_consumer(rx, &reporter, started).await;

    // Wait for the runner to finish.
    let _ = runner_handle.await;

    info!(
        "Test run complete. {} passed, {} failed, {} total ({:.2?}).",
        suite_result.passed, suite_result.failed, suite_result.total, suite_result.duration
    );

    // Report final suite result.
    reporter.report_suite_result(&suite_result);

    suite_result.all_passed()
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
                results.push(result);
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

    let test_cases = match discover_tests(&cmd.test_dirs) {
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
