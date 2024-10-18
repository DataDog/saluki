use saluki_app::prelude::*;
use saluki_error::GenericError;
use tracing::{error, info};

mod config;
use self::config::Config;

mod corpus;

mod driver;
use self::driver::Driver;

mod target;

fn main() {
    if let Err(e) = initialize_logging(None) {
        fatal_and_exit(format!("failed to initialize logging: {}", e));
    }

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
