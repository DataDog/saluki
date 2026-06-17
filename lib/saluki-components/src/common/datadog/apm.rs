use saluki_component_config::traces as leaf;
use stringtheory::MetaString;

const fn default_rare_sampler_tps() -> f64 {
    5.0
}

const fn default_rare_sampler_cooldown_secs() -> f64 {
    300.0 // 5 minutes
}

const fn default_rare_sampler_cardinality() -> usize {
    200
}

/// Rare sampler tuning configuration (`apm_config.rare_sampler.*`).
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
struct RareSamplerConfig {
    tps: f64,

    cooldown: f64,

    cardinality: usize,
}

impl RareSamplerConfig {
    fn from_native(cfg: &leaf::RareSamplerConfig) -> Self {
        Self {
            tps: cfg.tps,
            cooldown: cfg.cooldown,
            cardinality: cfg.cardinality,
        }
    }
}

impl Default for RareSamplerConfig {
    fn default() -> Self {
        Self {
            tps: default_rare_sampler_tps(),
            cooldown: default_rare_sampler_cooldown_secs(),
            cardinality: default_rare_sampler_cardinality(),
        }
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
struct ProbabilisticSamplerConfig {
    enabled: bool,

    sampling_percentage: f64,
}

impl ProbabilisticSamplerConfig {
    fn from_native(cfg: &leaf::ProbabilisticSamplerConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            sampling_percentage: cfg.sampling_percentage,
        }
    }
}

impl Default for ProbabilisticSamplerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sampling_percentage: 100.0,
        }
    }
}

/// APM configuration.
///
/// This configuration mirrors the Agent's trace agent configuration. It is a behavior-carrying
/// runtime type built from its leaf mirror via [`ApmConfig::from_native`]. The injected `hostname`
/// is environment-derived state, not configuration, and is set separately.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ApmConfig {
    target_traces_per_second: f64,

    errors_per_second: f64,

    probabilistic_sampler: ProbabilisticSamplerConfig,

    error_sampling_enabled: bool,

    error_tracking_standalone: bool,

    compute_stats_by_span_kind: bool,

    peer_tags_aggregation: bool,

    peer_tags: Vec<MetaString>,

    default_env: MetaString,

    /// Default hostname to use when traces don't provide one.
    ///
    /// Injected environment-derived state; defaults to empty (no fallback).
    hostname: MetaString,

    enable_rare_sampler: bool,

    rare_sampler: RareSamplerConfig,
}

impl ApmConfig {
    /// Builds the runtime APM configuration from its leaf mirror.
    ///
    /// The injected `hostname` starts empty; callers set it via [`set_hostname_if_empty`].
    ///
    /// [`set_hostname_if_empty`]: ApmConfig::set_hostname_if_empty
    pub fn from_native(cfg: &leaf::ApmConfig) -> Self {
        Self {
            target_traces_per_second: cfg.target_traces_per_second,
            errors_per_second: cfg.errors_per_second,
            probabilistic_sampler: ProbabilisticSamplerConfig::from_native(&cfg.probabilistic_sampler),
            error_sampling_enabled: cfg.error_sampling_enabled,
            error_tracking_standalone: cfg.error_tracking_standalone,
            compute_stats_by_span_kind: cfg.compute_stats_by_span_kind,
            peer_tags_aggregation: cfg.peer_tags_aggregation,
            peer_tags: cfg.peer_tags.clone(),
            default_env: cfg.default_env.clone(),
            hostname: MetaString::default(),
            enable_rare_sampler: cfg.enable_rare_sampler,
            rare_sampler: RareSamplerConfig::from_native(&cfg.rare_sampler),
        }
    }

    /// Returns the target traces per second for priority sampling.
    pub const fn target_traces_per_second(&self) -> f64 {
        self.target_traces_per_second
    }

    /// Returns the target traces per second for error sampling.
    pub const fn errors_per_second(&self) -> f64 {
        self.errors_per_second
    }

    /// Returns `true` if probabilistic sampling is enabled.
    pub const fn probabilistic_sampler_enabled(&self) -> bool {
        self.probabilistic_sampler.enabled
    }

    /// Returns the probabilistic sampler sampling percentage.
    pub const fn probabilistic_sampler_sampling_percentage(&self) -> f64 {
        self.probabilistic_sampler.sampling_percentage
    }

    /// Returns `true` if error sampling is enabled.
    pub const fn error_sampling_enabled(&self) -> bool {
        self.error_sampling_enabled
    }

    /// Returns `true` if error tracking standalone mode is enabled.
    pub const fn error_tracking_standalone_enabled(&self) -> bool {
        self.error_tracking_standalone
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

    /// Returns `true` if the rare sampler is enabled.
    pub const fn rare_sampler_enabled(&self) -> bool {
        self.enable_rare_sampler
    }

    /// Returns the rare sampler target traces per second.
    pub const fn rare_sampler_tps(&self) -> f64 {
        self.rare_sampler.tps
    }

    /// Returns the rare sampler cooldown period in seconds.
    pub const fn rare_sampler_cooldown_period_secs(&self) -> f64 {
        self.rare_sampler.cooldown
    }

    /// Returns the rare sampler cardinality limit per shard.
    pub const fn rare_sampler_cardinality(&self) -> usize {
        self.rare_sampler.cardinality
    }
}

impl Default for ApmConfig {
    fn default() -> Self {
        Self::from_native(&leaf::ApmConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use saluki_component_config::traces as leaf;

    use super::*;

    #[test]
    fn from_native_maps_fields_and_leaves_hostname_empty() {
        let mut leaf_cfg = leaf::ApmConfig {
            enable_rare_sampler: true,
            error_tracking_standalone: true,
            ..Default::default()
        };
        leaf_cfg.rare_sampler.tps = 10.0;
        leaf_cfg.rare_sampler.cooldown = 60.0;
        leaf_cfg.rare_sampler.cardinality = 100;

        let config = ApmConfig::from_native(&leaf_cfg);

        assert!(config.rare_sampler_enabled());
        assert!(config.error_tracking_standalone_enabled());
        assert_eq!(config.rare_sampler_tps(), 10.0);
        assert_eq!(config.rare_sampler_cooldown_period_secs(), 60.0);
        assert_eq!(config.rare_sampler_cardinality(), 100);
        assert!(config.hostname().is_empty());
    }

    #[test]
    fn set_hostname_if_empty_only_sets_once() {
        let mut config = ApmConfig::from_native(&leaf::ApmConfig::default());
        config.set_hostname_if_empty("host-a");
        assert_eq!(config.hostname().as_ref(), "host-a");
        config.set_hostname_if_empty("host-b");
        assert_eq!(config.hostname().as_ref(), "host-a");
    }
}
