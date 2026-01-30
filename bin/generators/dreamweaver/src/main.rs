//! Dreamweaver - A distributed system OTLP workload generator.
//!
//! Dreamweaver generates realistic, correlated OTLP telemetry (traces, metrics, logs) by simulating
//! a user-defined distributed system architecture. Users describe their imaginary system in YAML,
//! and dreamweaver brings it to life by generating realistic telemetry and sending it to a
//! downstream OTLP receiver.

#![deny(warnings)]
#![deny(missing_docs)]

use saluki_error::GenericError;
use tracing::{error, info};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

mod config;
mod driver;
mod generator;
mod model;
mod sender;
mod simulation;

use self::driver::Driver;

fn main() {
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

    match run() {
        Ok(()) => info!("dreamweaver stopped."),
        Err(e) => {
            error!("{:?}", e);
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), GenericError> {
    info!("dreamweaver starting...");

    // We only accept a single command line argument: the path to the configuration file.
    let config_path = match std::env::args().nth(1) {
        Some(path) => path,
        None => {
            error!("Path to the configuration file must be passed as the first (and only) argument to `dreamweaver`.");
            std::process::exit(1);
        }
    };

    let config = config::Config::try_from_file(&config_path)?;
    let driver = Driver::new(config)?;

    // Run the simulation using the tokio runtime.
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| saluki_error::generic_error!("Failed to create tokio runtime: {}", e))?;

    runtime.block_on(driver.run())
}
