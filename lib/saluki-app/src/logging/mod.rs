//! Logging.

// TODO: `AgentLikeFieldVisitor` currently allocates a `String` to hold the message field when it finds it. This is
// suboptimal because it means we allocate a string literally every time we log a message. Logging is rare, but it's
// just a recipe for small, unnecessary allocations over time... and makes it that much more inefficient to enable
// debug/trace-level logging in production.
//
// We might consider _something_ like a string pool in the future, but we can defer that until we have a better idea of
// what the potential impact is in practice.

use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use saluki_error::{generic_error, GenericError};
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_rolling_file::RollingFileAppenderBase;
use tracing_subscriber::{layer::SubscriberExt as _, reload, util::SubscriberInitExt as _, EnvFilter, Layer, Registry};

mod api;
use self::api::set_logging_api_handler;
pub use self::api::{acquire_logging_api_handler, LoggingAPIHandler};

mod config;
pub use self::config::{LogLevel, LoggingConfiguration};

mod layer;
use self::layer::build_formatting_layer;

// Number of buffered lines in each non-blocking log writer.
//
// This directly influences the idle memory usage since each logging backend (console, file, etc) will have a bounded
// channel that can hold this many elements, and each element is roughly 32 bytes, so 1,000 elements/lines consumes a
// minimum of ~32KB, etc.
const NB_LOG_WRITER_BUFFER_SIZE: usize = 4096;

type OutputStack = Vec<Box<dyn Layer<Registry> + Send + Sync>>;

pub(crate) struct LoggingGuard {
    worker_guards: Vec<WorkerGuard>,
    stack_handle: reload::Handle<OutputStack, Registry>,
    filter_handle: reload::Handle<EnvFilter, Registry>,
    base_filter: Arc<Mutex<EnvFilter>>,
}

impl LoggingGuard {
    /// Reloads the logging subsystem from the given configuration.
    ///
    /// Rebuilds the output layer stack and updates the level filter from `config`, swapping both atomically into the
    /// already-installed `tracing` subscriber. Worker guards for the previous outputs are dropped after the swap, which
    /// flushes any buffered log lines to their original destinations.
    pub(crate) fn reload(&mut self, config: LoggingConfiguration) -> Result<(), GenericError> {
        let (new_stack, new_guards) = build_output_stack(&config)?;
        let new_filter = config.log_level.as_env_filter();

        self.stack_handle
            .reload(new_stack)
            .map_err(|e| generic_error!("Failed to swap logging output stack: {}", e))?;
        self.filter_handle
            .reload(new_filter.clone())
            .map_err(|e| generic_error!("Failed to swap logging filter: {}", e))?;
        *self.base_filter.lock().unwrap() = new_filter;

        // Drop the old worker guards _after_ the swap so any buffered lines are flushed to their original destinations
        // before the worker threads exit.
        let _old_guards = std::mem::replace(&mut self.worker_guards, new_guards);

        Ok(())
    }
}

/// Initializes the logging subsystem for `tracing` with the ability to dynamically update the log filtering directives
/// at runtime.
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
pub(crate) async fn initialize_logging(config: LoggingConfiguration) -> Result<LoggingGuard, GenericError> {
    // TODO: Support for logging to syslog.

    // Build the initial output stack from the supplied configuration. This is later swappable via
    // `BootstrapGuard::reload_logging` once the Datadog Agent provides authoritative configuration.
    let (output_stack, worker_guards) = build_output_stack(&config)?;
    let (output_layer, stack_handle) = reload::Layer::new(output_stack);

    // Set up our log level filtering and dynamic filter layer.
    let level_filter = config.log_level.as_env_filter();
    let (filter_layer, filter_handle) = reload::Layer::new(level_filter.clone());

    // The base filter is the level the override-restore should land on after a `/logging/override` timeout. It starts
    // as the bootstrap level, and is updated by `LoggingGuard::reload` once the Agent's configuration is applied.
    let base_filter = Arc::new(Mutex::new(level_filter));
    set_logging_api_handler(LoggingAPIHandler::new(base_filter.clone(), filter_handle.clone()));

    tracing_subscriber::registry()
        .with(output_layer.with_filter(filter_layer))
        .try_init()?;

    Ok(LoggingGuard {
        worker_guards,
        stack_handle,
        filter_handle,
        base_filter,
    })
}

fn build_output_stack(config: &LoggingConfiguration) -> Result<(OutputStack, Vec<WorkerGuard>), GenericError> {
    let mut layers: OutputStack = Vec::new();
    let mut guards = Vec::new();

    if config.log_to_console {
        let (nb_stdout, guard) = writer_to_nonblocking("console", std::io::stdout());
        guards.push(guard);
        layers.push(build_formatting_layer(config, nb_stdout));
    }

    if !config.log_file.is_empty() {
        let appender_builder = RollingFileAppenderBase::builder();
        let appender = appender_builder
            .filename(config.log_file.clone())
            .max_filecount(config.log_file_max_rolls)
            .condition_max_file_size(config.log_file_max_size.as_u64())
            .build()
            .map_err(|e| generic_error!("Failed to build log file appender: {}", e))?;

        let (nb_appender, guard) = writer_to_nonblocking("file", appender);
        guards.push(guard);
        layers.push(build_formatting_layer(config, nb_appender));
    }

    Ok((layers, guards))
}

fn writer_to_nonblocking<W>(writer_name: &'static str, writer: W) -> (NonBlocking, WorkerGuard)
where
    W: Write + Send + 'static,
{
    let thread_name = format!("log-writer-{}", writer_name);
    NonBlockingBuilder::default()
        .thread_name(&thread_name)
        .buffered_lines_limit(NB_LOG_WRITER_BUFFER_SIZE)
        .lossy(true)
        .finish(writer)
}
