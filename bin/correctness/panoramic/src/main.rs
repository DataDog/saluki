//! Test runner for running integration tests.

#![deny(warnings)]
#![deny(missing_docs)]

use std::{path::PathBuf, process::ExitCode, time::Instant};

use chrono::Local;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter};

mod assertions;
mod cli;
use self::cli::{Cli, Command};

mod config;
use self::config::discover_tests;

mod events;
use self::events::{create_event_channel, TestEvent};

mod reporter;
use self::reporter::{OutputFormat, Reporter, TestResult, TestSuiteResult};

mod runner;
mod tui;

#[tokio::main]
async fn main() -> ExitCode {
    let cli: Cli = argh::from_env();

    // See if we should use TUI mode.
    //
    // This influences how we configure things since some output gets redirected/rendered differently in TUI mode.
    let use_tui = match &cli.command {
        Command::Run(cmd) => !cmd.no_tui && cmd.output == "text" && atty::is(atty::Stream::Stdout),
        Command::List(_) => false,
    };

    if !use_tui {
        initialize_logging();

        info!("Panoramic starting...");
    }

    let result = match cli.command {
        Command::Run(cmd) => run_tests(cmd, use_tui).await,
        Command::List(cmd) => list_tests(cmd).await,
    };

    if !use_tui {
        match result {
            ExitCode::SUCCESS => info!("Panoramic stopped."),
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
    let mut test_cases = match discover_tests(&cmd.test_dir) {
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
        if use_tui {
            eprintln!("No test cases found in '{}'.", cmd.test_dir.display());
        } else {
            error!("No test cases found in '{}'.", cmd.test_dir.display());
        }
        return ExitCode::from(2);
    }

    // Filter tests if specific ones are requested.
    if let Some(ref filter) = cmd.tests {
        let requested_tests = filter.split(',').map(|s| s.trim()).collect::<Vec<_>>();
        test_cases.retain(|t| requested_tests.contains(&t.name.as_str()));

        if test_cases.is_empty() {
            if use_tui {
                eprintln!("No tests matched the filter '{}'.", filter);
            } else {
                error!("No tests matched the filter '{}'.", filter);
            }
            return ExitCode::from(2);
        }
    }

    // Create log directory if log capture is enabled.
    let log_dir = if cmd.no_logs {
        None
    } else {
        match create_log_dir() {
            Ok(dir) => Some(dir),
            Err(e) => {
                if use_tui {
                    eprintln!("Failed to create log directory: {}", e);
                } else {
                    error!("Failed to create log directory: {}", e);
                }
                return ExitCode::from(2);
            }
        }
    };

    // Create the event channel and cancellation token.
    let (tx, rx) = create_event_channel();
    let cancel_token = CancellationToken::new();

    // Spawn the test runner task (same code path for both modes).
    let runner_handle = tokio::spawn(runner::run_tests(
        test_cases,
        cmd.parallelism,
        cmd.fail_fast,
        log_dir.clone(),
        tx,
        cancel_token.clone(),
    ));

    // Spawn the appropriate consumer based on mode.
    let all_passed = if use_tui {
        run_with_tui_consumer(rx, cancel_token, log_dir, runner_handle).await
    } else {
        run_with_logging_consumer(rx, &cmd, log_dir, runner_handle).await
    };

    if all_passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Create a unique temporary directory for container logs.
fn create_log_dir() -> std::io::Result<PathBuf> {
    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    let dir_name = format!("panoramic-{}", timestamp);
    let log_dir = std::env::temp_dir().join(dir_name);
    std::fs::create_dir_all(&log_dir)?;
    Ok(log_dir)
}

/// Run with the TUI consumer.
async fn run_with_tui_consumer(
    rx: mpsc::UnboundedReceiver<TestEvent>, cancel_token: CancellationToken, log_dir: Option<PathBuf>,
    runner_handle: tokio::task::JoinHandle<Vec<TestResult>>,
) -> bool {
    // Run the TUI consumer (blocks until AllDone or user cancels).
    if let Err(e) = tui::run_tui_consumer(rx, cancel_token, log_dir).await {
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
            Some(TestEvent::TestCompleted { result }) => {
                reporter.report_test_result(&result);
                results.push(result);
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
    info!("Discovering test cases from '{}'...", cmd.test_dir.display());

    let test_cases = match discover_tests(&cmd.test_dir) {
        Ok(tests) => tests,
        Err(e) => {
            error!("Failed to discover tests: {}", e);
            return ExitCode::from(2);
        }
    };

    if test_cases.is_empty() {
        info!("No test cases found in '{}'.", cmd.test_dir.display());
        return ExitCode::SUCCESS;
    }

    info!("Discovered {} test case(s).", test_cases.len());

    println!();
    println!("Available tests ({}):", test_cases.len());
    println!();

    for test_case in &test_cases {
        println!("  {}", test_case.name);
        if let Some(ref description) = test_case.description {
            println!("    {}", description);
        }
        println!(
            "    Timeout: {:?}, Assertions: {}",
            test_case.timeout.0,
            test_case.assertions.len()
        );
        println!();
    }

    ExitCode::SUCCESS
}
