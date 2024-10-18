use clap::Parser as _;
use saluki_app::prelude::*;
use saluki_error::{generic_error, GenericError};
use tracing::{error, info};

mod config;
use self::config::{Action, Cli};

mod driver;
use self::driver::{Driver, DriverConfig};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Err(e) = initialize_logging(Some(cli.log_level())) {
        fatal_and_exit(format!("failed to initialize logging: {}", e));
    }

    match run(cli).await {
        Ok(()) => info!("airlock stopped."),
        Err(e) => {
            error!("{:?}", e);
            std::process::exit(1);
        }
    }
}

async fn run(cli: Cli) -> Result<(), GenericError> {
    info!("airlock starting...");

    match cli.action {
        Action::RunMillstone(config) => {
            info!("User requested 'millstone' driver. Starting...");

            let driver_config = DriverConfig::millstone(config).await?;
            let mut driver = Driver::from_config(cli.isolation_group_id, driver_config)?;
            let _ = driver.start().await?;
            driver.wait_for_container_exit().await?;

            info!("Driver stopped. Cleaning up...");

            driver.cleanup().await?;
        }
        Action::RunMetricsIntake(config) => {
            info!("User requested 'metrics-intake' driver. Starting...");

            let driver_config = DriverConfig::metrics_intake(config).await?;
            let mut driver = Driver::from_config(cli.isolation_group_id, driver_config)?;
            let details = driver.start().await?;
            let intake_port = details
                .try_get_exposed_port("tcp", 2049)
                .ok_or_else(|| generic_error!("Failed to get exposed port details for metrics intake container"))?;
            info!(
                "Metrics intake container started, available on host port {}",
                intake_port
            );

            driver.wait_for_container_exit().await?;

            info!("Driver stopped. Cleaning up...");

            driver.cleanup().await?;
        }
        Action::RunDogstatsd(config) => {
            info!("User requested 'dogstatsd' driver. Starting...");

            let driver_config = DriverConfig::dogstatsd(config).await?;
            let mut driver = Driver::from_config(cli.isolation_group_id, driver_config)?;
            let _ = driver.start().await?;
            driver.wait_for_container_exit().await?;

            info!("Driver stopped. Cleaning up...");

            driver.cleanup().await?;
        }
        Action::RunAgentDataPlane(config) => {
            info!("User requested 'agent-data-plane' driver. Starting...");

            let driver_config = DriverConfig::agent_data_plane(config).await?;
            let mut driver = Driver::from_config(cli.isolation_group_id, driver_config)?;
            let _ = driver.start().await?;
            driver.wait_for_container_exit().await?;

            info!("Driver stopped. Cleaning up...");

            driver.cleanup().await?;
        }
    }

    Ok(())
}
