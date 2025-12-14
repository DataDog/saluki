use std::{path::PathBuf, process::ExitCode, time::Instant};

use chrono::Local;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter};

mod assertions;
mod cli;
mod concurrent;
mod config;
mod reporter;
mod runner;
mod tui;

use cli::{Cli, Command};
use config::discover_tests;
use reporter::{OutputFormat, Reporter, TestSuiteResult};

fn main() -> ExitCode {
    // Parse CLI arguments first to determine if we need tracing.
    let cli: Cli = argh::from_env();

    // Determine if we'll use TUI mode (for run command).
    let use_tui = match &cli.command {
        Command::Run(cmd) => !cmd.no_tui && cmd.output == "text" && atty::is(atty::Stream::Stdout),
        Command::List(_) => false,
    };

    // Only initialize tracing if not using TUI mode.
    // TUI mode handles its own output.
    if !use_tui {
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

        info!("Panoramic starting...");
    }

    // Run the appropriate command.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime.");

    let result = runtime.block_on(async {
        match cli.command {
            Command::Run(cmd) => run_tests(cmd, use_tui).await,
            Command::List(cmd) => list_tests(cmd).await,
        }
    });

    if !use_tui {
        match result {
            ExitCode::SUCCESS => info!("Panoramic stopped."),
            _ => error!("Panoramic stopped with errors."),
        }
    }

    result
}

async fn run_tests(cmd: cli::RunCommand, use_tui: bool) -> ExitCode {
    // Discover test cases.
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
        let requested: Vec<&str> = filter.split(',').map(|s| s.trim()).collect();
        test_cases.retain(|t| requested.contains(&t.name.as_str()));

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

    if use_tui {
        run_tests_with_tui(test_cases, cmd.parallelism, cmd.fail_fast, log_dir).await
    } else {
        run_tests_plain(cmd, test_cases, log_dir).await
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

async fn run_tests_with_tui(
    test_cases: Vec<config::TestCase>, parallelism: usize, fail_fast: bool, log_dir: Option<PathBuf>,
) -> ExitCode {
    match tui::run_with_tui(test_cases, parallelism, fail_fast, log_dir).await {
        Ok(results) => {
            let all_passed = results.iter().all(|r| r.passed);
            if all_passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("TUI error: {}", e);
            ExitCode::from(2)
        }
    }
}

async fn run_tests_plain(
    cmd: cli::RunCommand, test_cases: Vec<config::TestCase>, log_dir: Option<PathBuf>,
) -> ExitCode {
    use concurrent::ConcurrentRunner;

    info!("Discovering test cases from '{}'...", cmd.test_dir.display());

    let output_format = match OutputFormat::from_str(&cmd.output) {
        Some(format) => format,
        None => {
            error!("Invalid output format '{}'. Use 'text' or 'json'.", cmd.output);
            return ExitCode::from(2);
        }
    };

    let reporter = Reporter::new(output_format, cmd.verbose);

    info!("Discovered {} test case(s).", test_cases.len());

    if let Some(ref dir) = log_dir {
        info!("Container logs will be written to '{}'.", dir.display());
    }

    info!("Starting test run with parallelism of {}...", cmd.parallelism);

    let started = Instant::now();

    // Run tests.
    let mut runner = ConcurrentRunner::new(test_cases, cmd.parallelism);
    if let Some(dir) = log_dir {
        runner = runner.with_log_dir(dir);
    }

    let results = if cmd.fail_fast {
        info!("Fail-fast mode enabled; will stop on first failure.");
        runner.run_fail_fast().await
    } else {
        runner.run_all().await
    };

    let suite_result = TestSuiteResult::from_results(results, started.elapsed());

    info!(
        "Test run complete. {} passed, {} failed, {} total ({:.2?}).",
        suite_result.passed, suite_result.failed, suite_result.total, suite_result.duration
    );

    // Report results.
    for result in &suite_result.results {
        reporter.report_test_result(result);
    }
    reporter.report_suite_result(&suite_result);

    if suite_result.all_passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
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
