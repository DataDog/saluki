//! Bootstrap utilities.

use saluki_config::GenericConfiguration;
use saluki_error::{ErrorContext as _, GenericError};

use crate::{
    logging::{initialize_logging, LoggingConfiguration, LoggingGuard},
    metrics::initialize_metrics,
    tls::initialize_tls,
};

/// A drop guard for ensuring deferred cleanup of resources acquired during bootstrap.
pub struct BootstrapGuard {
    logging_guard: LoggingGuard,
}

impl BootstrapGuard {
    /// Reloads the logging subsystem with the given configuration.
    ///
    /// Rebuilds the output layer stack and updates the level filter to match `config`, swapping both atomically into
    /// the already-installed `tracing` subscriber. Worker guards for the previous outputs are dropped after the swap,
    /// which flushes any buffered log lines to their original destinations.
    ///
    /// This is intended to be called exactly once, after the Datadog Agent has provided its authoritative
    /// configuration. Further runtime reconfiguration of logging is not supported.
    ///
    /// # Errors
    ///
    /// Returns an error if the new output layers cannot be constructed (e.g., the configured log file path is
    /// inaccessible).
    pub fn reload_logging(&mut self, config: LoggingConfiguration) -> Result<(), GenericError> {
        self.logging_guard.reload(config)
    }
}

/// Early application initialization.
///
/// This helper type is used to configure the various low-level shared resources required by the application, such as
/// the logging and metrics subsystems.
pub struct AppBootstrapper {
    logging_config: LoggingConfiguration,
    // TODO: Just prefix at the moment.
    metrics_config: String,
}

impl AppBootstrapper {
    /// Creates a new `AppBootstrapper`.
    ///
    /// The bootstrapper is initialized with a [`simple`][LoggingConfiguration::simple] logging configuration. Callers
    /// that have application-specific logging requirements should follow up with
    /// [`with_logging_configuration`][Self::with_logging_configuration] to override this default.
    ///
    /// # Errors
    ///
    /// This currently does not fail, but the signature returns `Result` to leave room for future failures.
    pub fn from_configuration(_config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(Self {
            logging_config: LoggingConfiguration::simple(),
            metrics_config: "saluki".to_string(),
        })
    }

    /// Sets the prefix to use for internal metrics.
    ///
    /// Defaults to "saluki".
    pub fn with_metrics_prefix<S: Into<String>>(mut self, prefix: S) -> Self {
        self.metrics_config = prefix.into();
        self
    }

    /// Sets the logging configuration to use during bootstrap.
    ///
    /// Replaces the [`simple`][LoggingConfiguration::simple] default that
    /// [`from_configuration`][Self::from_configuration] installs.
    pub fn with_logging_configuration(mut self, logging_config: LoggingConfiguration) -> Self {
        self.logging_config = logging_config;
        self
    }

    /// Executes the bootstrap operation, initializing all configured subsystems.
    ///
    /// A [`BootstrapGuard`] is returned, which must be held until the application is ready to shut down. This guard
    /// ensures that all relevant resources created during the bootstrap phase are properly cleaned up/flushed before
    /// the application exits.
    ///
    /// # Errors
    ///
    /// If any of the bootstrap steps fail, an error will be returned.
    pub async fn bootstrap(self) -> Result<BootstrapGuard, GenericError> {
        // Initialize the logging subsystem first, since we want to make it possible to get any logs from the rest of
        // the bootstrap process.
        let logging_guard = initialize_logging(self.logging_config)
            .await
            .error_context("Failed to initialize logging subsystem.")?;

        // Initialize everything else.
        initialize_tls().error_context("Failed to initialize TLS subsystem.")?;
        initialize_metrics(self.metrics_config)
            .await
            .error_context("Failed to initialize metrics subsystem.")?;

        Ok(BootstrapGuard { logging_guard })
    }
}
