use std::collections::BTreeMap;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::{
    de::{DeserializeOwned, Error as _},
    Deserialize, Deserializer,
};
use serde_json::Value;

use crate::correctness::analysis::AnalysisMode;
use crate::correctness::config::{
    Config as CorrectnessConfig, DatadogIntakeConfig as CorrectnessDatadogIntakeConfig,
    MillstoneConfig as CorrectnessMillstoneConfig, Runtime as CorrectnessRuntime,
    TargetConfig as CorrectnessTargetConfig,
};
use crate::reporter::TestResult;
use crate::test::{Test, TestContext, TestSuite};

/// A duration that can be parsed from human-readable strings like `10s`, `1m`, `500ms`.
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

/// The deserializable configuration struct that defines an integration test. Not to be confused with
/// `CorrectnessConfig` which is a different testing modality.
#[derive(Clone, Debug, Deserialize)]
pub struct IntegrationConfig {
    /// Name of the test case.
    pub name: String,

    /// Optional description of what the test verifies.
    #[serde(default)]
    pub description: Option<String>,

    /// Overall timeout for the test case.
    pub timeout: HumanDuration,

    /// Container configuration. Optional; defaults to an empty configuration. The container
    /// image is selected by the active runtime, not the test case.
    #[serde(default)]
    pub container: ContainerConfig,

    /// Datadog intake sidecar configuration. Optional; disabled by default.
    #[serde(default)]
    pub intake: IntakeConfig,

    /// Environment variables to set on the target process(es).
    ///
    /// Top-level (not under `container`) because both the linux and `mac` runtimes apply
    /// these the same way: docker injects them as container env, the Unix runner passes them
    /// to the spawned ADP / Core Agent processes.
    ///
    /// Keys are variable names, values are the string the process receives. Values must be YAML
    /// strings, so quote anything that would otherwise parse as a boolean or a number (`"true"`,
    /// `"8125"`).
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: HashMap<String, String>,

    /// Ordered list of steps (assertions and actions) to execute.
    pub procedure: Vec<AssertionStep>,

    /// Runtimes under which this test is eligible to run.
    ///
    /// Each value must be `"linux"` (the default), `"mac"`, or `"windows"`. The active
    /// runtime for any given panoramic invocation is chosen at the CLI level (`--runtime`,
    /// defaulting to the host's native runtime); a test discovers only when this list contains
    /// that active runtime. Tests with multiple entries are portable across runtimes, but still
    /// execute only once per invocation, in the active runtime.
    #[serde(default = "default_integration_runtimes")]
    pub runtimes: Vec<String>,

    /// Active runtime for this test instance.
    ///
    /// Empty at parse time; the discovery layer sets it to whichever runtime the CLI is scoped
    /// to (after confirming that runtime is listed in `runtimes`). Used by `Test::run` to
    /// dispatch to the right runner and by `Test::runtime` / `Test::images` to report the
    /// effective runtime to the CI pipeline generator.
    #[serde(skip)]
    pub active_runtime: String,

    /// Base path for resolving relative file paths.
    #[serde(skip)]
    pub base_path: PathBuf,
}

fn default_integration_runtimes() -> Vec<String> {
    vec![default_host_runtime().to_string()]
}

/// Runtime identifier for integration tests that run as host processes on macOS (no Docker, no
/// virtualization). Validated on macOS only today; future host-process runtimes for other Unix
/// platforms will get their own identifiers.
pub const MAC_RUNTIME: &str = "mac";

/// Runtime identifier for integration tests that run inside a Linux container.
pub const LINUX_RUNTIME: &str = "linux";

/// Runtime identifier for integration tests that run inside a Windows container.
pub const WINDOWS_RUNTIME: &str = "windows";

/// Default container image used by `linux`-runtime integration tests.
pub const DEFAULT_LINUX_TARGET_IMAGE: &str = "saluki-images/datadog-agent:testing-devel";

/// Default container image used by `windows`-runtime integration tests.
pub const DEFAULT_WINDOWS_TARGET_IMAGE: &str = "saluki-images/agent-data-plane:testing-windows";

/// Returns the integration-test target image for the given runtime, if the runtime uses one.
///
/// `mac` runs ADP as a host process and has no target image. All other runtimes resolve to a
/// fixed, harness-owned image; tests do not select images per case.
pub fn target_image_for_runtime(runtime: &str) -> Option<&'static str> {
    match runtime {
        LINUX_RUNTIME => Some(DEFAULT_LINUX_TARGET_IMAGE),
        WINDOWS_RUNTIME => Some(DEFAULT_WINDOWS_TARGET_IMAGE),
        _ => None,
    }
}

/// Returns the integration-test runtime that is native to the host OS.
///
/// `mac` on macOS hosts, `windows` on Windows hosts, and `linux` everywhere else. Used as the default when a panoramic
/// subcommand is invoked without an explicit `--runtime` flag, so that callers on the most
/// common host get the most common runtime without having to spell it out.
pub fn default_host_runtime() -> &'static str {
    if cfg!(target_os = "macos") {
        MAC_RUNTIME
    } else if cfg!(target_os = "windows") {
        WINDOWS_RUNTIME
    } else {
        LINUX_RUNTIME
    }
}

/// Datadog intake sidecar configuration for a test case.
///
/// When enabled, the runner starts a `datadog-intake` container in the test's isolation group,
/// reachable from the target under a fixed network alias. Tests point the target's intake URL at
/// that alias and assert on what the intake received.
///
/// Only the `linux` runtime can host the sidecar: the `mac` runtime runs the target as a host
/// process with no container network, and the `windows` runtime needs a `nat` network that the
/// sidecar's Linux container cannot share. Runners reject the combination rather than skip it, so a
/// test that needs the sidecar must declare `runtimes: [linux]`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct IntakeConfig {
    /// Whether to start the intake sidecar.
    #[serde(default)]
    pub enabled: bool,
}

/// Container image providing the `datadog-intake` binary.
pub const DEFAULT_INTAKE_IMAGE: &str = "saluki-images/correctness-tools:latest";

/// Docker network alias under which the target reaches the intake sidecar.
pub const INTAKE_NETWORK_ALIAS: &str = "datadog-intake";

/// Container-side HTTP port of the intake sidecar.
pub const INTAKE_HTTP_PORT: u16 = 2049;

/// Container configuration for a test case.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ContainerConfig {
    /// Optional entrypoint override.
    #[serde(default)]
    pub entrypoint: Vec<String>,

    /// Optional command override.
    #[serde(default)]
    pub command: Vec<String>,

    /// Files to mount (host_path:container_path format).
    #[serde(default)]
    pub files: Vec<String>,

    /// Ports to expose (port/protocol format, for example, "8125/udp").
    #[serde(default)]
    pub exposed_ports: Vec<String>,

    /// Whether the Linux target joins the Docker host's cgroup namespace.
    ///
    /// Defaults to `false` to preserve cgroup namespace isolation. Enable this only when a test
    /// needs the target's local cgroup path to expose its Docker container ID.
    #[serde(default)]
    pub host_cgroup_namespace: bool,
}

/// A single step in the assertion pipeline.
///
/// Each step is either a single assertion, a parallel block of assertions, or a sequential action.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum AssertionStep {
    /// A block of assertions that run concurrently.
    Parallel {
        /// The assertions to run in parallel.
        parallel: Vec<AssertionConfig>,
    },
    /// A single action that runs on its own.
    Action(ActionConfig),
    /// A single assertion that runs on its own.
    Single(AssertionConfig),
}

/// Configuration for a single action.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ActionConfig {
    /// Set a runtime configuration value through the Core Agent command API.
    CoreAgentConfigSet {
        /// Runtime configuration key to set.
        key: String,
        /// Value to set. Strings are sent without JSON quoting; other values are serialized as JSON.
        value: Value,
        /// Core Agent runtime config endpoint template. `{key}` is replaced with `key`.
        #[serde(default = "crate::actions::default_core_agent_config_endpoint_template")]
        endpoint: String,
        /// Timeout for waiting for the Core Agent API to accept the mutation.
        #[serde(default = "default_action_timeout")]
        timeout: HumanDuration,
    },

    /// Invoke the tested ADP binary as a CLI command.
    AdpCli {
        /// Arguments appended after the runtime-specific ADP binary and global configuration prefix.
        args: Vec<String>,
        /// Timeout for waiting for the command to succeed.
        #[serde(default = "default_action_timeout")]
        timeout: HumanDuration,
    },

    /// Capture, replay, and verify DogStatsD traffic through the tested ADP process.
    DogstatsdReplay {
        /// Command that sends the traffic to be captured in the target environment.
        sender: Vec<String>,
        /// How long the capture remains active after it starts.
        capture_duration: HumanDuration,
        /// Number of seconds the post-replay statistics collection runs.
        stats_duration_secs: u64,
        /// Metric names that must appear during the post-replay statistics collection.
        expected_metrics: Vec<String>,
        /// Timeout for each target command invoked by the action.
        #[serde(default = "default_action_timeout")]
        timeout: HumanDuration,
    },

    /// Invoke the Core Agent binary as a CLI command.
    CoreAgentCli {
        /// Arguments appended after the runtime-specific Core Agent binary and global configuration prefix.
        args: Vec<String>,
        /// Optional substring that must appear in successful command output.
        #[serde(default)]
        output_contains: Option<String>,
        /// Timeout for waiting for the command to succeed.
        #[serde(default = "default_action_timeout")]
        timeout: HumanDuration,
    },

    /// Send one DogStatsD datagram over UDP to the target.
    DogstatsdSend {
        /// Datagram payload, in DogStatsD text protocol.
        payload: String,
        /// Container-side UDP port to send to. Must be exposed by the test case.
        port: u16,
        /// Timeout for the send.
        #[serde(default = "default_action_timeout")]
        timeout: HumanDuration,
    },

    /// Run a command inside the target environment.
    TargetExec {
        /// Command and arguments to run in the target environment.
        command: Vec<String>,
        /// Timeout for waiting for the command to succeed.
        #[serde(default = "default_action_timeout")]
        timeout: HumanDuration,
    },
}

fn default_action_timeout() -> HumanDuration {
    HumanDuration(Duration::from_secs(30))
}

/// Configuration for a single assertion.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "assertion", rename_all = "snake_case")]
pub enum AssertionConfig {
    /// Check that the process doesn't exit for a specified duration.
    ProcessStableFor {
        /// How long the process should remain stable.
        duration: HumanDuration,
    },

    /// Check that ADP itself exits with a specific exit code, abstracting over the runtime's
    /// observation mechanism.
    ///
    /// On the `linux` runtime the converged image wraps ADP under s6, which keeps the
    /// container alive across ADP restarts and logs `agent-data-plane exited with code N` from
    /// `docker/s6-services/agent-data-plane/finish`. This assertion greps the log buffer for
    /// that line. On the `mac` runtime ADP is spawned directly; the assertion reads
    /// the exit code recorded by the Unix runner when ADP's child process exited.
    AdpExitsWith {
        /// The expected exit code.
        expected_code: i64,
        /// Timeout for waiting for the exit to be observed.
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

    /// Check that a pattern doesn't appear in the logs for a duration.
    LogNotContains {
        /// The pattern that shouldn't appear.
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

    /// Probe an HTTP/HTTPS endpoint and assert on the response status code.
    ///
    /// HTTPS endpoints are supported with optional certificate verification skipping. The status
    /// matcher accepts either "must equal" or "must not equal" semantics; the latter is useful for
    /// asserting only that a route is registered without having to know what status code the
    /// endpoint would otherwise return.
    HttpCheck {
        /// The endpoint URL to check. Both `http://` and `https://` schemes are accepted.
        endpoint: String,
        /// Matcher applied to the response status code.
        status: HttpStatusMatcher,
        /// Whether to skip TLS certificate verification for `https://` endpoints.
        ///
        /// Defaults to `false`. Set to `true` when probing endpoints that serve self-signed
        /// certificates (such as the ADP privileged API in integration tests).
        #[serde(default)]
        insecure_skip_verify: bool,
        /// Timeout for the check to succeed.
        timeout: HumanDuration,
    },

    /// Check that a file exists in the container, and optionally that its contents match a pattern.
    FileContains {
        /// Absolute path to the file inside the container.
        path: String,
        /// Optional pattern that must appear in the file's contents. If omitted, only file existence is checked.
        #[serde(default)]
        pattern: Option<String>,
        /// Whether to interpret `pattern` as a regex.
        #[serde(default)]
        regex: bool,
        /// Timeout for waiting for the file (and pattern, if any) to appear.
        timeout: HumanDuration,
    },

    /// Poll an ADP configuration view through its authenticated CLI until one key equals the expected value.
    AdpConfigKeyEquals {
        /// Configuration key to compare. Dotted paths address nested objects.
        key: String,
        /// Expected value.
        value: Value,
        /// Configuration endpoint selecting `/config` or the translated runtime view at `/config/runtime`.
        #[serde(default = "crate::assertions::default_adp_config_endpoint")]
        endpoint: String,
        /// Timeout for waiting for the value to appear.
        timeout: HumanDuration,
    },

    /// Poll the intake sidecar until it holds a metric matching the given criteria.
    IntakeHasMetric {
        /// Metric name. Matched exactly.
        name: String,
        /// Optional metric type the matching value must have.
        #[serde(default)]
        metric_type: Option<MetricTypeMatcher>,
        /// Optional numeric value the matching value must carry. Not applicable to sketches.
        #[serde(default)]
        value: Option<f64>,
        /// Tags that must all be present on the matching metric. Extra tags are allowed.
        #[serde(default)]
        tags: Vec<String>,
        /// Timeout for waiting for the metric to arrive.
        timeout: HumanDuration,
    },
}

/// Metric type accepted by an [`AssertionConfig::IntakeHasMetric`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MetricTypeMatcher {
    Count,
    Rate,
    Gauge,
    Sketch,
}

/// Which log streams to check.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
    #[default]
    Both,
}

/// Matcher for the response status code of an [`AssertionConfig::HttpCheck`].
///
/// Exactly one variant is set at deserialization time, so the assertion either requires a specific
/// status code or rejects a specific status code.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpStatusMatcher {
    /// Assertion passes when the response status code equals this value.
    Equal(u16),
    /// Assertion passes when the response status code is anything other than this value.
    NotEqual(u16),
}

impl ActionConfig {
    /// Replaces `{{PANORAMIC_DYNAMIC_*}}` placeholders in string fields with resolved values.
    pub fn resolve_dynamic_vars(&mut self, vars: &HashMap<String, String>) {
        match self {
            ActionConfig::CoreAgentConfigSet {
                key, endpoint, value, ..
            } => {
                crate::dynamic_vars::resolve_placeholders(key, vars);
                crate::dynamic_vars::resolve_placeholders(endpoint, vars);
                if let serde_json::Value::String(s) = value {
                    crate::dynamic_vars::resolve_placeholders(s, vars);
                }
            }
            ActionConfig::AdpCli { args, .. } => {
                for arg in args {
                    crate::dynamic_vars::resolve_placeholders(arg, vars);
                }
            }
            ActionConfig::CoreAgentCli {
                args, output_contains, ..
            } => {
                for arg in args {
                    crate::dynamic_vars::resolve_placeholders(arg, vars);
                }
                if let Some(output_contains) = output_contains {
                    crate::dynamic_vars::resolve_placeholders(output_contains, vars);
                }
            }
            ActionConfig::DogstatsdReplay {
                sender,
                expected_metrics,
                ..
            } => {
                for arg in sender.iter_mut().chain(expected_metrics) {
                    crate::dynamic_vars::resolve_placeholders(arg, vars);
                }
            }
            ActionConfig::DogstatsdSend { payload, .. } => {
                crate::dynamic_vars::resolve_placeholders(payload, vars);
            }
            ActionConfig::TargetExec { command, .. } => {
                for arg in command {
                    crate::dynamic_vars::resolve_placeholders(arg, vars);
                }
            }
        }
    }

    /// Returns any unresolved `{{PANORAMIC_DYNAMIC_*}}` placeholders in string fields.
    pub fn unresolved_placeholders(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self {
            ActionConfig::CoreAgentConfigSet {
                key, endpoint, value, ..
            } => {
                crate::dynamic_vars::find_unresolved(key, &mut out);
                crate::dynamic_vars::find_unresolved(endpoint, &mut out);
                if let serde_json::Value::String(s) = value {
                    crate::dynamic_vars::find_unresolved(s, &mut out);
                }
            }
            ActionConfig::AdpCli { args, .. } => {
                for arg in args {
                    crate::dynamic_vars::find_unresolved(arg, &mut out);
                }
            }
            ActionConfig::CoreAgentCli {
                args, output_contains, ..
            } => {
                for arg in args {
                    crate::dynamic_vars::find_unresolved(arg, &mut out);
                }
                if let Some(output_contains) = output_contains {
                    crate::dynamic_vars::find_unresolved(output_contains, &mut out);
                }
            }
            ActionConfig::DogstatsdReplay {
                sender,
                expected_metrics,
                ..
            } => {
                for arg in sender.iter().chain(expected_metrics) {
                    crate::dynamic_vars::find_unresolved(arg, &mut out);
                }
            }
            ActionConfig::DogstatsdSend { payload, .. } => {
                crate::dynamic_vars::find_unresolved(payload, &mut out);
            }
            ActionConfig::TargetExec { command, .. } => {
                for arg in command {
                    crate::dynamic_vars::find_unresolved(arg, &mut out);
                }
            }
        }
        out
    }
}

impl AssertionConfig {
    /// Replaces `{{PANORAMIC_DYNAMIC_*}}` placeholders in string fields with resolved values.
    pub fn resolve_dynamic_vars(&mut self, vars: &HashMap<String, String>) {
        match self {
            AssertionConfig::LogContains { pattern, .. } | AssertionConfig::LogNotContains { pattern, .. } => {
                crate::dynamic_vars::resolve_placeholders(pattern, vars);
            }
            AssertionConfig::HttpCheck { endpoint, .. } => {
                crate::dynamic_vars::resolve_placeholders(endpoint, vars);
            }
            AssertionConfig::PortListening { protocol, .. } => {
                crate::dynamic_vars::resolve_placeholders(protocol, vars);
            }
            AssertionConfig::FileContains { path, pattern, .. } => {
                crate::dynamic_vars::resolve_placeholders(path, vars);
                if let Some(p) = pattern {
                    crate::dynamic_vars::resolve_placeholders(p, vars);
                }
            }
            AssertionConfig::AdpConfigKeyEquals { key, endpoint, .. } => {
                crate::dynamic_vars::resolve_placeholders(key, vars);
                crate::dynamic_vars::resolve_placeholders(endpoint, vars);
            }
            AssertionConfig::IntakeHasMetric { name, tags, .. } => {
                crate::dynamic_vars::resolve_placeholders(name, vars);
                for tag in tags {
                    crate::dynamic_vars::resolve_placeholders(tag, vars);
                }
            }
            AssertionConfig::ProcessStableFor { .. } | AssertionConfig::AdpExitsWith { .. } => {}
        }
    }

    /// Returns any unresolved `{{PANORAMIC_DYNAMIC_*}}` placeholders in string fields.
    pub fn unresolved_placeholders(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self {
            AssertionConfig::LogContains { pattern, .. } | AssertionConfig::LogNotContains { pattern, .. } => {
                crate::dynamic_vars::find_unresolved(pattern, &mut out);
            }
            AssertionConfig::HttpCheck { endpoint, .. } => {
                crate::dynamic_vars::find_unresolved(endpoint, &mut out);
            }
            AssertionConfig::PortListening { protocol, .. } => {
                crate::dynamic_vars::find_unresolved(protocol, &mut out);
            }
            AssertionConfig::FileContains { path, pattern, .. } => {
                crate::dynamic_vars::find_unresolved(path, &mut out);
                if let Some(p) = pattern {
                    crate::dynamic_vars::find_unresolved(p, &mut out);
                }
            }
            AssertionConfig::AdpConfigKeyEquals { key, endpoint, .. } => {
                crate::dynamic_vars::find_unresolved(key, &mut out);
                crate::dynamic_vars::find_unresolved(endpoint, &mut out);
            }
            AssertionConfig::IntakeHasMetric { name, tags, .. } => {
                crate::dynamic_vars::find_unresolved(name, &mut out);
                for tag in tags {
                    crate::dynamic_vars::find_unresolved(tag, &mut out);
                }
            }
            AssertionConfig::ProcessStableFor { .. } | AssertionConfig::AdpExitsWith { .. } => {}
        }
        out
    }
}

impl AssertionStep {
    /// Replaces `{{PANORAMIC_DYNAMIC_*}}` placeholders in all assertion configs within this step.
    pub fn resolve_dynamic_vars(&mut self, vars: &HashMap<String, String>) {
        match self {
            AssertionStep::Single(config) => config.resolve_dynamic_vars(vars),
            AssertionStep::Action(config) => config.resolve_dynamic_vars(vars),
            AssertionStep::Parallel { parallel } => {
                for config in parallel {
                    config.resolve_dynamic_vars(vars);
                }
            }
        }
    }

    /// Returns any unresolved `{{PANORAMIC_DYNAMIC_*}}` placeholders in this step.
    pub fn unresolved_placeholders(&self) -> Vec<String> {
        match self {
            AssertionStep::Single(config) => config.unresolved_placeholders(),
            AssertionStep::Action(config) => config.unresolved_placeholders(),
            AssertionStep::Parallel { parallel } => parallel.iter().flat_map(|c| c.unresolved_placeholders()).collect(),
        }
    }
}

#[async_trait]
impl Test for IntegrationConfig {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn suite(&self) -> TestSuite {
        TestSuite::Integration
    }

    fn description(&self) -> Option<String> {
        self.description.clone()
    }

    fn case_path(&self) -> PathBuf {
        self.base_path.clone()
    }

    fn timeout(&self) -> Duration {
        self.timeout.0
    }

    fn images(&self) -> BTreeMap<&str, String> {
        let mut m = BTreeMap::new();
        if let Some(image) = target_image_for_runtime(&self.active_runtime) {
            m.insert("container", image.to_string());
        }
        if self.intake.enabled {
            m.insert("intake", DEFAULT_INTAKE_IMAGE.to_string());
        }
        m
    }

    fn runtime(&self) -> String {
        if self.active_runtime.is_empty() {
            LINUX_RUNTIME.to_string()
        } else {
            self.active_runtime.clone()
        }
    }

    async fn run(&self, tctx: TestContext) -> TestResult {
        match self.active_runtime.as_str() {
            MAC_RUNTIME => {
                let mut runner = crate::unix_runner::UnixIntegrationRunner::new(self.clone(), tctx);
                runner.run().await
            }
            // Default to the Linux container path for "linux" or unset.
            _ => {
                let mut runner = crate::runner::IntegrationRunner::new(self.clone(), tctx);
                runner.run().await
            }
        }
    }
}

impl IntegrationConfig {
    /// Replaces `{{PANORAMIC_DYNAMIC_*}}` placeholders in all assertion steps.
    pub fn resolve_dynamic_vars(&mut self, vars: &HashMap<String, String>) {
        for step in &mut self.procedure {
            step.resolve_dynamic_vars(vars);
        }
    }

    /// Returns any unresolved `{{PANORAMIC_DYNAMIC_*}}` placeholders across all assertion steps.
    pub fn unresolved_placeholders(&self) -> Vec<String> {
        self.procedure
            .iter()
            .flat_map(|s| s.unresolved_placeholders())
            .collect()
    }

    /// Count total individual assertions across all steps.
    pub fn total_assertion_count(&self) -> usize {
        self.procedure
            .iter()
            .map(|step| match step {
                AssertionStep::Single(_) | AssertionStep::Action(_) => 1,
                AssertionStep::Parallel { parallel } => parallel.len(),
            })
            .sum()
    }
}

impl CaseConfig for IntegrationConfig {
    fn base_path(&self) -> &Path {
        &self.base_path
    }

    fn set_base_path(&mut self, base_path: PathBuf) {
        self.base_path = base_path;
    }
}

/// A single variant in a `correctness_matrix` test.
///
/// Each variant expands into an independent correctness test. The variant's `env` is overlaid on
/// the environment variables of both the baseline and comparison targets in the base
/// configuration, allowing a single test directory to exercise multiple agent configurations
/// without duplicating the full config layout.
#[derive(Clone, Deserialize)]
pub struct MatrixVariant {
    /// Name suffix for this variant.
    ///
    /// The expanded test name is `{base_name}/{variant_name}`, for example,
    /// `dsd-origin-detection-matrix/unified`.
    pub name: String,

    /// Environment variables overlaid on both the baseline and comparison targets.
    ///
    /// Same mapping form as `env` on [`CorrectnessTargetConfig`]. A variant entry replaces the base
    /// configuration's value for the same variable name; base entries the variant does not name are
    /// left alone.
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: BTreeMap<String, String>,
}

/// A matrix correctness test that fans out into one independent test per variant.
///
/// This is the deserialized form of a `type: correctness_matrix` config file. It shares all
/// structural fields with a standard `correctness` config, but adds a `variants` list. At
/// discovery time each variant is expanded into a standalone [`CorrectnessConfig`] named
/// `{base_name}/{variant_name}`, which the runner treats as a fully independent test case.
#[derive(Clone, Deserialize)]
pub struct MatrixConfig {
    /// Container runtime backend to use.
    pub runtime: CorrectnessRuntime,

    /// Analysis mode to use.
    pub analysis_mode: AnalysisMode,

    /// Millstone configuration (shared across all variants).
    #[serde(default)]
    pub millstone: CorrectnessMillstoneConfig,

    /// Datadog intake configuration (shared across all variants).
    #[serde(default)]
    pub datadog_intake: CorrectnessDatadogIntakeConfig,

    /// Baseline target configuration (shared base; variant env vars are overlaid on it).
    pub baseline: CorrectnessTargetConfig,

    /// Comparison target configuration (shared base; variant env vars are overlaid on it).
    pub comparison: CorrectnessTargetConfig,

    /// When analysis mode is traces: if true, use OTLP-direct analysis (baseline is OTel-based).
    ///
    /// Propagated unchanged to every expanded [`CorrectnessConfig`].
    #[serde(default)]
    pub otlp_direct_analysis_mode: bool,

    /// When analysis mode is traces: additional span field paths to ignore when diffing baseline
    /// vs comparison.
    ///
    /// Propagated unchanged to every expanded [`CorrectnessConfig`].
    #[serde(default)]
    pub additional_span_ignore_fields: Vec<String>,

    /// Whether each expanded correctness run must capture at least one forwarded DogStatsD packet.
    #[serde(default)]
    pub require_dogstatsd_forwarded_packets: bool,

    /// Matrix variants. Each entry produces one expanded test case.
    pub variants: Vec<MatrixVariant>,

    #[serde(skip, default = "PathBuf::new")]
    base_path: PathBuf,
}

impl CaseConfig for MatrixConfig {
    fn base_path(&self) -> &Path {
        &self.base_path
    }

    fn set_base_path(&mut self, base_path: PathBuf) {
        self.base_path = base_path;
    }
}

impl MatrixConfig {
    /// Expands this matrix into one [`CorrectnessConfig`] per variant.
    ///
    /// Each expanded config is a clone of the base configuration with the variant's `env` overlaid
    /// on both the baseline and comparison targets, so a variable named by both the base config and
    /// the variant takes the variant's value.
    fn expand(self, base_name: &str) -> Vec<CorrectnessConfig> {
        self.variants
            .iter()
            .map(|variant| {
                let mut baseline = self.baseline.clone();
                baseline.env.extend(variant.env.clone());

                let mut comparison = self.comparison.clone();
                comparison.env.extend(variant.env.clone());

                CorrectnessConfig {
                    name: format!("{}/{}", base_name, variant.name),
                    runtime: self.runtime.clone(),
                    analysis_mode: self.analysis_mode.clone(),
                    millstone: CorrectnessMillstoneConfig {
                        image: self.millstone.image.clone(),
                        binary_path: self.millstone.binary_path.clone(),
                        config_path: self.resolve_path(&self.millstone.config_path),
                    },
                    datadog_intake: CorrectnessDatadogIntakeConfig {
                        image: self.datadog_intake.image.clone(),
                        binary_path: self.datadog_intake.binary_path.clone(),
                    },
                    baseline: CorrectnessTargetConfig {
                        image: baseline.image,
                        entrypoint: baseline.entrypoint,
                        command: baseline.command,
                        files: baseline.files.iter().map(|f| anchor_file_entry(f, &self)).collect(),
                        env: baseline.env,
                    },
                    comparison: CorrectnessTargetConfig {
                        image: comparison.image,
                        entrypoint: comparison.entrypoint,
                        command: comparison.command,
                        files: comparison.files.iter().map(|f| anchor_file_entry(f, &self)).collect(),
                        env: comparison.env,
                    },
                    otlp_direct_analysis_mode: self.otlp_direct_analysis_mode,
                    additional_span_ignore_fields: self.additional_span_ignore_fields.clone(),
                    require_dogstatsd_forwarded_packets: self.require_dogstatsd_forwarded_packets,
                    // Every variant comes from the matrix config's directory. Reports name it as the
                    // case each expanded test came from, and it anchors any path this expansion did
                    // not already make absolute.
                    base_path: self.base_path.clone(),
                }
            })
            .collect()
    }
}

/// Rewrites the host-path portion of a `host_path:container_path` file entry to an absolute path
/// anchored at the case directory, mirroring the anchoring that the correctness runtime performs
/// for its own `files` entries.
///
/// An entry with no `:` is returned as written, leaving the runtime to reject it.
fn anchor_file_entry(entry: &str, case: &impl CaseConfig) -> String {
    match entry.split_once(':') {
        Some((host, container)) => format!("{}:{}", case.resolve_path(host).display(), container),
        None => entry.to_string(),
    }
}

/// Deserializes a map of environment variables, requiring every value to be a YAML string.
///
/// A bare `true` or `8125` is a YAML boolean or number rather than the text a process receives, so a
/// case that writes one is rejected by name instead of being handed a value it did not ask for. YAML
/// scalars are otherwise loosely typed enough that the same file would load differently depending on
/// whether a field happens to be typed as a `String`.
///
/// # Errors
///
/// Returns an error naming the first variable whose value is not a YAML string.
pub(crate) fn deserialize_env_map<'de, D, M>(deserializer: D) -> Result<M, D::Error>
where
    D: Deserializer<'de>,
    M: FromIterator<(String, String)>,
{
    BTreeMap::<String, serde_yaml::Value>::deserialize(deserializer)?
        .into_iter()
        .map(|(name, value)| match value {
            serde_yaml::Value::String(value) => Ok((name, value)),
            _ => Err(D::Error::custom(format!(
                "environment variable '{}' must be a YAML string: quote any value that would otherwise \
                 parse as a boolean or a number (\"true\", \"8125\")",
                name
            ))),
        })
        .collect()
}

/// A test case configuration read from a case directory's `config.yaml`.
///
/// The directory holding that file anchors every relative path the case declares, so [`load_case`]
/// hands it back to the configuration once deserialization succeeds. Implementors carry it in a
/// `#[serde(skip)]` field, since it comes from where the file was found rather than from the file.
pub trait CaseConfig {
    /// Returns the directory holding the `config.yaml` this case was loaded from.
    fn base_path(&self) -> &Path;

    /// Records the directory holding the `config.yaml` this case was loaded from.
    fn set_base_path(&mut self, base_path: PathBuf);

    /// Resolves a path declared by the case against the case directory.
    ///
    /// An absolute path is returned unchanged. Symlinks in the result are left as they are, since a
    /// case may name a path that nothing has created yet.
    fn resolve_path<P: AsRef<Path>>(&self, path: P) -> PathBuf {
        let path = path.as_ref();
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_path().join(path)
        }
    }
}

/// Prefix of the environment variables that override values from a case's `config.yaml`.
///
/// A variable named `PANORAMIC_<SECTION>__<FIELD>` replaces the matching field within that
/// section, creating the section if the case does not declare it. CI correctness jobs use these to
/// run a case with the images built for the commit, such as `PANORAMIC_BASELINE__IMAGE`. A variable
/// without `__` names a top-level field; names the configuration does not declare are ignored,
/// which is how unrelated `PANORAMIC_*` variables (command-line arguments, `PANORAMIC_DYNAMIC_*`)
/// pass through with no effect.
const ENV_OVERRIDE_PREFIX: &str = "PANORAMIC_";

/// Loads one test case configuration from its `config.yaml`, anchored at that file's directory.
///
/// Every case type loads the same way: plain YAML deserialization, then the case directory recorded
/// on the result. YAML typing is what the case gets: an unquoted `true` is a boolean and does not
/// stand in for the string `"true"`. Values from `PANORAMIC_<SECTION>__<FIELD>` environment
/// variables replace the matching fields in the document. Boolean fields accept only `true` or
/// `false`; string fields keep the environment value unchanged.
///
/// # Errors
///
/// Returns an error if `config_path` cannot be resolved or read, an override has an invalid
/// boolean value, or the result does not deserialize into `T`.
pub fn load_case<T, P>(config_path: P) -> Result<T, GenericError>
where
    T: CaseConfig + DeserializeOwned,
    P: AsRef<Path>,
{
    let config_path = config_path.as_ref();

    // Canonicalizing the file rather than its parent directory keeps a bare `config.yaml` working:
    // `Path::parent` would hand back an empty path, which does not canonicalize.
    let config_path = config_path.canonicalize().error_context(format!(
        "Failed to resolve configuration file: {}",
        config_path.display()
    ))?;

    let content = std::fs::read_to_string(&config_path)
        .error_context(format!("Failed to read configuration file: {}", config_path.display()))?;

    let mut document: serde_yaml::Value = serde_yaml::from_str(&content)
        .error_context(format!("Failed to parse configuration file: {}", config_path.display()))?;

    apply_env_overrides(&mut document, std::env::vars())?;

    let mut case: T = serde_yaml::from_value(document).error_context(format!(
        "Failed to deserialize configuration file: {}",
        config_path.display()
    ))?;

    let base_path = config_path
        .parent()
        .expect("Canonicalized file path always has a parent.")
        .to_path_buf();
    case.set_base_path(base_path);

    Ok(case)
}

/// Applies case-configuration overrides from `PANORAMIC_*` variables to a parsed document.
///
/// Takes the variables to apply rather than reading the environment itself, so a caller can pass
/// anything, including nothing.
///
/// # Errors
///
/// Returns an error naming any boolean override whose value is not `true` or `false`.
fn apply_env_overrides(
    document: &mut serde_yaml::Value, vars: impl Iterator<Item = (String, String)>,
) -> Result<(), GenericError> {
    for (name, value) in vars {
        let Some(rest) = name.strip_prefix(ENV_OVERRIDE_PREFIX) else {
            continue;
        };

        // The name below the prefix, split into field path segments: `MILLSTONE__IMAGE` becomes
        // `millstone`, `image`. An empty segment (a leading or doubled `__`) matches nothing in a
        // case, so the variable is ignored rather than creating a field nothing reads.
        let segments: Vec<String> = rest.split("__").map(str::to_ascii_lowercase).collect();
        if segments.iter().any(String::is_empty) {
            continue;
        }

        let value = match segments.join(".").as_str() {
            "intake.enabled"
            | "container.host_cgroup_namespace"
            | "otlp_direct_analysis_mode"
            | "require_dogstatsd_forwarded_packets" => match value.as_str() {
                "true" => serde_yaml::Value::Bool(true),
                "false" => serde_yaml::Value::Bool(false),
                _ => {
                    return Err(generic_error!(
                        "Invalid value for {}: expected 'true' or 'false', got '{}'",
                        name,
                        value
                    ))
                }
            },
            _ => serde_yaml::Value::String(value),
        };

        set_override(document, &segments, value);
    }
    Ok(())
}

/// Replaces the value at a field path in a document, creating the parent sections it lacks.
///
/// Skips the override when the path runs through a value that is not a mapping: the document the
/// case actually declared wins over a variable trying to traverse it.
fn set_override(document: &mut serde_yaml::Value, segments: &[String], value: serde_yaml::Value) {
    let Some((last, parents)) = segments.split_last() else {
        return;
    };

    let mut current = document;
    for segment in parents {
        let Some(map) = current.as_mapping_mut() else { return };
        let key = serde_yaml::Value::String(segment.clone());
        if !map.contains_key(&key) {
            map.insert(key.clone(), serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        }
        current = map.get_mut(&key).expect("key was just inserted");
    }

    if let Some(map) = current.as_mapping_mut() {
        map.insert(serde_yaml::Value::String(last.clone()), value);
    }
}

/// Discover all test cases across one or more directories.
///
/// Each `config.yaml` found in a direct subdirectory must have a top-level `type` field set to
/// `"integration"`, `"correctness"`, or `"correctness_matrix"`. Files with a missing or unknown
/// `type` cause a panic. Multiple test types may coexist freely within the same directory.
///
/// `integration_runtime` scopes integration-test discovery to a single runtime: an integration
/// test is included if and only if its `runtimes:` list contains this value. Correctness tests
/// are unaffected; they always discover.
pub fn discover_tests(dirs: &[PathBuf], integration_runtime: &str) -> Result<Vec<Box<dyn Test>>, GenericError> {
    let mut tests: Vec<Box<dyn Test>> = Vec::new();

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
                    match try_load_test(&config_path, &path, integration_runtime) {
                        Ok(loaded) => tests.extend(loaded),
                        Err(e) => {
                            // Previously we had a warning here that cannot be seen in TUI-mode. It is better to fail
                            // loudly and fast when we have a bad test configuration than to falsely believe our test is
                            // working when we see that all tests passed.
                            panic!("Failed to load test case, bad configuration: {e:?}");
                        }
                    }
                }
            }
        }
    }

    // Sort by name for deterministic ordering.
    tests.sort_by_key(|a| a.name());

    Ok(tests)
}

/// Load one or more test cases from a config file, dispatching on the top-level `type` field.
///
/// Returns a `Vec` because a `correctness_matrix` config expands into multiple independent test
/// cases—one per variant. `integration` configs produce zero or one test case depending on
/// whether the active `integration_runtime` is in the test's `runtimes:` list. `correctness`
/// configs produce exactly one test case.
fn try_load_test(
    config_path: &Path, dir_path: &Path, integration_runtime: &str,
) -> Result<Vec<Box<dyn Test>>, GenericError> {
    let content = std::fs::read_to_string(config_path)
        .error_context(format!("Failed to read config file: {}", config_path.display()))?;

    let peek: serde_yaml::Value = serde_yaml::from_str(&content).error_context(format!(
        "Failed to parse config file as YAML: {}",
        config_path.display()
    ))?;

    let test_type = peek.get("type").and_then(|v| v.as_str()).ok_or_else(|| {
        generic_error!("Missing required 'type' field (expected 'integration', 'correctness', or 'correctness_matrix')")
    })?;

    match test_type {
        "integration" => {
            let config: IntegrationConfig = load_case(config_path)?;
            if config.runtimes.is_empty() {
                return Err(generic_error!(
                    "integration test '{}' has empty runtimes list",
                    config.name
                ));
            }
            // Validate every declared runtime up front so a typo in any list surfaces at discovery
            // time, even on hosts that wouldn't actually run that runtime.
            for runtime in &config.runtimes {
                if runtime != LINUX_RUNTIME && runtime != MAC_RUNTIME && runtime != WINDOWS_RUNTIME {
                    return Err(generic_error!(
                        "integration test '{}' declares unknown runtime '{}' (expected '{}', '{}', or '{}')",
                        config.name,
                        runtime,
                        LINUX_RUNTIME,
                        MAC_RUNTIME,
                        WINDOWS_RUNTIME
                    ));
                }
            }
            // Scope to the active runtime: skip tests that don't opt in to it.
            if !config.runtimes.iter().any(|r| r == integration_runtime) {
                return Ok(Vec::new());
            }
            let mut variant = config.clone();
            variant.active_runtime = integration_runtime.to_string();
            Ok(vec![Box::new(variant)])
        }
        "correctness" => {
            let mut config: CorrectnessConfig = load_case(config_path)?;
            config.name = dir_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            Ok(vec![Box::new(config)])
        }
        "correctness_matrix" => {
            let base_name = dir_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            let matrix: MatrixConfig = load_case(config_path)?;
            if matrix.variants.is_empty() {
                return Err(generic_error!(
                    "correctness_matrix '{}' has no variants defined",
                    base_name
                ));
            }
            Ok(matrix
                .expand(&base_name)
                .into_iter()
                .map(|c| Box::new(c) as Box<dyn Test>)
                .collect())
        }
        other => Err(generic_error!(
            "Unknown test type '{}' (expected 'integration', 'correctness', or 'correctness_matrix')",
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
    fn parse_duration_handles_units_and_combinations() {
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn parse_duration_treats_a_trailing_unitless_number_as_seconds() {
        // Documented no-unit fallback: a trailing number with no unit is interpreted as seconds, both on
        // its own and after a unit-qualified component.
        assert_eq!(parse_duration("42").unwrap(), Duration::from_secs(42));
        assert_eq!(parse_duration("1m30").unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn parse_duration_rejects_a_zero_duration() {
        // Documented zero-duration rejection: a total of zero is an error regardless of how it is spelled.
        let unitless = parse_duration("0").expect_err("a bare zero should be rejected");
        assert!(unitless.contains("greater than zero"), "unexpected error: {unitless}");

        let with_unit = parse_duration("0s").expect_err("an explicit zero-second duration should be rejected");
        assert!(with_unit.contains("greater than zero"), "unexpected error: {with_unit}");
    }

    #[test]
    fn parse_port_spec_parses_valid_specs_and_rejects_invalid_ones() {
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
    fn parse_file_spec_splits_on_first_colon() {
        let (host, container) = parse_file_spec("./config.yaml:/etc/config.yaml").unwrap();
        assert_eq!(host, "./config.yaml");
        assert_eq!(container, "/etc/config.yaml");

        assert!(parse_file_spec("nocolon").is_err());
    }

    #[test]
    fn target_exec_action_deserializes_command() {
        let action: ActionConfig = serde_yaml::from_str(
            r#"
action: target_exec
command: ["pwsh", "-File", "C:\\test\\send.ps1"]
timeout: 12s
"#,
        )
        .unwrap();

        let ActionConfig::TargetExec { command, timeout } = action else {
            panic!("expected target_exec action");
        };
        assert_eq!(command, vec!["pwsh", "-File", "C:\\test\\send.ps1"]);
        assert_eq!(timeout.0, Duration::from_secs(12));
    }

    #[test]
    fn dogstatsd_replay_action_deserializes_capture_and_stats_configuration() {
        let action: ActionConfig = serde_yaml::from_str(
            r#"
action: dogstatsd_replay
sender: ["python3", "/tmp/send.py"]
capture_duration: 2s
stats_duration_secs: 3
expected_metrics: ["replay.one", "replay.two"]
timeout: 30s
"#,
        )
        .unwrap();

        let ActionConfig::DogstatsdReplay {
            sender,
            capture_duration,
            stats_duration_secs,
            expected_metrics,
            timeout,
        } = action
        else {
            panic!("expected dogstatsd_replay action");
        };
        assert_eq!(sender, vec!["python3", "/tmp/send.py"]);
        assert_eq!(capture_duration.0, Duration::from_secs(2));
        assert_eq!(stats_duration_secs, 3);
        assert_eq!(expected_metrics, vec!["replay.one", "replay.two"]);
        assert_eq!(timeout.0, Duration::from_secs(30));
    }

    #[test]
    fn windows_runtime_is_valid_for_integration_discovery() {
        let base_dir = create_test_case_dir(
            "windows-smoke",
            r#"
type: integration
name: windows-smoke
timeout: 10s
runtimes: [windows]
procedure: []
"#,
        );

        let tests = discover_tests(&[base_dir.path().to_path_buf()], "windows").unwrap();

        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].name(), "windows-smoke");
        assert_eq!(tests[0].runtime(), "windows");
    }

    #[test]
    fn windows_runtime_reports_harness_owned_container_image() {
        let base_dir = create_test_case_dir(
            "windows-smoke",
            r#"
type: integration
name: windows-smoke
timeout: 10s
runtimes: [windows]
procedure: []
"#,
        );

        let tests = discover_tests(&[base_dir.path().to_path_buf()], "windows").unwrap();
        let images = tests[0].images();

        assert_eq!(images.get("container"), Some(&DEFAULT_WINDOWS_TARGET_IMAGE.to_string()));
    }

    #[test]
    fn linux_runtime_reports_harness_owned_container_image() {
        let base_dir = create_test_case_dir(
            "linux-smoke",
            r#"
type: integration
name: linux-smoke
timeout: 10s
runtimes: [linux]
procedure: []
"#,
        );

        let tests = discover_tests(&[base_dir.path().to_path_buf()], "linux").unwrap();
        let images = tests[0].images();

        assert_eq!(images.get("container"), Some(&DEFAULT_LINUX_TARGET_IMAGE.to_string()));
    }

    #[test]
    fn matrix_variants_report_the_case_directory_they_expanded_from() {
        let base_dir = create_test_case_dir(
            "dsd-matrix",
            r#"
type: correctness_matrix
runtime: docker
analysis_mode: metrics
baseline:
  image: saluki-images/datadog-agent:testing-release
comparison:
  image: saluki-images/datadog-agent:testing-release
variants:
  - name: first
    env:
      DD_EXAMPLE: "1"
  - name: second
    env:
      DD_EXAMPLE: "2"
"#,
        );

        let tests = discover_tests(&[base_dir.path().to_path_buf()], "linux").unwrap();

        assert_eq!(tests.len(), 2);
        // Canonicalized, because the loader canonicalizes the config path it derives this from.
        let case_dir = base_dir.path().join("dsd-matrix").canonicalize().unwrap();
        for test in &tests {
            assert_eq!(test.case_path(), case_dir, "case path for '{}'", test.name());
        }
    }

    #[test]
    fn correctness_target_env_becomes_key_value_assignments_ordered_by_name() {
        let base_dir = create_test_case_dir(
            "dsd-env",
            r#"
type: correctness
runtime: docker
analysis_mode: metrics
baseline:
  image: saluki-images/datadog-agent:testing-release
  env:
    DD_TAGS: "a=1,b=2"
    DD_API_KEY: correctness-test
comparison:
  image: saluki-images/datadog-agent:testing-release
  env:
    DD_API_KEY: correctness-test
    DD_DATA_PLANE_ENABLED: "true"
"#,
        );
        let config_path = base_dir.path().join("dsd-env").join("config.yaml");

        let config: CorrectnessConfig = load_case(&config_path).expect("case should parse");

        // What the Docker adapter hands to Airlock: one assignment per variable, ordered by name,
        // with a value containing '=' left intact.
        assert_eq!(
            config.baseline.env_assignments(),
            vec!["DD_API_KEY=correctness-test".to_string(), "DD_TAGS=a=1,b=2".to_string()]
        );

        // Each target owns its own environment: the comparison-only variable does not leak into the
        // baseline.
        assert_eq!(
            config.comparison.env_assignments(),
            vec![
                "DD_API_KEY=correctness-test".to_string(),
                "DD_DATA_PLANE_ENABLED=true".to_string()
            ]
        );
    }

    #[test]
    fn correctness_target_env_rejects_an_unquoted_boolean_value() {
        // An unquoted `true` is a YAML boolean, not the string the process would receive, so the
        // case must fail to load rather than guess at a value.
        let base_dir = create_test_case_dir(
            "dsd-bare-bool",
            r#"
type: correctness
runtime: docker
analysis_mode: metrics
baseline:
  image: saluki-images/datadog-agent:testing-release
comparison:
  image: saluki-images/datadog-agent:testing-release
  env:
    DD_DATA_PLANE_ENABLED: true
"#,
        );
        let config_path = base_dir.path().join("dsd-bare-bool").join("config.yaml");

        let error = match load_case::<CorrectnessConfig, _>(&config_path) {
            Ok(_) => panic!("an unquoted boolean env value should be rejected"),
            Err(e) => format!("{e:?}"),
        };

        assert!(error.contains("DD_DATA_PLANE_ENABLED"), "unexpected error: {error}");
    }

    #[test]
    fn integration_case_env_rejects_an_unquoted_boolean_value() {
        // Both suites load through one loader, so an integration case is held to the same rule as a
        // correctness case: the value a process receives is whatever the YAML string says.
        let yaml = r#"
type: integration
name: bare-bool-case
timeout: 60s
env:
  DD_DATA_PLANE_ENABLED: true
procedure:
  - assertion: process_stable_for
    duration: 5s
"#;

        let error = match serde_yaml::from_str::<IntegrationConfig>(yaml) {
            Ok(_) => panic!("an unquoted boolean env value should be rejected"),
            Err(e) => format!("{e:?}"),
        };

        assert!(error.contains("DD_DATA_PLANE_ENABLED"), "unexpected error: {error}");
    }

    #[test]
    fn a_case_relative_path_anchors_at_the_case_directory() {
        let base_dir = create_test_case_dir(
            "dsd-paths",
            r#"
type: correctness
runtime: docker
analysis_mode: metrics
millstone:
  config_path: millstone.yaml
baseline:
  image: saluki-images/datadog-agent:testing-release
comparison:
  image: saluki-images/datadog-agent:testing-release
"#,
        );
        let config_path = base_dir.path().join("dsd-paths").join("config.yaml");

        let config: CorrectnessConfig = load_case(&config_path).expect("case should parse");

        // Canonicalized, because the loader canonicalizes the config path it derives the case
        // directory from. On macOS that turns the temp directory's `/tmp` into `/private/tmp`.
        let case_dir = base_dir.path().join("dsd-paths").canonicalize().unwrap();
        assert_eq!(config.millstone_config().config_path, case_dir.join("millstone.yaml"));

        // An absolute path is the case's own choice and passes through untouched. Pick one that is
        // absolute on the platform running the test: a Unix-style root is not absolute on Windows.
        let absolute_path = if cfg!(windows) {
            PathBuf::from(r"C:\etc\datadog-agent\datadog.yaml")
        } else {
            PathBuf::from("/etc/datadog-agent/datadog.yaml")
        };
        assert_eq!(config.resolve_path(&absolute_path), absolute_path);
    }

    #[test]
    fn env_overrides_replace_a_declared_field_and_create_an_undeclared_section() {
        let mut document = serde_yaml::from_str(
            r#"
millstone:
  image: saluki-images/correctness-tools:latest
"#,
        )
        .expect("document should parse");

        apply_env_overrides(
            &mut document,
            [
                (
                    "PANORAMIC_MILLSTONE__IMAGE".to_string(),
                    "registry.ddbuild.io/saluki/correctness-tools:abc123".to_string(),
                ),
                (
                    "PANORAMIC_DATADOG_INTAKE__IMAGE".to_string(),
                    "registry.ddbuild.io/saluki/correctness-tools:abc123".to_string(),
                ),
                // No `__`, so this names a top-level field no case declares. It lands in the
                // document and is ignored at deserialization, like the CLI's own variables are.
                ("PANORAMIC_LOG_DIR".to_string(), "somewhere".to_string()),
            ]
            .into_iter(),
        )
        .expect("string overrides should parse");

        // The declared field takes the override, and the missing section is created by it.
        #[derive(Deserialize)]
        struct MillstoneAndIntake {
            millstone: CorrectnessMillstoneConfig,
            datadog_intake: CorrectnessDatadogIntakeConfig,
        }
        let config: MillstoneAndIntake = serde_yaml::from_value(document).expect("config should deserialize");
        assert_eq!(
            config.millstone.image,
            "registry.ddbuild.io/saluki/correctness-tools:abc123"
        );
        assert_eq!(
            config.datadog_intake.image,
            "registry.ddbuild.io/saluki/correctness-tools:abc123"
        );
    }

    #[test]
    fn env_overrides_parse_boolean_fields_even_when_absent_from_the_case() {
        let mut integration = serde_yaml::from_str(
            r#"
name: original
timeout: 10s
container:
  host_cgroup_namespace: true
procedure: []
"#,
        )
        .unwrap();
        apply_env_overrides(
            &mut integration,
            [
                ("PANORAMIC_INTAKE__ENABLED".into(), "true".into()),
                ("PANORAMIC_CONTAINER__HOST_CGROUP_NAMESPACE".into(), "false".into()),
                ("PANORAMIC_NAME".into(), "true".into()),
            ]
            .into_iter(),
        )
        .unwrap();
        let integration: IntegrationConfig = serde_yaml::from_value(integration).unwrap();
        assert!(integration.intake.enabled);
        assert!(!integration.container.host_cgroup_namespace);
        assert_eq!(integration.name, "true");

        let mut correctness = serde_yaml::from_str(
            r#"
runtime: docker
analysis_mode: metrics
baseline:
  image: baseline
comparison:
  image: comparison
"#,
        )
        .unwrap();
        apply_env_overrides(
            &mut correctness,
            [
                ("PANORAMIC_OTLP_DIRECT_ANALYSIS_MODE".into(), "true".into()),
                ("PANORAMIC_REQUIRE_DOGSTATSD_FORWARDED_PACKETS".into(), "true".into()),
                ("PANORAMIC_BASELINE__IMAGE".into(), "8125".into()),
            ]
            .into_iter(),
        )
        .unwrap();
        let correctness: CorrectnessConfig = serde_yaml::from_value(correctness).unwrap();
        assert!(correctness.otlp_direct_analysis_mode);
        assert!(correctness.require_dogstatsd_forwarded_packets);
        assert_eq!(correctness.baseline.image, "8125");
    }

    #[test]
    fn env_overrides_reject_invalid_boolean_values() {
        for name in [
            "PANORAMIC_INTAKE__ENABLED",
            "PANORAMIC_CONTAINER__HOST_CGROUP_NAMESPACE",
            "PANORAMIC_OTLP_DIRECT_ANALYSIS_MODE",
            "PANORAMIC_REQUIRE_DOGSTATSD_FORWARDED_PACKETS",
        ] {
            for value in ["tru", "yes", "1"] {
                let mut document = serde_yaml::from_str("{}").unwrap();
                let error = apply_env_overrides(&mut document, [(name.into(), value.into())].into_iter())
                    .expect_err("invalid boolean override should fail");
                let error = format!("{error:?}");
                assert!(error.contains(name), "unexpected error: {error}");
                assert!(error.contains(value), "unexpected error: {error}");
                assert!(error.contains("true' or 'false"), "unexpected error: {error}");
            }
        }
    }

    #[test]
    fn matrix_variant_env_overlays_both_targets_and_wins_on_a_shared_variable() {
        let base_dir = create_test_case_dir(
            "dsd-matrix",
            r#"
type: correctness_matrix
runtime: docker
analysis_mode: metrics
baseline:
  image: saluki-images/datadog-agent:testing-release
  env:
    DD_API_KEY: correctness-test
    DD_DOGSTATSD_TAG_CARDINALITY: low
comparison:
  image: saluki-images/datadog-agent:testing-release
  env:
    DD_API_KEY: correctness-test
    DD_DOGSTATSD_TAG_CARDINALITY: low
    DD_DATA_PLANE_ENABLED: "true"
variants:
  - name: high-cardinality
    env:
      DD_DOGSTATSD_TAG_CARDINALITY: high
      DD_ORIGIN_DETECTION_UNIFIED: "true"
"#,
        );
        let config_path = base_dir.path().join("dsd-matrix").join("config.yaml");

        let matrix: MatrixConfig = load_case(&config_path).expect("matrix should parse");
        let expanded = matrix.expand("dsd-matrix");

        assert_eq!(expanded.len(), 1);
        let variant = &expanded[0];
        for (side, target) in [("baseline", &variant.baseline), ("comparison", &variant.comparison)] {
            // The variant wins on the shared variable, adds its own, and leaves the rest of the base
            // environment alone.
            assert_eq!(
                target.env.get("DD_DOGSTATSD_TAG_CARDINALITY").map(String::as_str),
                Some("high"),
                "{side} should take the variant's cardinality"
            );
            assert_eq!(
                target.env.get("DD_ORIGIN_DETECTION_UNIFIED").map(String::as_str),
                Some("true"),
                "{side} should gain the variant's own variable"
            );
            assert_eq!(
                target.env.get("DD_API_KEY").map(String::as_str),
                Some("correctness-test"),
                "{side} should keep base variables the variant does not name"
            );
        }

        // The variant overlay does not merge the two targets: comparison-only settings stay there.
        assert_eq!(
            variant.comparison.env.get("DD_DATA_PLANE_ENABLED").map(String::as_str),
            Some("true")
        );
        assert!(!variant.baseline.env.contains_key("DD_DATA_PLANE_ENABLED"));
    }

    fn dynamic_vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn core_agent_config_set_resolves_placeholders_in_all_string_fields() {
        let vars = dynamic_vars(&[("KEY", "resolved_key"), ("VAL", "resolved_val"), ("EP", "resolved_ep")]);
        let mut action = ActionConfig::CoreAgentConfigSet {
            key: "prefix.{{PANORAMIC_DYNAMIC_KEY}}".to_string(),
            value: Value::String("{{PANORAMIC_DYNAMIC_VAL}}".to_string()),
            endpoint: "http://agent/{{PANORAMIC_DYNAMIC_EP}}".to_string(),
            timeout: HumanDuration(Duration::from_secs(1)),
        };

        assert!(
            !action.unresolved_placeholders().is_empty(),
            "placeholders should be detected before resolution"
        );
        action.resolve_dynamic_vars(&vars);

        let ActionConfig::CoreAgentConfigSet {
            key, value, endpoint, ..
        } = action
        else {
            panic!("expected core_agent_config_set action");
        };
        assert_eq!(key, "prefix.resolved_key");
        assert_eq!(value, Value::String("resolved_val".to_string()));
        assert_eq!(endpoint, "http://agent/resolved_ep");
    }

    #[test]
    fn integration_case_parses_intake_send_and_metric_steps() {
        let yaml = r#"
type: integration
name: intake-case
timeout: 60s
intake:
  enabled: true
procedure:
  - action: dogstatsd_send
    payload: "example.counter:3|c"
    port: 58125
  - assertion: intake_has_metric
    name: "example.counter"
    metric_type: count
    value: 3
    tags: ["source:integration-test"]
    timeout: 30s
"#;

        let config: IntegrationConfig = serde_yaml::from_str(yaml).expect("case should parse");

        assert!(config.intake.enabled);
        let AssertionStep::Action(ActionConfig::DogstatsdSend { payload, port, .. }) = &config.procedure[0] else {
            panic!("first step should parse as a dogstatsd_send action");
        };
        assert_eq!(payload, "example.counter:3|c");
        assert_eq!(*port, 58125);
        let AssertionStep::Single(AssertionConfig::IntakeHasMetric {
            name,
            metric_type,
            value,
            tags,
            ..
        }) = &config.procedure[1]
        else {
            panic!("second step should parse as an intake_has_metric assertion");
        };
        assert_eq!(name, "example.counter");
        assert_eq!(*metric_type, Some(MetricTypeMatcher::Count));
        assert_eq!(*value, Some(3.0));
        assert_eq!(tags, &["source:integration-test".to_string()]);
    }

    #[test]
    fn integration_case_without_intake_block_leaves_the_sidecar_disabled() {
        let yaml = r#"
type: integration
name: no-intake-case
timeout: 60s
procedure:
  - assertion: process_stable_for
    duration: 5s
"#;

        let config: IntegrationConfig = serde_yaml::from_str(yaml).expect("case should parse");

        assert!(!config.intake.enabled);
    }

    #[test]
    fn target_exec_resolves_placeholders_in_each_command_argument() {
        let vars = dynamic_vars(&[("IP", "10.0.0.5")]);
        let mut action = ActionConfig::TargetExec {
            command: vec!["ping".to_string(), "{{PANORAMIC_DYNAMIC_IP}}".to_string()],
            timeout: HumanDuration(Duration::from_secs(1)),
        };

        action.resolve_dynamic_vars(&vars);

        let ActionConfig::TargetExec { command, .. } = action else {
            panic!("expected target_exec action");
        };
        assert_eq!(command, vec!["ping".to_string(), "10.0.0.5".to_string()]);
    }

    #[test]
    fn adp_cli_resolves_placeholders_in_each_argument() {
        let vars = dynamic_vars(&[("LEVEL", "INFO")]);
        let mut action = ActionConfig::AdpCli {
            args: vec![
                "debug".to_string(),
                "set-metric-level".to_string(),
                "{{PANORAMIC_DYNAMIC_LEVEL}}".to_string(),
            ],
            timeout: HumanDuration(Duration::from_secs(1)),
        };

        action.resolve_dynamic_vars(&vars);
        assert!(action.unresolved_placeholders().is_empty());

        let ActionConfig::AdpCli { args, .. } = action else {
            panic!("expected adp_cli action");
        };
        assert_eq!(args, vec!["debug", "set-metric-level", "INFO"]);
    }

    #[test]
    fn core_agent_cli_resolves_placeholders_in_arguments_and_output_matcher() {
        let vars = dynamic_vars(&[("COMMAND", "status"), ("MATCH", "Agent Version")]);
        let mut action = ActionConfig::CoreAgentCli {
            args: vec!["{{PANORAMIC_DYNAMIC_COMMAND}}".to_string()],
            output_contains: Some("Built Against {{PANORAMIC_DYNAMIC_MATCH}}".to_string()),
            timeout: HumanDuration(Duration::from_secs(1)),
        };

        assert!(!action.unresolved_placeholders().is_empty());
        action.resolve_dynamic_vars(&vars);
        assert!(action.unresolved_placeholders().is_empty());

        let ActionConfig::CoreAgentCli {
            args, output_contains, ..
        } = action
        else {
            panic!("expected core_agent_cli action");
        };
        assert_eq!(args, vec!["status"]);
        assert_eq!(output_contains.as_deref(), Some("Built Against Agent Version"));
    }

    #[test]
    fn assertion_variants_resolve_placeholders_in_their_documented_fields() {
        let vars = dynamic_vars(&[("IP", "10.0.0.5"), ("PORT", "8125")]);

        let mut log = AssertionConfig::LogContains {
            pattern: "listen:{{PANORAMIC_DYNAMIC_IP}}".to_string(),
            regex: false,
            timeout: HumanDuration(Duration::from_secs(1)),
            stream: LogStream::default(),
        };
        log.resolve_dynamic_vars(&vars);
        let AssertionConfig::LogContains { pattern, .. } = &log else {
            panic!("expected log_contains");
        };
        assert_eq!(pattern, "listen:10.0.0.5");
        assert!(log.unresolved_placeholders().is_empty());

        let mut http = AssertionConfig::HttpCheck {
            endpoint: "http://{{PANORAMIC_DYNAMIC_IP}}:{{PANORAMIC_DYNAMIC_PORT}}/health".to_string(),
            status: HttpStatusMatcher::Equal(200),
            insecure_skip_verify: false,
            timeout: HumanDuration(Duration::from_secs(1)),
        };
        http.resolve_dynamic_vars(&vars);
        let AssertionConfig::HttpCheck { endpoint, .. } = &http else {
            panic!("expected http_check");
        };
        assert_eq!(endpoint, "http://10.0.0.5:8125/health");

        let mut file = AssertionConfig::FileContains {
            path: "/run/{{PANORAMIC_DYNAMIC_IP}}.pid".to_string(),
            pattern: Some("addr={{PANORAMIC_DYNAMIC_IP}}".to_string()),
            regex: false,
            timeout: HumanDuration(Duration::from_secs(1)),
        };
        file.resolve_dynamic_vars(&vars);
        let AssertionConfig::FileContains { path, pattern, .. } = &file else {
            panic!("expected file_contains");
        };
        assert_eq!(path, "/run/10.0.0.5.pid");
        assert_eq!(pattern.as_deref(), Some("addr=10.0.0.5"));
    }

    #[test]
    fn unresolved_placeholders_reports_a_reference_with_no_matching_variable() {
        // A variant that references a variable that was never provided must still report the leftover
        // placeholder — this is how the runner fails a test that used an undefined dynamic variable.
        let mut assertion = AssertionConfig::LogContains {
            pattern: "value={{PANORAMIC_DYNAMIC_MISSING}}".to_string(),
            regex: false,
            timeout: HumanDuration(Duration::from_secs(1)),
            stream: LogStream::default(),
        };

        assertion.resolve_dynamic_vars(&dynamic_vars(&[("PRESENT", "x")]));

        assert_eq!(
            assertion.unresolved_placeholders(),
            vec!["{{PANORAMIC_DYNAMIC_MISSING}}".to_string()]
        );
    }

    struct TestCaseDir {
        path: PathBuf,
    }

    impl TestCaseDir {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestCaseDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn create_test_case_dir(case_name: &str, config: &str) -> TestCaseDir {
        let unique = format!(
            "panoramic-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let base_dir = std::env::temp_dir().join(unique);
        let case_dir = base_dir.join(case_name);
        std::fs::create_dir_all(&case_dir).unwrap();
        std::fs::write(case_dir.join("config.yaml"), config).unwrap();

        TestCaseDir { path: base_dir }
    }
}
