//! Translation between the Datadog Agent's [`GenericConfiguration`] and ADP's [`LoggingConfiguration`].
//!
//! ADP's logging behavior must follow the Datadog Agent's logging configuration for the settings that are sensibly
//! shared (level, format, console output, rotation), but it must use its own per-subagent destination so it does not
//! collide with the Core Agent's own log file. This module owns those rules in one place.

use bytesize::ByteSize;
use saluki_app::logging::{LogLevel, LoggingConfiguration};
use saluki_common::deser::PermissiveBool;
use saluki_config::GenericConfiguration;
use saluki_error::{ErrorContext as _, GenericError};
use serde::Deserialize;
use serde_with::serde_as;

use crate::internal::platform::PlatformSettings;

const DATA_PLANE_LOG_FILE_KEY: &str = "data_plane.log_file";

/// Logging configuration translator for matching the Datadog Agent's logging behavior.
///
/// In the Datadog Agent, all processes generally follow the same logging configuration, paying attention to the same
/// settings/keys for determining log level, log format, and so on. They differ in some ways, such as determining what
/// file to write to when logging to file is enabled. We want ADP to follow the same pattern.
///
/// `LoggingConfigurationTranslator` takes an incoming generic configuration and allows generating a Saluki-oriented
/// [`LoggingConfiguration`] from it, overriding the ADP-specific setting as necessary. This ensures that we obey
/// all the logging configuration rules set by the Datadog Agent but log to the right location for the ADP process.
pub struct LoggingConfigurationTranslator;

impl LoggingConfigurationTranslator {
    /// Builds a [`LoggingConfiguration`] from the given configuration, applying ADP's per-subagent rules.
    ///
    /// # Errors
    ///
    /// Returns an error if any of the consulted keys is present but cannot be parsed into the expected type.
    pub fn translate(config: &GenericConfiguration) -> Result<LoggingConfiguration, GenericError> {
        let mut logging = LoggingConfiguration::simple();

        if let Some(level) = config
            .try_get_typed::<LogLevel>("log_level")
            .error_context("Failed to read `log_level`.")?
        {
            logging.log_level = level;
        }

        if let Some(format_json) = read_permissive_bool(config, "log_format_json")? {
            logging.log_format_json = format_json;
        }

        if let Some(to_console) = read_permissive_bool(config, "log_to_console")? {
            logging.log_to_console = to_console;
        }

        if let Some(max_size) = config
            .try_get_typed::<ByteSize>("log_file_max_size")
            .error_context("Failed to read `log_file_max_size`.")?
        {
            logging.log_file_max_size = max_size;
        }

        if let Some(max_rolls) = config
            .try_get_typed::<usize>("log_file_max_rolls")
            .error_context("Failed to read `log_file_max_rolls`.")?
        {
            logging.log_file_max_rolls = max_rolls;
        }

        // File destination: per-subagent key only, with the platform default as fallback. The Core Agent's `log_file`
        // is deliberately never consulted. `disable_file_logging` short-circuits the file output entirely.
        let disable_file_logging = read_permissive_bool(config, "disable_file_logging")?.unwrap_or(false);
        logging.log_file = if disable_file_logging {
            String::new()
        } else {
            let configured = config
                .try_get_typed::<String>(DATA_PLANE_LOG_FILE_KEY)
                .with_error_context(|| format!("Failed to read `{}`.", DATA_PLANE_LOG_FILE_KEY))?;
            match configured {
                Some(path) if !path.is_empty() => path,
                _ => PlatformSettings::get_default_log_file_path()
                    .to_string_lossy()
                    .into_owned(),
            }
        };

        Ok(logging)
    }
}

/// Reads a configuration key as a permissive boolean (accepts `true`/`false`, `"true"`/`"false"`, `"1"`/`"0"`, etc.).
///
/// Returns `Ok(None)` if the key is absent.
fn read_permissive_bool(config: &GenericConfiguration, key: &str) -> Result<Option<bool>, GenericError> {
    Ok(config
        .try_get_typed::<PermissiveBoolValue>(key)
        .with_error_context(|| format!("Failed to read `{}`.", key))?
        .map(|v| v.0))
}

#[serde_as]
#[derive(Deserialize)]
struct PermissiveBoolValue(#[serde_as(as = "PermissiveBool")] bool);
