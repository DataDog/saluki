//! Logging.

use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

/// Logs a message to standard error and exits the process with a non-zero exit code.
pub fn fatal_and_exit(message: String) {
    eprintln!("FATAL: {}", message);
    std::process::exit(1);
}

/// Initializes the logging subsystem for `tracing`.
///
/// This function reads the `DD_LOG_LEVEL` environment variable to determine the log level to use. If the environment
/// variable is not set, the default log level is `INFO`.
///
/// ## Errors
///
/// If the logging subsystem was already initialized, an error will be returned.
pub fn initialize_logging() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .with_env_var("DD_LOG_LEVEL")
                .from_env_lossy(),
        )
        .with_ansi(true)
        .with_target(true)
        .try_init()
}
