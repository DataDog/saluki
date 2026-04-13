//! Test runner for comparing the telemetry data outputs of two similar targets when fed by an identical, deterministic input.

#![deny(warnings)]
#![deny(missing_docs)]

use std::path::PathBuf;

use argh::FromArgs;
use futures::stream::{self, StreamExt as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tracing::{error, info};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

mod analysis;

mod config;
use self::config::Config;
use crate::analysis::{AnalysisMode, AnalysisRunner, TracesAnalysisOptions};

mod runner;
use self::runner::TestRunner;

mod sync;

/// Ground truth: correctness test runner for Agent Data Plane.
#[derive(FromArgs)]
struct Cli {
    #[argh(subcommand)]
    command: Command,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum Command {
    Run(RunCommand),
    RunAll(RunAllCommand),
}

/// Run a single correctness test case.
#[derive(FromArgs)]
#[argh(subcommand, name = "run")]
struct RunCommand {
    /// path to the test case config.yaml file
    #[argh(positional)]
    config_path: PathBuf,
}

/// Run all correctness test cases discovered in a directory.
#[derive(FromArgs)]
#[argh(subcommand, name = "run-all")]
struct RunAllCommand {
    /// path to the directory containing test case subdirectories
    #[argh(option, short = 'd')]
    test_dir: PathBuf,

    /// number of test cases to run in parallel (default: 4)
    #[argh(option, short = 'p', default = "4")]
    parallelism: usize,
}

#[tokio::main]
async fn main() -> Result<(), GenericError> {
    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with_ansi(true)
        .with_target(true)
        .init();

    let cli: Cli = argh::from_env();

    match cli.command {
        Command::Run(cmd) => {
            let config_path = cmd.config_path.to_string_lossy().into_owned();
            let config = Config::from_yaml(&config_path).error_context("Failed to load configuration file.")?;

            info!("Loaded test case configuration from '{}'.", config_path);

            match run_single(config).await {
                Ok(()) => info!("ground-truth stopped."),
                Err(e) => {
                    error!("{:?}", e);
                    std::process::exit(1);
                }
            }
        }
        Command::RunAll(cmd) => {
            let configs = discover_configs(&cmd.test_dir)?;
            if configs.is_empty() {
                error!("No test cases found in '{}'.", cmd.test_dir.display());
                std::process::exit(1);
            }

            info!(
                "Discovered {} test case(s) in '{}'. Running with parallelism={}.",
                configs.len(),
                cmd.test_dir.display(),
                cmd.parallelism
            );

            let failed = run_all(configs, cmd.parallelism).await;
            if failed > 0 {
                error!("{} test case(s) failed.", failed);
                std::process::exit(1);
            }

            info!("All test cases passed.");
        }
    }

    Ok(())
}

/// Discover all `config.yaml` files in subdirectories of `test_dir`.
fn discover_configs(test_dir: &PathBuf) -> Result<Vec<(String, Config)>, GenericError> {
    if !test_dir.is_dir() {
        return Err(generic_error!("Test directory does not exist: {}", test_dir.display()));
    }

    let mut entries: Vec<_> = std::fs::read_dir(test_dir)
        .error_context(format!("Failed to read test directory: {}", test_dir.display()))?
        .collect::<Result<_, _>>()
        .error_context("Failed to read directory entry")?;

    entries.sort_by_key(|e| e.file_name());

    let mut configs = Vec::new();
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            let config_path = path.join("config.yaml");
            if config_path.exists() {
                let config_path_str = config_path.to_string_lossy().into_owned();
                match Config::from_yaml(&config_path_str) {
                    Ok(config) => {
                        let name = path.file_name().unwrap().to_string_lossy().into_owned();
                        configs.push((name, config));
                    }
                    Err(e) => {
                        error!(
                            "Failed to load test case config '{}', skipping: {:?}",
                            config_path.display(),
                            e
                        );
                    }
                }
            }
        }
    }

    Ok(configs)
}

/// Run all discovered test cases in parallel, returning the number of failures.
async fn run_all(configs: Vec<(String, Config)>, parallelism: usize) -> usize {
    let parallelism = parallelism.max(1);

    let failures: Vec<bool> = stream::iter(configs)
        .map(|(name, config)| async move {
            info!(test_case = %name, "Starting test case.");
            match run_single(config).await {
                Ok(()) => {
                    info!(test_case = %name, "Test case passed.");
                    false
                }
                Err(e) => {
                    error!(test_case = %name, error = ?e, "Test case failed.");
                    true
                }
            }
        })
        .buffer_unordered(parallelism)
        .collect()
        .await;

    failures.into_iter().filter(|&failed| failed).count()
}

async fn run_single(config: Config) -> Result<(), GenericError> {
    info!("Test run starting...");

    let test_runner = TestRunner::from_config(&config).await?;
    let (baseline_data, comparison_data) = test_runner
        .run()
        .await
        .error_context("Failed to run test to completion.")?;

    info!("Test run complete. Analyzing results...");

    let traces_options = match config.analysis_mode {
        AnalysisMode::Traces => Some(TracesAnalysisOptions {
            otlp_direct_analysis_mode: config.otlp_direct_analysis_mode,
            additional_span_ignore_fields: config.additional_span_ignore_fields,
        }),
        AnalysisMode::Metrics => None,
    };
    let analysis_runner = AnalysisRunner::new(config.analysis_mode, baseline_data, comparison_data, traces_options);
    analysis_runner.run_analysis()?;

    info!("Analysis complete: no difference detected between baseline and comparison.");

    Ok(())
}
