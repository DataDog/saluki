//! Bootstrap utilities.

use metrics::Level;
use saluki_core::runtime::Supervisor;
use saluki_error::{ErrorContext as _, GenericError};

use crate::{
    logging::{initialize_logging, LoggingConfiguration, LoggingGuard},
    metrics::initialize_metrics,
    tls::initialize_tls,
};

/// The result of running [`AppBootstrapper::bootstrap`].
///
/// Bundles together the [`BootstrapGuard`] that must be held for the lifetime of the application with the
/// [`Supervisor`] that drives the background workers spawned during bootstrap. Callers must arrange for the
/// supervisor to be run (either directly or by adding it to a parent supervisor) for those workers to make
/// progress.
pub struct Bootstrap {
    /// Supervisor populated with workers for all background async tasks created during bootstrap.
    pub supervisor: Supervisor,

    /// Drop guard for resources acquired during bootstrap.
    pub guard: BootstrapGuard,
}

/// A drop guard for ensuring deferred cleanup of resources acquired during bootstrap.
pub struct BootstrapGuard {
    logging_guard: LoggingGuard,
}

impl BootstrapGuard {
    /// Returns a reference to the [`LoggingGuard`].
    ///
    /// Use this to obtain a [`LoggingOverrideController`][crate::logging::LoggingOverrideController] clone (via
    /// [`LoggingGuard::controller`]) for downstream callers that drive runtime filter changes.
    pub fn logging(&self) -> &LoggingGuard {
        &self.logging_guard
    }

    /// Returns a mutable reference to the [`LoggingGuard`].
    ///
    /// Use this to swap the entire logging configuration (outputs, format, level) via [`LoggingGuard::reload`].
    pub fn logging_mut(&mut self) -> &mut LoggingGuard {
        &mut self.logging_guard
    }
}

/// Early application initialization.
///
/// This helper type is used to configure the various low-level shared resources required by the application, such as
/// the logging and metrics subsystems.
pub struct AppBootstrapper {
    logging_config: LoggingConfiguration,
    metrics_prefix: String,
    metrics_default_level: Level,
}

impl Default for AppBootstrapper {
    fn default() -> Self {
        Self {
            logging_config: LoggingConfiguration::simple(),
            metrics_prefix: "saluki".to_string(),
            metrics_default_level: Level::INFO,
        }
    }
}

impl AppBootstrapper {
    /// Creates a new `AppBootstrapper`.
    ///
    /// The bootstrapper is initialized with a [`simple`][LoggingConfiguration::simple] logging configuration. Callers
    /// that have application-specific logging requirements should follow up with
    /// [`with_logging_configuration`][Self::with_logging_configuration] to override this default.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the prefix to use for internal metrics.
    ///
    /// Defaults to "saluki".
    pub fn with_metrics_prefix<S: Into<String>>(mut self, prefix: S) -> Self {
        self.metrics_prefix = prefix.into();
        self
    }

    /// Sets the default filter level for internal metrics.
    ///
    /// Metrics whose level is more verbose than this default are filtered out at flush time. The default also drives
    /// the level that the filter is restored to whenever a runtime override is reset.
    ///
    /// Defaults to [`Level::INFO`].
    pub fn with_metrics_default_level(mut self, level: Level) -> Self {
        self.metrics_default_level = level;
        self
    }

    /// Sets the logging configuration to use during bootstrap.
    ///
    /// Replaces the [`simple`][LoggingConfiguration::simple] default that
    /// [`new`][Self::new] installs.
    pub fn with_logging_configuration(mut self, logging_config: LoggingConfiguration) -> Self {
        self.logging_config = logging_config;
        self
    }

    /// Executes the bootstrap operation, initializing all configured subsystems.
    ///
    /// Returns a [`Bootstrap`] containing both a [`BootstrapGuard`] (which must be held until the application is
    /// ready to shut down) and a [`Supervisor`] populated with workers for all background async tasks created
    /// during bootstrap. Callers must arrange for the supervisor to run (typically by adding it to a parent
    /// supervisor or calling [`Supervisor::run_with_shutdown`]) for those workers to make progress.
    ///
    /// # Errors
    ///
    /// If any of the bootstrap steps fail, an error will be returned.
    pub async fn bootstrap(self) -> Result<Bootstrap, GenericError> {
        // Initialize the logging subsystem first, since we want to make it possible to get any logs from the rest of
        // the bootstrap process.
        let (logging_guard, logging_override) = initialize_logging(self.logging_config)
            .await
            .error_context("Failed to initialize logging subsystem.")?;

        // Initialize everything else.
        initialize_tls().error_context("Failed to initialize TLS subsystem.")?;
        let metrics_workers = initialize_metrics(self.metrics_prefix, self.metrics_default_level)
            .await
            .error_context("Failed to initialize metrics subsystem.")?;

        // Build the supervisor for all bootstrap-spawned background workers. The default ambient runtime mode is
        // appropriate here: these are lightweight tasks that share the parent runtime, and the runtime metrics
        // worker has already eagerly captured the parent's `Handle` so it always describes the right runtime.
        let mut supervisor =
            Supervisor::new("app-bootstrap").error_context("Failed to construct app bootstrap supervisor.")?;
        supervisor.add_worker(logging_override);
        supervisor.add_worker(metrics_workers.flusher);
        supervisor.add_worker(metrics_workers.runtime);
        supervisor.add_worker(metrics_workers.override_processor);

        Ok(Bootstrap {
            supervisor,
            guard: BootstrapGuard { logging_guard },
        })
    }
}
