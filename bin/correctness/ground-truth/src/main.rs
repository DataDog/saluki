//! Test runner for comparing the outputs of DogStatsD and Agent Data Plane when fed deterministic inputs.

#![deny(warnings)]
#![deny(missing_docs)]

use clap::Parser as _;
use saluki_error::{ErrorContext as _, GenericError};
use tracing::{error, info};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

mod analysis;

mod config;
use self::config::Cli;

mod runner;
use self::runner::TestRunner;

mod sync;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with_ansi(true)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .init();

    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => info!("ground-truth stopped."),
        Err(e) => {
            error!("{:?}", e);
            std::process::exit(1);
        }
    }
}

async fn run(cli: Cli) -> Result<(), GenericError> {
    info!("ground-truth starting...");

    let test_runner = TestRunner::from_cli(&cli);
    let raw_results = test_runner.run().await?;

    info!("Running analysis...");

    raw_results.run_analysis().error_context("Analysis failed.")?;

    info!("Analysis complete: no difference detected between DogStatsD and Agent Data Plane.");

    Ok(())
}
