//! Test runner for comparing the telemetry data outputs of two similar targets when fed by an identical, deterministic input.

#![deny(warnings)]
#![deny(missing_docs)]

use saluki_error::{ErrorContext as _, GenericError};
use tracing::{error, info};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

mod analysis;

mod config;
use self::config::Config;
use crate::analysis::AnalysisRunner;

mod runner;
use self::runner::TestRunner;

mod sync;

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

    // Load our configuration.
    //
    // The first argument passed to `ground-truth` should be the path to the configuration file in YAML format.
    let config_path = std::env::args().nth(1).expect("Missing configuration file path.");
    let config = Config::from_yaml(&config_path).error_context("Failed to load configuration file.")?;

    info!("Loaded test case configuration from '{}'.", config_path);

    match run(config).await {
        Ok(()) => info!("ground-truth stopped."),
        Err(e) => {
            error!("{:?}", e);
            std::process::exit(1);
        }
    }

    Ok(())
}

async fn run(config: Config) -> Result<(), GenericError> {
    info!("Test run starting...");

    let test_runner = TestRunner::from_config(&config).await?;
    let (baseline_data, comparison_data) = test_runner
        .run()
        .await
        .error_context("Failed to run test to completion.")?;

    info!("Test run complete. Analyzing results...");

    let analysis_runner = AnalysisRunner::new(config.analysis_mode, baseline_data, comparison_data);
    analysis_runner.run_analysis()?;

    info!("Analysis complete: no difference detected between baseline and comparison.");

    Ok(())
}
