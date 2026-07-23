use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use serde::Deserialize;
use stringtheory::MetaString;

use super::obfuscation::ObfuscationConfig;

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

    /// Obfuscation config read from flat `apm_obfuscation_*` keys.
    /// KEY_ALIASES in `crate::config` bridge the YAML nested paths to these flat keys.
    #[serde(default, flatten)]
    obfuscation: ObfuscationConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ApmConfig {
    /// Enables an additional stats computation check on spans to see if they have an eligible `span.kind` (server, consumer, client, producer).
    /// If enabled, a span with an eligible `span.kind` will have stats computed. If disabled, only top-level and measured spans will have stats computed.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_compute_stats_by_span_kind")]
    compute_stats_by_span_kind: bool,

    /// Enables aggregation of peer related tags (for example, `peer.service`, `db.instance`, etc.) in the Agent.
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
    /// Defaults to `"none"`.
    #[serde(default = "default_env")]
    default_env: MetaString,

    /// Default hostname to use when traces don't provide one.
    ///
    /// Defaults to empty string (no fallback).
    #[serde(skip)]
    hostname: MetaString,

    /// Obfuscation configuration for trace data.
    ///
    /// Populated from `ApmConfiguration.obfuscation` in `from_configuration`; not read from serde directly.
    #[serde(skip)]
    obfuscation: ObfuscationConfig,
}

impl ApmConfig {
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let wrapper = config.as_typed::<ApmConfiguration>()?;
        let mut apm_config = wrapper.apm_config;
        apm_config.obfuscation = wrapper.obfuscation;
        Ok(apm_config)
    }

    /// Returns `true` if stats computation by span kind is enabled.
    pub const fn compute_stats_by_span_kind(&self) -> bool {
        self.compute_stats_by_span_kind
    }

    /// Returns `true` if peer tags aggregation is enabled.
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

    /// Sets the hostname if it's currently empty.
    pub fn set_hostname_if_empty(&mut self, hostname: impl Into<MetaString>) {
        if self.hostname.is_empty() {
            self.hostname = hostname.into();
        }
    }

    /// Returns the obfuscation configuration.
    pub fn obfuscation(&self) -> &ObfuscationConfig {
        &self.obfuscation
    }
}

impl Default for ApmConfig {
    fn default() -> Self {
        Self {
            compute_stats_by_span_kind: default_compute_stats_by_span_kind(),
            peer_tags_aggregation: default_peer_tags_aggregation(),
            peer_tags: Vec::new(),
            default_env: default_env(),
            hostname: MetaString::default(),
            obfuscation: ObfuscationConfig::default(),
        }
    }
}
