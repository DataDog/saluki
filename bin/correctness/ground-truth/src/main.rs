//! Test runner for comparing the outputs of DogStatsD and Agent Data Plane when fed deterministic inputs.

#![deny(warnings)]
#![deny(missing_docs)]

use saluki_error::{ErrorContext as _, GenericError};
use tracing::{error, info};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

mod analysis;

mod config;
use self::config::Config;

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
    info!("ground-truth starting...");

    let test_runner = TestRunner::from_config(&config).await?;
    let raw_results = test_runner.run().await?;

    info!("Running analysis...");

    raw_results.run_analysis().error_context("Analysis failed.")?;

    info!("Analysis complete: no difference detected between DogStatsD and Agent Data Plane.");

    Ok(())
}
