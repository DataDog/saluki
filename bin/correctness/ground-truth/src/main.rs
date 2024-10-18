use clap::Parser as _;
use saluki_app::prelude::*;
use saluki_error::GenericError;
use tracing::{error, info};

mod analysis;

mod config;
use self::config::Cli;

mod runner;
use self::runner::TestRunner;

mod sync;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Err(e) = initialize_logging(Some(cli.log_level())) {
        fatal_and_exit(format!("failed to initialize logging: {}", e));
    }

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

    info!("All tests completed successfully. Running analysis...");

    info!(
        "Received {} metrics from DogStatsD, and {} metrics from Agent Data Plane.",
        raw_results.dsd_metrics().len(),
        raw_results.adp_metrics().len()
    );

    // TODO: filter out DSD/ADP internal telemetry from dumps

    // TODO: compare basic context count between DSD and ADP

    info!("Analysis complete.");

    Ok(())
}
