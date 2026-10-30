//! Inputs of the DogStatsD filters and the Multi-Region Failover metric filter.

use agent_data_plane_config::domains::{dogstatsd, multi_region_failover};

use crate::components::{
    dogstatsd_filterlist::{MetricNameFilter, MetricPrefixRule},
    metric_filter::MetricMirroring,
    tag_filterlist::{FilterAction, MetricTagFilterEntry, MetricTagValueAllowlistEntry, TagValueMismatchAction},
};

/// Builds the metric names or prefixes the DogStatsD filters drop.
pub(crate) fn metric_name_filter(filter: &dogstatsd::MetricFilter) -> MetricNameFilter {
    MetricNameFilter {
        values: filter.values.clone(),
        match_prefix: filter.match_prefix,
        prefix_rules: filter
            .prefix_rules
            .iter()
            .map(|rule| MetricPrefixRule {
                prefix: rule.prefix.clone(),
                except_prefix: rule.except_prefix.clone(),
                except_exact: rule.except_exact.clone(),
            })
            .collect(),
    }
}

/// Builds the whole-tag filter entries.
pub(crate) fn tag_filter_entries(entries: &[dogstatsd::MetricTagFilterEntry]) -> Vec<MetricTagFilterEntry> {
    entries
        .iter()
        .map(|entry| MetricTagFilterEntry {
            metric_name: entry.metric_name.clone(),
            action: match entry.action {
                dogstatsd::FilterAction::Include => FilterAction::Include,
                dogstatsd::FilterAction::Exclude => FilterAction::Exclude,
            },
            tags: entry.tags.clone(),
        })
        .collect()
}

/// Builds the tag value allow-list entries, which the configuration layer has already validated.
pub(crate) fn tag_value_allowlist(
    entries: &[dogstatsd::MetricTagValueAllowlistEntry],
) -> Vec<MetricTagValueAllowlistEntry> {
    entries
        .iter()
        .map(|entry| MetricTagValueAllowlistEntry {
            metric_prefix: entry.metric_prefix.clone(),
            tag_name: entry.tag_name.clone(),
            values: entry.values.clone(),
            on_miss: match entry.on_miss {
                dogstatsd::TagValueMismatchAction::Remove => TagValueMismatchAction::Remove,
                dogstatsd::TagValueMismatchAction::Replace => TagValueMismatchAction::Replace,
            },
            replacement: entry.replacement.clone(),
        })
        .collect()
}

/// Builds the live Multi-Region Failover metric mirroring settings.
pub(crate) fn metric_mirroring(mirroring: &multi_region_failover::MetricMirroring) -> MetricMirroring {
    MetricMirroring {
        enabled: mirroring.enabled,
        allowlist: mirroring.allowlist.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_name_filter_comes_from_resolved_configuration() {
        let filter = metric_name_filter(&dogstatsd::MetricFilter {
            values: vec!["blocked.metric".to_string()],
            match_prefix: true,
            prefix_rules: vec![dogstatsd::MetricPrefixRule {
                prefix: "dropped.".to_string(),
                except_prefix: vec!["dropped.keep.".to_string()],
                except_exact: vec!["dropped.exact".to_string()],
            }],
        });

        assert_eq!(
            MetricNameFilter {
                values: vec!["blocked.metric".to_string()],
                match_prefix: true,
                prefix_rules: vec![MetricPrefixRule {
                    prefix: "dropped.".to_string(),
                    except_prefix: vec!["dropped.keep.".to_string()],
                    except_exact: vec!["dropped.exact".to_string()],
                }],
            },
            filter
        );
    }

    #[test]
    fn tag_filter_entries_come_from_resolved_configuration() {
        let entries = tag_filter_entries(&[
            dogstatsd::MetricTagFilterEntry {
                metric_name: "included.metric".to_string(),
                action: dogstatsd::FilterAction::Include,
                tags: vec!["env".to_string()],
            },
            dogstatsd::MetricTagFilterEntry {
                metric_name: "excluded.metric".to_string(),
                action: dogstatsd::FilterAction::Exclude,
                tags: vec!["host".to_string()],
            },
        ]);

        assert_eq!(
            vec![
                MetricTagFilterEntry {
                    metric_name: "included.metric".to_string(),
                    action: FilterAction::Include,
                    tags: vec!["env".to_string()],
                },
                MetricTagFilterEntry {
                    metric_name: "excluded.metric".to_string(),
                    action: FilterAction::Exclude,
                    tags: vec!["host".to_string()],
                },
            ],
            entries
        );
    }

    #[test]
    fn tag_value_allowlist_comes_from_resolved_configuration() {
        let entries = tag_value_allowlist(&[
            dogstatsd::MetricTagValueAllowlistEntry {
                metric_prefix: "removed.".to_string(),
                tag_name: "customer_id".to_string(),
                values: vec!["top-1".to_string()],
                on_miss: dogstatsd::TagValueMismatchAction::Remove,
                replacement: "unused".to_string(),
            },
            dogstatsd::MetricTagValueAllowlistEntry {
                metric_prefix: "replaced.".to_string(),
                tag_name: "region".to_string(),
                values: vec!["us-east-1".to_string()],
                on_miss: dogstatsd::TagValueMismatchAction::Replace,
                replacement: "other".to_string(),
            },
        ]);

        assert_eq!(
            vec![
                MetricTagValueAllowlistEntry {
                    metric_prefix: "removed.".to_string(),
                    tag_name: "customer_id".to_string(),
                    values: vec!["top-1".to_string()],
                    on_miss: TagValueMismatchAction::Remove,
                    replacement: "unused".to_string(),
                },
                MetricTagValueAllowlistEntry {
                    metric_prefix: "replaced.".to_string(),
                    tag_name: "region".to_string(),
                    values: vec!["us-east-1".to_string()],
                    on_miss: TagValueMismatchAction::Replace,
                    replacement: "other".to_string(),
                },
            ],
            entries
        );
    }

    #[test]
    fn metric_mirroring_comes_from_resolved_configuration() {
        let mirroring = metric_mirroring(&multi_region_failover::MetricMirroring {
            enabled: true,
            allowlist: vec!["mirrored.metric".to_string()],
        });

        assert_eq!(
            MetricMirroring {
                enabled: true,
                allowlist: vec!["mirrored.metric".to_string()],
            },
            mirroring
        );
    }
}
