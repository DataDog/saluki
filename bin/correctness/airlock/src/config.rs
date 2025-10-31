use std::path::PathBuf;

/// Millstone configuration.
#[derive(Clone)]
pub struct MillstoneConfig {
    /// Container image to use.
    ///
    /// This must be a valid image reference -- `millstone:x.y.z`, `registry.ddbuild.io/saluki/millstone:x.y.z`, etc --
    /// to an image containing the `millstone` binary.
    pub image: String,

    /// Path to the millstone binary.
    ///
    /// Defaults to `/usr/local/bin/millstone`.
    pub binary_path: Option<String>,

    /// Path to the millstone configuration file to use.
    ///
    /// This file is mapped into the millstone container and so it must exist on the system where this command is run
    /// from.
    pub config_path: PathBuf,
}

/// datadog-intake configuration.
#[derive(Clone)]
pub struct DatadogIntakeConfig {
    /// Container image to use.
    ///
    /// This must be a valid image reference -- `datadog-intake:x.y.z`,
    /// `registry.ddbuild.io/saluki/datadog-intake:x.y.z`, etc -- to an image containing the `datadog-intake` binary.
    pub image: String,

    /// Path to the datadog-intake binary.
    ///
    /// Defaults to `/usr/local/bin/datadog-intake`.
    pub binary_path: Option<String>,

    /// Path to the datadog-intake configuration file to use.
    ///
    /// This file is mapped into the datadog-intake container and so it must exist on the system where this command is run
    /// from.
    pub config_path: PathBuf,
}

/// Target configuration.
#[derive(Clone)]
pub struct TargetConfig {
    /// Container image to use.
    ///
    /// This must be a valid image reference -- `agent-data-plane:x.y.z`,
    /// `registry.ddbuild.io/saluki/agent-data-plane:x.y.z`, etc -- to an image containing the `agent-data-plane`
    /// binary.
    pub image: String,

    /// Entrypoint to execute.
    pub entrypoint: Vec<String>,

    /// Command to run.
    pub command: Vec<String>,

    /// Additional environment variables to be passed into the target container.
    ///
    /// These should be in the form of `KEY=VALUE`.
    pub additional_env_args: Vec<String>,
}
