//! A deterministic load generator, in the spirit of Lading, that also adds determinism around how many payloads are
//! sent to a target.

#![deny(warnings)]
#![deny(missing_docs)]

use std::path::PathBuf;

use saluki_error::GenericError;
use tracing::{error, info};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

mod config;
use self::config::Config;

mod corpus;

mod driver;
use self::driver::Driver;

mod target;

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
        Ok(()) => info!("millstone stopped."),
        Err(e) => {
            error!("{:?}", e);
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), GenericError> {
    info!("millstone starting...");

    let args: Vec<String> = std::env::args().collect();

    // The first argument is always the path to the configuration file.
    let config_path = match args.get(1) {
        Some(path) => path,
        None => {
            error!("Usage: millstone <config-path> [--output-file <path>]");
            std::process::exit(1);
        }
    };

    // Check for an optional `--output-file` flag that redirects output to a file.
    let output_file = args
        .iter()
        .position(|a| a == "--output-file")
        .map(|i| {
            args.get(i + 1).unwrap_or_else(|| {
                error!("--output-file requires a file path argument.");
                std::process::exit(1);
            })
        })
        .map(PathBuf::from);

    let config = Config::try_from_file(config_path)?;
    let driver = Driver::new(config, output_file)?;
    driver.run()
}
