use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use serde::Deserialize;
use stringtheory::MetaString;

const fn default_target_traces_per_second() -> f64 {
    10.0
}

const fn default_errors_per_second() -> f64 {
    10.0
}
const fn default_peer_tags_aggregation() -> bool {
    true
}

const fn default_compute_stats_by_span_kind() -> bool {
    true
}

fn default_env() -> MetaString {
    MetaString::from("none")
}

/// APM configuration.
///
/// This configuration mirrors the Agent's trace agent configuration..
#[derive(Clone, Debug, Deserialize)]
struct ApmConfiguration {
    #[serde(default)]
    apm_config: ApmConfig,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ApmConfig {
    /// Target traces per second for priority sampling.
    ///
    /// Defaults to 10.0.
    #[serde(default = "default_target_traces_per_second")]
    target_traces_per_second: f64,

    /// Target traces per second for error sampling.
    ///
    /// Defaults to 10.0.
    #[serde(default = "default_errors_per_second")]
    errors_per_second: f64,

    /// Enables an additional stats computation check on spans to see if they have an eligible `span.kind` (server, consumer, client, producer).
    /// If enabled, a span with an eligible `span.kind` will have stats computed. If disabled, only top-level and measured spans will have stats computed.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_compute_stats_by_span_kind")]
    compute_stats_by_span_kind: bool,

    /// Enables aggregation of peer related tags (e.g., `peer.service`, `db.instance`, etc.) in the Agent.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_peer_tags_aggregation")]
    peer_tags_aggregation: bool,

    /// Optional list of supplementary peer tags that go beyond the defaults. The Datadog backend validates all tags
    /// and will drop ones that are unapproved.
    ///
    /// Defaults to an empty list.
    #[serde(default)]
    peer_tags: Vec<MetaString>,

    /// Default environment to use when traces don't provide one.
    ///
    /// Defaults to "none".
    #[serde(default = "default_env")]
    default_env: MetaString,

    /// Default hostname to use when traces don't provide one.
    ///
    /// Defaults to empty string (no fallback).
    #[serde(skip)]
    hostname: MetaString,
}

impl ApmConfig {
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let wrapper = config.as_typed::<ApmConfiguration>()?;
        Ok(wrapper.apm_config)
    }

    /// Returns the target traces per second for priority sampling.
    pub const fn target_traces_per_second(&self) -> f64 {
        self.target_traces_per_second
    }

    /// Returns the target traces per second for error sampling.
    pub const fn errors_per_second(&self) -> f64 {
        self.errors_per_second
    }

    /// Returns if stats computation by span kind is enabled.
    pub const fn compute_stats_by_span_kind(&self) -> bool {
        self.compute_stats_by_span_kind
    }

    /// Returns if peer tags aggregation is enabled.
    pub const fn peer_tags_aggregation(&self) -> bool {
        self.peer_tags_aggregation
    }

    /// Returns the supplementary peer tags.
    pub fn peer_tags(&self) -> &[MetaString] {
        &self.peer_tags
    }

    /// Returns the default environment to use when traces don't provide one.
    pub fn default_env(&self) -> &MetaString {
        &self.default_env
    }

    /// Returns the default hostname to use when traces don't provide one.
    pub fn hostname(&self) -> &MetaString {
        &self.hostname
    }

    /// Sets the hostname if it is currently empty.
    pub fn set_hostname_if_empty(&mut self, hostname: impl Into<MetaString>) {
        if self.hostname.is_empty() {
            self.hostname = hostname.into();
        }
    }
}

impl Default for ApmConfig {
    fn default() -> Self {
        Self {
            target_traces_per_second: default_target_traces_per_second(),
            errors_per_second: default_errors_per_second(),
            compute_stats_by_span_kind: default_compute_stats_by_span_kind(),
            peer_tags_aggregation: default_peer_tags_aggregation(),
            peer_tags: Vec::new(),
            default_env: default_env(),
            hostname: MetaString::default(),
        }
    }
}
