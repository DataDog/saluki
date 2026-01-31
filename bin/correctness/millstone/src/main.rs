//! A deterministic load generator, in the spirit of Lading, that also adds determinism around how many payloads are
//! sent to a target.

#![deny(warnings)]
#![deny(missing_docs)]

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

    // We only accept a single command line argument: the path to the configuration file.
    let config_path = match std::env::args().nth(1) {
        Some(path) => path,
        None => {
            error!("Path to the configuration file must be passed as the first (and only) argument to `millstone`.");
            std::process::exit(1);
        }
    };

    let config = Config::try_from_file(&config_path)?;
    let driver = Driver::new(config)?;
    driver.run()
}
