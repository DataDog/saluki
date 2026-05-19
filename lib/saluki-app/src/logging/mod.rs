//! Logging.

// TODO: `AgentLikeFieldVisitor` currently allocates a `String` to hold the message field when it finds it. This is
// suboptimal because it means we allocate a string literally every time we log a message. Logging is rare, but it's
// just a recipe for small, unnecessary allocations over time... and makes it that much more inefficient to enable
// debug/trace-level logging in production.
//
// We might consider _something_ like a string pool in the future, but we can defer that until we have a better idea of
// what the potential impact is in practice.

use std::io::Write;

use saluki_error::{generic_error, GenericError};
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_rolling_file::RollingFileAppenderBase;
use tracing_subscriber::{layer::SubscriberExt as _, reload, util::SubscriberInitExt as _, Layer, Registry};

mod api;
pub use self::api::{LoggingAPIHandler, LoggingOverrideController, LoggingOverrideWorker};

mod config;
pub use self::config::{LogLevel, LoggingConfiguration};

mod layer;
use self::layer::{build_formatting_layer, build_syslog_formatting_layer};

mod syslog;
use self::syslog::SyslogWriter;

// Number of buffered lines in each non-blocking log writer.
//
// This directly influences the idle memory usage since each logging backend (console, file, etc) will have a bounded
// channel that can hold this many elements, and each element is roughly 32 bytes, so 1,000 elements/lines consumes a
// minimum of ~32KB, etc.
const NB_LOG_WRITER_BUFFER_SIZE: usize = 4096;

type OutputStack = Vec<Box<dyn Layer<Registry> + Send + Sync>>;

/// A handle to the dynamic logging subsystem.
///
/// Held by [`BootstrapGuard`][crate::bootstrap::BootstrapGuard] for the lifetime of the application. Owns the
/// worker guards (which flush buffered output on drop), and exposes [`reload`][Self::reload] for swapping the
/// entire logging configuration plus [`controller`][Self::controller] for driving runtime filter changes through
/// the override worker.
pub struct LoggingGuard {
    worker_guards: Vec<WorkerGuard>,
    stack_handle: reload::Handle<OutputStack, Registry>,
    controller: LoggingOverrideController,
}

impl LoggingGuard {
    /// Reloads the logging subsystem from the given configuration.
    ///
    /// Rebuilds the output layer stack from `config` and routes the new level filter through the override worker
    /// as the new base filter, so an active override is preserved (the new base will take effect once the override
    /// expires). Worker guards for the previous outputs are dropped after the swap, which flushes any buffered log
    /// lines to their original destinations.
    ///
    /// This is the right entry point when the entire logging configuration may have changed (for example, outputs,
    /// format, level). For runtime base-filter changes only -- such as following a `log_level` config update --
    /// use [`controller`][Self::controller] and call
    /// [`update_base`][LoggingOverrideController::update_base] directly.
    ///
    /// # Errors
    ///
    /// Returns an error if the new output layers can't be constructed (for example, the configured log file path is
    /// inaccessible) or if the override worker is no longer running.
    pub async fn reload(&mut self, config: LoggingConfiguration) -> Result<(), GenericError> {
        let (new_stack, new_guards) = build_output_stack(&config)?;
        let new_filter = config.log_level.as_env_filter();

        self.stack_handle
            .reload(new_stack)
            .map_err(|e| generic_error!("Failed to swap logging output stack: {}", e))?;
        self.controller.update_base(new_filter).await?;

        // Drop the old worker guards _after_ the swap so any buffered lines are flushed to their original destinations
        // before the worker threads exit.
        let _old_guards = std::mem::replace(&mut self.worker_guards, new_guards);

        Ok(())
    }

    /// Returns a logging override controller that can be used to change the default filter directives.
    pub fn controller(&self) -> LoggingOverrideController {
        self.controller.clone()
    }
}

/// Initializes the logging subsystem for `tracing` with the ability to dynamically update the log filtering directives
/// at runtime.
///
/// Returns a [`LoggingGuard`] which must be held until the application is about to shutdown, plus a
/// [`LoggingOverrideWorker`] that must be added to a [`Supervisor`][saluki_core::runtime::Supervisor] to drive
/// the dynamic override processor; the worker also asserts the privileged API routes for runtime filter control.
/// Without the worker running, override requests are accepted but never applied.
///
/// # Errors
///
/// If the logging subsystem was already initialized, an error will be returned.
pub(crate) async fn initialize_logging(
    config: LoggingConfiguration,
) -> Result<(LoggingGuard, LoggingOverrideWorker), GenericError> {
    // Build the initial output stack from the supplied configuration. This is later swappable via
    // `BootstrapGuard::reload_logging` once the Datadog Agent provides authoritative configuration.
    let (output_stack, worker_guards) = build_output_stack(&config)?;
    let (output_layer, stack_handle) = reload::Layer::new(output_stack);

    // Set up our log level filtering and dynamic filter layer.
    let (filter_layer, filter_handle) = reload::Layer::new(config.log_level.as_env_filter());

    // The override worker owns the canonical base filter -- the directives the system restores to after an override
    // expires or is reset. It seeds the base from the reload handle on startup and is updated via the controller,
    // both by `LoggingGuard::reload` once the Agent's configuration is applied and by any other caller (for example, a
    // runtime `log_level` watcher) wired up via [`LoggingGuard::controller`].
    let (override_worker, controller) = LoggingOverrideWorker::new(filter_handle);

    tracing_subscriber::registry()
        .with(output_layer.with_filter(filter_layer))
        .try_init()?;

    Ok((
        LoggingGuard {
            worker_guards,
            stack_handle,
            controller,
        },
        override_worker,
    ))
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

    if config.log_to_syslog {
        let syslog_writer = SyslogWriter::from_uri(&config.syslog_uri)
            .map_err(|e| generic_error!("Failed to build syslog log writer: {}", e))?;
        // Keep syslog on the same lossy non-blocking path as console/file so logging never
        // backpressures ADP.
        let (nb_syslog, guard) = writer_to_nonblocking("syslog", syslog_writer);
        guards.push(guard);
        layers.push(build_syslog_formatting_layer(config, nb_syslog));
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

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SYSLOG_URI: &str = "udp://127.0.0.1:9";

    #[test]
    fn output_stack_skips_syslog_when_disabled() {
        let config = logging_config_without_outputs();

        let (layers, guards) = build_output_stack(&config).expect("build output stack");

        assert_eq!(layers.len(), 0);
        assert_eq!(guards.len(), 0);
    }

    #[test]
    fn output_stack_adds_syslog_layer_and_guard_when_enabled() {
        let config = logging_config_with_syslog(TEST_SYSLOG_URI);

        let (layers, guards) = build_output_stack(&config).expect("build output stack with syslog");

        assert_eq!(layers.len(), 1);
        assert_eq!(guards.len(), 1);
    }

    #[test]
    fn output_stack_fails_when_syslog_uri_is_invalid() {
        let config = logging_config_with_syslog("http://127.0.0.1:514");

        let error = match build_output_stack(&config) {
            Ok(_) => panic!("invalid syslog URI should fail output stack build"),
            Err(error) => error,
        };

        let error = error.to_string();
        assert!(error.contains("Failed to build syslog log writer"));
        assert!(error.contains("Unsupported syslog URI scheme 'http'"));
    }

    #[tokio::test]
    async fn reload_can_enable_change_disable_syslog_and_preserves_previous_stack_on_invalid_config() {
        use saluki_core::runtime::Supervisor;
        use tokio::sync::oneshot;

        let config = logging_config_without_outputs();
        let (output_stack, worker_guards) = build_output_stack(&config).expect("build initial output stack");
        let (output_layer, stack_handle) = reload::Layer::new(output_stack);
        let (filter_layer, filter_handle) = reload::Layer::new(config.log_level.as_env_filter());
        let (override_worker, controller) = LoggingOverrideWorker::new(filter_handle);
        let mut guard = LoggingGuard {
            worker_guards,
            stack_handle,
            controller,
        };
        let _keep_layers_alive = (output_layer, filter_layer);

        // Run the override worker inside a Supervisor so it has the dataspace context required for
        // its route assertion. The supervisor's shutdown signal drives the worker to exit cleanly.
        let mut sup = Supervisor::new("test-logging-override").expect("create supervisor");
        sup.add_worker(override_worker);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let sup_handle = tokio::spawn(async move { sup.run_with_shutdown(shutdown_rx).await });

        guard
            .reload(logging_config_with_syslog(TEST_SYSLOG_URI))
            .await
            .expect("reload should enable syslog");
        assert_eq!(guard.worker_guards.len(), 1);

        guard
            .reload(logging_config_with_syslog("udp://127.0.0.1:10"))
            .await
            .expect("reload should change syslog URI");
        assert_eq!(guard.worker_guards.len(), 1);

        guard
            .reload(logging_config_without_outputs())
            .await
            .expect("reload should disable syslog");
        assert_eq!(guard.worker_guards.len(), 0);

        guard
            .reload(logging_config_with_syslog(TEST_SYSLOG_URI))
            .await
            .expect("reload should re-enable syslog");
        assert_eq!(guard.worker_guards.len(), 1);

        let error = guard
            .reload(logging_config_with_syslog("http://127.0.0.1:514"))
            .await
            .expect_err("invalid syslog URI should fail reload");
        assert!(error.to_string().contains("Failed to build syslog log writer"));
        assert_eq!(guard.worker_guards.len(), 1);

        // Trigger supervisor shutdown so the worker exits via its shutdown branch rather than via
        // the channel-close path (which would prompt a restart attempt by the supervisor).
        shutdown_tx.send(()).expect("send shutdown");
        sup_handle
            .await
            .expect("supervisor task joins")
            .expect("supervisor should exit cleanly");
        drop(guard);
    }

    fn logging_config_without_outputs() -> LoggingConfiguration {
        let mut config = LoggingConfiguration::simple();
        config.log_to_console = false;
        config.log_file.clear();
        config.log_to_syslog = false;
        config.syslog_uri.clear();
        config
    }

    fn logging_config_with_syslog(uri: &str) -> LoggingConfiguration {
        let mut config = logging_config_without_outputs();
        config.log_to_syslog = true;
        config.syslog_uri = uri.to_string();
        config
    }
}
