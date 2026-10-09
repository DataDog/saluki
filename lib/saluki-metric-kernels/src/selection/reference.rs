//! Frozen copies of the native name matching that the selections in this module replace.
//!
//! The functions below are copied verbatim, with only their imports and visibility adjusted, the configuration types
//! they read reduced to the fields they use, and comments linking another product's source left out, from:
//!
//! - `agent-data-plane`'s metric filter (`metric_filter`), which matches endpoint allowlists and the multi-region
//!   failover allowlist, with the prefix compaction it used from `saluki-components`
//!   (`config::metrics_endpoint_routing::compact_metric_prefixes`);
//! - `saluki-components`' autoscaling failover gateway;
//! - `agent-data-plane`'s DogStatsD post-aggregate filter (`dogstatsd_post_aggregate_filter`).
//!
//! Component builders, telemetry, live views, and memory bounds are left out, since they carry no matching semantics;
//! [`allowlist_sizes`] and [`DogStatsDPostAggregateFilter::histogram_values`] expose what the telemetry and the matcher
//! were built from. The property tests compare the selections with these copies, so that they keep the native behavior
//! while the legacy components call them. Do not change them.

use std::collections::HashSet;

use saluki_core::data_model::event::{
    metric::{Metric, MetricValues},
    Event,
};
use saluki_error::{generic_error, GenericError};
use stringtheory::MetaString;

use crate::{
    intake_name::{is_normalized, normalize_into, NameBuf},
    matcher::SortedSetMatcher,
};

// From `saluki-components`' endpoint routing configuration.

pub(super) fn compact_metric_prefixes(prefixes: &mut Vec<String>) {
    prefixes.sort_unstable();
    if !prefixes.is_empty() {
        let mut retained = 0;
        for next in 1..prefixes.len() {
            if !prefixes[next].starts_with(&prefixes[retained]) {
                retained += 1;
                prefixes.swap(retained, next);
            }
        }
        prefixes.truncate(retained + 1);
    }
}

// From `agent-data-plane-config`'s multi-region failover domain.

pub(super) struct MetricMirroring {
    pub(super) enabled: bool,
    pub(super) allowlist: Vec<String>,
}

// From `agent-data-plane`'s metric filter.

pub(super) enum Filter {
    DropAll,
    All,
    MrfAllowlist(HashSet<String>),
    Allowlist {
        names: HashSet<Vec<u8>>,
        prefixes: Vec<String>,
    },
}

impl Filter {
    pub(super) fn for_allowlist(names: &[String], prefixes: &[String]) -> Self {
        let mut prefixes = prefixes.to_vec();
        // Remove covered prefixes so a binary search only needs to check its predecessor.
        compact_metric_prefixes(&mut prefixes);
        Self::Allowlist {
            names: names.iter().map(|name| name.as_bytes().to_vec()).collect(),
            prefixes,
        }
    }

    pub(super) fn for_mrf(enabled: bool, routing: &MetricMirroring) -> Self {
        if !enabled || !routing.enabled {
            Self::DropAll
        } else if routing.allowlist.is_empty() {
            Self::All
        } else {
            Self::MrfAllowlist(routing.allowlist.iter().cloned().collect())
        }
    }

    pub(super) fn should_forward(&self, event: &Event) -> bool {
        let Event::Metric(metric) = event else {
            return false;
        };
        match self {
            Self::DropAll => false,
            Self::All => true,
            Self::MrfAllowlist(names) => names.contains(metric.context().name().as_ref()),
            Self::Allowlist { names, prefixes } => {
                let name = metric.context().name().as_ref();
                if is_normalized(name) {
                    return Self::matches_allowlist(name.as_bytes(), names, prefixes);
                }
                let mut buf = NameBuf::new();
                match normalize_into(&mut buf, name) {
                    Some(normalized) => Self::matches_allowlist(normalized, names, prefixes),
                    None => false,
                }
            }
        }
    }

    fn matches_allowlist(name: &[u8], names: &HashSet<Vec<u8>>, prefixes: &[String]) -> bool {
        if names.contains(name) {
            return true;
        }
        match prefixes.binary_search_by(|prefix| prefix.as_bytes().cmp(name)) {
            Ok(_) => true,
            Err(index) => index > 0 && name.starts_with(prefixes[index - 1].as_bytes()),
        }
    }
}

/// The sizes the metric filter's telemetry reported for an allowlist: stored exact names and compacted prefixes.
pub(super) fn allowlist_sizes(filter: &Filter) -> Option<(usize, usize)> {
    match filter {
        Filter::Allowlist { names, prefixes } => Some((names.len(), prefixes.len())),
        Filter::DropAll | Filter::All | Filter::MrfAllowlist(_) => None,
    }
}

// From `saluki-components`' autoscaling failover gateway and its configuration.

pub(super) struct AutoscalingFailoverConfiguration {
    pub(super) enabled: bool,
    pub(super) metrics: Vec<String>,
}

impl AutoscalingFailoverConfiguration {
    fn is_branch_requested(&self) -> bool {
        self.enabled && !self.metrics.is_empty()
    }

    fn metrics(&self) -> &[String] {
        &self.metrics
    }
}

enum GatewayMode {
    Inactive,
    FilteredForward { allowlist: HashSet<String> },
}

pub(super) struct AutoscalingFailoverGateway {
    mode: GatewayMode,
}

impl AutoscalingFailoverGateway {
    pub(super) fn new(failover_config: AutoscalingFailoverConfiguration) -> Self {
        let mode = Self::mode_for_config(&failover_config);

        Self { mode }
    }

    fn mode_for_config(failover_config: &AutoscalingFailoverConfiguration) -> GatewayMode {
        if !failover_config.is_branch_requested() {
            GatewayMode::Inactive
        } else {
            GatewayMode::FilteredForward {
                allowlist: failover_config.metrics().iter().cloned().collect(),
            }
        }
    }

    pub(super) fn should_forward(&self, event: &Event) -> bool {
        let GatewayMode::FilteredForward { allowlist } = &self.mode else {
            return false;
        };
        let Event::Metric(metric) = event else {
            return false;
        };
        if !matches!(
            metric.values(),
            MetricValues::Counter(..) | MetricValues::Rate(..) | MetricValues::Gauge(..) | MetricValues::Set(..)
        ) {
            return false;
        }

        allowlist.contains(metric.context().name().as_ref())
    }
}

// From `agent-data-plane`'s DogStatsD post-aggregate filter.

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HistogramSuffixes {
    values: Vec<MetaString>,
}

impl HistogramSuffixes {
    pub(super) fn try_new(aggregates: &[String], percentiles: &[String]) -> Result<Self, GenericError> {
        let mut values = aggregates
            .iter()
            .map(|aggregate| MetaString::from(aggregate.as_str()))
            .collect::<Vec<_>>();

        for percentile in percentiles {
            let quantile = percentile
                .parse::<f64>()
                .map_err(|_| generic_error!("Invalid percentile: {}", percentile))?;
            if !(0.0..=1.0).contains(&quantile) {
                return Err(generic_error!("Percentile out of range: {}", percentile));
            }

            let suffix = format!("{}percentile", (quantile * 100.0 + 0.5) as u32);
            values.push(suffix.into());
        }

        Ok(Self { values })
    }

    fn contains_filter_entry(&self, value: &str) -> bool {
        self.values.iter().any(|suffix| {
            let suffix: &str = suffix.as_ref();
            value
                .strip_suffix(suffix)
                .map(|prefix| prefix.ends_with('.'))
                .unwrap_or(false)
        })
    }
}

fn is_scalar_series_metric(metric: &Metric) -> bool {
    matches!(
        metric.values(),
        MetricValues::Counter(_) | MetricValues::Rate(_, _) | MetricValues::Gauge(_) | MetricValues::Set(_)
    )
}

pub(super) struct DogStatsDPostAggregateFilter {
    pub(super) matcher: SortedSetMatcher,
    pub(super) histogram_suffixes: HistogramSuffixes,
}

impl DogStatsDPostAggregateFilter {
    /// `sync_matcher`, with the live filterlist's current value passed in.
    pub(super) fn sync_matcher(&mut self, values: &[String], match_prefix: bool) {
        let histogram_values = values
            .iter()
            .filter(|value| self.histogram_suffixes.contains_filter_entry(value))
            .cloned()
            .collect::<Vec<_>>();

        self.matcher = SortedSetMatcher::new(histogram_values.iter().map(String::as_str), match_prefix);
    }

    /// The entries `sync_matcher` keeps, in order.
    pub(super) fn histogram_values<'a>(&self, values: &'a [String]) -> Vec<&'a str> {
        values
            .iter()
            .filter(|value| self.histogram_suffixes.contains_filter_entry(value))
            .map(String::as_str)
            .collect()
    }

    pub(super) fn should_filter_metric(&self, metric: &Metric) -> bool {
        is_scalar_series_metric(metric) && self.matcher.matches_intake_name(metric.context().name())
    }
}
