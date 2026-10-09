//! ADP's logging rules on top of the shared subagent logging support.
//!
//! ADP logs to its own default log file and treats its own crate as a first-party log target. Everything else about
//! translating the Datadog Agent's logging settings lives in [`datadog_agent_runtime::logging`].

use std::future::Future;

use agent_data_plane_config::{control::Logging, Live};
use datadog_agent_commons::platform::PlatformSettings;
use datadog_agent_runtime::logging::{LiveValue, LoggingSettings, LoggingTranslator};

/// Creates the logging translator for ADP.
///
/// It logs to ADP's platform-specific default log file unless one is configured explicitly, and applies a plain log
/// level to ADP's own crate on top of the shared first-party log targets.
pub fn adp_logging_translator() -> LoggingTranslator {
    LoggingTranslator::new(PlatformSettings::get_default_log_file_path())
        .with_first_party_targets(&["agent_data_plane"])
}

/// Maps ADP's typed logging configuration to the shared logging settings.
///
/// The log file and its maximum size are only carried over when they were set explicitly, so that defaulted values
/// select ADP's default log file and the logging stack's default rotation size.
pub fn logging_settings(logging: &Logging) -> LoggingSettings {
    let mut settings = LoggingSettings::default();
    settings.level = logging.level.clone();
    settings.format_json = logging.format_json;
    settings.format_rfc3339 = logging.format_rfc3339;
    settings.to_console = logging.to_console;
    settings.to_syslog = logging.to_syslog;
    settings.syslog_rfc = logging.syslog_rfc;
    settings.syslog_uri = logging.syslog_uri.clone();
    settings.file = logging.file.is_explicit().then(|| logging.file.value.clone());
    settings.disable_file_logging = logging.disable_file_logging;
    settings.file_max_rolls = logging.file_max_rolls;
    settings.file_max_size = logging
        .file_max_size
        .is_explicit()
        .then_some(logging.file_max_size.value);
    settings
}

/// The configured log level, as a [`LiveValue`] for the dynamic log level worker.
#[derive(Clone)]
pub struct LiveLogLevel(pub Live<String>);

impl LiveValue for LiveLogLevel {
    type Value = String;

    fn changed(&mut self) -> impl Future<Output = String> + Send {
        self.0.changed()
    }
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::ConfigValue;
    use bytesize::ByteSize;

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

    fn translate_level(level: &str) -> Vec<String> {
        let logging = Logging {
            level: level.to_string(),
            ..defaulted_logging()
        };

        let config = adp_logging_translator()
            .translate(&logging_settings(&logging))
            .expect("translate logging config");
        config
            .log_level
            .as_targets()
            .to_string()
            .split(',')
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn plain_log_level_covers_adp_and_the_subagent_runtime() {
        let directives = translate_level("debug");

        assert!(directives.contains(&"agent_data_plane=debug".to_string()));
        assert!(directives.contains(&"datadog_agent_runtime=debug".to_string()));
        assert!(directives.contains(&"saluki_components=debug".to_string()));
    }

    #[test]
    fn defaulted_log_file_uses_the_platform_default_path() {
        let config = adp_logging_translator()
            .translate(&logging_settings(&defaulted_logging()))
            .expect("translate logging config");

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
        let config = adp_logging_translator()
            .translate(&logging_settings(&logging))
            .expect("translate logging config");

        assert_eq!(config.log_file, "/tmp/adp.log");
    }

    #[test]
    fn defaulted_max_size_keeps_the_binary_rotation_default() {
        let config = adp_logging_translator()
            .translate(&logging_settings(&defaulted_logging()))
            .expect("translate logging config");

        assert_eq!(config.log_file_max_size, ByteSize::mib(10));
    }

    #[test]
    fn explicit_max_size_is_used() {
        let logging = Logging {
            file_max_size: ConfigValue::explicit(1_048_576),
            ..defaulted_logging()
        };
        let config = adp_logging_translator()
            .translate(&logging_settings(&logging))
            .expect("translate logging config");

        assert_eq!(config.log_file_max_size, ByteSize::b(1_048_576));
    }

    #[test]
    fn every_setting_is_carried_over() {
        let logging = Logging {
            level: "warn".to_string(),
            format_rfc3339: true,
            format_json: true,
            to_console: false,
            to_syslog: true,
            syslog_rfc: true,
            syslog_uri: "udp://127.0.0.1:1514".to_string(),
            file: ConfigValue::explicit("/tmp/adp.log".to_string()),
            disable_file_logging: true,
            file_max_rolls: 5,
            file_max_size: ConfigValue::explicit(1_048_576),
        };
        let settings = logging_settings(&logging);

        assert_eq!(settings.level, "warn");
        assert!(settings.format_rfc3339);
        assert!(settings.format_json);
        assert!(!settings.to_console);
        assert!(settings.to_syslog);
        assert!(settings.syslog_rfc);
        assert_eq!(settings.syslog_uri, "udp://127.0.0.1:1514");
        assert_eq!(settings.file.as_deref(), Some("/tmp/adp.log"));
        assert!(settings.disable_file_logging);
        assert_eq!(settings.file_max_rolls, 5);
        assert_eq!(settings.file_max_size, Some(1_048_576));
    }
}
