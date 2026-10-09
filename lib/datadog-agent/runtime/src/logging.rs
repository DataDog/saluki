//! Translation between a subagent's logging settings and the logging stack's own configuration.
//!
//! Subagent logging must follow the Datadog Agent's logging configuration for the settings that are sensibly shared
//! (level, format, console output, rotation), but each subagent must use its own destination so it doesn't collide with
//! the Core Agent's own log file. This module owns those rules in one place.

use std::{future::Future, path::PathBuf};

use async_trait::async_trait;
use bytesize::ByteSize;
use datadog_agent_commons::platform::PlatformSettings;
use saluki_app::logging::{LogLevel, LoggingConfiguration, LoggingOverrideController};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{InitializationError, Supervisable, SupervisorFuture};
use saluki_error::{ErrorContext as _, GenericError};
use tokio::{pin, select};
use tracing::{debug, warn};

/// Log targets that a plain log level applies to, regardless of which subagent is running.
///
/// These cover the Saluki and Datadog crates that subagents are built from. `tracing` targets use Rust crate and module
/// names, so Cargo package names with hyphens appear with underscores. A subagent adds its own crates with
/// [`LoggingTranslator::with_first_party_targets`].
pub const FIRST_PARTY_LOG_TARGETS: &[&str] = &[
    "containerd_protos",
    "datadog_protos",
    "datadog_agent_commons",
    "datadog_agent_runtime",
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
    "saluki_core",
    "saluki_env",
    "saluki_error",
    "saluki_io",
    "saluki_metadata",
    "saluki_metrics",
    "saluki_tls",
    "stringtheory",
];

/// Logging settings shared with the Datadog Agent.
///
/// These mirror the Datadog Agent's own logging settings. A subagent fills them in from its configuration system and
/// hands them to a [`LoggingTranslator`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct LoggingSettings {
    /// Log level: either a plain level name (`trace`, `debug`, `info`, `warn`, `error`, or `off`) or a comma-separated
    /// list of filter directives.
    ///
    /// A plain level name only applies to first-party log targets. Defaults to `info`.
    pub level: String,

    /// Whether to emit log records as JSON. Defaults to `false`.
    pub format_json: bool,

    /// Whether to use RFC 3339 timestamps in log output. Defaults to `false`.
    pub format_rfc3339: bool,

    /// Whether to write log records to standard output. Defaults to `true`.
    pub to_console: bool,

    /// Whether to write log records to syslog. Defaults to `false`.
    pub to_syslog: bool,

    /// Whether to use the Agent's RFC-style syslog header. Only applies when `to_syslog` is `true`. Defaults to
    /// `false`.
    pub syslog_rfc: bool,

    /// URI of the syslog destination. Only applies when `to_syslog` is `true`.
    ///
    /// Defaults to empty, which selects the platform's default local syslog URI.
    pub syslog_uri: String,

    /// Path to the log file, if one was configured explicitly.
    ///
    /// Defaults to `None`, which selects the subagent's default log file. An explicitly empty path also selects the
    /// default log file.
    pub file: Option<String>,

    /// Whether to disable logging to a file entirely. Takes precedence over `file`. Defaults to `false`.
    pub disable_file_logging: bool,

    /// Maximum number of rolled-over log files to retain. Defaults to `1`.
    pub file_max_rolls: usize,

    /// Maximum size, in bytes, of a log file before it's rolled over, if one was configured explicitly.
    ///
    /// Defaults to `None`, which keeps the logging stack's own default of 10 MiB.
    pub file_max_size: Option<u64>,
}

impl Default for LoggingSettings {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format_json: false,
            format_rfc3339: false,
            to_console: true,
            to_syslog: false,
            syslog_rfc: false,
            syslog_uri: String::new(),
            file: None,
            disable_file_logging: false,
            file_max_rolls: 1,
            file_max_size: None,
        }
    }
}

/// Logging configuration translator for matching the Datadog Agent's logging behavior.
///
/// In the Datadog Agent, all processes generally follow the same logging configuration, paying attention to the same
/// settings for determining log level, log format, and so on. They differ in some ways, such as determining what file
/// to write to when logging to file is enabled. Subagents follow the same pattern.
///
/// `LoggingTranslator` takes [`LoggingSettings`] and generates a Saluki-oriented [`LoggingConfiguration`] from it,
/// applying the subagent's own default log file and first-party log targets. This ensures that a subagent obeys all
/// the logging configuration rules set by the Datadog Agent but logs to the right location for its own process.
#[derive(Clone, Debug)]
pub struct LoggingTranslator {
    default_log_file: PathBuf,
    first_party_targets: Vec<&'static str>,
}

impl LoggingTranslator {
    /// Creates a new `LoggingTranslator` that logs to `default_log_file` unless a log file is configured explicitly.
    ///
    /// A plain log level applies to [`FIRST_PARTY_LOG_TARGETS`]. Add the subagent's own crates with
    /// [`with_first_party_targets`](Self::with_first_party_targets).
    pub fn new(default_log_file: impl Into<PathBuf>) -> Self {
        Self {
            default_log_file: default_log_file.into(),
            first_party_targets: FIRST_PARTY_LOG_TARGETS.to_vec(),
        }
    }

    /// Adds the given log targets to the ones that a plain log level applies to.
    pub fn with_first_party_targets(mut self, targets: &[&'static str]) -> Self {
        self.first_party_targets.extend_from_slice(targets);
        self
    }

    /// Builds a [`LoggingConfiguration`] from the given settings, applying this subagent's rules.
    ///
    /// # Errors
    ///
    /// Returns an error if the configured log level is not a level name or a valid set of filter directives.
    pub fn translate(&self, settings: &LoggingSettings) -> Result<LoggingConfiguration, GenericError> {
        let mut config = LoggingConfiguration::simple();

        config.log_level = self.parse_log_level(&settings.level)?;
        config.log_format_json = settings.format_json;
        config.log_format_rfc3339 = settings.format_rfc3339;
        config.log_to_console = settings.to_console;
        config.log_to_syslog = settings.to_syslog;

        if settings.to_syslog {
            config.syslog_rfc = settings.syslog_rfc;
            config.syslog_uri = if settings.syslog_uri.is_empty() {
                PlatformSettings::get_default_syslog_uri().to_string()
            } else {
                settings.syslog_uri.clone()
            };
        }

        // Preserve the logging stack's binary default when no maximum size was configured.
        if let Some(file_max_size) = settings.file_max_size {
            config.log_file_max_size = ByteSize::b(file_max_size);
        }
        config.log_file_max_rolls = settings.file_max_rolls;

        // Use the subagent's default unless its log file was set explicitly.
        config.log_file = if settings.disable_file_logging {
            String::new()
        } else {
            match &settings.file {
                Some(file) if !file.is_empty() => file.clone(),
                _ => self.default_log_file.to_string_lossy().into_owned(),
            }
        };

        Ok(config)
    }

    /// Parses a configured log level, expanding a plain level name into per-target directives.
    ///
    /// Plain levels apply to first-party log targets; other values are `target=level` filter directives.
    ///
    /// # Errors
    ///
    /// Returns an error if the value is not a level name or a valid set of filter directives.
    pub fn parse_log_level(&self, value: &str) -> Result<LogLevel, GenericError> {
        let trimmed = value.trim();
        if let Some(level) = plain_log_level(trimmed) {
            self.first_party_log_level_filter(level)
        } else {
            LogLevel::try_from(value.to_string()).error_context("Failed to parse log filter directives.")
        }
    }

    fn first_party_log_level_filter(&self, level: &str) -> Result<LogLevel, GenericError> {
        let filter = self
            .first_party_targets
            .iter()
            .map(|target| format!("{target}={level}"))
            .collect::<Vec<_>>()
            .join(",");

        LogLevel::try_from(filter).error_context("Failed to parse first-party log filter directives.")
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

/// A value that can change at runtime, such as a setting that the Datadog Agent updates.
///
/// This lets the runtime watch a value from a subagent's configuration system without depending on that system.
pub trait LiveValue: Clone + Send + Sync + 'static {
    /// The type of the watched value.
    type Value;

    /// Waits for the value to change and returns the new value.
    ///
    /// Changes may be coalesced: if the value changes several times before this is called, only the latest value is
    /// returned. Once the value can no longer change, the returned future **MUST** never resolve, so that callers can
    /// wait on it unconditionally.
    fn changed(&mut self) -> impl Future<Output = Self::Value> + Send;
}

/// A worker that watches the configured log level and adjusts the logging stack's current filter directives to match.
///
/// The worker relies on dynamic configuration; if the log level can't change, the worker simply idles until shutdown.
pub struct DynamicLogLevelWorker<L>
where
    L: LiveValue<Value = String>,
{
    level: L,
    translator: LoggingTranslator,
    controller: LoggingOverrideController,
}

impl<L> DynamicLogLevelWorker<L>
where
    L: LiveValue<Value = String>,
{
    /// Creates a new `DynamicLogLevelWorker` watching the given log level.
    ///
    /// Each new level is parsed with `translator`, so a plain level applies to the same first-party log targets as at
    /// startup.
    pub fn new(level: L, translator: LoggingTranslator, controller: LoggingOverrideController) -> Self {
        Self {
            level,
            translator,
            controller,
        }
    }
}

#[async_trait]
impl<L> Supervisable for DynamicLogLevelWorker<L>
where
    L: LiveValue<Value = String>,
{
    fn name(&self) -> &str {
        "dynamic-log-level"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let mut level = self.level.clone();
        let translator = self.translator.clone();
        let controller = self.controller.clone();

        Ok(Box::pin(async move {
            pin!(process_shutdown);

            debug!("Dynamic log level worker started.");

            loop {
                select! {
                    _ = &mut process_shutdown => break,
                    new_level = level.changed() => {
                        match translator.parse_log_level(&new_level) {
                            Ok(log_level) => {
                                if let Err(e) = controller.update_base(log_level.as_targets()).await {
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

    const DEFAULT_LOG_FILE: &str = "/var/log/datadog/example-subagent.log";

    fn translator() -> LoggingTranslator {
        LoggingTranslator::new(DEFAULT_LOG_FILE).with_first_party_targets(&["example_subagent"])
    }

    fn translate(settings: &LoggingSettings) -> LoggingConfiguration {
        translator().translate(settings).expect("translate logging config")
    }

    fn translate_level(level: &str) -> Result<Vec<String>, GenericError> {
        let settings = LoggingSettings {
            level: level.to_string(),
            ..LoggingSettings::default()
        };

        translator()
            .translate(&settings)
            .map(|config| config.log_level.as_targets().to_string())
            .map(|filter| filter.split(',').map(str::to_string).collect())
    }

    #[test]
    fn default_log_level_becomes_first_party_info() {
        let directives = translate_level("info").expect("translate logging config");

        assert!(directives.contains(&"example_subagent=info".to_string()));
    }

    #[test]
    fn plain_log_level_becomes_first_party_filter() {
        let directives = translate_level("warn").expect("translate logging config");

        assert!(directives.contains(&"example_subagent=warn".to_string()));
        assert!(directives.contains(&"saluki_components=warn".to_string()));

        assert!(!directives.contains(&"hyper=warn".to_string()));
        assert!(!directives.contains(&"tokio=warn".to_string()));
        assert!(!directives.contains(&"tonic=warn".to_string()));
        assert!(!directives.contains(&"warn".to_string()));
    }

    #[test]
    fn plain_log_level_covers_the_subagent_runtime() {
        // Registration, the configuration stream, and the status, flare, and telemetry services live in this crate, so
        // a plain level must reach its logs no matter which subagent is running.
        let directives = translate_level("debug").expect("translate logging config");

        assert!(directives.contains(&"datadog_agent_runtime=debug".to_string()));
    }

    #[test]
    fn plain_log_level_is_case_insensitive() {
        let directives = translate_level("WaRn").expect("translate logging config");

        assert!(directives.contains(&"example_subagent=warn".to_string()));
    }

    #[test]
    fn advanced_log_level_directives_are_preserved() {
        let directives = translate_level("warn,example_subagent=debug,hyper=warn").expect("translate logging config");

        assert!(directives.contains(&"warn".to_string()));
        assert!(directives.contains(&"example_subagent=debug".to_string()));
        assert!(directives.contains(&"hyper=warn".to_string()));
    }

    #[test]
    fn unparseable_log_level_returns_error() {
        for level in ["example_subagent=verbose", ""] {
            assert!(
                translate_level(level).is_err(),
                "log level `{level}` should be rejected"
            );
        }
    }

    #[test]
    fn format_and_console_settings_are_carried_through() {
        let config = translate(&LoggingSettings {
            format_json: true,
            format_rfc3339: true,
            to_console: false,
            ..LoggingSettings::default()
        });

        assert!(config.log_format_json);
        assert!(config.log_format_rfc3339);
        assert!(!config.log_to_console);
    }

    #[test]
    fn defaults_leave_syslog_disabled_with_no_destination() {
        let config = translate(&LoggingSettings::default());

        assert!(!config.log_to_syslog);
        assert!(config.syslog_uri.is_empty());
        assert!(!config.syslog_rfc);
    }

    #[test]
    fn enabled_syslog_uses_configured_uri_and_framing() {
        let config = translate(&LoggingSettings {
            to_syslog: true,
            syslog_uri: "udp://127.0.0.1:1514".to_string(),
            syslog_rfc: true,
            ..LoggingSettings::default()
        });

        assert!(config.log_to_syslog);
        assert_eq!(config.syslog_uri, "udp://127.0.0.1:1514");
        assert!(config.syslog_rfc);
    }

    #[test]
    fn enabled_syslog_with_empty_uri_uses_platform_default() {
        let config = translate(&LoggingSettings {
            to_syslog: true,
            ..LoggingSettings::default()
        });

        assert!(config.log_to_syslog);
        assert_eq!(config.syslog_uri, PlatformSettings::get_default_syslog_uri());
        assert!(!config.syslog_rfc);
    }

    #[test]
    fn syslog_settings_have_no_effect_when_syslog_is_disabled() {
        let config = translate(&LoggingSettings {
            to_syslog: false,
            syslog_uri: "udp://127.0.0.1:1514".to_string(),
            syslog_rfc: true,
            ..LoggingSettings::default()
        });

        assert!(!config.log_to_syslog);
        assert!(config.syslog_uri.is_empty());
        assert!(!config.syslog_rfc);
    }

    #[test]
    fn unset_log_file_uses_the_default_path() {
        let config = translate(&LoggingSettings::default());

        assert_eq!(config.log_file, DEFAULT_LOG_FILE);
    }

    #[test]
    fn explicitly_configured_log_file_is_used() {
        let config = translate(&LoggingSettings {
            file: Some("/tmp/subagent.log".to_string()),
            ..LoggingSettings::default()
        });

        assert_eq!(config.log_file, "/tmp/subagent.log");
    }

    #[test]
    fn explicitly_empty_log_file_uses_the_default_path() {
        let config = translate(&LoggingSettings {
            file: Some(String::new()),
            ..LoggingSettings::default()
        });

        assert_eq!(config.log_file, DEFAULT_LOG_FILE);
    }

    #[test]
    fn disabled_file_logging_overrides_a_configured_log_file() {
        let config = translate(&LoggingSettings {
            disable_file_logging: true,
            file: Some("/tmp/subagent.log".to_string()),
            to_syslog: true,
            ..LoggingSettings::default()
        });

        assert!(config.log_file.is_empty());
        assert!(config.log_to_syslog);
    }

    #[test]
    fn unset_max_size_keeps_the_binary_rotation_default() {
        let config = translate(&LoggingSettings::default());

        assert_eq!(config.log_file_max_size, ByteSize::mib(10));
        assert_eq!(config.log_file_max_rolls, 1);
    }

    #[test]
    fn explicit_max_size_and_rolls_are_used() {
        let config = translate(&LoggingSettings {
            file_max_size: Some(1_048_576),
            file_max_rolls: 5,
            ..LoggingSettings::default()
        });

        assert_eq!(config.log_file_max_size, ByteSize::b(1_048_576));
        assert_eq!(config.log_file_max_rolls, 5);
    }
}
