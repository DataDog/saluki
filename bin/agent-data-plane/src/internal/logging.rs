use agent_data_plane_config::DatadogBootstrap;
use async_trait::async_trait;
use bytesize::ByteSize;
use datadog_agent_commons::platform::PlatformSettings;
use saluki_app::logging::{LogLevel, LoggingConfiguration, LoggingOverrideController};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_component_config::ScopedConfig;
use saluki_core::runtime::{InitializationError, Supervisable, SupervisorFuture};
use saluki_error::{ErrorContext as _, GenericError};
use tokio::{pin, select};
use tracing::{debug, warn};

const FIRST_PARTY_LOG_TARGETS: &[&str] = &[
    "agent_data_plane",
    "containerd_protos",
    "datadog_protos",
    "datadog_agent_commons",
    "ddsketch",
    "resource_accounting",
    "otlp_protos",
    "ottl",
    "process_memory",
    "prometheus_exposition",
    "saluki_api",
    "saluki_app",
    "saluki_common",
    "saluki_components",
    "saluki_context",
    "saluki_core",
    "saluki_env",
    "saluki_error",
    "saluki_io",
    "saluki_metadata",
    "saluki_metrics",
    "saluki_tls",
    "stringtheory",
];

/// Logging configuration translator for matching the Datadog Agent's logging behavior.
pub struct LoggingConfigurationTranslator;

impl LoggingConfigurationTranslator {
    /// Builds a [`LoggingConfiguration`] from the typed bootstrap slice.
    ///
    /// # Errors
    ///
    /// Returns an error if the configured log level can't be parsed.
    pub fn translate(config: &DatadogBootstrap) -> Result<LoggingConfiguration, GenericError> {
        let mut logging = LoggingConfiguration::simple();

        logging.log_level = parse_optional_log_level_raw(config.log_level.clone())?;

        if let Some(format_json) = config.log_format_json {
            logging.log_format_json = format_json;
        }
        if let Some(format_rfc3339) = config.log_format_rfc3339 {
            logging.log_format_rfc3339 = format_rfc3339;
        }
        if let Some(to_console) = config.log_to_console {
            logging.log_to_console = to_console;
        }
        if let Some(to_syslog) = config.log_to_syslog {
            logging.log_to_syslog = to_syslog;
        }

        if logging.log_to_syslog {
            if let Some(syslog_rfc) = config.syslog_rfc {
                logging.syslog_rfc = syslog_rfc;
            }
            logging.syslog_uri = match config.syslog_uri.as_deref() {
                Some(uri) if !uri.is_empty() => uri.to_string(),
                _ => PlatformSettings::get_default_syslog_uri().to_string(),
            };
        }

        if let Some(max_size) = config.log_file_max_size_bytes {
            logging.log_file_max_size = ByteSize::b(max_size);
        }
        if let Some(max_rolls) = config.log_file_max_rolls {
            logging.log_file_max_rolls = max_rolls;
        }

        logging.log_file = if config.disable_file_logging.unwrap_or(false) {
            String::new()
        } else {
            match config.data_plane_log_file.as_deref() {
                Some(path) if !path.is_empty() => path.to_string(),
                _ => PlatformSettings::get_default_log_file_path()
                    .to_string_lossy()
                    .into_owned(),
            }
        };

        Ok(logging)
    }
}

fn parse_optional_log_level_raw(maybe_log_level: Option<String>) -> Result<LogLevel, GenericError> {
    match maybe_log_level {
        Some(log_level) => parse_adp_log_level(&log_level),
        None => first_party_log_level_filter("info"),
    }
}

fn parse_adp_log_level(value: &str) -> Result<LogLevel, GenericError> {
    let trimmed = value.trim();
    if let Some(level) = plain_log_level(trimmed) {
        first_party_log_level_filter(level)
    } else {
        LogLevel::try_from(value.to_string()).error_context("Failed to parse log filter directives.")
    }
}

fn plain_log_level(value: &str) -> Option<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        "trace" => Some("trace"),
        "debug" => Some("debug"),
        "info" => Some("info"),
        "warn" => Some("warn"),
        "error" => Some("error"),
        "off" => Some("off"),
        _ => None,
    }
}

fn first_party_log_level_filter(level: &str) -> Result<LogLevel, GenericError> {
    let filter = FIRST_PARTY_LOG_TARGETS
        .iter()
        .map(|target| format!("{target}={level}"))
        .collect::<Vec<_>>()
        .join(",");

    LogLevel::try_from(filter).error_context("Failed to parse first-party log filter directives.")
}

/// A worker that watches a typed log-level handle and adjusts current filter directives to match.
pub struct DynamicLogLevelWorker {
    log_level: ScopedConfig<Option<String>>,
    controller: LoggingOverrideController,
}

impl DynamicLogLevelWorker {
    /// Creates a new `DynamicLogLevelWorker`.
    pub fn new(log_level: ScopedConfig<Option<String>>, controller: LoggingOverrideController) -> Self {
        Self { log_level, controller }
    }
}

#[async_trait]
impl Supervisable for DynamicLogLevelWorker {
    fn name(&self) -> &str {
        "dynamic-log-level"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let mut log_level = self.log_level.clone();
        let controller = self.controller.clone();

        Ok(Box::pin(async move {
            pin!(process_shutdown);

            debug!("Dynamic log level worker started.");

            loop {
                select! {
                    _ = &mut process_shutdown => break,
                    _ = log_level.changed() => {
                        match parse_optional_log_level_raw(log_level.current()) {
                            Ok(level) => {
                                if let Err(e) = controller.update_base(level.as_env_filter()).await {
                                    warn!(error = %e, %level, "Failed to apply updated log level.");
                                }
                            }
                            Err(e) => warn!(error = %e, "Failed to parse updated log level."),
                        }
                    }
                }
            }

            debug!("Dynamic log level worker stopped.");

            Ok(())
        }))
    }
}
