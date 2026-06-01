use std::fmt;

use bytesize::ByteSize;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

const DEFAULT_LOG_FILE_MAX_SIZE: ByteSize = ByteSize::mib(10);
const DEFAULT_LOG_FILE_MAX_ROLLS: usize = 1;

/// Logging configuration.
///
/// This is a plain value type. Callers construct instances directly, either via [`simple`] for a basic
/// console-only configuration or via a translator that maps from the application's wider configuration into this
/// type, applying any application-specific rules.
///
/// [`simple`]: LoggingConfiguration::simple
pub struct LoggingConfiguration {
    /// Verbosity directives (for example, `info`, `debug`, `saluki=trace`).
    pub log_level: LogLevel,

    /// Whether to emit log records as JSON instead of the default human-readable format.
    pub log_format_json: bool,

    /// Whether to use RFC 3339 timestamps (`2024-12-31T23:59:59Z`) in log output.
    ///
    /// When `false` (the default), timestamps use the legacy format (`2024-12-31 23:59:59 UTC`).
    ///
    /// Defaults to `false`.
    pub log_format_rfc3339: bool,

    /// Whether to write log records to standard output.
    pub log_to_console: bool,

    /// Whether to write log records to syslog.
    ///
    /// Defaults to `false`. When this is `true` and [`syslog_uri`] is empty, callers should resolve the destination to
    /// the platform's default local syslog URI before constructing the logging output stack.
    ///
    /// [`syslog_uri`]: LoggingConfiguration::syslog_uri
    pub log_to_syslog: bool,

    /// URI of the syslog destination.
    ///
    /// Defaults to an empty string. An empty value means "use the platform default" only when [`log_to_syslog`] is
    /// enabled; otherwise it has no effect. Supported URI schemes are handled by the syslog output implementation.
    ///
    /// [`log_to_syslog`]: LoggingConfiguration::log_to_syslog
    pub syslog_uri: String,

    /// Whether to use the Agent's RFC-style syslog header.
    ///
    /// Defaults to `false`, which preserves the Agent's legacy syslog header format. Set this to `true` when the
    /// receiving syslog daemon expects the Agent's RFC-style header.
    pub syslog_rfc: bool,

    /// Path to the log file to write to, or empty to disable file logging.
    pub log_file: String,

    /// Maximum size of a log file before it's rolled over.
    pub log_file_max_size: ByteSize,

    /// Maximum number of rolled-over log files to retain.
    pub log_file_max_rolls: usize,
}

impl LoggingConfiguration {
    /// Returns a configuration that writes only to the console in human-readable format at INFO level.
    ///
    /// Used as a safe default when an application hasn't yet supplied an explicit configuration.
    pub fn simple() -> Self {
        Self {
            log_level: LevelFilter::INFO.into(),
            log_format_json: false,
            log_format_rfc3339: false,
            log_to_console: true,
            log_to_syslog: false,
            syslog_uri: String::new(),
            syslog_rfc: false,
            log_file: String::new(),
            log_file_max_size: DEFAULT_LOG_FILE_MAX_SIZE,
            log_file_max_rolls: DEFAULT_LOG_FILE_MAX_ROLLS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_defaults_to_console_only_logging_with_syslog_disabled() {
        let config = LoggingConfiguration::simple();

        assert_eq!(config.log_level.as_env_filter().to_string(), "info");
        assert!(!config.log_format_json);
        assert!(!config.log_format_rfc3339);
        assert!(config.log_to_console);
        assert!(!config.log_to_syslog);
        assert!(config.syslog_uri.is_empty());
        assert!(!config.syslog_rfc);
        assert!(config.log_file.is_empty());
        assert_eq!(config.log_file_max_size, DEFAULT_LOG_FILE_MAX_SIZE);
        assert_eq!(config.log_file_max_rolls, DEFAULT_LOG_FILE_MAX_ROLLS);
    }
}

/// A parsed `tracing` log level filter.
///
/// Wraps [`EnvFilter`] so it can be deserialized from a string (for example, `"info"`, `"saluki=trace,info"`).
#[derive(Deserialize)]
#[serde(try_from = "String")]
pub struct LogLevel(EnvFilter);

impl LogLevel {
    /// Returns the underlying `EnvFilter`.
    pub fn as_env_filter(&self) -> EnvFilter {
        self.0.clone()
    }
}

impl From<LevelFilter> for LogLevel {
    fn from(level: LevelFilter) -> Self {
        Self(EnvFilter::default().add_directive(level.into()))
    }
}

impl TryFrom<String> for LogLevel {
    type Error = GenericError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(generic_error!("Log level cannot be empty."));
        }

        EnvFilter::builder()
            .parse(value)
            .map(Self)
            .error_context("Failed to parse valid log level.")
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
