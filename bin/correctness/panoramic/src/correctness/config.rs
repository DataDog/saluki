use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use airlock::{
    config::{
        DatadogIntakeConfig as AirlockDatadogIntakeConfig, MillstoneConfig as AirlockMillstoneConfig,
        TargetConfig as AirlockTargetConfig,
    },
    driver::DriverConfig,
};
use async_trait::async_trait;
use saluki_config::ConfigurationLoader;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;

use crate::correctness::analysis::AnalysisMode;
use crate::reporter::TestResult;
use crate::test::{Test, TestContext, TestSuite};

// Correctness tests run two isolation groups (baseline + comparison), each with multiple
// containers, so they need more time than the default.
const CORRECTNESS_TIMEOUT: Duration = Duration::from_mins(20);

/// The container runtime backend to use for a correctness test.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    /// Run test groups as Docker containers.
    Docker,
    /// Run test groups as pods in a kind (Kubernetes in Docker) cluster.
    KubernetesInDocker,
}

fn default_millstone_binary_path() -> String {
    "/usr/local/bin/millstone".to_string()
}

fn default_datadog_intake_binary_path() -> String {
    "/usr/local/bin/datadog-intake".to_string()
}

fn default_otlp_direct_analysis_mode() -> bool {
    false
}

#[derive(Clone, Deserialize)]
pub struct Config {
    #[serde(skip)]
    pub(crate) name: String,

    /// Container runtime backend to use.
    pub runtime: Runtime,

    /// Analysis mode to use.
    pub analysis_mode: AnalysisMode,

    /// Millstone configuration.
    pub millstone: MillstoneConfig,

    /// Datadog intake configuration.
    pub datadog_intake: DatadogIntakeConfig,

    /// Baseline target configuration.
    pub baseline: TargetConfig,

    /// Comparison target configuration.
    pub comparison: TargetConfig,

    /// When analysis mode is traces: if true, use OTLP-direct analysis (baseline is OTel-based).
    /// Equivalent to skipping trace stats comparison and not requiring baseline SSI metadata.
    #[serde(default = "default_otlp_direct_analysis_mode")]
    pub otlp_direct_analysis_mode: bool,

    /// When analysis mode is traces: additional span field paths to ignore when diffing baseline vs comparison.
    /// Merged with the built-in list (SSI metadata, deprecated fields). Use for OTel vs ADP differences (for example, `agent_metadata.target_tps`, `metrics._top_level`, `metrics._dd.measured`).
    #[serde(default)]
    pub additional_span_ignore_fields: Vec<String>,

    #[serde(skip, default = "PathBuf::new")]
    pub(crate) base_config_path: PathBuf,
}

#[derive(Clone, Deserialize)]
pub struct MillstoneConfig {
    /// Container image to use for millstone.
    ///
    /// This must be a valid image reference: `millstone:x.y.z`, `registry.ddbuild.io/saluki/millstone:x.y.z`, etc.
    pub image: String,

    /// Path to the millstone binary.
    ///
    /// Defaults to `/usr/local/bin/millstone`.
    #[serde(default = "default_millstone_binary_path")]
    pub binary_path: String,

    /// Path to the millstone configuration file to use.
    ///
    /// This file is mapped into the baseline target's `millstone` container and so it must exist on the system where
    /// this command is run from.
    pub config_path: PathBuf,
}

#[derive(Clone, Deserialize)]
pub struct DatadogIntakeConfig {
    /// Container image to use for datadog-intake.
    ///
    /// This must be a valid image reference: `datadog-intake:x.y.z`, `registry.ddbuild.io/saluki/datadog-intake:x.y.z`, etc.
    pub image: String,

    /// Path to the datadog-intake binary.
    ///
    /// Defaults to `/usr/local/bin/datadog-intake`.
    #[serde(default = "default_datadog_intake_binary_path")]
    pub binary_path: String,

    /// Path to the datadog-intake configuration file to use.
    ///
    /// This must be a valid path to a file on the host system.
    pub config_path: PathBuf,
}

#[derive(Clone, Deserialize)]
pub struct TargetConfig {
    /// Container image to use for target.
    ///
    /// This must be a valid image reference: `name:x.y.z`, `docker.io/datadog/name:x.y.z`, etc.
    pub image: String,

    /// Entrypoint for the target container.
    #[serde(default = "Vec::new")]
    pub entrypoint: Vec<String>,

    /// Command to run in the container to start the target.
    #[serde(default = "Vec::new")]
    pub command: Vec<String>,

    /// Files to be mapped into the target container.
    ///
    /// Entries must be in the form of `host_path:container_path`.
    #[serde(default = "Vec::new")]
    pub files: Vec<String>,

    /// Additional environment variables to be passed into the target container.
    ///
    /// These should be in the form of `KEY=VALUE`.
    #[serde(default = "Vec::new")]
    pub additional_env_vars: Vec<String>,
}

#[async_trait]
impl Test for Config {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn suite(&self) -> TestSuite {
        TestSuite::Correctness
    }

    fn description(&self) -> Option<String> {
        None
    }

    fn timeout(&self) -> Duration {
        CORRECTNESS_TIMEOUT
    }

    fn images(&self) -> BTreeMap<&str, String> {
        let mut m = BTreeMap::new();
        m.insert("baseline", self.baseline.image.clone());
        m.insert("comparison", self.comparison.image.clone());
        m.insert("datadog-intake", self.datadog_intake.image.clone());
        m.insert("millstone", self.millstone.image.clone());
        m
    }

    fn runtime(&self) -> String {
        match self.runtime {
            Runtime::Docker => "docker".to_string(),
            Runtime::KubernetesInDocker => "kubernetes_in_docker".to_string(),
        }
    }

    async fn run(&self, tctx: TestContext) -> TestResult {
        crate::correctness::runner::run_correctness_test(self.name.clone(), self.clone(), tctx).await
    }
}

impl Config {
    pub fn from_yaml(config_path: &str) -> Result<Self, GenericError> {
        let config_path = PathBuf::from(config_path)
            .canonicalize()
            .error_context("Failed to canonicalize configuration file path.")?;

        // We load the configuration file from the given path, and also environment variables, and then deserialize.
        let mut config = ConfigurationLoader::default()
            .from_yaml(&config_path)
            .error_context("Failed to load configuration file.")?
            .from_environment("PANORAMIC")
            .expect("Environment variable prefix should not be empty.")
            .into_typed::<Config>()
            .error_context("Failed to deserialize configuration file.")?;

        // Now that we've deserialized things, calculate the base path of the configuration file we loaded, which we
        // then use as the base path for any configuration fields which also specify paths to files. We only use the
        // base path if those paths aren't already absolute.
        config.base_config_path = config_path
            .parent()
            .expect("Configuration file path must be an absolute file path.")
            .to_path_buf();

        Ok(config)
    }

    pub fn get_canonicalized_config_path<P: AsRef<Path>>(&self, path: P) -> PathBuf {
        let path = path.as_ref();
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_config_path.join(path)
        }
    }

    pub fn millstone_config(&self) -> AirlockMillstoneConfig {
        AirlockMillstoneConfig {
            image: self.millstone.image.clone(),
            binary_path: Some(self.millstone.binary_path.clone()),
            config_path: self.get_canonicalized_config_path(&self.millstone.config_path),
        }
    }

    pub fn datadog_intake_config(&self) -> AirlockDatadogIntakeConfig {
        AirlockDatadogIntakeConfig {
            image: self.datadog_intake.image.clone(),
            binary_path: Some(self.datadog_intake.binary_path.clone()),
            config_path: self.get_canonicalized_config_path(&self.datadog_intake.config_path),
        }
    }

    async fn target_driver_config(&self, target_config: &TargetConfig) -> Result<DriverConfig, GenericError> {
        let airlock_target_config = AirlockTargetConfig {
            image: target_config.image.clone(),
            entrypoint: target_config.entrypoint.clone(),
            command: target_config.command.clone(),
            additional_env_vars: target_config.additional_env_vars.clone(),
        };

        let mut driver_config = DriverConfig::target("target", airlock_target_config).await?;

        for file in &target_config.files {
            // Parse the two file paths -- host path and container path -- from the entry,
            // and canonicalize the host path. The container path must be absolute.
            match file.split_once(':') {
                Some((host_path, container_path)) => {
                    let host_path = self.get_canonicalized_config_path(host_path);
                    let container_path = Path::new(container_path);
                    if !container_path.is_absolute() {
                        return Err(generic_error!(
                            "Container path '{}' must be absolute.",
                            container_path.display()
                        ));
                    }

                    driver_config = driver_config.with_bind_mount(host_path, container_path)
                }
                None => {
                    return Err(generic_error!(
                        "Invalid file entry format (expected 'host_path:container_path', got '{}')",
                        file,
                    ))
                }
            };
        }

        Ok(driver_config)
    }

    pub async fn baseline_target_driver_config(&self) -> Result<DriverConfig, GenericError> {
        self.target_driver_config(&self.baseline).await
    }

    pub async fn comparison_target_driver_config(&self) -> Result<DriverConfig, GenericError> {
        self.target_driver_config(&self.comparison).await
    }
}

/// Resolves `$GROUP` per-target in a millstone config template by walking the `targets:` map.
///
/// The template uses one shared placeholder, `$GROUP`, that the orchestrator must substitute
/// differently for each target. This helper parses the template as YAML, expects a top-level
/// `targets:` mapping, and for each entry replaces `$GROUP` in that entry's address using the
/// caller-supplied lookup (typically `key -> "baseline" | "comparison"` for Docker, or
/// `key -> pod cluster IP` for k8s TCP/gRPC). Other parts of the document are left untouched.
///
/// The lookup closure must return `Some` for every target key present in the template; missing
/// keys are reported as an error rather than silently leaving `$GROUP` unresolved.
///
/// Returns the rewritten YAML as a `String` ready to be written to the millstone container.
pub fn resolve_group_placeholders<F>(template: &str, mut lookup: F) -> Result<String, GenericError>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(template).error_context("Failed to parse millstone config template as YAML.")?;

    let targets = doc
        .get_mut("targets")
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| generic_error!("Millstone config template is missing a `targets:` mapping."))?;

    for (key, value) in targets.iter_mut() {
        let key_str = key
            .as_str()
            .ok_or_else(|| generic_error!("Non-string key in `targets:` mapping."))?;
        let addr = value
            .as_str()
            .ok_or_else(|| generic_error!("Non-string address for target '{}'.", key_str))?;

        if addr.contains("$GROUP") {
            let resolved = lookup(key_str).ok_or_else(|| {
                generic_error!(
                    "No `$GROUP` substitution provided for target '{}' in millstone config.",
                    key_str
                )
            })?;
            let new_addr = addr.replace("$GROUP", &resolved);
            *value = serde_yaml::Value::String(new_addr);
        }
    }

    serde_yaml::to_string(&doc).error_context("Failed to serialize resolved millstone config.")
}

/// Returns true if every entry in the `targets:` mapping of the template is a Unix-socket target
/// (either `unixgram://` or `unix://`).
///
/// The k8s path uses this to decide whether the millstone pod's startup wait should include a
/// `[ -S ... ]` socket-file check. Mixed-transport configs (some socket, some TCP) are not
/// supported by the existing test suite and are not handled here; they would return `false`.
pub fn millstone_targets_all_sockets(template: &str) -> bool {
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(template) else {
        return false;
    };
    let Some(targets) = doc.get("targets").and_then(|v| v.as_mapping()) else {
        return false;
    };
    if targets.is_empty() {
        return false;
    }
    targets.iter().all(|(_, v)| {
        v.as_str()
            .map(|s| s.starts_with("unixgram://") || s.starts_with("unix://"))
            .unwrap_or(false)
    })
}

/// Extracts the port from the first non-Unix target in the `targets:` mapping.
///
/// All current correctness configs use one transport across all targets, so probing the first
/// non-socket entry is sufficient to learn the agent port. Returns `None` if every target is a
/// Unix socket or the template can't be parsed.
pub fn millstone_first_network_port(template: &str) -> Option<u16> {
    let doc: serde_yaml::Value = serde_yaml::from_str(template).ok()?;
    let targets = doc.get("targets")?.as_mapping()?;
    for (_, value) in targets.iter() {
        let addr = value.as_str()?;
        if addr.starts_with("unixgram://") || addr.starts_with("unix://") {
            continue;
        }
        // scheme://HOST:PORT[/path] -> take the part after `://`, then everything after the
        // first `:` up to either `/` or end.
        let after_scheme = addr.split("://").nth(1)?;
        let after_host = after_scheme.split(':').nth(1)?;
        return after_host.split('/').next()?.parse().ok();
    }
    None
}

#[cfg(test)]
mod millstone_helper_tests {
    use super::*;

    #[test]
    fn resolves_group_per_target() {
        let template = r#"
targets:
  baseline: "udp://$GROUP:8125"
  comparison: "udp://$GROUP:8125"
volume: 1
"#;
        let resolved = resolve_group_placeholders(template, |k| Some(k.to_string())).unwrap();
        assert!(resolved.contains("udp://baseline:8125"));
        assert!(resolved.contains("udp://comparison:8125"));
        assert!(!resolved.contains("$GROUP"));
    }

    #[test]
    fn resolves_group_with_custom_lookup() {
        // k8s case: TCP target keys map to pod cluster IPs, not group names.
        let template = r#"
targets:
  baseline: "grpc://$GROUP:4317/svc/Method"
  comparison: "grpc://$GROUP:4317/svc/Method"
"#;
        let resolved = resolve_group_placeholders(template, |k| match k {
            "baseline" => Some("10.0.0.1".to_string()),
            "comparison" => Some("10.0.0.2".to_string()),
            _ => None,
        })
        .unwrap();
        assert!(resolved.contains("grpc://10.0.0.1:4317"));
        assert!(resolved.contains("grpc://10.0.0.2:4317"));
    }

    #[test]
    fn errors_on_missing_substitution() {
        // If the lookup returns None for a key whose address contains $GROUP, fail loudly rather
        // than ship an unresolved placeholder to the container.
        let template = r#"
targets:
  baseline: "udp://$GROUP:8125"
"#;
        let err = resolve_group_placeholders(template, |_| None).unwrap_err();
        assert!(format!("{}", err).contains("baseline"));
    }

    #[test]
    fn leaves_address_alone_when_no_placeholder() {
        // Lookup must not be called for entries without `$GROUP`; verify by panicking inside it.
        let template = r#"
targets:
  baseline: "udp://10.0.0.1:8125"
  comparison: "udp://10.0.0.2:8125"
"#;
        let resolved = resolve_group_placeholders(template, |_| {
            panic!("lookup must not be called when no $GROUP is present")
        })
        .unwrap();
        assert!(resolved.contains("10.0.0.1"));
        assert!(resolved.contains("10.0.0.2"));
    }

    #[test]
    fn all_sockets_recognised_for_unixgram() {
        assert!(millstone_targets_all_sockets(
            r#"
targets:
  baseline: "unixgram:///x/metrics.sock"
  comparison: "unixgram:///y/metrics.sock"
"#
        ));
    }

    #[test]
    fn all_sockets_false_for_mixed_and_tcp() {
        // Mixed transports are not supported by the existing suite; verifying the function
        // reports false ensures the k8s readiness path won't skip its port-readiness check by
        // mistake on a mixed config.
        assert!(!millstone_targets_all_sockets(
            r#"
targets:
  baseline: "unixgram:///x/metrics.sock"
  comparison: "udp://comparison:8125"
"#
        ));
        assert!(!millstone_targets_all_sockets(
            r#"
targets:
  baseline: "udp://baseline:8125"
"#
        ));
    }

    #[test]
    fn first_network_port_skips_sockets() {
        // Sockets come first; the helper must scan past them to find the TCP/UDP port.
        let port = millstone_first_network_port(
            r#"
targets:
  baseline: "unixgram:///x/metrics.sock"
  comparison: "udp://comparison:8125"
"#,
        );
        assert_eq!(port, Some(8125));
    }

    #[test]
    fn first_network_port_grpc_with_path() {
        // gRPC addresses include a `/service/Method` suffix; the port parse must stop at `/`.
        let port = millstone_first_network_port(
            r#"
targets:
  baseline: "grpc://$GROUP:4317/opentelemetry.proto.collector.metrics.v1.MetricsService/Export"
"#,
        );
        assert_eq!(port, Some(4317));
    }
}
