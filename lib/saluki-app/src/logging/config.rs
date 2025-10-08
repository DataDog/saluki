use bytesize::ByteSize;
use saluki_common::deser::PermissiveBool;
use saluki_config::{ConfigurationLoader, GenericConfiguration};
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
pub struct LoggingConfiguration {
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
    pub fn from_config(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let logging_config = config.as_typed()?;
        Ok(logging_config)
    }

    pub async fn from_environment() -> Result<Self, GenericError> {
        // TODO: For the sake of transitioning to this new bootstrapping pattern, we're just creating a configuration
        // manually here so that we can drive everything through use of `LoggingConfiguration` instead of two different
        // code paths. That means we want to use `GenericConfiguration` to source our environment variables instead of
        // querying them manually... mostly to ensure that doing it that way (the way we want to do it overall) is
        // consistent with how we're doing it by hand at the moment.
        let config = ConfigurationLoader::default()
            .from_environment("DD")
            .expect("Environment variable prefix is not empty.")
            .into_generic()
            .await?;
        Self::from_config(&config)
    }
}

#[derive(Deserialize)]
#[serde(try_from = "String")]
pub struct LogLevel(EnvFilter);

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
