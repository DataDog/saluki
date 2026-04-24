use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;

use crate::correctness::config::Config as CorrectnessConfig;

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

/// A discovered test, either an integration test or a correctness test.
pub enum DiscoveredTest {
    /// An integration test case (panoramic schema).
    Integration(TestCase),
    /// A correctness test case.
    Correctness {
        /// Name of the test (derived from directory name).
        name: String,
        /// The correctness test configuration.
        config: CorrectnessConfig,
    },
}

impl DiscoveredTest {
    /// Returns the name of the test.
    pub fn name(&self) -> &str {
        match self {
            DiscoveredTest::Integration(tc) => &tc.name,
            DiscoveredTest::Correctness { name, .. } => name,
        }
    }

    /// Returns the timeout for the test.
    pub fn timeout(&self) -> Duration {
        match self {
            DiscoveredTest::Integration(tc) => tc.timeout.0,
            DiscoveredTest::Correctness { .. } => Duration::from_secs(20 * 60),
        }
    }

    /// Returns the description of the test, if any.
    pub fn description(&self) -> Option<&str> {
        match self {
            DiscoveredTest::Integration(tc) => tc.description.as_deref(),
            DiscoveredTest::Correctness { .. } => None,
        }
    }
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

    /// List of assertion steps to run.
    pub assertions: Vec<AssertionStep>,

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

    /// Ports to expose (port/protocol format, for example, "8125/udp").
    #[serde(default)]
    pub exposed_ports: Vec<String>,
}

/// A single step in the assertion pipeline.
///
/// Each step is either a single assertion or a parallel block of assertions
/// that run concurrently.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum AssertionStep {
    /// A block of assertions that run concurrently.
    Parallel {
        /// The assertions to run in parallel.
        parallel: Vec<AssertionConfig>,
    },
    /// A single assertion that runs on its own.
    Single(AssertionConfig),
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

    /// Check that the process exits with a specific exit code.
    ProcessExitsWith {
        /// The expected exit code.
        expected_code: i64,
        /// Timeout for waiting for the process to exit.
        timeout: HumanDuration,
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
    /// Count total individual assertions across all steps.
    pub fn total_assertion_count(&self) -> usize {
        self.assertions
            .iter()
            .map(|step| match step {
                AssertionStep::Single(_) => 1,
                AssertionStep::Parallel { parallel } => parallel.len(),
            })
            .sum()
    }

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

/// Discover all test cases across one or more directories.
///
/// Each `config.yaml` found in a direct subdirectory must have a top-level `type` field set to
/// either `"integration"` or `"correctness"`. Files with a missing or unknown `type` are skipped
/// with a warning. Multiple test types may coexist freely within the same directory.
pub fn discover_tests(dirs: &[PathBuf]) -> Result<Vec<DiscoveredTest>, GenericError> {
    let mut tests = Vec::new();

    for base_path in dirs {
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
                    match try_load_test(&config_path, &path) {
                        Ok(test) => tests.push(test),
                        Err(e) => {
                            // Previously we had a warning here that cannot be seen in TUI-mode. It is better to fail
                            // loudly and fast when we have a bad test configuration than to falsely believe our test is
                            // working when we see that all tests passed.
                            panic!("Failed to load test case, bad configuration: {e}");
                        }
                    }
                }
            }
        }
    }

    // Sort by name for deterministic ordering.
    tests.sort_by(|a, b| a.name().cmp(b.name()));

    Ok(tests)
}

/// Load a test case from a config file, dispatching on the top-level `type` field.
fn try_load_test(config_path: &Path, dir_path: &Path) -> Result<DiscoveredTest, GenericError> {
    let content = std::fs::read_to_string(config_path)
        .error_context(format!("Failed to read config file: {}", config_path.display()))?;

    let peek: serde_yaml::Value = serde_yaml::from_str(&content).error_context(format!(
        "Failed to parse config file as YAML: {}",
        config_path.display()
    ))?;

    let test_type = peek
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| generic_error!("Missing required 'type' field (expected 'integration' or 'correctness')"))?;

    match test_type {
        "integration" => TestCase::from_yaml(config_path).map(DiscoveredTest::Integration),
        "correctness" => {
            let config_path_str = config_path
                .to_str()
                .ok_or_else(|| generic_error!("Invalid UTF-8 in config path: {}", config_path.display()))?;
            let config = CorrectnessConfig::from_yaml(config_path_str)?;
            let name = dir_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            Ok(DiscoveredTest::Correctness { name, config })
        }
        other => Err(generic_error!(
            "Unknown test type '{}' (expected 'integration' or 'correctness')",
            other
        )),
    }
}

/// Parse a port specification (for example, "8125/udp") into port number and protocol.
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

/// Parse a file mount specification (for example, "host_path:container_path").
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
