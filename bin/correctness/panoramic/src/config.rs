use std::collections::{BTreeMap, BTreeSet};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::{
    de::{DeserializeOwned, Error as _},
    Deserialize, Deserializer,
};
use serde_json::Value;

use crate::core::participant::Os;
use crate::correctness::{case::CorrectnessTestCase, config::Config as CorrectnessConfig};
use crate::image_override::ImageOverrides;
use crate::integration::{IntegrationSettings, IntegrationTestCase};
use crate::participant_case::ParticipantCase;
use crate::steps::intake::TagMatch;
use crate::test::Test;

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

    /// Canonical configuration file path, recorded by the loader.
    #[serde(skip)]
    pub(crate) loaded_from: PathBuf,
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

/// Runtime identifier for cases that run participants in pods, which need a kind cluster.
pub const KIND_RUNTIME: &str = "kubernetes_in_docker";

/// The runtime name of the subject OS `os`.
pub(crate) fn runtime_of(os: Os) -> &'static str {
    match os {
        Os::Linux => LINUX_RUNTIME,
        Os::Macos => MAC_RUNTIME,
        Os::Windows => WINDOWS_RUNTIME,
    }
}

/// The subject OS a runtime name selects, or `None` for a name that is not a runtime.
pub(crate) fn os_of_runtime(runtime: &str) -> Option<Os> {
    [Os::Linux, Os::Macos, Os::Windows]
        .into_iter()
        .find(|&os| runtime_of(os) == runtime)
}

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

/// Container name the integration-test target carries in [`Test::images`] and image overrides.
pub(crate) const TARGET_IMAGE_NAME: &str = "container";

/// Container name the intake sidecar carries in [`Test::images`] and image overrides.
pub(crate) const INTAKE_IMAGE_NAME: &str = "intake";

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

impl ActionConfig {
    /// Returns `timeout`, or the combined command and wait budget for `dogstatsd_replay`.
    pub(crate) fn budget(&self) -> Duration {
        match self {
            // Capture and sender each get `timeout`, followed by the capture wait. The retry loop gets another
            // `timeout`, but checks its deadline before sleeping. Its final iteration can add one sleep interval
            // and one `timeout` for the concurrent stats and replay commands.
            Self::DogstatsdReplay {
                capture_duration,
                timeout,
                ..
            } => timeout
                .0
                .saturating_mul(4)
                .saturating_add(capture_duration.0)
                .saturating_add(Duration::from_millis(50)),
            Self::CoreAgentConfigSet { timeout, .. }
            | Self::AdpCli { timeout, .. }
            | Self::CoreAgentCli { timeout, .. }
            | Self::DogstatsdSend { timeout, .. }
            | Self::TargetExec { timeout, .. } => timeout.0,
        }
    }
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

    /// Probe an HTTP/HTTPS endpoint and assert on the response status code, and optionally its body.
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
        /// Optional matcher applied to the response body. If omitted, the body is not checked.
        #[serde(default)]
        body: Option<HttpBodyMatcher>,
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
        /// Tags compared against the matching metric's tags, as `tags_match` directs.
        #[serde(default)]
        tags: Vec<String>,
        /// How `tags` must relate to the matching metric's tags. Defaults to `subset`.
        #[serde(default)]
        tags_match: TagMatch,
        /// Timeout for waiting for the metric to arrive.
        timeout: HumanDuration,
    },
}

impl AssertionConfig {
    /// Returns `duration` or `during` for windowed checks, otherwise `timeout`.
    pub(crate) fn budget(&self) -> Duration {
        match self {
            Self::ProcessStableFor { duration } => duration.0,
            Self::LogNotContains { during, .. } => during.0,
            Self::AdpExitsWith { timeout, .. }
            | Self::PortListening { timeout, .. }
            | Self::LogContains { timeout, .. }
            | Self::HttpCheck { timeout, .. }
            | Self::FileContains { timeout, .. }
            | Self::AdpConfigKeyEquals { timeout, .. }
            | Self::IntakeHasMetric { timeout, .. } => timeout.0,
        }
    }
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

/// Matcher for the response body of an [`AssertionConfig::HttpCheck`].
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpBodyMatcher {
    /// Assertion passes when the response body equals this value exactly.
    Equal(String),
    /// Assertion passes when the response body contains this value.
    Contains(String),
}

impl CaseConfig for IntegrationConfig {
    fn set_loaded_from(&mut self, path: PathBuf) {
        self.loaded_from = path;
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
    /// Same mapping form as `env` on [`crate::correctness::config::TargetConfig`]. A variant entry replaces the base
    /// configuration's value for the same variable name; base entries the variant does not name are
    /// left alone.
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: BTreeMap<String, String>,
}

/// Variant metadata parsed separately from the shared correctness schema.
#[derive(Deserialize)]
struct MatrixVariants {
    variants: Vec<MatrixVariant>,
}

impl MatrixVariants {
    /// Expands environments without resolving paths or changing the base data.
    fn expand(self, base: &CorrectnessConfig, base_name: &str) -> Vec<(String, CorrectnessConfig)> {
        self.variants
            .into_iter()
            .map(|variant| {
                let mut config = base.clone();
                config.baseline.env.extend(variant.env.clone());
                config.comparison.env.extend(variant.env);
                (format!("{}/{}", base_name, variant.name), config)
            })
            .collect()
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
    BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?
        .into_iter()
        .map(|(name, value)| match value {
            serde_json::Value::String(value) => Ok((name, value)),
            _ => Err(D::Error::custom(format!(
                "environment variable '{}' must be a YAML string: quote any value that would otherwise \
                 parse as a boolean or a number (\"true\", \"8125\")",
                name
            ))),
        })
        .collect()
}

/// Configuration data that records which file the loader read.
pub trait CaseConfig {
    /// Records the canonical configuration file path after deserialization.
    fn set_loaded_from(&mut self, path: PathBuf);
}

/// Loads one test case configuration and records its canonical file path.
///
/// Every case type loads the same way: plain YAML deserialization, then the canonical file path recorded
/// on the result. Values come from the file alone, so YAML typing is what the case gets: an unquoted
/// `true` is a boolean and does not stand in for the string `"true"`. Images a case declares can be
/// replaced after loading; see [`crate::image_override`].
///
/// # Errors
///
/// Returns an error if `config_path` cannot be resolved or read, or if its contents do not
/// deserialize into `T`.
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

    let mut case: T = serde_saphyr::from_str(&content)
        .error_context(format!("Failed to parse configuration file: {}", config_path.display()))?;

    case.set_loaded_from(config_path);

    Ok(case)
}

/// Discover all test cases across one or more directories.
///
/// A `config.yaml` found in a direct subdirectory is routed by its shape. One with a top-level `type` field is a
/// legacy case: `type` must be `"integration"`, `"correctness"`, or `"correctness_matrix"`, and a legacy file with an
/// unknown `type` causes a panic. One without it is a participant case, loaded and planned for every OS it declares.
/// Multiple test types may coexist freely within the same directory.
///
/// `integration_runtime` scopes integration-test discovery to a single runtime: an integration
/// test is included if and only if its `runtimes:` list contains this value. Correctness tests
/// are unaffected by runtime scoping, but cases marked `disabled: true` are omitted. Image
/// overrides apply across this entire scope, before the caller selects tests by name.
///
/// Every participant case is included, for `subject_os`: a case that does not declare it is skipped when it runs. `set`
/// holds the run's replacements of the images participant cases name.
///
/// # Errors
///
/// Returns an error if a test directory cannot be read, an image override is duplicate or unmatched, a participant
/// case does not load or plan, or `set` replaces an image no participant case uses.
pub fn discover_tests(
    dirs: &[PathBuf], integration_runtime: &str, image_overrides: &[crate::image_override::ImageOverride],
    subject_os: Os, set: &BTreeMap<String, String>,
) -> Result<Vec<Box<dyn Test>>, GenericError> {
    let overrides = ImageOverrides::new(image_overrides);
    let settings = IntegrationSettings::new(integration_runtime, &overrides);
    let registry = crate::registry();
    let mut tests: Vec<Box<dyn Test>> = Vec::new();
    let mut case_errors = Vec::new();
    let mut replaced = BTreeSet::new();

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
                if config_path.exists() && !is_legacy_case(&config_path)? {
                    match ParticipantCase::load(&path, subject_os, set, &registry) {
                        // Loaded and planned all the same, so a disabled case cannot rot.
                        Ok((case, _)) if case.disabled => {}
                        Ok((case, images)) => {
                            replaced.extend(images.into_iter().filter(|image| set.contains_key(image)));
                            tests.push(Box::new(case));
                        }
                        Err(errors) => case_errors.extend(errors.iter().map(ToString::to_string)),
                    }
                } else if config_path.exists() {
                    if is_comparison_case(&config_path)? {
                        case_errors.push(format!(
                            "{}: legacy comparison cases no longer run; write it as a participant case",
                            config_path.display()
                        ));
                        continue;
                    }
                    match try_load_test(&config_path, &path, integration_runtime, &settings, &overrides) {
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

    // The loop does not return on a participant case that fails to load or plan: it collects the errors in
    // `case_errors`, so a run reports every bad case at once. `replaced` holds each `--set` image some discovered
    // participant case uses, so every `--set` name left out of it is one no case uses, and is an error too. Only when
    // every case loaded: a case that failed may be the one that uses the name, so `replaced` is incomplete then. All
    // the errors are returned together.
    if case_errors.is_empty() {
        case_errors.extend(set.keys().filter(|name| !replaced.contains(*name)).map(|name| {
            format!(
                "--set names image '{}', which no discovered participant case uses.",
                name
            )
        }));
    }
    if !case_errors.is_empty() {
        return Err(generic_error!("{}", case_errors.join("\n")));
    }

    // Sort by name for deterministic ordering.
    tests.sort_by_key(|a| a.name());

    overrides.validate(&tests)?;
    Ok(tests)
}

/// The top-level `type` of the case file at `config_path`, or `None` for a participant case, which has none.
fn legacy_type(config_path: &Path) -> Result<Option<Value>, GenericError> {
    let content = std::fs::read_to_string(config_path)
        .error_context(format!("Failed to read config file: {}", config_path.display()))?;
    let shape: Value = serde_saphyr::from_str(&content).error_context(format!(
        "Failed to parse config file as YAML: {}",
        config_path.display()
    ))?;
    Ok(shape.get("type").cloned())
}

/// Whether the case file at `config_path` is a legacy case: one with a top-level `type`.
pub(crate) fn is_legacy_case(config_path: &Path) -> Result<bool, GenericError> {
    legacy_type(config_path).map(|legacy| legacy.is_some())
}

/// Whether the case file at `config_path` is a legacy comparison case, which no longer runs.
fn is_comparison_case(config_path: &Path) -> Result<bool, GenericError> {
    let legacy = legacy_type(config_path)?;
    Ok(matches!(
        legacy.as_ref().and_then(Value::as_str),
        Some("correctness" | "correctness_matrix")
    ))
}

/// Load one or more test cases from a config file, dispatching on the top-level `type` field.
///
/// Returns a `Vec` because a `correctness_matrix` config expands into multiple independent test
/// cases—one per variant. `integration` configs produce zero or one test case depending on
/// whether the active `integration_runtime` is in the test's `runtimes:` list. Disabled
/// correctness configs do not produce test cases.
fn try_load_test(
    config_path: &Path, dir_path: &Path, integration_runtime: &str, settings: &IntegrationSettings,
    overrides: &ImageOverrides<'_>,
) -> Result<Vec<Box<dyn Test>>, GenericError> {
    let content = std::fs::read_to_string(config_path)
        .error_context(format!("Failed to read config file: {}", config_path.display()))?;

    let peek: serde_json::Value = serde_saphyr::from_str(&content).error_context(format!(
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
            Ok(vec![Box::new(IntegrationTestCase::new(config, settings))])
        }
        "correctness" => {
            let mut config: CorrectnessConfig = load_case(config_path)?;
            if config.disabled {
                return Ok(Vec::new());
            }
            let name = dir_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            overrides.apply_correctness(&mut config);
            Ok(vec![Box::new(CorrectnessTestCase::new(name, config))])
        }
        "correctness_matrix" => {
            let base_name = dir_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            // Parse both schemas from YAML text: flattening or a Value roundtrip changes scalar coercion.
            let base: CorrectnessConfig = load_case(config_path)?;
            let matrix: MatrixVariants = serde_saphyr::from_str(&content)
                .error_context(format!("Failed to parse configuration file: {}", config_path.display()))?;
            if matrix.variants.is_empty() {
                return Err(generic_error!(
                    "correctness_matrix '{}' has no variants defined",
                    base_name
                ));
            }
            if base.disabled {
                return Ok(Vec::new());
            }
            Ok(matrix
                .expand(&base, &base_name)
                .into_iter()
                .map(|(name, mut config)| {
                    overrides.apply_correctness(&mut config);
                    Box::new(CorrectnessTestCase::new(name, config)) as Box<dyn Test>
                })
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
    fn each_step_kind_declares_its_budget() {
        let assertion_budget = |yaml: &str| {
            serde_saphyr::from_str::<AssertionConfig>(yaml)
                .expect("assertion should parse")
                .budget()
        };
        let action_budget = |yaml: &str| {
            serde_saphyr::from_str::<ActionConfig>(yaml)
                .expect("action should parse")
                .budget()
        };

        assert_eq!(
            assertion_budget("assertion: process_stable_for\nduration: 10s\n"),
            Duration::from_secs(10)
        );
        assert_eq!(
            assertion_budget("assertion: log_not_contains\npattern: x\nduring: 5s\n"),
            Duration::from_secs(5)
        );
        assert_eq!(
            assertion_budget("assertion: log_contains\npattern: x\ntimeout: 7s\n"),
            Duration::from_secs(7)
        );
        assert_eq!(
            action_budget("action: target_exec\ncommand: [echo]\n"),
            Duration::from_secs(30)
        );
        assert_eq!(
            action_budget(
                "action: dogstatsd_replay\nsender: [echo]\ncapture_duration: 3s\nstats_duration_secs: 1\n\
                 expected_metrics: []\ntimeout: 30s\n"
            ),
            Duration::from_millis(123_050)
        );
    }

    #[test]
    fn target_exec_action_deserializes_command() {
        let action: ActionConfig = serde_saphyr::from_str(
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
        let action: ActionConfig = serde_saphyr::from_str(
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

        let tests = discover_tests(
            &[base_dir.path().to_path_buf()],
            "windows",
            &[],
            Os::Linux,
            &BTreeMap::new(),
        )
        .unwrap();

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

        let tests = discover_tests(
            &[base_dir.path().to_path_buf()],
            "windows",
            &[],
            Os::Linux,
            &BTreeMap::new(),
        )
        .unwrap();
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

        let tests = discover_tests(
            &[base_dir.path().to_path_buf()],
            "linux",
            &[],
            Os::Linux,
            &BTreeMap::new(),
        )
        .unwrap();
        let images = tests[0].images();

        assert_eq!(images.get("container"), Some(&DEFAULT_LINUX_TARGET_IMAGE.to_string()));
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

        let error = match serde_saphyr::from_str::<IntegrationConfig>(yaml) {
            Ok(_) => panic!("an unquoted boolean env value should be rejected"),
            Err(e) => format!("{e:?}"),
        };

        assert!(error.contains("DD_DATA_PLANE_ENABLED"), "unexpected error: {error}");
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
            !crate::dynamic_vars::unresolved_action(&action).is_empty(),
            "placeholders should be detected before resolution"
        );
        crate::dynamic_vars::resolve_action(&mut action, &vars);

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
    port: 48125
  - assertion: intake_has_metric
    name: "example.counter"
    metric_type: count
    value: 3
    tags: ["source:integration-test"]
    timeout: 30s
"#;

        let config: IntegrationConfig = serde_saphyr::from_str(yaml).expect("case should parse");

        assert!(config.intake.enabled);
        let AssertionStep::Action(ActionConfig::DogstatsdSend { payload, port, .. }) = &config.procedure[0] else {
            panic!("first step should parse as a dogstatsd_send action");
        };
        assert_eq!(payload, "example.counter:3|c");
        assert_eq!(*port, 48125);
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
    fn intake_has_metric_tags_match_defaults_to_subset_and_rejects_unknown_modes() {
        let parse = |tags_match: &str| {
            let yaml = format!(
                r#"
type: integration
name: tags-match-case
timeout: 60s
procedure:
  - assertion: intake_has_metric
    name: "example.gauge"
    tags: []
    {}
    timeout: 30s
"#,
                tags_match
            );
            serde_saphyr::from_str::<IntegrationConfig>(&yaml).map(|config| match &config.procedure[0] {
                AssertionStep::Single(AssertionConfig::IntakeHasMetric { tags_match, .. }) => *tags_match,
                _ => panic!("step should parse as an intake_has_metric assertion"),
            })
        };

        assert_eq!(parse("").expect("case should parse"), TagMatch::Subset);
        assert_eq!(
            parse("tags_match: subset").expect("case should parse"),
            TagMatch::Subset
        );
        assert_eq!(parse("tags_match: exact").expect("case should parse"), TagMatch::Exact);
        assert!(parse("tags_match: fuzzy").is_err());
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

        let config: IntegrationConfig = serde_saphyr::from_str(yaml).expect("case should parse");

        assert!(!config.intake.enabled);
    }

    #[test]
    fn target_exec_resolves_placeholders_in_each_command_argument() {
        let vars = dynamic_vars(&[("IP", "10.0.0.5")]);
        let mut action = ActionConfig::TargetExec {
            command: vec!["ping".to_string(), "{{PANORAMIC_DYNAMIC_IP}}".to_string()],
            timeout: HumanDuration(Duration::from_secs(1)),
        };

        crate::dynamic_vars::resolve_action(&mut action, &vars);

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

        crate::dynamic_vars::resolve_action(&mut action, &vars);
        assert!(crate::dynamic_vars::unresolved_action(&action).is_empty());

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

        assert!(!crate::dynamic_vars::unresolved_action(&action).is_empty());
        crate::dynamic_vars::resolve_action(&mut action, &vars);
        assert!(crate::dynamic_vars::unresolved_action(&action).is_empty());

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
        crate::dynamic_vars::resolve_assertion(&mut log, &vars);
        let AssertionConfig::LogContains { pattern, .. } = &log else {
            panic!("expected log_contains");
        };
        assert_eq!(pattern, "listen:10.0.0.5");
        assert!(crate::dynamic_vars::unresolved_assertion(&log).is_empty());

        let mut http = AssertionConfig::HttpCheck {
            endpoint: "http://{{PANORAMIC_DYNAMIC_IP}}:{{PANORAMIC_DYNAMIC_PORT}}/health".to_string(),
            status: HttpStatusMatcher::Equal(200),
            body: None,
            insecure_skip_verify: false,
            timeout: HumanDuration(Duration::from_secs(1)),
        };
        crate::dynamic_vars::resolve_assertion(&mut http, &vars);
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
        crate::dynamic_vars::resolve_assertion(&mut file, &vars);
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

        crate::dynamic_vars::resolve_assertion(&mut assertion, &dynamic_vars(&[("PRESENT", "x")]));

        assert_eq!(
            crate::dynamic_vars::unresolved_assertion(&assertion),
            vec!["{{PANORAMIC_DYNAMIC_MISSING}}".to_string()]
        );
    }

    #[test]
    fn integration_environment_rejects_non_string_yaml_values() {
        for value in [
            "true",
            "false",
            "8125",
            "01",
            "1e3",
            "1.5",
            "null",
            "[text]",
            "{key: text}",
        ] {
            let integration = format!("name: strict\ntimeout: 1s\nprocedure: []\nenv: {{INVALID: {value}}}\n");
            let error = serde_saphyr::from_str::<IntegrationConfig>(&integration).unwrap_err();
            assert!(error
                .to_string()
                .contains("environment variable 'INVALID' must be a YAML string"));
        }
    }

    #[test]
    fn discovery_scopes_integration_images_to_runtime() {
        let integration = create_test_case_dir(
            "portable",
            "type: integration\nname: portable\ntimeout: 1s\nruntimes: [linux, mac, windows]\nprocedure: []\n",
        );
        let dirs = [integration.path().to_path_buf()];
        for (runtime, target) in [
            (LINUX_RUNTIME, Some(DEFAULT_LINUX_TARGET_IMAGE)),
            (WINDOWS_RUNTIME, Some(DEFAULT_WINDOWS_TARGET_IMAGE)),
            (MAC_RUNTIME, None),
        ] {
            let cases = discover_tests(&dirs, runtime, &[], Os::Linux, &BTreeMap::new()).unwrap();
            assert_eq!(cases.len(), 1);
            assert_eq!(cases[0].runtime(), runtime);
            let expected: BTreeMap<&str, String> = target
                .into_iter()
                .map(|image| (TARGET_IMAGE_NAME, image.to_string()))
                .collect();
            assert_eq!(cases[0].images(), expected);
        }
        let override_entry = ["container=target:override".parse().unwrap()];
        let error = discover_tests(&dirs, MAC_RUNTIME, &override_entry, Os::Linux, &BTreeMap::new())
            .err()
            .expect("no eligible target image");
        assert!(error
            .to_string()
            .contains("No test in this run uses a container named 'container'"));
    }

    #[test]
    fn a_legacy_comparison_case_fails_discovery() {
        for kind in ["correctness", "correctness_matrix"] {
            let dir = create_test_case_dir(
                "comparison",
                &format!("type: {kind}\nruntime: docker\nanalysis_mode: metrics\nbaseline: {{image: base}}\ncomparison: {{image: comp}}\n"),
            );
            let error = discover_tests(
                &[dir.path().to_path_buf()],
                LINUX_RUNTIME,
                &[],
                Os::Linux,
                &BTreeMap::new(),
            )
            .err()
            .unwrap_or_else(|| panic!("a {kind} case should fail discovery"));
            let error = error.to_string();
            assert!(
                error.contains("legacy comparison cases no longer run; write it as a participant case"),
                "{error}"
            );
        }
    }

    #[test]
    fn declared_runtime_validation_precedes_eligibility_filtering() {
        for (runtimes, expected) in [
            ("[]", "empty runtimes list"),
            ("[linux, typo]", "unknown runtime 'typo'"),
        ] {
            let yaml = format!("type: integration\nname: invalid\ntimeout: 1s\nruntimes: {runtimes}\nprocedure: []\n");
            let dir = create_test_case_dir("invalid", &yaml);
            let overrides = ImageOverrides::new(&[]);
            let settings = IntegrationSettings::new(MAC_RUNTIME, &overrides);
            let case_dir = dir.path().join("invalid");
            let error = try_load_test(
                &case_dir.join("config.yaml"),
                &case_dir,
                MAC_RUNTIME,
                &settings,
                &overrides,
            )
            .err()
            .expect("invalid declared runtime");
            assert!(error.to_string().contains(expected), "{error}");
        }
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
