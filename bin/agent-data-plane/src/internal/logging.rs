//! ADP's logging rules on top of the shared remote agent logging support.
//!
//! ADP logs to its own default log file and treats its own crates as first-party log targets. Everything else about
//! translating the Datadog Agent's logging settings lives in [`datadog_agent_runtime::logging`].

use std::sync::OnceLock;

use agent_data_plane_config::Logging;
use agent_data_plane_config_system::ConfigurationSystem;
use datadog_agent_commons::platform::PlatformSettings;
use datadog_agent_runtime::logging::{LoggingSettings, LoggingTranslator};
use futures::Stream;
use saluki_app::logging::{LogLevel, LoggingConfiguration};
use saluki_error::GenericError;

static LOGGING_TRANSLATOR: OnceLock<LoggingTranslator> = OnceLock::new();

/// Creates the logging translator for ADP.
///
/// It logs to ADP's platform-specific default log file unless one is configured explicitly, and applies a plain log
/// level to ADP's own crates on top of the shared first-party log targets. The `agent_data_plane` target is a prefix,
/// so it also covers `agent_data_plane_config` and `agent_data_plane_config_system`.
pub fn get_logging_translator() -> &'static LoggingTranslator {
    LOGGING_TRANSLATOR.get_or_init(|| {
        LoggingTranslator::new(PlatformSettings::get_default_log_file_path())
            .with_first_party_targets(&["agent_data_plane"])
    })
}

/// Gets the Datadog Agent-specific logging settings from the given Agent Data Plane-specific logging configuration.
///
/// This gets all of the settings and remote agent-specific defaults lined up and in place: the log file and its
/// maximum size are only carried over when they were set explicitly, so that defaulted values select ADP's default log
/// file and the logging stack's default rotation size.
pub fn get_logging_settings(adp_log_config: &Logging) -> LoggingSettings {
    let mut agent_log_settings = LoggingSettings::default();
    agent_log_settings.level = adp_log_config.level.clone();
    agent_log_settings.format_json = adp_log_config.format_json;
    agent_log_settings.format_rfc3339 = adp_log_config.format_rfc3339;
    agent_log_settings.to_console = adp_log_config.to_console;
    agent_log_settings.to_syslog = adp_log_config.to_syslog;
    agent_log_settings.syslog_rfc = adp_log_config.syslog_rfc;
    agent_log_settings.syslog_uri = adp_log_config.syslog_uri.clone();
    agent_log_settings.file = adp_log_config
        .file
        .is_explicit()
        .then(|| adp_log_config.file.value.clone());
    agent_log_settings.disable_file_logging = adp_log_config.disable_file_logging;
    agent_log_settings.file_max_rolls = adp_log_config.file_max_rolls;
    agent_log_settings.file_max_size = adp_log_config
        .file_max_size
        .is_explicit()
        .then_some(adp_log_config.file_max_size.value);
    agent_log_settings
}

/// Gets a translated logging configuration from the given Agent Data Plane-specific logging configuration.
///
/// This converts the Agent Data Plane-specific logging configuration (which itself is a layered version of the Datadog
/// Agent's logging configuration) into the Saluki-specific logging configuration necessary for bootstrapping the
/// application.
///
/// # Errors
///
/// If any of the logging configuration is invalid, or cannot be parsed, an error is returned.
pub fn get_translated_logging_config(adp_log_config: &Logging) -> Result<LoggingConfiguration, GenericError> {
    // We need to first go from the Agent Data Plane-specific logging configuration shape to the Datadog Agent-specific
    // logging configuration, and then translate that into the underlying logging configuration used by Saluki.
    get_logging_translator().translate(get_logging_settings(adp_log_config))
}

/// Gets a translated stream of log level updates from the given configuration system.
///
/// This allows feeding dynamic updates to the log level in the configuration system to the logging system to be
/// applied.
pub fn get_translated_log_level_stream(
    config_sys: &ConfigurationSystem,
) -> impl Stream<Item = LogLevel> + Send + 'static {
    let log_levels = config_sys.live(|config| &config.control.logging.level).into_changes();
    get_logging_translator().translate_log_levels(log_levels)
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::ConfigValue;
    use agent_data_plane_config_system::{EnvPrecedence, LoadedConfiguration};
    use bytesize::ByteSize;
    use tracing::Level;

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
            file_max_size: ConfigValue::defaulted(10_485_760),
        }
    }

    #[test]
    fn plain_log_level_covers_adp_and_the_remote_agent_runtime() {
        let logging = Logging {
            level: "debug".to_string(),
            ..defaulted_logging()
        };
        let targets = get_translated_logging_config(&logging)
            .expect("translate logging config")
            .log_level
            .as_targets();

        for target in [
            "agent_data_plane",
            "agent_data_plane_config",
            "agent_data_plane_config_system",
            "datadog_agent_runtime",
            "saluki_app",
            "saluki_components",
        ] {
            assert!(
                targets.would_enable(target, &Level::DEBUG),
                "target `{target}` should be enabled"
            );
        }
    }

    #[test]
    fn semantic_registry_logs_use_the_first_party_level() {
        // Moving the decoder out of saluki-components must not hide its warnings or debug selection logs.
        let logging = defaulted_logging();
        let config = get_translated_logging_config(&logging).expect("translate default logging config");
        let target = "datadog_agent_apm::semantics::remote_config";
        let filter = config.log_level.as_targets();
        assert!(filter.would_enable(target, &tracing::Level::WARN));
        assert!(!filter.would_enable(target, &tracing::Level::DEBUG));

        let logging = Logging {
            level: "debug".to_string(),
            ..logging
        };
        let config = get_translated_logging_config(&logging).expect("translate debug logging config");
        let filter = config.log_level.as_targets();
        assert!(filter.would_enable(target, &tracing::Level::WARN));
        assert!(filter.would_enable(target, &tracing::Level::DEBUG));
    }

    #[test]
    fn defaulted_log_file_uses_the_platform_default_path() {
        let config = get_translated_logging_config(&defaulted_logging()).expect("translate logging config");

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
        let config = get_translated_logging_config(&logging).expect("translate logging config");

        assert_eq!(config.log_file, "/tmp/adp.log");
    }

    #[test]
    fn defaulted_max_size_keeps_the_binary_rotation_default() {
        let config = get_translated_logging_config(&defaulted_logging()).expect("translate logging config");

        assert_eq!(config.log_file_max_size, ByteSize::mib(10));
    }

    #[test]
    fn defaulted_max_size_is_not_passed_to_the_logging_stack() {
        // The value is ignored while it is a default, so a differing default would still leave the
        // binary's own threshold in place.
        let logging = Logging {
            file_max_size: ConfigValue::defaulted(1),
            ..defaulted_logging()
        };
        let config = get_translated_logging_config(&logging).expect("translate logging config");

        assert_eq!(config.log_file_max_size, ByteSize::mib(10));
    }

    #[test]
    fn explicit_max_size_is_used() {
        let logging = Logging {
            file_max_size: ConfigValue::explicit(1_048_576),
            ..defaulted_logging()
        };
        let config = get_translated_logging_config(&logging).expect("translate logging config");

        assert_eq!(config.log_file_max_size, ByteSize::b(1_048_576));
    }

    /// Loads `yaml` through the production local-configuration path and translates its logging settings.
    async fn logging_config_from_yaml(name: &str, yaml: &str) -> LoggingConfiguration {
        let path = std::env::temp_dir().join(format!("adp_logging_{name}_{}.yaml", std::process::id()));
        std::fs::write(&path, yaml).expect("write configuration file");
        let loaded = LoadedConfiguration::load(&path, EnvPrecedence::Disabled).await;
        std::fs::remove_file(&path).ok();

        let loaded = loaded.expect("local sources load");
        get_translated_logging_config(&loaded.local().control.logging).expect("translate logging config")
    }

    #[tokio::test]
    async fn configured_log_file_max_size_reaches_the_logging_stack_in_binary_units() {
        let config = logging_config_from_yaml("unset", "{}\n").await;
        assert_eq!(config.log_file_max_size, ByteSize::mib(10));

        // `10MB` selects the same rotation threshold in ADP and the Agent.
        let config = logging_config_from_yaml("mb", "log_file_max_size: 10MB\n").await;
        assert_eq!(config.log_file_max_size, ByteSize::mib(10));

        let config = logging_config_from_yaml("kb", "log_file_max_size: 5KB\n").await;
        assert_eq!(config.log_file_max_size, ByteSize::b(5_120));
    }
}
