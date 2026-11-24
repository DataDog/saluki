//! Metric routing filter predicates for endpoint allowlists, mirroring, and failover branches.

use std::collections::HashSet;

use saluki_core::data_model::event::metric::{Metric, MetricValues};
use serde::{Deserialize, Serialize};

use crate::metric_name::{is_normalized, normalize_into, NameBuf};

/// Sorts `prefixes` and removes any prefix already covered by a shorter prefix.
pub fn compact_metric_prefixes(prefixes: &mut Vec<String>) {
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

/// Mirroring policy settings for a failover metric branch.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MetricMirroring {
    /// Whether metrics are mirrored to the failover destination.
    pub enabled: bool,

    /// Metric names permitted to be mirrored, matched verbatim. While mirroring is enabled, an empty list mirrors
    /// every metric.
    pub allowlist: Vec<String>,
}

/// Compiled metric routing filter predicate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetricAllowlistKernel {
    /// Drops every metric.
    DropAll,

    /// Forwards every metric unchanged.
    AllowAll,

    /// Forwards metrics whose raw name is in `names`.
    ExactAllowlist {
        /// Raw metric names permitted through.
        names: HashSet<String>,
    },

    /// Forwards metrics whose normalized name matches an exact entry or compacted prefix.
    NormalizedAllowlist {
        /// Exact normalized names permitted through.
        names: HashSet<Vec<u8>>,
        /// Sorted, compacted prefixes permitted through.
        prefixes: Vec<String>,
    },

    /// Forwards only scalar series metrics (counter, rate, gauge, set) whose raw name is in `names`.
    SeriesAllowlist {
        /// Raw metric names permitted through.
        names: HashSet<String>,
    },
}

impl MetricAllowlistKernel {
    /// Builds a normalized endpoint-routing allowlist kernel from exact names and prefixes.
    ///
    /// Empty `names` and `prefixes` drop every metric.
    pub fn for_allowlist(names: &[String], prefixes: &[String]) -> Self {
        let mut compacted_prefixes = prefixes.to_vec();
        compact_metric_prefixes(&mut compacted_prefixes);
        Self::NormalizedAllowlist {
            names: names.iter().map(|name| name.as_bytes().to_vec()).collect(),
            prefixes: compacted_prefixes,
        }
    }

    /// Builds a mirroring filter kernel.
    ///
    /// When `enabled` or `mirroring.enabled` is `false`, every metric is dropped. When both are `true` and
    /// `mirroring.allowlist` is empty, every metric is forwarded.
    pub fn for_mirroring(enabled: bool, mirroring: &MetricMirroring) -> Self {
        if !enabled || !mirroring.enabled {
            Self::DropAll
        } else if mirroring.allowlist.is_empty() {
            Self::AllowAll
        } else {
            Self::ExactAllowlist {
                names: mirroring.allowlist.iter().cloned().collect(),
            }
        }
    }

    /// Builds a scalar-series allowlist kernel for autoscaling failover.
    ///
    /// When `enabled` is `false` or `metrics` is empty, every metric is dropped.
    pub fn for_series_allowlist(enabled: bool, metrics: &[String]) -> Self {
        if !enabled || metrics.is_empty() {
            Self::DropAll
        } else {
            Self::SeriesAllowlist {
                names: metrics.iter().cloned().collect(),
            }
        }
    }

    /// Returns `true` if `metric` should be forwarded.
    pub fn should_forward(&self, metric: &Metric) -> bool {
        match self {
            Self::DropAll => false,
            Self::AllowAll => true,
            Self::ExactAllowlist { names } => names.contains(metric.context().name().as_ref()),
            Self::NormalizedAllowlist { names, prefixes } => {
                let name = metric.context().name().as_ref();
                if is_normalized(name) {
                    return matches_normalized_allowlist(name.as_bytes(), names, prefixes);
                }
                let mut buf = NameBuf::new();
                match normalize_into(&mut buf, name) {
                    Some(normalized) => matches_normalized_allowlist(normalized, names, prefixes),
                    None => false,
                }
            }
            Self::SeriesAllowlist { names } => {
                if !matches!(
                    metric.values(),
                    MetricValues::Counter(..)
                        | MetricValues::Rate(..)
                        | MetricValues::Gauge(..)
                        | MetricValues::Set(..)
                ) {
                    return false;
                }
                names.contains(metric.context().name().as_ref())
            }
        }
    }

    /// Returns `true` when this kernel unconditionally drops every metric.
    pub fn drops_all(&self) -> bool {
        matches!(self, Self::DropAll)
            || matches!(
                self,
                Self::NormalizedAllowlist { names, prefixes } if names.is_empty() && prefixes.is_empty()
            )
    }

    /// Returns `true` when this kernel unconditionally forwards every metric.
    pub fn allows_all(&self) -> bool {
        matches!(self, Self::AllowAll)
    }
}

fn matches_normalized_allowlist(name: &[u8], names: &HashSet<Vec<u8>>, prefixes: &[String]) -> bool {
    if names.contains(name) {
        return true;
    }
    match prefixes.binary_search_by(|prefix| prefix.as_bytes().cmp(name)) {
        Ok(_) => true,
        Err(index) => index > 0 && name.starts_with(prefixes[index - 1].as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_metric_prefixes_removes_covered_entries() {
        let mut prefixes = vec![
            "app.db.".to_string(),
            "app.".to_string(),
            "sys.".to_string(),
            "app.".to_string(),
        ];
        compact_metric_prefixes(&mut prefixes);
        assert_eq!(prefixes, vec!["app.", "sys."]);
    }

    #[test]
    fn normalized_allowlist_matches_exact_and_prefix() {
        let kernel = MetricAllowlistKernel::for_allowlist(&["my_metric".to_string()], &["app.".to_string()]);
        assert!(kernel.should_forward(&Metric::gauge("my-metric", 1.0)));
        assert!(kernel.should_forward(&Metric::gauge("app.latency", 1.0)));
        assert!(!kernel.should_forward(&Metric::gauge("other.metric", 1.0)));
    }

    #[test]
    fn mirroring_handles_disabled_empty_and_allowlist() {
        let disabled = MetricAllowlistKernel::for_mirroring(
            true,
            &MetricMirroring {
                enabled: false,
                allowlist: vec![],
            },
        );
        assert!(!disabled.should_forward(&Metric::gauge("anything", 1.0)));

        let all = MetricAllowlistKernel::for_mirroring(
            true,
            &MetricMirroring {
                enabled: true,
                allowlist: vec![],
            },
        );
        assert!(all.should_forward(&Metric::gauge("anything", 1.0)));

        let filtered = MetricAllowlistKernel::for_mirroring(
            true,
            &MetricMirroring {
                enabled: true,
                allowlist: vec!["only.this".to_string()],
            },
        );
        assert!(filtered.should_forward(&Metric::gauge("only.this", 1.0)));
        assert!(!filtered.should_forward(&Metric::gauge("other", 1.0)));
    }

    #[test]
    fn series_allowlist_drops_sketches_and_histograms() {
        let kernel = MetricAllowlistKernel::for_series_allowlist(true, &["cpu.usage".to_string()]);
        assert!(kernel.should_forward(&Metric::gauge("cpu.usage", 1.0)));
        assert!(kernel.should_forward(&Metric::counter("cpu.usage", 1.0)));
        assert!(!kernel.should_forward(&Metric::distribution("cpu.usage", 1.0)));
        assert!(!kernel.should_forward(&Metric::histogram("cpu.usage", 1.0)));
    }
}
