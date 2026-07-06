//! Translation between ADP's typed logging model and ADP's [`LoggingConfiguration`].
//!
//! ADP's logging behavior must follow the Datadog Agent's logging configuration for the settings that are sensibly
//! shared (level, format, console output, rotation), but it must use its own per-subagent destination so it doesn't
//! collide with the Core Agent's own log file. This module owns those rules in one place.

use agent_data_plane_config::{Live, Logging};
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
    "resource_accounting",
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
/// `LoggingConfigurationTranslator` takes the typed logging model and generates a Saluki-oriented
/// [`LoggingConfiguration`] from it, applying the ADP-specific rules. This ensures that we obey all the logging
/// configuration rules set by the Datadog Agent but log to the right location for the ADP process.
pub struct LoggingConfigurationTranslator;

impl LoggingConfigurationTranslator {
    /// Builds a [`LoggingConfiguration`] from the typed logging model, applying ADP's per-subagent rules.
    ///
    /// # Errors
    ///
    /// Returns an error if the configured log level can't be parsed into valid filter directives.
    pub fn translate(logging: &Logging) -> Result<LoggingConfiguration, GenericError> {
        let log_level = parse_adp_log_level(&logging.level)?;

        // Syslog settings only take effect when syslog output is enabled; otherwise they stay at their disabled
        // defaults regardless of what the model carries. An empty URI resolves to the platform's default local syslog.
        let (syslog_uri, syslog_rfc) = if logging.to_syslog {
            let uri = if logging.syslog_uri.is_empty() {
                PlatformSettings::get_default_syslog_uri().to_string()
            } else {
                logging.syslog_uri.clone()
            };
            (uri, logging.syslog_rfc)
        } else {
            (String::new(), false)
        };

        // File destination: the per-subagent path, falling back to the platform default when unset (or blank). The
        // Core Agent's own `log_file` is deliberately never consulted. `disable_file_logging` short-circuits file
        // output.
        let log_file = if logging.disable_file_logging {
            String::new()
        } else {
            logging.file.clone().filter(|path| !path.is_empty()).unwrap_or_else(|| {
                PlatformSettings::get_default_log_file_path()
                    .to_string_lossy()
                    .into_owned()
            })
        };

        Ok(LoggingConfiguration {
            log_level,
            log_format_json: logging.format_json,
            log_format_rfc3339: logging.format_rfc3339,
            log_to_console: logging.to_console,
            log_to_syslog: logging.to_syslog,
            syslog_uri,
            syslog_rfc,
            log_file,
            log_file_max_size: ByteSize::b(logging.file_max_size),
            log_file_max_rolls: logging.file_max_rolls,
        })
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

/// A worker that watches for updates to the log level and adjusts the logging stack's current filter directives to
/// match.
///
/// The worker relies on dynamic configuration; if the view is fixed (local mode, tests), it simply idles until
/// shutdown.
pub struct DynamicLogLevelWorker {
    log_level: Live<String>,
    controller: LoggingOverrideController,
}

impl DynamicLogLevelWorker {
    /// Creates a new `DynamicLogLevelWorker` watching the given live log level.
    pub fn new(log_level: Live<String>, controller: LoggingOverrideController) -> Self {
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
                        match parse_adp_log_level(&log_level.current()) {
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
    use std::sync::Arc;

    use agent_data_plane_config::SalukiConfiguration;
    use arc_swap::ArcSwap;
    use tokio::sync::watch;

    use super::*;

    /// A logging model mirroring the schema-driven defaults `drive` produces for an empty configuration.
    fn default_logging() -> Logging {
        Logging {
            level: "info".to_string(),
            format_rfc3339: false,
            format_json: false,
            to_console: true,
            to_syslog: false,
            syslog_rfc: false,
            syslog_uri: String::new(),
            file: None,
            disable_file_logging: false,
            file_max_rolls: 1,
            file_max_size: 10_000_000,
        }
    }

    fn translate(logging: &Logging) -> LoggingConfiguration {
        LoggingConfigurationTranslator::translate(logging).expect("translate logging config")
    }

    fn filter(logging: &Logging) -> String {
        translate(logging).log_level.as_env_filter().to_string()
    }

    fn filter_directives(logging: &Logging) -> Vec<String> {
        filter(logging).split(',').map(str::to_string).collect()
    }

    #[test]
    fn default_log_level_becomes_first_party_info() {
        assert!(filter(&default_logging()).contains("agent_data_plane=info"));
    }

    #[test]
    fn plain_log_level_becomes_first_party_filter() {
        let directives = filter_directives(&Logging {
            level: "warn".to_string(),
            ..default_logging()
        });

        assert!(directives.contains(&"agent_data_plane=warn".to_string()));
        assert!(directives.contains(&"saluki_components=warn".to_string()));
    }

    #[test]
    fn plain_log_level_does_not_enable_dependency_targets() {
        let directives = filter_directives(&Logging {
            level: "warn".to_string(),
            ..default_logging()
        });

        assert!(!directives.contains(&"hyper=warn".to_string()));
        assert!(!directives.contains(&"tokio=warn".to_string()));
        assert!(!directives.contains(&"tonic=warn".to_string()));
    }

    #[test]
    fn plain_log_level_does_not_include_global_directive() {
        let directives = filter_directives(&Logging {
            level: "warn".to_string(),
            ..default_logging()
        });

        assert!(!directives.contains(&"warn".to_string()));
    }

    #[test]
    fn plain_log_level_is_case_insensitive() {
        let filter = filter(&Logging {
            level: "WaRn".to_string(),
            ..default_logging()
        });

        assert!(filter.contains("agent_data_plane=warn"));
    }

    #[test]
    fn advanced_log_level_directives_are_preserved() {
        let directives = filter_directives(&Logging {
            level: "warn,agent_data_plane=debug,hyper=warn".to_string(),
            ..default_logging()
        });

        assert!(directives.contains(&"warn".to_string()));
        assert!(directives.contains(&"agent_data_plane=debug".to_string()));
        assert!(directives.contains(&"hyper=warn".to_string()));
    }

    #[test]
    fn invalid_log_level_returns_error() {
        let result = LoggingConfigurationTranslator::translate(&Logging {
            level: "agent_data_plane=verbose".to_string(),
            ..default_logging()
        });

        assert!(result.is_err());
    }

    #[test]
    fn missing_syslog_values_default_to_disabled() {
        let logging = translate(&default_logging());

        assert!(!logging.log_to_syslog);
        assert!(logging.syslog_uri.is_empty());
        assert!(!logging.syslog_rfc);
    }

    #[test]
    fn syslog_values_are_translated() {
        let logging = translate(&Logging {
            to_syslog: true,
            syslog_uri: "udp://127.0.0.1:1514".to_string(),
            syslog_rfc: true,
            ..default_logging()
        });

        assert!(logging.log_to_syslog);
        assert_eq!(logging.syslog_uri, "udp://127.0.0.1:1514");
        assert!(logging.syslog_rfc);
    }

    #[test]
    fn enabled_syslog_with_empty_uri_uses_platform_default() {
        let logging = translate(&Logging {
            to_syslog: true,
            syslog_uri: String::new(),
            ..default_logging()
        });

        assert!(logging.log_to_syslog);
        assert_eq!(logging.syslog_uri, PlatformSettings::get_default_syslog_uri());
    }

    #[test]
    fn configured_syslog_uri_has_no_effect_when_syslog_is_disabled() {
        let logging = translate(&Logging {
            to_syslog: false,
            syslog_uri: "udp://127.0.0.1:1514".to_string(),
            ..default_logging()
        });

        assert!(!logging.log_to_syslog);
        assert!(logging.syslog_uri.is_empty());
    }

    #[test]
    fn syslog_rfc_has_no_effect_when_syslog_is_disabled() {
        let logging = translate(&Logging {
            to_syslog: false,
            syslog_rfc: true,
            ..default_logging()
        });

        assert!(!logging.log_to_syslog);
        assert!(!logging.syslog_rfc);
    }

    #[test]
    fn syslog_rfc_defaults_to_false_when_syslog_is_enabled() {
        let logging = translate(&Logging {
            to_syslog: true,
            syslog_uri: "udp://127.0.0.1:1514".to_string(),
            syslog_rfc: false,
            ..default_logging()
        });

        assert!(logging.log_to_syslog);
        assert!(!logging.syslog_rfc);
    }

    #[test]
    fn configured_log_file_is_used() {
        let logging = translate(&Logging {
            file: Some("/tmp/adp.log".to_string()),
            ..default_logging()
        });

        assert_eq!(logging.log_file, "/tmp/adp.log");
    }

    #[test]
    fn disable_file_logging_produces_empty_log_file() {
        let logging = translate(&Logging {
            disable_file_logging: true,
            file: Some("/tmp/adp.log".to_string()),
            ..default_logging()
        });

        assert!(logging.log_file.is_empty());
    }

    #[test]
    fn unset_log_file_falls_back_to_platform_default() {
        let logging = translate(&Logging {
            file: None,
            ..default_logging()
        });

        assert_eq!(
            logging.log_file,
            PlatformSettings::get_default_log_file_path().to_string_lossy()
        );
    }

    #[test]
    fn blank_log_file_falls_back_to_platform_default() {
        // A configured-but-blank path is treated as unset, matching the pre-migration behavior.
        let logging = translate(&Logging {
            file: Some(String::new()),
            ..default_logging()
        });

        assert_eq!(
            logging.log_file,
            PlatformSettings::get_default_log_file_path().to_string_lossy()
        );
    }

    #[test]
    fn log_file_max_size_is_converted_from_bytes() {
        let logging = translate(&Logging {
            file_max_size: 64_000,
            ..default_logging()
        });

        assert_eq!(logging.log_file_max_size, ByteSize::b(64_000));
    }

    /// A driven [`Live`] view over the log level, mirroring how the config system drives runtime updates.
    fn drivable_log_level(level: &str) -> (Arc<ArcSwap<SalukiConfiguration>>, watch::Sender<()>, Live<String>) {
        let mut config = SalukiConfiguration::default();
        config.control.logging.level = level.to_string();

        let cell = Arc::new(ArcSwap::from_pointee(config));
        let (tx, rx) = watch::channel(());
        let live = Live::dynamic(Arc::clone(&cell), rx, |c| &c.control.logging.level);
        (cell, tx, live)
    }

    fn set_log_level(cell: &ArcSwap<SalukiConfiguration>, tx: &watch::Sender<()>, level: &str) {
        let mut config = SalukiConfiguration::default();
        config.control.logging.level = level.to_string();
        cell.store(Arc::new(config));
        tx.send(()).expect("live cell should have a receiver");
    }

    /// Flipping the level through the driven view wakes the worker's watch and yields the new first-party filter that
    /// the worker feeds to the logging controller.
    #[tokio::test]
    async fn worker_reacts_to_live_log_level_change() {
        let (cell, tx, mut live) = drivable_log_level("info");
        assert!(parse_adp_log_level(&live.current())
            .expect("parse initial level")
            .as_env_filter()
            .to_string()
            .contains("agent_data_plane=info"));

        set_log_level(&cell, &tx, "debug");
        tokio::time::timeout(std::time::Duration::from_secs(2), live.changed())
            .await
            .expect("live view should observe the level update");

        let filter = parse_adp_log_level(&live.current())
            .expect("parse updated level")
            .as_env_filter()
            .to_string();
        assert!(filter.contains("agent_data_plane=debug"));
    }
}
