//! Translation between the Datadog Agent's typed logging bootstrap slice and ADP's [`LoggingConfiguration`].
//!
//! ADP's logging behavior must follow the Datadog Agent's logging configuration for the settings that are sensibly
//! shared (level, format, console output, rotation), but it must use its own per-subagent destination so it doesn't
//! collide with the Core Agent's own log file. This module owns those rules in one place.

use std::str::FromStr as _;

use agent_data_plane_config::LoggingBootstrap;
use async_trait::async_trait;
use bytesize::ByteSize;
use datadog_agent_commons::platform::PlatformSettings;
use saluki_app::logging::{LogLevel, LoggingConfiguration, LoggingOverrideController};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_component_config::ScopedConfig;
use saluki_core::runtime::{InitializationError, Supervisable, SupervisorFuture};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{pin, select};
use tracing::{debug, warn};

// `tracing` targets use Rust crate/module names, so Cargo package names with hyphens appear with underscores.
const FIRST_PARTY_LOG_TARGETS: &[&str] = &[
    "agent_data_plane",
    "agent_data_plane_config",
    "agent_data_plane_config_system",
    "containerd_protos",
    "datadog_protos",
    "datadog_agent_commons",
    "datadog_agent_config",
    "ddsketch",
    "resource_accounting",
    "otlp_protos",
    "ottl",
    "process_memory",
    "prometheus_exposition",
    "saluki_api",
    "saluki_app",
    "saluki_common",
    "saluki_component_config",
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
    /// Builds a [`LoggingConfiguration`] from the typed logging bootstrap slice, applying ADP's
    /// per-subagent rules.
    ///
    /// # Errors
    ///
    /// Returns an error if the configured log level cannot be parsed, or the configured
    /// `log_file_max_size` cannot be parsed as a byte size.
    pub fn translate(logging_bootstrap: &LoggingBootstrap) -> Result<LoggingConfiguration, GenericError> {
        let mut logging = LoggingConfiguration::simple();

        logging.log_level = parse_optional_log_level_raw(logging_bootstrap.log_level.clone())?;

        if let Some(format_json) = logging_bootstrap.log_format_json {
            logging.log_format_json = format_json;
        }

        if let Some(format_rfc3339) = logging_bootstrap.log_format_rfc3339 {
            logging.log_format_rfc3339 = format_rfc3339;
        }

        if let Some(to_console) = logging_bootstrap.log_to_console {
            logging.log_to_console = to_console;
        }

        if let Some(to_syslog) = logging_bootstrap.log_to_syslog {
            logging.log_to_syslog = to_syslog;
        }

        if logging.log_to_syslog {
            if let Some(syslog_rfc) = logging_bootstrap.syslog_rfc {
                logging.syslog_rfc = syslog_rfc;
            }

            logging.syslog_uri = match logging_bootstrap.syslog_uri.as_deref() {
                Some(uri) if !uri.is_empty() => uri.to_string(),
                _ => PlatformSettings::get_default_syslog_uri().to_string(),
            };
        }

        if let Some(max_size) = logging_bootstrap.log_file_max_size.as_deref() {
            logging.log_file_max_size = ByteSize::from_str(max_size)
                .map_err(|e| generic_error!("Failed to parse `log_file_max_size`: {}", e))?;
        }

        if let Some(max_rolls) = logging_bootstrap.log_file_max_rolls {
            logging.log_file_max_rolls = max_rolls;
        }

        // File destination: per-subagent key only, with the platform default as fallback. The Core Agent's `log_file`
        // is deliberately never consulted. `disable_file_logging` short-circuits the file output entirely.
        let disable_file_logging = logging_bootstrap.disable_file_logging.unwrap_or(false);
        logging.log_file = if disable_file_logging {
            String::new()
        } else {
            match logging_bootstrap.data_plane_log_file.as_deref() {
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

/// A worker that reacts to typed `log_level` updates and adjusts the logging stack's current filter
/// directives to match.
///
/// The worker consumes the typed [`ScopedConfig<String>`] log-level handle from the config-system's
/// dynamic handle bundle. When the handle is fixed (local-snapshot mode), it simply idles until
/// shutdown.
pub struct DynamicLogLevelWorker {
    log_level: ScopedConfig<String>,
    controller: LoggingOverrideController,
}

impl DynamicLogLevelWorker {
    /// Creates a new `DynamicLogLevelWorker` from the typed log-level handle.
    pub fn new(log_level: ScopedConfig<String>, controller: LoggingOverrideController) -> Self {
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
                        let new_log_level = log_level.current();
                        match parse_optional_log_level_raw(Some(new_log_level)) {
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
    use super::*;

    /// Builds a `LoggingBootstrap` with all fields defaulted; tests set only what they exercise.
    fn bootstrap() -> LoggingBootstrap {
        LoggingBootstrap::default()
    }

    fn translate(b: &LoggingBootstrap) -> Result<LoggingConfiguration, GenericError> {
        LoggingConfigurationTranslator::translate(b)
    }

    fn filter_directives(b: &LoggingBootstrap) -> Vec<String> {
        let logging = translate(b).expect("translate logging config");
        logging
            .log_level
            .as_env_filter()
            .to_string()
            .split(',')
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn missing_log_level_defaults_to_first_party_info() {
        let directives = filter_directives(&bootstrap());
        assert!(directives.contains(&"agent_data_plane=info".to_string()));
    }

    #[test]
    fn plain_log_level_becomes_first_party_filter() {
        let mut b = bootstrap();
        b.log_level = Some("warn".to_string());
        let directives = filter_directives(&b);
        assert!(directives.contains(&"agent_data_plane=warn".to_string()));
        assert!(directives.contains(&"saluki_components=warn".to_string()));
    }

    #[test]
    fn plain_log_level_does_not_enable_dependency_targets() {
        let mut b = bootstrap();
        b.log_level = Some("warn".to_string());
        let directives = filter_directives(&b);
        assert!(!directives.contains(&"hyper=warn".to_string()));
        assert!(!directives.contains(&"tokio=warn".to_string()));
        assert!(!directives.contains(&"tonic=warn".to_string()));
    }

    #[test]
    fn plain_log_level_does_not_include_global_directive() {
        let mut b = bootstrap();
        b.log_level = Some("warn".to_string());
        let directives = filter_directives(&b);
        assert!(!directives.contains(&"warn".to_string()));
    }

    #[test]
    fn plain_log_level_is_case_insensitive() {
        let mut b = bootstrap();
        b.log_level = Some("WaRn".to_string());
        let directives = filter_directives(&b);
        assert!(directives.contains(&"agent_data_plane=warn".to_string()));
    }

    #[test]
    fn advanced_log_level_directives_are_preserved() {
        let mut b = bootstrap();
        b.log_level = Some("warn,agent_data_plane=debug,hyper=warn".to_string());
        let directives = filter_directives(&b);
        assert!(directives.contains(&"warn".to_string()));
        assert!(directives.contains(&"agent_data_plane=debug".to_string()));
        assert!(directives.contains(&"hyper=warn".to_string()));
    }

    #[test]
    fn invalid_log_level_returns_error() {
        let mut b = bootstrap();
        b.log_level = Some("agent_data_plane=verbose".to_string());
        assert!(translate(&b).is_err());
    }

    #[test]
    fn missing_syslog_values_default_to_disabled() {
        let logging = translate(&bootstrap()).expect("translate logging config");
        assert!(!logging.log_to_syslog);
        assert!(logging.syslog_uri.is_empty());
        assert!(!logging.syslog_rfc);
    }

    #[test]
    fn syslog_values_are_translated() {
        let mut b = bootstrap();
        b.log_to_syslog = Some(true);
        b.syslog_uri = Some("udp://127.0.0.1:1514".to_string());
        b.syslog_rfc = Some(true);
        let logging = translate(&b).expect("translate logging config");
        assert!(logging.log_to_syslog);
        assert_eq!(logging.syslog_uri, "udp://127.0.0.1:1514");
        assert!(logging.syslog_rfc);
    }

    #[test]
    fn enabled_syslog_with_missing_uri_uses_platform_default() {
        let mut b = bootstrap();
        b.log_to_syslog = Some(true);
        let logging = translate(&b).expect("translate logging config");
        assert!(logging.log_to_syslog);
        assert_eq!(logging.syslog_uri, PlatformSettings::get_default_syslog_uri());
    }

    #[test]
    fn enabled_syslog_with_empty_uri_uses_platform_default() {
        let mut b = bootstrap();
        b.log_to_syslog = Some(true);
        b.syslog_uri = Some(String::new());
        let logging = translate(&b).expect("translate logging config");
        assert!(logging.log_to_syslog);
        assert_eq!(logging.syslog_uri, PlatformSettings::get_default_syslog_uri());
    }

    #[test]
    fn configured_syslog_uri_has_no_effect_when_syslog_is_disabled() {
        let mut b = bootstrap();
        b.log_to_syslog = Some(false);
        b.syslog_uri = Some("udp://127.0.0.1:1514".to_string());
        let logging = translate(&b).expect("translate logging config");
        assert!(!logging.log_to_syslog);
        assert!(logging.syslog_uri.is_empty());
    }

    #[test]
    fn syslog_rfc_has_no_effect_when_syslog_is_disabled() {
        let mut b = bootstrap();
        b.log_to_syslog = Some(false);
        b.syslog_rfc = Some(true);
        let logging = translate(&b).expect("translate logging config");
        assert!(!logging.log_to_syslog);
        assert!(!logging.syslog_rfc);
    }

    #[test]
    fn syslog_rfc_defaults_to_false_when_syslog_is_enabled() {
        let mut b = bootstrap();
        b.log_to_syslog = Some(true);
        b.syslog_uri = Some("udp://127.0.0.1:1514".to_string());
        let logging = translate(&b).expect("translate logging config");
        assert!(logging.log_to_syslog);
        assert!(!logging.syslog_rfc);
    }

    #[test]
    fn data_plane_log_file_behavior_remains_unchanged_with_syslog_config() {
        let mut b = bootstrap();
        b.data_plane_log_file = Some("/tmp/adp.log".to_string());
        b.log_to_syslog = Some(true);
        let logging = translate(&b).expect("translate logging config");
        assert_eq!(logging.log_file, "/tmp/adp.log");
        assert!(logging.log_to_syslog);
    }

    #[test]
    fn disable_file_logging_behavior_remains_unchanged_with_syslog_config() {
        let mut b = bootstrap();
        b.disable_file_logging = Some(true);
        b.data_plane_log_file = Some("/tmp/adp.log".to_string());
        b.log_to_syslog = Some(true);
        let logging = translate(&b).expect("translate logging config");
        assert!(logging.log_file.is_empty());
        assert!(logging.log_to_syslog);
    }
}
