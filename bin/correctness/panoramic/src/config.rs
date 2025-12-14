use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;

/// A duration that can be parsed from human-readable strings like "10s", "1m", "500ms".
#[derive(Clone, Debug)]
pub struct HumanDuration(pub Duration);

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse_duration(&s).map(HumanDuration).map_err(serde::de::Error::custom)
    }
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration string".to_string());
    }

    let mut total = Duration::ZERO;
    let mut current_num = String::new();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            current_num.push(c);
        } else if c.is_alphabetic() {
            if current_num.is_empty() {
                return Err(format!("unexpected unit '{}' without a number", c));
            }

            let num: u64 = current_num
                .parse()
                .map_err(|_| format!("invalid number: {}", current_num))?;
            current_num.clear();

            // Collect the full unit string
            let mut unit = String::from(c);
            while chars.peek().map(|c| c.is_alphabetic()).unwrap_or(false) {
                unit.push(chars.next().unwrap());
            }

            let duration = match unit.as_str() {
                "ns" => Duration::from_nanos(num),
                "us" | "µs" => Duration::from_micros(num),
                "ms" => Duration::from_millis(num),
                "s" => Duration::from_secs(num),
                "m" => Duration::from_secs(num * 60),
                "h" => Duration::from_secs(num * 3600),
                _ => return Err(format!("unknown duration unit: {}", unit)),
            };

            total += duration;
        } else if c.is_whitespace() {
            // Skip whitespace
        } else {
            return Err(format!("unexpected character: {}", c));
        }
    }

    // Handle trailing number without unit (assume seconds)
    if !current_num.is_empty() {
        let num: u64 = current_num
            .parse()
            .map_err(|_| format!("invalid number: {}", current_num))?;
        total += Duration::from_secs(num);
    }

    if total.is_zero() {
        return Err("duration must be greater than zero".to_string());
    }

    Ok(total)
}

/// Root test case configuration.
#[derive(Clone, Debug, Deserialize)]
pub struct TestCase {
    /// Name of the test case.
    pub name: String,

    /// Optional description of what the test verifies.
    #[serde(default)]
    pub description: Option<String>,

    /// Overall timeout for the test case.
    pub timeout: HumanDuration,

    /// Container configuration.
    pub container: ContainerConfig,

    /// List of assertions to run.
    pub assertions: Vec<AssertionConfig>,

    /// Base path for resolving relative file paths.
    #[serde(skip)]
    pub base_path: PathBuf,
}

/// Container configuration for a test case.
#[derive(Clone, Debug, Deserialize)]
pub struct ContainerConfig {
    /// Container image to use.
    pub image: String,

    /// Optional entrypoint override.
    #[serde(default)]
    pub entrypoint: Vec<String>,

    /// Optional command override.
    #[serde(default)]
    pub command: Vec<String>,

    /// Environment variables to set.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Files to mount (host_path:container_path format).
    #[serde(default)]
    pub files: Vec<String>,

    /// Ports to expose (port/protocol format, e.g., "8125/udp").
    #[serde(default)]
    pub exposed_ports: Vec<String>,
}

/// Configuration for a single assertion.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssertionConfig {
    /// Check that the process doesn't exit for a specified duration.
    ProcessStableFor {
        /// How long the process should remain stable.
        duration: HumanDuration,
    },

    /// Check that a port is listening.
    PortListening {
        /// The port number to check.
        port: u16,
        /// The protocol (tcp or udp).
        protocol: String,
        /// Timeout for waiting for the port to become available.
        timeout: HumanDuration,
    },

    /// Check that a pattern appears in the logs.
    LogContains {
        /// The pattern to search for.
        pattern: String,
        /// Whether to interpret the pattern as a regex.
        #[serde(default)]
        regex: bool,
        /// Timeout for waiting for the pattern to appear.
        timeout: HumanDuration,
        /// Which log stream to check (stdout, stderr, or both).
        #[serde(default)]
        stream: LogStream,
    },

    /// Check that a pattern does NOT appear in the logs for a duration.
    LogNotContains {
        /// The pattern that should not appear.
        pattern: String,
        /// Whether to interpret the pattern as a regex.
        #[serde(default)]
        regex: bool,
        /// How long to check for the pattern's absence.
        during: HumanDuration,
        /// Which log stream to check (stdout, stderr, or both).
        #[serde(default)]
        stream: LogStream,
    },

    /// Check an HTTP health endpoint.
    HealthCheck {
        /// The endpoint URL to check.
        endpoint: String,
        /// The expected HTTP status code.
        expected_status: u16,
        /// Timeout for the health check to succeed.
        timeout: HumanDuration,
    },
}

/// Which log stream(s) to check.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
    #[default]
    Both,
}

impl TestCase {
    /// Load a test case from a YAML configuration file.
    pub fn from_yaml<P: AsRef<Path>>(path: P) -> Result<Self, GenericError> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .error_context(format!("Failed to read configuration file: {}", path.display()))?;

        let mut test_case: TestCase = serde_yaml::from_str(&content)
            .error_context(format!("Failed to parse configuration file: {}", path.display()))?;

        test_case.base_path = path
            .parent()
            .unwrap_or(Path::new("."))
            .canonicalize()
            .error_context("Failed to canonicalize base path")?;

        Ok(test_case)
    }

    /// Resolve a file path relative to the test case's base path.
    pub fn resolve_path<P: AsRef<Path>>(&self, path: P) -> PathBuf {
        let path = path.as_ref();
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_path.join(path)
        }
    }
}

/// Discover all test cases in a directory.
pub fn discover_tests<P: AsRef<Path>>(base_path: P) -> Result<Vec<TestCase>, GenericError> {
    let base_path = base_path.as_ref();
    let mut test_cases = Vec::new();

    if !base_path.is_dir() {
        return Err(generic_error!("Test directory does not exist: {}", base_path.display()));
    }

    let entries = std::fs::read_dir(base_path)
        .error_context(format!("Failed to read test directory: {}", base_path.display()))?;

    for entry in entries {
        let entry = entry.error_context("Failed to read directory entry")?;
        let path = entry.path();

        if path.is_dir() {
            let config_path = path.join("config.yaml");
            if config_path.exists() {
                match TestCase::from_yaml(&config_path) {
                    Ok(test_case) => test_cases.push(test_case),
                    Err(e) => {
                        tracing::warn!(
                            path = %config_path.display(),
                            error = %e,
                            "Failed to load test case, skipping"
                        );
                    }
                }
            }
        }
    }

    // Sort by name for deterministic ordering
    test_cases.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(test_cases)
}

/// Parse a port specification (e.g., "8125/udp") into port number and protocol.
pub fn parse_port_spec(spec: &str) -> Result<(u16, &str), GenericError> {
    let parts: Vec<&str> = spec.split('/').collect();
    if parts.len() != 2 {
        return Err(generic_error!(
            "Invalid port specification '{}': expected format 'port/protocol'",
            spec
        ));
    }

    let port: u16 = parts[0]
        .parse()
        .map_err(|_| generic_error!("Invalid port number: {}", parts[0]))?;

    let protocol = parts[1];
    if protocol != "tcp" && protocol != "udp" {
        return Err(generic_error!(
            "Invalid protocol '{}': expected 'tcp' or 'udp'",
            protocol
        ));
    }

    Ok((port, protocol))
}

/// Parse a file mount specification (e.g., "host_path:container_path").
pub fn parse_file_spec(spec: &str) -> Result<(&str, &str), GenericError> {
    let parts: Vec<&str> = spec.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Err(generic_error!(
            "Invalid file specification '{}': expected format 'host_path:container_path'",
            spec
        ));
    }

    Ok((parts[0], parts[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn test_parse_port_spec() {
        let (port, protocol) = parse_port_spec("8125/udp").unwrap();
        assert_eq!(port, 8125);
        assert_eq!(protocol, "udp");

        let (port, protocol) = parse_port_spec("443/tcp").unwrap();
        assert_eq!(port, 443);
        assert_eq!(protocol, "tcp");

        assert!(parse_port_spec("invalid").is_err());
        assert!(parse_port_spec("8125/http").is_err());
    }

    #[test]
    fn test_parse_file_spec() {
        let (host, container) = parse_file_spec("./config.yaml:/etc/config.yaml").unwrap();
        assert_eq!(host, "./config.yaml");
        assert_eq!(container, "/etc/config.yaml");

        assert!(parse_file_spec("nocolon").is_err());
    }
}
