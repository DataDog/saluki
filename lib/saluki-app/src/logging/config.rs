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
    /// Verbosity directives (e.g., `info`, `debug`, `saluki=trace`).
    pub log_level: LogLevel,

    /// Whether to emit log records as JSON instead of the default human-readable format.
    pub log_format_json: bool,

    /// Whether to write log records to standard output.
    pub log_to_console: bool,

    /// Path to the log file to write to, or empty to disable file logging.
    pub log_file: String,

    /// Maximum size of a log file before it is rolled over.
    pub log_file_max_size: ByteSize,

    /// Maximum number of rolled-over log files to retain.
    pub log_file_max_rolls: usize,
}

impl LoggingConfiguration {
    /// Returns a configuration that writes only to the console in human-readable format at INFO level.
    ///
    /// Used as a safe default when an application has not yet supplied an explicit configuration.
    pub fn simple() -> Self {
        Self {
            log_level: LevelFilter::INFO.into(),
            log_format_json: false,
            log_to_console: true,
            log_file: String::new(),
            log_file_max_size: DEFAULT_LOG_FILE_MAX_SIZE,
            log_file_max_rolls: DEFAULT_LOG_FILE_MAX_ROLLS,
        }
    }
}

/// A parsed `tracing` log level filter.
///
/// Wraps [`EnvFilter`] so it can be deserialized from a string (e.g., `"info"`, `"saluki=trace,info"`).
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
