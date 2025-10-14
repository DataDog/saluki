//! Bootstrap utilities.

use saluki_config::GenericConfiguration;
use saluki_error::{ErrorContext as _, GenericError};

use crate::{
    logging::{initialize_logging, LoggingConfiguration, LoggingGuard},
    memory::initialize_allocator_telemetry,
    metrics::initialize_metrics,
    tls::initialize_tls,
};

/// A drop guard for ensuring deferred cleanup of resources acquired during bootstrap.
#[derive(Default)]
pub struct BootstrapGuard {
    logging_guard: Option<LoggingGuard>,
}

/// Early application initialization.
///
/// This helper type is used to configure the various low-level shared resources required by the application, such as
/// the logging and metrics subsystems. It allows for programatic configuration through the use of environment
/// variables.
pub struct AppBootstrapper {
    logging_config: LoggingConfiguration,
    // TODO: Just prefix at the moment.
    metrics_config: String,
}

impl AppBootstrapper {
    /// Creates a new `AppBootstrapper` from environment-based configuration.
    ///
    /// Configuration for bootstrapping will be loaded from environment variables, with a prefix of `DD`.
    ///
    /// # Errors
    ///
    /// If the given configuration cannot be deserialized correctly, an error is returned.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let logging_config = LoggingConfiguration::from_configuration(config)?;
        Ok(Self {
            logging_config,
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
        let mut bootstrap_guard = BootstrapGuard::default();

        // Initialize the logging subsystem first, since we want to make it possible to get any logs from the rest of
        // the bootstrap process.
        let logging_guard = initialize_logging(&self.logging_config)
            .await
            .error_context("Failed to initialize logging subsystem.")?;
        bootstrap_guard.logging_guard = Some(logging_guard);

        // Initialize everything else.
        initialize_tls().error_context("Failed to initialize TLS subsystem.")?;
        initialize_allocator_telemetry()
            .await
            .error_context("Failed to initialize allocator telemetry subsystem.")?;
        initialize_metrics(self.metrics_config)
            .await
            .error_context("Failed to initialize metrics subsystem.")?;

        Ok(bootstrap_guard)
    }
}
