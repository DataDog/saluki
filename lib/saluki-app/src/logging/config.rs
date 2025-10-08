use bytesize::ByteSize;
use saluki_common::deser::PermissiveBool;
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;
use serde_with::serde_as;
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

fn default_log_level() -> LogLevel {
    LevelFilter::INFO.into()
}

const fn default_true() -> bool {
    true
}

const fn default_false() -> bool {
    false
}

const fn default_log_file_max_size() -> ByteSize {
    ByteSize::mib(10)
}

const fn default_log_file_max_rolls() -> usize {
    1
}

#[serde_as]
#[derive(Deserialize)]
pub(crate) struct LoggingConfiguration {
    #[serde(default = "default_log_level")]
    pub log_level: LogLevel,
    #[serde_as(as = "PermissiveBool")]
    #[serde(default = "default_false")]
    pub log_format_json: bool,

    #[serde_as(as = "PermissiveBool")]
    #[serde(default = "default_true")]
    pub log_to_console: bool,

    #[serde(default = "String::new")]
    pub log_file: String,
    #[serde(default = "default_log_file_max_size")]
    pub log_file_max_size: ByteSize,
    #[serde(default = "default_log_file_max_rolls")]
    pub log_file_max_rolls: usize,
}

impl LoggingConfiguration {
    /// Creates a new `LoggingConfiguration` instance from the given configuration.
    ///
    /// # Errors
    ///
    /// If the configuration cannot be deserialized as `LoggingConfiguration`, an error is returned.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let logging_config = config.as_typed()?;
        Ok(logging_config)
    }
}

#[derive(Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct LogLevel(EnvFilter);

impl LogLevel {
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
