//! The closed registry of configuration bindings that plan parameters may name.
//!
//! A plan parameter bound to configuration names a binding, `{"config": "<binding>"}`, never a configuration key. This
//! module is the only place that maps binding names to the typed configuration model:
//!
//! - **Names are typed-model paths**, below the model's top-level section (`domains.dogstatsd.metric_filter.values` is
//!   `dogstatsd.metric_filter.values`), never Datadog keys: a Datadog key can have several sources with a precedence
//!   that translation has already resolved, such as `metric_filterlist` and the legacy `statsd_metric_blocklist`.
//!   Derived bindings, whose values combine several settings, have names of their own, such as
//!   `dogstatsd.post_aggregate_filter.values`.
//! - **Keyed bindings** have one instance per key, such as the lists of each endpoint routing group, keyed by the
//!   group's fingerprint.
//! - **Liveness follows native behavior, and the structure that follows live configuration.** `Live` bindings are the
//!   inputs a native component reads through a live view (`dogstatsd.metric_filter.*`, `dogstatsd.tag_filterlist`,
//!   and `multi_region_failover.metric_mirroring.*`), and the inputs of the output branches, which follow the current
//!   configuration since outputs became live (`metrics_endpoint_routing.group.*`, `multi_region_failover.enabled`,
//!   and `autoscaling_failover.metrics`); a derived binding is live when one of its inputs is. Every other binding is
//!   `Startup`: it resolves from the configuration the process started with, so a change to it takes effect only at
//!   restart, as natively.
//! - **Credentials are never bindable.** No binding reads an API key, a token, or a secret.
//!
//! Each binding's value is written as the parameter that names it reads it: configuration values are converted into
//! plan parameter values (durations as canonical duration strings, histogram percentiles as whole percents) and then
//! normalized by decoding and re-encoding them as the binding's type, so that every lowering of the same
//! configuration writes, and hashes, the same plan.

use agent_data_plane_config::SalukiConfiguration;
use saluki_components::config::MetricsEndpointRoutingConfiguration;
use saluki_metric_kernels::{params::HistogramParams, selection::histogram_statistic_entries};
use saluki_metric_plan::{
    canonical::MAX_SAFE_INTEGER,
    ir::{format_duration, BindingName, ParamType, ParamValue},
    operators::{
        encode_mapper_profiles, encode_tag_filter_rules, encode_tag_value_rules, HISTOGRAM_AGGREGATES, MAPPER_PROFILES,
        TAG_FILTER_RULES, TAG_VALUE_RULES,
    },
    validate::Liveness,
};
use serde_json::{json, Value};

use super::resources::routing_group_key;
use crate::{
    cli::run::dogstatsd_mapper_configuration,
    components::tag_filterlist::{tag_filter_rules, tag_value_rules},
};

const STRINGS: ParamType = ParamType::List(&ParamType::String);
const COUNT: ParamType = ParamType::Integer {
    min: 0,
    max: MAX_SAFE_INTEGER,
};
const POSITIVE_COUNT: ParamType = ParamType::Integer {
    min: 1,
    max: MAX_SAFE_INTEGER,
};
const HISTOGRAM_AGGREGATE_NAMES: ParamType = ParamType::List(&ParamType::Enum(HISTOGRAM_AGGREGATES));
const HISTOGRAM_PERCENTILES: ParamType = ParamType::List(&ParamType::Integer { min: 0, max: 100 });

/// The configuration snapshots bindings resolve from.
#[derive(Clone, Copy, Debug)]
pub struct Snapshots<'a> {
    /// The configuration the process started with, which `Startup` bindings, and the startup inputs of derived
    /// bindings, resolve from.
    pub startup: &'a SalukiConfiguration,

    /// The current configuration, which the live inputs of `Live` bindings resolve from.
    pub current: &'a SalukiConfiguration,
}

/// How a binding's value is resolved from configuration.
#[derive(Clone, Copy, Debug)]
pub enum Resolve {
    /// The value is read from one configuration snapshot: the startup one for a `Startup` binding, the current one for
    /// a `Live` binding.
    ///
    /// Returns `None` if the configuration has no value of the binding's type, such as histogram statistics that
    /// aggregation cannot compute.
    Value(fn(&SalukiConfiguration) -> Option<Value>),

    /// The value is derived from a live input in the current snapshot (the first argument) and from startup inputs in
    /// the startup snapshot (the second).
    Derived(fn(&SalukiConfiguration, &SalukiConfiguration) -> Option<Value>),

    /// The value of the instance with the given key is read from one configuration snapshot, as for
    /// [`Value`][Self::Value].
    ///
    /// Returns `None` if the configuration has no instance with that key.
    Keyed(fn(&SalukiConfiguration, &str) -> Option<Value>),
}

/// The keys a keyed binding has instances for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingKey {
    /// An endpoint routing group, keyed by its fingerprint: 16 lowercase hexadecimal digits (see
    /// [`routing_group_key`]).
    RoutingGroup,
}

impl BindingKey {
    /// The placeholder for the key in a keyed binding's name pattern.
    pub const PLACEHOLDER: &'static str = "{key}";

    /// Returns `true` if `key` is a valid key of this kind.
    pub fn accepts(self, key: &str) -> bool {
        match self {
            Self::RoutingGroup => {
                key.len() == 16 && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }
        }
    }

    /// Returns the keys the given configuration has instances for, in order: the current configuration's, for a live
    /// keyed binding.
    ///
    /// Returns no keys if the configuration's endpoint routing policies are invalid, which fails lowering anyway.
    pub fn keys(self, startup: &SalukiConfiguration) -> Vec<String> {
        match self {
            Self::RoutingGroup => routing_groups(startup)
                .map(|routing| {
                    routing
                        .policy_groups()
                        .iter()
                        .map(|group| routing_group_key(&group.endpoints))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// One entry of the binding registry.
#[derive(Clone, Copy, Debug)]
pub struct BindingSpec {
    /// The binding's name, or, for a keyed binding, its name pattern, with [`BindingKey::PLACEHOLDER`] where the key
    /// goes.
    pub name: &'static str,

    /// The keys of a keyed binding, or `None` for a binding with one instance.
    pub key: Option<BindingKey>,

    /// The type of the binding's value. A parameter bound to it must admit this type.
    pub ty: ParamType,

    /// Whether the value can change while the process runs.
    pub liveness: Liveness,

    /// How the value is resolved.
    pub resolve: Resolve,
}

impl BindingSpec {
    /// Returns the key of the instance `name` names, if it is this binding or one of its instances.
    ///
    /// For a binding with one instance, the key is empty.
    fn match_name<'n>(&self, name: &'n str) -> Option<&'n str> {
        match self.key {
            None => (name == self.name).then_some(""),
            Some(key) => {
                let (before, after) = self.name.split_once(BindingKey::PLACEHOLDER)?;
                let found = name.strip_prefix(before)?.strip_suffix(after)?;
                key.accepts(found).then_some(found)
            }
        }
    }

    /// Returns the name of this binding's instance with the given key.
    pub fn instance_name(&self, key: &str) -> String {
        self.name.replace(BindingKey::PLACEHOLDER, key)
    }

    /// Resolves the value of this binding's instance with the given key (empty for a binding with one instance).
    ///
    /// The value is written in its canonical form, such as a set in canonical order. A value that is not of the
    /// binding's type, such as an integer beyond the plan's integer range, is returned as it is, so that validating a
    /// plan that uses it reports the problem.
    ///
    /// Returns `None` if the configuration has no value for it, such as histogram statistics that aggregation cannot
    /// compute, or a duration no plan duration represents.
    pub fn resolve(&self, snapshots: Snapshots<'_>, key: &str) -> Option<Value> {
        let live = match self.liveness {
            Liveness::Live => snapshots.current,
            Liveness::Startup => snapshots.startup,
        };
        let value = match self.resolve {
            Resolve::Value(read) => read(live)?,
            Resolve::Derived(derive) => derive(live, snapshots.startup)?,
            Resolve::Keyed(read) => read(live, key)?,
        };
        Some(match ParamValue::decode(&self.ty, &value, self.name) {
            Ok(decoded) => decoded.encode(),
            Err(_) => value,
        })
    }
}

/// The binding registry: every configuration binding a plan parameter may name.
pub static BINDINGS: &[BindingSpec] = &[
    // The DogStatsD mapper.
    BindingSpec {
        name: "dogstatsd.mapper.profiles",
        key: None,
        ty: MAPPER_PROFILES,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(mapper_profiles),
    },
    BindingSpec {
        name: "dogstatsd.mapper.cache_size",
        key: None,
        ty: COUNT,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| Some(json!(config.domains.dogstatsd.mapper.cache_size))),
    },
    BindingSpec {
        name: "dogstatsd.mapper.string_interner_size_bytes",
        key: None,
        ty: POSITIVE_COUNT,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| Some(json!(config.domains.dogstatsd.mapper.string_interner_size_bytes.get()))),
    },
    // The namespace prefix.
    BindingSpec {
        name: "dogstatsd.prefix_filter.metric_namespace",
        key: None,
        ty: ParamType::String,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| Some(json!(config.domains.dogstatsd.prefix_filter.metric_namespace))),
    },
    BindingSpec {
        name: "dogstatsd.prefix_filter.metric_namespace_blocklist",
        key: None,
        ty: STRINGS,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| {
            Some(json!(config.domains.dogstatsd.prefix_filter.metric_namespace_blocklist))
        }),
    },
    // The metric filterlist: live, as the prefix filter and the post-aggregation filter read it natively.
    BindingSpec {
        name: "dogstatsd.metric_filter.values",
        key: None,
        ty: STRINGS,
        liveness: Liveness::Live,
        resolve: Resolve::Value(|config| Some(json!(config.domains.dogstatsd.metric_filter.values))),
    },
    BindingSpec {
        name: "dogstatsd.metric_filter.match_prefix",
        key: None,
        ty: ParamType::Bool,
        liveness: Liveness::Live,
        resolve: Resolve::Value(|config| Some(json!(config.domains.dogstatsd.metric_filter.match_prefix))),
    },
    // The filterlist entries the post-aggregation filter matches: those naming a histogram statistic's series. Live
    // with the filterlist; the statistics are read at startup, as natively.
    BindingSpec {
        name: "dogstatsd.post_aggregate_filter.values",
        key: None,
        ty: STRINGS,
        liveness: Liveness::Live,
        resolve: Resolve::Derived(post_aggregate_filter_values),
    },
    // Tag projection.
    BindingSpec {
        name: "dogstatsd.tag_filterlist",
        key: None,
        ty: TAG_FILTER_RULES,
        liveness: Liveness::Live,
        resolve: Resolve::Value(|config| {
            Some(encode_tag_filter_rules(&tag_filter_rules(
                &config.domains.dogstatsd.tag_filterlist,
            )))
        }),
    },
    BindingSpec {
        name: "dogstatsd.tag_value_allowlist",
        key: None,
        ty: TAG_VALUE_RULES,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| {
            Some(encode_tag_value_rules(&tag_value_rules(
                &config.domains.dogstatsd.tag_value_allowlist,
            )))
        }),
    },
    BindingSpec {
        name: "dogstatsd.aggregation.aggregator_tag_filter_cache_capacity",
        key: None,
        ty: COUNT,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| {
            Some(json!(
                config
                    .domains
                    .dogstatsd
                    .aggregation
                    .aggregator_tag_filter_cache_capacity
            ))
        }),
    },
    // Aggregation, and the window the timestamp split shares with it.
    BindingSpec {
        name: "dogstatsd.aggregation.window_duration_seconds",
        key: None,
        ty: ParamType::Duration,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| seconds(config.domains.dogstatsd.aggregation.window_duration_seconds.get())),
    },
    BindingSpec {
        name: "dogstatsd.aggregation.flush_interval",
        key: None,
        ty: ParamType::Duration,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| {
            format_duration(config.domains.dogstatsd.aggregation.flush_interval).map(Value::String)
        }),
    },
    BindingSpec {
        name: "dogstatsd.aggregation.context_limit",
        key: None,
        ty: COUNT,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| Some(json!(config.domains.dogstatsd.aggregation.context_limit))),
    },
    // An unset or zero expiry never expires counters, which the parameter writes as `0s`.
    BindingSpec {
        name: "dogstatsd.aggregation.counter_expiry_seconds",
        key: None,
        ty: ParamType::Duration,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| {
            seconds(
                config
                    .domains
                    .dogstatsd
                    .aggregation
                    .counter_expiry_seconds
                    .unwrap_or_default(),
            )
        }),
    },
    BindingSpec {
        name: "dogstatsd.aggregation.flush_open_windows",
        key: None,
        ty: ParamType::Bool,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| Some(json!(config.domains.dogstatsd.aggregation.flush_open_windows))),
    },
    // Histogram statistics, each listed once, in their configured order (see `histogram_params`).
    BindingSpec {
        name: "metrics_encoding.histogram.aggregates",
        key: None,
        ty: HISTOGRAM_AGGREGATE_NAMES,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| {
            let (histogram, _) = histogram_params(config).ok()?;
            Some(json!(histogram
                .aggregates
                .iter()
                .map(|aggregate| aggregate.as_str())
                .collect::<Vec<_>>()))
        }),
    },
    BindingSpec {
        name: "metrics_encoding.histogram.percentiles",
        key: None,
        ty: HISTOGRAM_PERCENTILES,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| Some(json!(histogram_params(config).ok()?.0.percentiles))),
    },
    BindingSpec {
        name: "metrics_encoding.histogram.copy_to_distribution",
        key: None,
        ty: ParamType::Bool,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| Some(json!(config.shared.metrics_encoding.histogram.copy_to_distribution))),
    },
    BindingSpec {
        name: "metrics_encoding.histogram.copy_to_distribution_prefix",
        key: None,
        ty: ParamType::String,
        liveness: Liveness::Startup,
        resolve: Resolve::Value(|config| {
            Some(json!(
                config.shared.metrics_encoding.histogram.copy_to_distribution_prefix
            ))
        }),
    },
    // Endpoint routing groups: the canonical lists of each group, as the group's filter matches them. Live, as the
    // groups are.
    BindingSpec {
        name: "metrics_endpoint_routing.group.{key}.names",
        key: Some(BindingKey::RoutingGroup),
        ty: STRINGS,
        liveness: Liveness::Live,
        resolve: Resolve::Keyed(|config, key| routing_group(config, key).map(|group| json!(group.metric_allowlist))),
    },
    BindingSpec {
        name: "metrics_endpoint_routing.group.{key}.prefixes",
        key: Some(BindingKey::RoutingGroup),
        ty: STRINGS,
        liveness: Liveness::Live,
        resolve: Resolve::Keyed(|config, key| {
            routing_group(config, key).map(|group| json!(group.metric_prefix_allowlist))
        }),
    },
    // Multi-region failover: whether it is enabled is live, as the branch, which exists only while it is; metric
    // mirroring is live, as natively.
    BindingSpec {
        name: "multi_region_failover.enabled",
        key: None,
        ty: ParamType::Bool,
        liveness: Liveness::Live,
        resolve: Resolve::Value(|config| Some(json!(config.domains.multi_region_failover.enabled))),
    },
    BindingSpec {
        name: "multi_region_failover.metric_mirroring.enabled",
        key: None,
        ty: ParamType::Bool,
        liveness: Liveness::Live,
        resolve: Resolve::Value(|config| Some(json!(config.domains.multi_region_failover.metric_mirroring.enabled))),
    },
    BindingSpec {
        name: "multi_region_failover.metric_mirroring.allowlist",
        key: None,
        ty: STRINGS,
        liveness: Liveness::Live,
        resolve: Resolve::Value(|config| Some(json!(config.domains.multi_region_failover.metric_mirroring.allowlist))),
    },
    // Autoscaling failover: live, as the branch, which exists only while its list is not empty.
    BindingSpec {
        name: "autoscaling_failover.metrics",
        key: None,
        ty: STRINGS,
        liveness: Liveness::Live,
        resolve: Resolve::Value(|config| Some(json!(config.shared.autoscaling_failover.metrics))),
    },
];

/// Returns the registry entry for the binding `name`, and the key of the instance it names (empty for a binding with
/// one instance), if the registry has it.
pub fn lookup(name: &BindingName) -> Option<(&'static BindingSpec, String)> {
    let name = name.to_string();
    BINDINGS
        .iter()
        .find_map(|spec| spec.match_name(&name).map(|key| (spec, key.to_string())))
}

/// Resolves the value of the binding `name`.
///
/// Returns `None` if the registry has no such binding, or if the configuration has no valid value for it.
pub fn resolve(name: &BindingName, snapshots: Snapshots<'_>) -> Option<Value> {
    let (spec, key) = lookup(name)?;
    spec.resolve(snapshots, &key)
}

/// Returns the histogram statistics aggregation computes, as native configuration parses them, each listed once, and
/// the statistics that configuration repeats, in order.
///
/// Native aggregation computes a repeated statistic as many times as it is listed, emitting the same series more than
/// once per flush, and a plan lists each statistic once, so the first of each is kept. Percentiles that round to the
/// same whole percent are repeats too.
///
/// # Errors
///
/// Returns the native error for a statistic that aggregation cannot compute.
pub fn histogram_params(
    config: &SalukiConfiguration,
) -> Result<(HistogramParams, Vec<String>), saluki_metric_kernels::params::InvalidHistogramStatistic> {
    let histogram = &config.shared.metrics_encoding.histogram;
    let mut params = HistogramParams::from_names(
        &histogram.aggregates,
        &histogram.percentiles,
        histogram.copy_to_distribution,
        histogram.copy_to_distribution_prefix.clone(),
    )?;
    let mut repeated = Vec::new();
    let mut seen = Vec::new();
    params.aggregates.retain(|aggregate| {
        let first = !seen.contains(aggregate);
        if first {
            seen.push(*aggregate);
        } else {
            repeated.push(aggregate.as_str().to_string());
        }
        first
    });
    let mut seen = Vec::new();
    params.percentiles.retain(|percentile| {
        let first = !seen.contains(percentile);
        if first {
            seen.push(*percentile);
        } else {
            repeated.push(format!("{}percentile", percentile));
        }
        first
    });
    Ok((params, repeated))
}

fn seconds(seconds: u64) -> Option<Value> {
    format_duration(std::time::Duration::from_secs(seconds)).map(Value::String)
}

fn mapper_profiles(config: &SalukiConfiguration) -> Option<Value> {
    let profiles = dogstatsd_mapper_configuration(&config.domains.dogstatsd.mapper)
        .profiles()
        .ok()?;
    Some(encode_mapper_profiles(&profiles))
}

fn post_aggregate_filter_values(live: &SalukiConfiguration, startup: &SalukiConfiguration) -> Option<Value> {
    let (histogram, _) = histogram_params(startup).ok()?;
    Some(json!(histogram_statistic_entries(
        &live.domains.dogstatsd.metric_filter.values,
        &histogram
    )))
}

/// Returns the endpoint routing groups of the given configuration, as `run.rs` resolves them.
pub(super) fn routing_groups(
    config: &SalukiConfiguration,
) -> Result<MetricsEndpointRoutingConfiguration, saluki_error::GenericError> {
    let routing = &config.domains.metrics_endpoint_routing;
    MetricsEndpointRoutingConfiguration::from_configuration(
        &routing.metric_allowlists,
        &routing.metric_prefix_allowlists,
        &config.shared.endpoints,
    )
}

fn routing_group(
    config: &SalukiConfiguration, key: &str,
) -> Option<saluki_components::config::metrics_endpoint_routing::EndpointAllowlistGroup> {
    routing_groups(config)
        .ok()?
        .policy_groups()
        .iter()
        .find(|group| routing_group_key(&group.endpoints) == key)
        .cloned()
}

#[cfg(test)]
mod tests;
