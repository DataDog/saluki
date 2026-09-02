//! Translation between ADP's typed logging configuration and the logging stack's own configuration.
//!
//! ADP's logging behavior must follow the Datadog Agent's logging configuration for the settings that are sensibly
//! shared (level, format, console output, rotation), but it must use its own per-subagent destination so it doesn't
//! collide with the Core Agent's own log file. This module owns those rules in one place.

use agent_data_plane_config::{control::Logging, Live};
use async_trait::async_trait;
use bytesize::ByteSize;
use datadog_agent_commons::platform::PlatformSettings;
use saluki_app::logging::{LogLevel, LoggingConfiguration, LoggingOverrideController};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{InitializationError, Supervisable, SupervisorFuture};
use saluki_error::{ErrorContext as _, GenericError};
use tokio::{pin, select};
use tracing::{debug, warn};

// `tracing` targets use Rust crate/module names, so Cargo package names with hyphens appear with underscores.
const FIRST_PARTY_LOG_TARGETS: &[&str] = &[
    "agent_data_plane",
    "containerd_protos",
    "datadog_protos",
    "datadog_agent_commons",
    "ddsketch",
    "otlp_protos",
    "ottl",
    "process_memory",
    "prometheus_exposition",
    "saluki_api",
    "saluki_app",
    "saluki_common",
    "saluki_components",
    "saluki_config",
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
///
/// In the Datadog Agent, all processes generally follow the same logging configuration, paying attention to the same
/// settings/keys for determining log level, log format, and so on. They differ in some ways, such as determining what
/// file to write to when logging to file is enabled. We want ADP to follow the same pattern.
///
/// `LoggingConfigurationTranslator` takes the typed [`Logging`] slice and allows generating a Saluki-oriented
/// [`LoggingConfiguration`] from it, overriding the ADP-specific setting as necessary. This ensures that we obey
/// all the logging configuration rules set by the Datadog Agent but log to the right location for the ADP process.
pub struct LoggingConfigurationTranslator;

impl LoggingConfigurationTranslator {
    /// Builds a [`LoggingConfiguration`] from the typed logging configuration, applying ADP's per-subagent rules.
    ///
    /// # Errors
    ///
    /// Returns an error if the configured log level is not a level name or a valid set of filter directives.
    pub fn translate(logging: &Logging) -> Result<LoggingConfiguration, GenericError> {
        let mut config = LoggingConfiguration::simple();

        config.log_level = parse_adp_log_level(&logging.level)?;
        config.log_format_json = logging.format_json;
        config.log_format_rfc3339 = logging.format_rfc3339;
        config.log_to_console = logging.to_console;
        config.log_to_syslog = logging.to_syslog;

        if logging.to_syslog {
            config.syslog_rfc = logging.syslog_rfc;
            config.syslog_uri = if logging.syslog_uri.is_empty() {
                PlatformSettings::get_default_syslog_uri().to_string()
            } else {
                logging.syslog_uri.clone()
            };
        }

        // Preserve the logging stack's binary default when the schema value is defaulted.
        if logging.file_max_size.is_explicit() {
            config.log_file_max_size = ByteSize::b(logging.file_max_size.value);
        }
        config.log_file_max_rolls = logging.file_max_rolls;

        // Use the platform default unless the ADP log file was set explicitly.
        config.log_file = if logging.disable_file_logging {
            String::new()
        } else if logging.file.is_explicit() && !logging.file.value.is_empty() {
            logging.file.value.clone()
        } else {
            PlatformSettings::get_default_log_file_path()
                .to_string_lossy()
                .into_owned()
        };

        Ok(config)
    }
}

/// Parses a configured log level, expanding a plain level name into per-target directives.
///
/// Plain levels apply to ADP crates; other values are `EnvFilter` directives.
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

/// A worker that watches the configured log level and adjusts the logging stack's current filter directives to match.
///
/// The worker relies on dynamic configuration; if it's not enabled, the worker simply idles until shutdown.
pub struct DynamicLogLevelWorker {
    level: Live<String>,
    controller: LoggingOverrideController,
}

impl DynamicLogLevelWorker {
    /// Creates a new `DynamicLogLevelWorker` watching the given log level.
    pub fn new(level: Live<String>, controller: LoggingOverrideController) -> Self {
        Self { level, controller }
    }
}

#[async_trait]
impl Supervisable for DynamicLogLevelWorker {
    fn name(&self) -> &str {
        "dynamic-log-level"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let mut level = self.level.clone();
        let controller = self.controller.clone();

        Ok(Box::pin(async move {
            pin!(process_shutdown);

            debug!("Dynamic log level worker started.");

            loop {
                select! {
                    _ = &mut process_shutdown => break,
                    new_level = level.changed() => {
                        match parse_adp_log_level(&new_level) {
                            Ok(log_level) => {
                                if let Err(e) = controller.update_base(log_level.as_env_filter()).await {
                                    warn!(error = %e, %log_level, "Failed to apply updated log level.");
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

#[cfg(test)]
mod tests {
    use agent_data_plane_config::ConfigValue;

    use super::*;

    fn defaulted_logging() -> Logging {
        Logging {
            level: "info".to_string(),
            format_rfc3339: false,
            format_json: false,
            to_console: true,
            to_syslog: false,
            syslog_rfc: false,
            syslog_uri: String::new(),
            file: ConfigValue::defaulted("/var/log/datadog/agent-data-plane.log".to_string()),
            disable_file_logging: false,
            file_max_rolls: 1,
            file_max_size: ConfigValue::defaulted(10_000_000),
        }
    }

    fn translate_level(level: &str) -> Result<Vec<String>, GenericError> {
        let logging = Logging {
            level: level.to_string(),
            ..defaulted_logging()
        };

        LoggingConfigurationTranslator::translate(&logging)
            .map(|config| config.log_level.as_env_filter().to_string())
            .map(|filter| filter.split(',').map(str::to_string).collect())
    }

    #[test]
    fn default_log_level_becomes_first_party_info() {
        let directives = translate_level("info").expect("translate logging config");

        assert!(directives.contains(&"agent_data_plane=info".to_string()));
    }

    #[test]
    fn plain_log_level_becomes_first_party_filter() {
        let directives = translate_level("warn").expect("translate logging config");

        assert!(directives.contains(&"agent_data_plane=warn".to_string()));
        assert!(directives.contains(&"saluki_components=warn".to_string()));

        assert!(!directives.contains(&"hyper=warn".to_string()));
        assert!(!directives.contains(&"tokio=warn".to_string()));
        assert!(!directives.contains(&"tonic=warn".to_string()));
        assert!(!directives.contains(&"warn".to_string()));
    }

    #[test]
    fn plain_log_level_is_case_insensitive() {
        let directives = translate_level("WaRn").expect("translate logging config");

        assert!(directives.contains(&"agent_data_plane=warn".to_string()));
    }

    #[test]
    fn advanced_log_level_directives_are_preserved() {
        let directives = translate_level("warn,agent_data_plane=debug,hyper=warn").expect("translate logging config");

        assert!(directives.contains(&"warn".to_string()));
        assert!(directives.contains(&"agent_data_plane=debug".to_string()));
        assert!(directives.contains(&"hyper=warn".to_string()));
    }

    #[test]
    fn unparseable_log_level_returns_error() {
        for level in ["agent_data_plane=verbose", ""] {
            assert!(
                translate_level(level).is_err(),
                "log level `{level}` should be rejected"
            );
        }
    }

    #[test]
    fn format_and_console_settings_are_carried_through() {
        let logging = Logging {
            format_json: true,
            format_rfc3339: true,
            to_console: false,
            ..defaulted_logging()
        };
        let config = LoggingConfigurationTranslator::translate(&logging).expect("translate logging config");

        assert!(config.log_format_json);
        assert!(config.log_format_rfc3339);
        assert!(!config.log_to_console);
    }

    #[test]
    fn defaults_leave_syslog_disabled_with_no_destination() {
        let config = LoggingConfigurationTranslator::translate(&defaulted_logging()).expect("translate logging config");

        assert!(!config.log_to_syslog);
        assert!(config.syslog_uri.is_empty());
        assert!(!config.syslog_rfc);
    }

    #[test]
    fn enabled_syslog_uses_configured_uri_and_framing() {
        let logging = Logging {
            to_syslog: true,
            syslog_uri: "udp://127.0.0.1:1514".to_string(),
            syslog_rfc: true,
            ..defaulted_logging()
        };
        let config = LoggingConfigurationTranslator::translate(&logging).expect("translate logging config");

        assert!(config.log_to_syslog);
        assert_eq!(config.syslog_uri, "udp://127.0.0.1:1514");
        assert!(config.syslog_rfc);
    }

    #[test]
    fn enabled_syslog_with_empty_uri_uses_platform_default() {
        let logging = Logging {
            to_syslog: true,
            ..defaulted_logging()
        };
        let config = LoggingConfigurationTranslator::translate(&logging).expect("translate logging config");

        assert!(config.log_to_syslog);
        assert_eq!(config.syslog_uri, PlatformSettings::get_default_syslog_uri());
        assert!(!config.syslog_rfc);
    }

    #[test]
    fn syslog_settings_have_no_effect_when_syslog_is_disabled() {
        let logging = Logging {
            to_syslog: false,
            syslog_uri: "udp://127.0.0.1:1514".to_string(),
            syslog_rfc: true,
            ..defaulted_logging()
        };
        let config = LoggingConfigurationTranslator::translate(&logging).expect("translate logging config");

        assert!(!config.log_to_syslog);
        assert!(config.syslog_uri.is_empty());
        assert!(!config.syslog_rfc);
    }

    #[test]
    fn defaulted_log_file_uses_the_platform_default_path() {
        let config = LoggingConfigurationTranslator::translate(&defaulted_logging()).expect("translate logging config");

        assert_eq!(
            config.log_file,
            PlatformSettings::get_default_log_file_path().to_string_lossy()
        );
    }

    #[test]
    fn explicitly_configured_log_file_is_used() {
        let logging = Logging {
            file: ConfigValue::explicit("/tmp/adp.log".to_string()),
            ..defaulted_logging()
        };
        let config = LoggingConfigurationTranslator::translate(&logging).expect("translate logging config");

        assert_eq!(config.log_file, "/tmp/adp.log");
    }

    #[test]
    fn explicitly_empty_log_file_uses_the_platform_default_path() {
        let logging = Logging {
            file: ConfigValue::explicit(String::new()),
            ..defaulted_logging()
        };
        let config = LoggingConfigurationTranslator::translate(&logging).expect("translate logging config");

        assert_eq!(
            config.log_file,
            PlatformSettings::get_default_log_file_path().to_string_lossy()
        );
    }

    #[test]
    fn disabled_file_logging_overrides_a_configured_log_file() {
        let logging = Logging {
            disable_file_logging: true,
            file: ConfigValue::explicit("/tmp/adp.log".to_string()),
            to_syslog: true,
            ..defaulted_logging()
        };
        let config = LoggingConfigurationTranslator::translate(&logging).expect("translate logging config");

        assert!(config.log_file.is_empty());
        assert!(config.log_to_syslog);
    }

    #[test]
    fn defaulted_max_size_keeps_the_binary_rotation_default() {
        let config = LoggingConfigurationTranslator::translate(&defaulted_logging()).expect("translate logging config");

        assert_eq!(config.log_file_max_size, ByteSize::mib(10));
        assert_eq!(config.log_file_max_rolls, 1);
    }

    #[test]
    fn explicit_max_size_and_rolls_are_used() {
        let logging = Logging {
            file_max_size: ConfigValue::explicit(1_048_576),
            file_max_rolls: 5,
            ..defaulted_logging()
        };
        let config = LoggingConfigurationTranslator::translate(&logging).expect("translate logging config");

        assert_eq!(config.log_file_max_size, ByteSize::b(1_048_576));
        assert_eq!(config.log_file_max_rolls, 5);
    }
}
