use std::collections::BTreeMap;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;

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

    /// Environment variables to set on the target process(es).
    ///
    /// Top-level (not under `container`) because both the linux and `mac` runtimes apply
    /// these the same way: docker injects them as container env, the Unix runner passes them
    /// to the spawned ADP / Core Agent processes.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// List of assertion steps to run.
    pub assertions: Vec<AssertionStep>,

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

    fn timeout(&self) -> Duration {
        self.timeout.0
    }

    fn images(&self) -> BTreeMap<&str, String> {
        let mut m = BTreeMap::new();
        if let Some(image) = target_image_for_runtime(&self.active_runtime) {
            m.insert("container", image.to_string());
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
        for step in &mut self.assertions {
            step.resolve_dynamic_vars(vars);
        }
    }

    /// Returns any unresolved `{{PANORAMIC_DYNAMIC_*}}` placeholders across all assertion steps.
    pub fn unresolved_placeholders(&self) -> Vec<String> {
        self.assertions
            .iter()
            .flat_map(|s| s.unresolved_placeholders())
            .collect()
    }

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

        let mut test_case: IntegrationConfig = serde_yaml::from_str(&content)
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

/// A single variant in a `correctness_matrix` test.
///
/// Each variant expands into an independent correctness test. The variant's `additional_env_vars`
/// are appended to the environment variables of both the baseline and comparison targets in the
/// base configuration, allowing a single test directory to exercise multiple agent configurations
/// without duplicating the full config layout.
#[derive(Clone, Deserialize)]
pub struct MatrixVariant {
    /// Name suffix for this variant.
    ///
    /// The expanded test name is `{base_name}/{variant_name}`, for example,
    /// `dsd-origin-detection-matrix/unified`.
    pub name: String,

    /// Environment variables appended to both the baseline and comparison targets.
    ///
    /// Entries must be in `KEY=VALUE` format, identical to `additional_env_vars` on
    /// [`CorrectnessTargetConfig`]. These are appended after the base config's env vars, so they
    /// can override defaults by relying on last-write-wins semantics in the agent's config loader.
    #[serde(default)]
    pub additional_env_vars: Vec<String>,
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

    /// Baseline target configuration (shared base; variant env vars are appended).
    pub baseline: CorrectnessTargetConfig,

    /// Comparison target configuration (shared base; variant env vars are appended).
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
    base_config_path: PathBuf,
}

impl MatrixConfig {
    fn from_yaml(config_path: &str) -> Result<Self, GenericError> {
        use saluki_config_tools::ConfigurationLoader;

        let config_path = PathBuf::from(config_path)
            .canonicalize()
            .error_context("Failed to canonicalize matrix configuration file path.")?;

        let mut config = ConfigurationLoader::default()
            .from_yaml(&config_path)
            .error_context("Failed to load matrix configuration file.")?
            .from_environment("PANORAMIC")
            .expect("Environment variable prefix should not be empty.")
            .into_typed::<MatrixConfig>()
            .error_context("Failed to deserialize matrix configuration file.")?;

        config.base_config_path = config_path
            .parent()
            .expect("Configuration file path must be an absolute file path.")
            .to_path_buf();

        Ok(config)
    }

    fn get_canonicalized_config_path<P: AsRef<Path>>(&self, path: P) -> PathBuf {
        let path = path.as_ref();
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_config_path.join(path)
        }
    }

    /// Expands this matrix into one [`CorrectnessConfig`] per variant.
    ///
    /// Each expanded config is a clone of the base configuration with the variant's
    /// `additional_env_vars` appended to both the baseline and comparison targets.
    fn expand(self, base_name: &str) -> Vec<CorrectnessConfig> {
        self.variants
            .iter()
            .map(|variant| {
                let mut baseline = self.baseline.clone();
                baseline
                    .additional_env_vars
                    .extend(variant.additional_env_vars.iter().cloned());

                let mut comparison = self.comparison.clone();
                comparison
                    .additional_env_vars
                    .extend(variant.additional_env_vars.iter().cloned());

                CorrectnessConfig {
                    name: format!("{}/{}", base_name, variant.name),
                    runtime: self.runtime.clone(),
                    analysis_mode: self.analysis_mode.clone(),
                    millstone: CorrectnessMillstoneConfig {
                        image: self.millstone.image.clone(),
                        binary_path: self.millstone.binary_path.clone(),
                        config_path: self.get_canonicalized_config_path(&self.millstone.config_path),
                    },
                    datadog_intake: CorrectnessDatadogIntakeConfig {
                        image: self.datadog_intake.image.clone(),
                        binary_path: self.datadog_intake.binary_path.clone(),
                    },
                    baseline: CorrectnessTargetConfig {
                        image: baseline.image,
                        entrypoint: baseline.entrypoint,
                        command: baseline.command,
                        files: baseline
                            .files
                            .iter()
                            .map(|f| canonicalize_file_entry(f, &self.base_config_path))
                            .collect(),
                        additional_env_vars: baseline.additional_env_vars,
                    },
                    comparison: CorrectnessTargetConfig {
                        image: comparison.image,
                        entrypoint: comparison.entrypoint,
                        command: comparison.command,
                        files: comparison
                            .files
                            .iter()
                            .map(|f| canonicalize_file_entry(f, &self.base_config_path))
                            .collect(),
                        additional_env_vars: comparison.additional_env_vars,
                    },
                    otlp_direct_analysis_mode: self.otlp_direct_analysis_mode,
                    additional_span_ignore_fields: self.additional_span_ignore_fields.clone(),
                    require_dogstatsd_forwarded_packets: self.require_dogstatsd_forwarded_packets,
                    base_config_path: PathBuf::new(),
                }
            })
            .collect()
    }
}

/// Rewrites the host-path portion of a `host_path:container_path` file entry to an absolute path
/// anchored at `base_path`, mirroring the canonicalization that `CorrectnessConfig::from_yaml`
/// performs for its own `files` entries.
fn canonicalize_file_entry(entry: &str, base_path: &Path) -> String {
    match entry.split_once(':') {
        Some((host, container)) => {
            let abs = if Path::new(host).is_absolute() {
                PathBuf::from(host)
            } else {
                base_path.join(host)
            };
            format!("{}:{}", abs.display(), container)
        }
        None => entry.to_string(),
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
            let config = IntegrationConfig::from_yaml(config_path)?;
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
            let config_path_str = config_path
                .to_str()
                .ok_or_else(|| generic_error!("Invalid UTF-8 in config path: {}", config_path.display()))?;
            let mut config = CorrectnessConfig::from_yaml(config_path_str)?;
            config.name = dir_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            Ok(vec![Box::new(config)])
        }
        "correctness_matrix" => {
            let config_path_str = config_path
                .to_str()
                .ok_or_else(|| generic_error!("Invalid UTF-8 in config path: {}", config_path.display()))?;
            let base_name = dir_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            let matrix = MatrixConfig::from_yaml(config_path_str)?;
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

    #[test]
    fn test_windows_runtime_is_valid_for_integration_discovery() {
        let base_dir = create_test_case_dir(
            "windows-smoke",
            r#"
type: integration
name: windows-smoke
timeout: 10s
runtimes: [windows]
assertions: []
"#,
        );

        let tests = discover_tests(&[base_dir.path().to_path_buf()], "windows").unwrap();

        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].name(), "windows-smoke");
        assert_eq!(tests[0].runtime(), "windows");
    }

    #[test]
    fn test_windows_runtime_reports_harness_owned_container_image() {
        let base_dir = create_test_case_dir(
            "windows-smoke",
            r#"
type: integration
name: windows-smoke
timeout: 10s
runtimes: [windows]
assertions: []
"#,
        );

        let tests = discover_tests(&[base_dir.path().to_path_buf()], "windows").unwrap();
        let images = tests[0].images();

        assert_eq!(images.get("container"), Some(&DEFAULT_WINDOWS_TARGET_IMAGE.to_string()));
    }

    #[test]
    fn test_linux_runtime_reports_harness_owned_container_image() {
        let base_dir = create_test_case_dir(
            "linux-smoke",
            r#"
type: integration
name: linux-smoke
timeout: 10s
runtimes: [linux]
assertions: []
"#,
        );

        let tests = discover_tests(&[base_dir.path().to_path_buf()], "linux").unwrap();
        let images = tests[0].images();

        assert_eq!(images.get("container"), Some(&DEFAULT_LINUX_TARGET_IMAGE.to_string()));
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
