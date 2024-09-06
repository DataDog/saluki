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
/// variable is not set, the default log level is `INFO`. Additionally, it reads the `DD_LOG_FORMAT_JSON` environment
/// variable to determine which output format to use. If it is set to `json` (case insensitive), the logs will be
/// formatted as JSON. If it is set to any other value, or not set at all, the logs will default to a rich, colored,
/// human-readable format.
///
/// ## Errors
///
/// If the logging subsystem was already initialized, an error will be returned.
pub fn initialize_logging() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let is_json = std::env::var("DD_LOG_FORMAT_JSON")
        .map(|s| s.trim().to_lowercase())
        .map(|s| s == "true" || s == "1")
        .unwrap_or(false);

    let level_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .with_env_var("DD_LOG_LEVEL")
        .from_env_lossy();

    if is_json {
        initialize_tracing_json(level_filter)
    } else {
        initialize_tracing_pretty(level_filter)
    }
}

fn initialize_tracing_json(level_filter: EnvFilter) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(level_filter)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .json()
        .flatten_event(true)
        .try_init()
}

fn initialize_tracing_pretty(level_filter: EnvFilter) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(level_filter)
        .with_ansi(true)
        .with_target(true)
        .try_init()
}
