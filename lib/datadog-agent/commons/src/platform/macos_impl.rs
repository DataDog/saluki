use std::path::Path;

/// Default configuration directory for the Datadog Agent.
pub const DATADOG_AGENT_CONF_DIR: &str = "/opt/datadog-agent/etc";

/// Default log directory for the Datadog Agent.
pub const DATADOG_AGENT_LOG_DIR: &str = "/opt/datadog-agent/logs";

/// Default local syslog URI for the Datadog Agent.
pub const DATADOG_AGENT_DEFAULT_SYSLOG_URI: &str = "unixgram:///var/run/syslog";

/// Returns the path to the default Datadog Agent configuration directory.
pub fn get_config_dir_path() -> &'static Path {
    Path::new(DATADOG_AGENT_CONF_DIR)
}

/// Returns the path to the default Datadog Agent log directory.
pub fn get_log_dir_path() -> &'static Path {
    Path::new(DATADOG_AGENT_LOG_DIR)
}
