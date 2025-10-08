//! Logging.

// TODO: `AgentLikeFieldVisitor` currently allocates a `String` to hold the message field when it finds it. This is
// suboptimal because it means we allocate a string literally every time we log a message. Logging is rare, but it's
// just a recipe for small, unnecessary allocations over time... and makes it that much more inefficient to enable
// debug/trace-level logging in production.
//
// We might consider _something_ like a string pool in the future, but we can defer that until we have a better idea of
// what the potential impact is in practice.

use saluki_error::{generic_error, GenericError};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_rolling_file::RollingFileAppenderBase;
use tracing_subscriber::{
    layer::SubscriberExt as _, reload::Layer as ReloadLayer, util::SubscriberInitExt as _, Layer,
};

mod api;
use self::api::set_logging_api_handler;
pub use self::api::{acquire_logging_api_handler, LoggingAPIHandler};

mod config;
pub(crate) use self::config::LoggingConfiguration;

mod layer;
use self::layer::build_formatting_layer;

#[derive(Default)]
pub(crate) struct LoggingGuard {
    worker_guards: Vec<WorkerGuard>,
}

impl LoggingGuard {
    fn add_worker_guard(&mut self, guard: WorkerGuard) {
        self.worker_guards.push(guard);
    }
}

/// Logs a message to standard error and exits the process with a non-zero exit code.
pub fn fatal_and_exit(message: String) {
    eprintln!("FATAL: {}", message);
    std::process::exit(1);
}

/// Initializes the logging subsystem for `tracing` with the ability to dynamically update the log filtering directives
/// at runtime.
///
/// This function reads the `DD_LOG_LEVEL` environment variable to determine the log level to use. If the environment
/// variable is not set, the default log level is `INFO`. Additionally, it reads the `DD_LOG_FORMAT_JSON` environment
/// variable to determine which output format to use. If it is set to `json` (case insensitive), the logs will be
/// formatted as JSON. If it is set to any other value, or not set at all, the logs will default to a rich, colored,
/// human-readable format.
///
/// An API handler can be acquired (via [`acquire_logging_api_handler`]) to install the API routes which allow for
/// dynamically controlling the logging level filtering. See [`LoggingAPIHandler`] for more information.
///
/// Returns a [`LoggingGuard`] which must be held until the application is about to shutdown, ensuring that any
/// configured logging backends are able to completely flush any pending logs before the application exits.
///
/// # Errors
///
/// If the logging subsystem was already initialized, an error will be returned.
pub(crate) async fn initialize_logging(config: &LoggingConfiguration) -> Result<LoggingGuard, GenericError> {
    // TODO: Support for logging to syslog.

    // Set up our log level filtering and dynamic filter layer.
    let level_filter = config.log_level.as_env_filter();
    let (filter_layer, reload_handle) = ReloadLayer::new(level_filter.clone());
    set_logging_api_handler(LoggingAPIHandler::new(level_filter, reload_handle));

    // Build all configured layers: one per output mechanism (console, file, etc).
    let mut configured_layers = Vec::new();
    let mut logging_guard = LoggingGuard::default();

    if config.log_to_console {
        let (nb_stdout, guard) = tracing_appender::non_blocking(std::io::stdout());
        logging_guard.add_worker_guard(guard);

        configured_layers.push(build_formatting_layer(config, nb_stdout));
    }

    if !config.log_file.is_empty() {
        let appender_builder = RollingFileAppenderBase::builder();
        let appender = appender_builder
            .filename(config.log_file.clone())
            .max_filecount(config.log_file_max_rolls)
            .condition_max_file_size(config.log_file_max_size.as_u64())
            .build()
            .map_err(|e| generic_error!("Failed to build log file appender: {}", e))?;

        let (nb_appender, guard) = tracing_appender::non_blocking(appender);
        logging_guard.add_worker_guard(guard);

        configured_layers.push(build_formatting_layer(config, nb_appender));
    }

    // `tracing` accepts a `Vec<L>` where `L` implements `Layer<S>`, which acts as a fanout.. and then we're applying
    // our filter layer on top of that, so that we filter out events once rather than per output layer.
    tracing_subscriber::registry()
        .with(configured_layers.with_filter(filter_layer))
        .try_init()?;

    Ok(logging_guard)
}
