use std::{collections::BTreeMap, path::PathBuf};

use serde::Deserialize;

use crate::config::{deserialize_env_map, CaseConfig};
use crate::correctness::analysis::AnalysisMode;

/// The container runtime backend to use for a correctness test.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    /// Run test groups as Docker containers.
    Docker,
    /// Run test groups as pods in a kind (Kubernetes in Docker) cluster.
    KubernetesInDocker,
}

fn default_otlp_direct_analysis_mode() -> bool {
    false
}

#[derive(Clone, Deserialize)]
pub struct Config {
    /// Container runtime backend to use.
    pub runtime: Runtime,

    /// Analysis mode to use.
    pub analysis_mode: AnalysisMode,

    /// Millstone configuration.
    #[serde(default)]
    pub millstone: MillstoneConfig,

    /// Datadog intake configuration.
    #[serde(default)]
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

    /// Whether the correctness run must capture at least one forwarded DogStatsD packet.
    #[serde(default)]
    pub require_dogstatsd_forwarded_packets: bool,

    /// Canonical configuration file path, recorded by the loader.
    #[serde(skip)]
    pub(crate) loaded_from: PathBuf,
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct MillstoneConfig {
    /// Container image to use for millstone.
    ///
    /// This must be a valid image reference: `millstone:x.y.z`, `registry.ddbuild.io/saluki/millstone:x.y.z`, etc.
    ///
    /// Defaults to `saluki-images/correctness-tools:latest`.
    pub image: String,

    /// Path to the millstone binary.
    ///
    /// Defaults to `/usr/local/bin/millstone`.
    pub binary_path: String,

    /// Path to the millstone configuration file to use.
    ///
    /// This file is mapped into the baseline target's `millstone` container and so it must exist on the system where
    /// this command is run from.
    ///
    /// Defaults to `millstone.yaml` in the current directory of the test case.
    pub config_path: PathBuf,
}

impl Default for MillstoneConfig {
    fn default() -> Self {
        Self {
            image: "saluki-images/correctness-tools:latest".to_string(),
            binary_path: "/usr/local/bin/millstone".to_string(),
            config_path: "millstone.yaml".into(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct DatadogIntakeConfig {
    /// Container image to use for datadog-intake.
    ///
    /// This must be a valid image reference: `datadog-intake:x.y.z`, `registry.ddbuild.io/saluki/datadog-intake:x.y.z`, etc.
    ///
    /// Defaults to `saluki-images/correctness-tools:latest`.
    pub image: String,

    /// Path to the datadog-intake binary.
    ///
    /// Defaults to `/usr/local/bin/datadog-intake`.
    pub binary_path: String,
}

impl Default for DatadogIntakeConfig {
    fn default() -> Self {
        Self {
            image: "saluki-images/correctness-tools:latest".to_string(),
            binary_path: "/usr/local/bin/datadog-intake".to_string(),
        }
    }
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

    /// Environment variables to be passed into the target container.
    ///
    /// Keys are variable names, values are the string the process receives. Values must be YAML
    /// strings, so quote anything that would otherwise parse as a boolean or a number (`"true"`,
    /// `"8125"`).
    ///
    /// Defaults to no variables. Baseline and comparison own their own maps; nothing is shared
    /// between them.
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: BTreeMap<String, String>,
}

impl CaseConfig for Config {
    fn set_loaded_from(&mut self, path: PathBuf) {
        self.loaded_from = path;
    }
}
