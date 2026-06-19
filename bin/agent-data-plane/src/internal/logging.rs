//! Translation between the Datadog Agent's [`GenericConfiguration`] and ADP's [`LoggingConfiguration`].
//!
//! ADP's logging behavior must follow the Datadog Agent's logging configuration for the settings that are sensibly
//! shared (level, format, console output, rotation), but it must use its own per-subagent destination so it doesn't
//! collide with the Core Agent's own log file. This module owns those rules in one place.

use async_trait::async_trait;
use bytesize::ByteSize;
use datadog_agent_commons::platform::PlatformSettings;
use saluki_app::logging::{LogLevel, LoggingConfiguration, LoggingOverrideController};
use saluki_common::{deser::PermissiveBool, sync::shutdown::ShutdownHandle};
use saluki_config_tools::GenericConfiguration;
use saluki_core::runtime::{InitializationError, Supervisable, SupervisorFuture};
use saluki_error::{ErrorContext as _, GenericError};
use serde::Deserialize;
use serde_with::serde_as;
use tokio::{pin, select};
use tracing::{debug, warn};

const DATA_PLANE_LOG_FILE_KEY: &str = "data_plane.log_file";
// `tracing` targets use Rust crate/module names, so Cargo package names with hyphens appear with underscores.
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
    "saluki_config_tools",
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
    /// Builds a [`LoggingConfiguration`] from the given configuration, applying ADP's per-subagent rules.
    ///
    /// # Errors
    ///
    /// Returns an error if any of the consulted keys is present but can't be parsed into the expected type.
    pub fn translate(config: &GenericConfiguration) -> Result<LoggingConfiguration, GenericError> {
        let mut logging = LoggingConfiguration::simple();

        let maybe_log_level = config
            .try_get_typed::<String>("log_level")
            .error_context("Failed to read `log_level`.")?;
        logging.log_level = parse_optional_log_level_raw(maybe_log_level)?;

        if let Some(format_json) = read_permissive_bool(config, "log_format_json")? {
            logging.log_format_json = format_json;
        }

        if let Some(format_rfc3339) = read_permissive_bool(config, "log_format_rfc3339")? {
            logging.log_format_rfc3339 = format_rfc3339;
        }

        if let Some(to_console) = read_permissive_bool(config, "log_to_console")? {
            logging.log_to_console = to_console;
        }

        if let Some(to_syslog) = read_permissive_bool(config, "log_to_syslog")? {
            logging.log_to_syslog = to_syslog;
        }

        if logging.log_to_syslog {
            if let Some(syslog_rfc) = read_permissive_bool(config, "syslog_rfc")? {
                logging.syslog_rfc = syslog_rfc;
            }

            let configured = config
                .try_get_typed::<String>("syslog_uri")
                .error_context("Failed to read `syslog_uri`.")?;
            logging.syslog_uri = match configured {
                Some(uri) if !uri.is_empty() => uri,
                _ => PlatformSettings::get_default_syslog_uri().to_string(),
            };
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

/// A worker that watches for updates to `log_level` and adjusts the logging stack's current filter directives to match.
///
/// The worker relies on dynamic configuration; if it's not enabled, the worker simply idles until shutdown.
pub struct DynamicLogLevelWorker {
    config: GenericConfiguration,
    controller: LoggingOverrideController,
}

impl DynamicLogLevelWorker {
    /// Creates a new `DynamicLogLevelWorker` watching the given configuration.
    pub fn new(config: &GenericConfiguration, controller: LoggingOverrideController) -> Self {
        Self {
            config: config.clone(),
            controller,
        }
    }
}

#[async_trait]
impl Supervisable for DynamicLogLevelWorker {
    fn name(&self) -> &str {
        "dynamic-log-level"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let mut watcher = self.config.watch_for_updates("log_level");
        let controller = self.controller.clone();

        Ok(Box::pin(async move {
            pin!(process_shutdown);

            debug!("Dynamic log level worker started.");

            loop {
                select! {
                    _ = &mut process_shutdown => break,
                    (_, new_log_level) = watcher.changed::<String>() => {
                        match parse_optional_log_level_raw(new_log_level) {
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
    use saluki_config_tools::ConfigurationLoader;
    use serde_json::{json, Value};

    use super::*;

    async fn translate_logging(config_json: Option<Value>) -> Result<LoggingConfiguration, GenericError> {
        let (config, _) = ConfigurationLoader::for_tests(config_json, None, false).await;
        LoggingConfigurationTranslator::translate(&config)
    }

    async fn translate_logging_with_env(env_vars: &[(String, String)]) -> Result<LoggingConfiguration, GenericError> {
        let (config, _) = ConfigurationLoader::for_tests(None, Some(env_vars), false).await;
        LoggingConfigurationTranslator::translate(&config)
    }

    async fn translate_filter(config_json: Option<Value>) -> Result<String, GenericError> {
        translate_logging(config_json)
            .await
            .map(|logging| logging.log_level.as_env_filter().to_string())
    }

    async fn translate_filter_directives(config_json: Option<Value>) -> Result<Vec<String>, GenericError> {
        translate_filter(config_json)
            .await
            .map(|filter| filter.split(',').map(str::to_string).collect())
    }

    #[tokio::test]
    async fn missing_log_level_defaults_to_first_party_info() {
        let filter = translate_filter(None).await.expect("translate logging config");

        assert!(filter.contains("agent_data_plane=info"));
    }

    #[tokio::test]
    async fn plain_log_level_becomes_first_party_filter() {
        let directives = translate_filter_directives(Some(json!({ "log_level": "warn" })))
            .await
            .expect("translate logging config");

        assert!(directives.contains(&"agent_data_plane=warn".to_string()));
        assert!(directives.contains(&"saluki_components=warn".to_string()));
    }

    #[tokio::test]
    async fn plain_log_level_does_not_enable_dependency_targets() {
        let directives = translate_filter_directives(Some(json!({ "log_level": "warn" })))
            .await
            .expect("translate logging config");

        assert!(!directives.contains(&"hyper=warn".to_string()));
        assert!(!directives.contains(&"tokio=warn".to_string()));
        assert!(!directives.contains(&"tonic=warn".to_string()));
    }

    #[tokio::test]
    async fn plain_log_level_does_not_include_global_directive() {
        let directives = translate_filter_directives(Some(json!({ "log_level": "warn" })))
            .await
            .expect("translate logging config");

        assert!(!directives.contains(&"warn".to_string()));
    }

    #[tokio::test]
    async fn env_plain_log_level_becomes_first_party_filter() {
        let env_vars = [("LOG_LEVEL".to_string(), "warn".to_string())];
        let (config, _) = ConfigurationLoader::for_tests(None, Some(&env_vars), false).await;
        let filter = LoggingConfigurationTranslator::translate(&config)
            .expect("translate logging config")
            .log_level
            .as_env_filter()
            .to_string();

        assert!(filter.contains("agent_data_plane=warn"));
    }

    #[tokio::test]
    async fn plain_log_level_is_case_insensitive() {
        let filter = translate_filter(Some(json!({ "log_level": "WaRn" })))
            .await
            .expect("translate logging config");

        assert!(filter.contains("agent_data_plane=warn"));
    }

    #[tokio::test]
    async fn advanced_log_level_directives_are_preserved() {
        let directives = translate_filter_directives(Some(json!({
            "log_level": "warn,agent_data_plane=debug,hyper=warn"
        })))
        .await
        .expect("translate logging config");

        assert!(directives.contains(&"warn".to_string()));
        assert!(directives.contains(&"agent_data_plane=debug".to_string()));
        assert!(directives.contains(&"hyper=warn".to_string()));
    }

    #[tokio::test]
    async fn invalid_log_level_returns_error() {
        let result = translate_filter(Some(json!({ "log_level": "agent_data_plane=verbose" }))).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn missing_syslog_values_default_to_disabled() {
        let logging = translate_logging(None).await.expect("translate logging config");

        assert!(!logging.log_to_syslog);
        assert!(logging.syslog_uri.is_empty());
        assert!(!logging.syslog_rfc);
    }

    #[tokio::test]
    async fn yaml_syslog_values_are_translated() {
        let logging = translate_logging(Some(json!({
            "log_to_syslog": true,
            "syslog_uri": "udp://127.0.0.1:1514",
            "syslog_rfc": true,
        })))
        .await
        .expect("translate logging config");

        assert!(logging.log_to_syslog);
        assert_eq!(logging.syslog_uri, "udp://127.0.0.1:1514");
        assert!(logging.syslog_rfc);
    }

    #[tokio::test]
    async fn env_syslog_values_are_translated() {
        let env_vars = [
            ("LOG_TO_SYSLOG".to_string(), "true".to_string()),
            ("SYSLOG_URI".to_string(), "tcp://127.0.0.1:1514".to_string()),
            ("SYSLOG_RFC".to_string(), "1".to_string()),
        ];
        let logging = translate_logging_with_env(&env_vars)
            .await
            .expect("translate logging config");

        assert!(logging.log_to_syslog);
        assert_eq!(logging.syslog_uri, "tcp://127.0.0.1:1514");
        assert!(logging.syslog_rfc);
    }

    #[tokio::test]
    async fn enabled_syslog_with_missing_uri_uses_platform_default() {
        let logging = translate_logging(Some(json!({ "log_to_syslog": true })))
            .await
            .expect("translate logging config");

        assert!(logging.log_to_syslog);
        assert_eq!(logging.syslog_uri, PlatformSettings::get_default_syslog_uri());
    }

    #[tokio::test]
    async fn enabled_syslog_with_empty_uri_uses_platform_default() {
        let logging = translate_logging(Some(json!({
            "log_to_syslog": true,
            "syslog_uri": "",
        })))
        .await
        .expect("translate logging config");

        assert!(logging.log_to_syslog);
        assert_eq!(logging.syslog_uri, PlatformSettings::get_default_syslog_uri());
    }

    #[tokio::test]
    async fn configured_syslog_uri_has_no_effect_when_syslog_is_disabled() {
        let logging = translate_logging(Some(json!({
            "log_to_syslog": false,
            "syslog_uri": "udp://127.0.0.1:1514",
        })))
        .await
        .expect("translate logging config");

        assert!(!logging.log_to_syslog);
        assert!(logging.syslog_uri.is_empty());
    }

    #[tokio::test]
    async fn syslog_rfc_has_no_effect_when_syslog_is_disabled() {
        let logging = translate_logging(Some(json!({
            "log_to_syslog": false,
            "syslog_rfc": true,
        })))
        .await
        .expect("translate logging config");

        assert!(!logging.log_to_syslog);
        assert!(!logging.syslog_rfc);
    }

    #[tokio::test]
    async fn syslog_rfc_defaults_to_false_when_syslog_is_enabled() {
        let logging = translate_logging(Some(json!({
            "log_to_syslog": true,
            "syslog_uri": "udp://127.0.0.1:1514",
        })))
        .await
        .expect("translate logging config");

        assert!(logging.log_to_syslog);
        assert!(!logging.syslog_rfc);
    }

    #[tokio::test]
    async fn data_plane_log_file_behavior_remains_unchanged_with_syslog_config() {
        let logging = translate_logging(Some(json!({
            "data_plane": {
                "log_file": "/tmp/adp.log",
            },
            "log_to_syslog": true,
        })))
        .await
        .expect("translate logging config");

        assert_eq!(logging.log_file, "/tmp/adp.log");
        assert!(logging.log_to_syslog);
    }

    #[tokio::test]
    async fn disable_file_logging_behavior_remains_unchanged_with_syslog_config() {
        let logging = translate_logging(Some(json!({
            "disable_file_logging": true,
            "data_plane": {
                "log_file": "/tmp/adp.log",
            },
            "log_to_syslog": true,
        })))
        .await
        .expect("translate logging config");

        assert!(logging.log_file.is_empty());
        assert!(logging.log_to_syslog);
    }
}
