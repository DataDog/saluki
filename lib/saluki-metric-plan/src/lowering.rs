//! Lowering native metric pipeline configuration into a complete, standalone [`MetricPlan`].
//!
//! Native configuration is treated as a front end: every policy setting (mapper profiles, namespace prefix, metric
//! filterlist, tag filterlist, value allowlists, aggregation window, histogram stats, endpoint routing allowlists) is
//! lowered as a literal inside the IR. Only host tags and live mirroring activation remain runtime bindings.

use std::num::NonZeroU64;

use saluki_metric_kernels::{
    filterlist::MetricNameFilter,
    histogram::HistogramConfiguration,
    mapper::DogStatsDMapperProfile,
    routing_filter::MetricMirroring,
    tag_filter::{MetricTagFilterEntry, MetricTagValueAllowlistEntry},
};

use crate::ir::{
    MetricPlan, MetricPlanBuilder, NodeId, Op, RouteFilterKind, SPLIT_AGGREGATE_PORT, SPLIT_PASSTHROUGH_PORT,
};

/// Default runtime binding name for host tags enrichment.
pub const HOST_TAGS_BINDING: &str = "host_tags";

/// Default runtime binding name for multi-region mirroring signal updates.
pub const MRF_SIGNAL_BINDING: &str = "mrf_mirroring";

/// Logical stream name for DogStatsD metrics.
pub const STREAM_DOGSTATSD: &str = "dogstatsd";

/// Logical stream name for OTLP metrics.
pub const STREAM_OTLP: &str = "otlp";

/// Logical stream name for Checks IPC metrics.
pub const STREAM_CHECKS: &str = "checks";

/// Logical stream name for liveness telemetry metrics.
pub const STREAM_LIVENESS: &str = "liveness";

/// Logical stream name for trace-sampler metrics.
pub const STREAM_TRACE_SAMPLER: &str = "trace_sampler";

/// Logical emit target name for the primary Datadog metrics encoder.
pub const TARGET_PRIMARY: &str = "primary";

/// Logical emit target name for the multi-region failover encoder.
pub const TARGET_MRF: &str = "mrf";

/// Logical emit target name for the autoscaling failover encoder.
pub const TARGET_AUTOSCALING_FAILOVER: &str = "autoscaling_failover";

/// Logical emit target name for DogStatsD post-aggregate client telemetry.
pub const TARGET_CLIENT_TELEMETRY: &str = "client_telemetry";

/// One endpoint-routing allowlist group lowered to a dedicated `RouteFilter -> Emit` branch.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RoutingPolicySpec {
    /// Logical emit target name (for example, `"routing_0"`).
    pub target: String,

    /// Exact metric names permitted through.
    pub metric_allowlist: Vec<String>,

    /// Literal metric-name prefixes permitted through.
    pub metric_prefix_allowlist: Vec<String>,
}

/// Model-independent snapshot of native metric pipeline settings for lowering into a [`MetricPlan`].
#[derive(Clone, Debug)]
pub struct NativePipelineConfig {
    /// Whether the DogStatsD metric source stream is enabled.
    pub dogstatsd_enabled: bool,

    /// Additional pass-through source streams entering directly at enrichment (such as `"otlp"`, `"checks"`,
    /// `"liveness"`, `"trace_sampler"`).
    pub passthrough_streams: Vec<String>,

    /// DogStatsD metric-name mapper profiles.
    pub mapper_profiles: Vec<DogStatsDMapperProfile>,

    /// DogStatsD namespace prefix.
    pub metric_prefix: String,

    /// Prefixes exempted from receiving `metric_prefix`.
    pub metric_prefix_blocklist: Vec<String>,

    /// DogStatsD metric name filterlist (used pre- and post-aggregation).
    pub metric_filter: MetricNameFilter,

    /// Whether timestamped metrics bypass tag filtering and aggregation.
    pub no_aggregation_pipeline: bool,

    /// Whole-tag filter entries applied to counter and sketch metrics before aggregation.
    pub tag_filter_entries: Vec<MetricTagFilterEntry>,

    /// Tag value allowlist entries applied to counter and sketch metrics before aggregation.
    pub tag_value_allowlists: Vec<MetricTagValueAllowlistEntry>,

    /// Aggregation window duration in seconds.
    pub aggregate_window_duration_secs: NonZeroU64,

    /// Maximum contexts retained by aggregation.
    pub aggregate_context_limit: usize,

    /// Counter keep-alive expiration in seconds (`0` disables keep-alive).
    pub aggregate_counter_expiry_secs: u64,

    /// Whether open windows are flushed on shutdown.
    pub aggregate_flush_open_windows: bool,

    /// Configured histogram aggregate names (such as `["max", "median", "avg", "count"]`).
    pub histogram_aggregates: Vec<String>,

    /// Configured histogram percentiles (such as `["0.95"]`).
    pub histogram_percentiles: Vec<String>,

    /// Whether histograms are also copied into distribution sketches.
    pub histogram_copy_to_distribution: bool,

    /// Prefix applied when copying histograms to distributions.
    pub histogram_copy_to_distribution_prefix: String,

    /// Whether DogStatsD post-aggregate client telemetry is tapped.
    pub client_telemetry_enabled: bool,

    /// Whether host-tags enrichment is enabled.
    pub host_tags_enabled: bool,

    /// Endpoint routing allowlist policy groups.
    pub routing_policies: Vec<RoutingPolicySpec>,

    /// Whether the multi-region failover branch is configured.
    pub mrf_branch_configured: bool,

    /// Startup-level enablement flag for MRF.
    pub mrf_enabled: bool,

    /// Initial MRF metric mirroring policy.
    pub mrf_mirroring: MetricMirroring,

    /// Whether the autoscaling failover branch is configured.
    pub autoscaling_failover_enabled: bool,

    /// Allowed series names for autoscaling failover.
    pub autoscaling_failover_metrics: Vec<String>,
}

impl Default for NativePipelineConfig {
    fn default() -> Self {
        Self {
            dogstatsd_enabled: true,
            passthrough_streams: Vec::new(),
            mapper_profiles: Vec::new(),
            metric_prefix: String::new(),
            metric_prefix_blocklist: Vec::new(),
            metric_filter: MetricNameFilter::default(),
            no_aggregation_pipeline: true,
            tag_filter_entries: Vec::new(),
            tag_value_allowlists: Vec::new(),
            aggregate_window_duration_secs: NonZeroU64::new(10).expect("10 is non-zero"),
            aggregate_context_limit: 1_000_000,
            aggregate_counter_expiry_secs: 300,
            aggregate_flush_open_windows: true,
            histogram_aggregates: vec![
                "max".to_string(),
                "median".to_string(),
                "avg".to_string(),
                "count".to_string(),
            ],
            histogram_percentiles: vec!["0.95".to_string()],
            histogram_copy_to_distribution: false,
            histogram_copy_to_distribution_prefix: String::new(),
            client_telemetry_enabled: false,
            host_tags_enabled: false,
            routing_policies: Vec::new(),
            mrf_branch_configured: false,
            mrf_enabled: false,
            mrf_mirroring: MetricMirroring::default(),
            autoscaling_failover_enabled: false,
            autoscaling_failover_metrics: Vec::new(),
        }
    }
}

/// Lowers `config` into a complete, standalone [`MetricPlan`].
pub fn lower_native_pipeline(config: &NativePipelineConfig) -> MetricPlan {
    let mut b = MetricPlanBuilder::default().with_description("native-metrics-pipeline");
    let mut enrich_inputs: Vec<NodeId> = Vec::new();

    if config.dogstatsd_enabled {
        let mut cursor = b.add_node(
            "dsd_in",
            Op::Source {
                stream: STREAM_DOGSTATSD.to_string(),
            },
        );

        // DogStatsD mapper runs first, matching native topology (`dsd_enrich`).
        let mapper = b.add_node(
            "dsd_mapper",
            Op::Map {
                profiles: config.mapper_profiles.clone(),
            },
        );
        b.add_edge(cursor, mapper.clone());
        cursor = mapper;

        // Namespace prefix + listener-side filterlist.
        let prefix_filter = b.add_node(
            "dsd_prefix_filter",
            Op::PrefixFilter {
                metric_prefix: config.metric_prefix.clone(),
                metric_prefix_blocklist: config.metric_prefix_blocklist.clone(),
                filter: config.metric_filter.clone(),
            },
        );
        b.add_edge(cursor, prefix_filter.clone());
        cursor = prefix_filter;

        let histogram = HistogramConfiguration::new(
            &config.histogram_aggregates,
            &config.histogram_percentiles,
            config.histogram_copy_to_distribution,
            config.histogram_copy_to_distribution_prefix.clone(),
        );

        let tag_filter = b.add_node(
            "dsd_tag_filterlist",
            Op::TagFilter {
                entries: config.tag_filter_entries.clone(),
                value_allowlists: config.tag_value_allowlists.clone(),
            },
        );

        let agg = b.add_node(
            "dsd_agg",
            Op::Aggregate {
                window_duration_secs: config.aggregate_window_duration_secs,
                context_limit: config.aggregate_context_limit,
                counter_expiry_secs: config.aggregate_counter_expiry_secs,
                flush_open_windows: config.aggregate_flush_open_windows,
                histogram,
            },
        );

        let post_agg_filter = b.add_node(
            "dsd_post_agg_filter",
            Op::PostAggregateFilter {
                filter: config.metric_filter.clone(),
                histogram_aggregates: config.histogram_aggregates.clone(),
                histogram_percentiles: config.histogram_percentiles.clone(),
            },
        );

        if config.no_aggregation_pipeline {
            let split = b.add_node("dsd_no_agg_split", Op::TimestampSplit);
            let rejoin = b.add_node("dsd_post_agg_merge", Op::Merge);

            b.add_edge(cursor, split.clone());
            b.add_edge_port(split.clone(), SPLIT_AGGREGATE_PORT, tag_filter.clone());
            b.add_edge(tag_filter, agg.clone());
            b.add_edge(agg, rejoin.clone());
            b.add_edge_port(split, SPLIT_PASSTHROUGH_PORT, rejoin.clone());
            b.add_edge(rejoin, post_agg_filter.clone());
        } else {
            b.add_edge(cursor, tag_filter.clone());
            b.add_edge(tag_filter, agg.clone());
            b.add_edge(agg, post_agg_filter.clone());
        }

        if config.client_telemetry_enabled {
            let post_agg_tee = b.add_node("dsd_post_agg_tee", Op::Tee);
            let client_telemetry = b.add_node(
                "dsd_client_telemetry_out",
                Op::Emit {
                    target: TARGET_CLIENT_TELEMETRY.to_string(),
                },
            );
            b.add_edge(post_agg_filter, post_agg_tee.clone());
            b.add_edge(post_agg_tee.clone(), client_telemetry);
            enrich_inputs.push(post_agg_tee);
        } else {
            enrich_inputs.push(post_agg_filter);
        }
    }

    for stream in &config.passthrough_streams {
        let src_id = b.add_node(format!("{stream}_in"), Op::Source { stream: stream.clone() });
        enrich_inputs.push(src_id);
    }

    // Converge all metric streams into the shared enrichment / fanout stage.
    let mut shared_tail = if enrich_inputs.len() > 1 {
        let merge = b.add_node("metrics_merge", Op::Merge);
        for input in enrich_inputs {
            b.add_edge(input, merge.clone());
        }
        merge
    } else if let Some(single) = enrich_inputs.pop() {
        single
    } else {
        // Guarantee a valid structure even if caller disabled all streams.
        b.add_node(
            "empty_in",
            Op::Source {
                stream: "empty".to_string(),
            },
        )
    };

    if config.host_tags_enabled {
        let host_tags = b.add_node(
            "metrics_host_tags",
            Op::EnrichHostTags {
                binding: HOST_TAGS_BINDING.to_string(),
            },
        );
        b.add_edge(shared_tail, host_tags.clone());
        shared_tail = host_tags;
    }

    // Collect all output branches fed by `shared_tail`.
    let mut output_branches: Vec<NodeId> = Vec::new();

    let primary_emit = b.add_node(
        "primary_out",
        Op::Emit {
            target: TARGET_PRIMARY.to_string(),
        },
    );
    output_branches.push(primary_emit);

    for (index, policy) in config.routing_policies.iter().enumerate() {
        let filter_id = b.add_node(
            format!("metrics_routing_filter_{index}"),
            Op::RouteFilter {
                kind: RouteFilterKind::Allowlist {
                    names: policy.metric_allowlist.clone(),
                    prefixes: policy.metric_prefix_allowlist.clone(),
                },
            },
        );
        let emit_id = b.add_node(
            format!("metrics_routing_out_{index}"),
            Op::Emit {
                target: policy.target.clone(),
            },
        );
        b.add_edge(filter_id.clone(), emit_id);
        output_branches.push(filter_id);
    }

    if config.mrf_branch_configured {
        let mrf_filter = b.add_node(
            "mrf_metrics_gateway",
            Op::RouteFilter {
                kind: RouteFilterKind::Mirroring {
                    enabled: config.mrf_enabled,
                    policy: config.mrf_mirroring.clone(),
                    signal_binding: Some(MRF_SIGNAL_BINDING.to_string()),
                },
            },
        );
        let mrf_emit = b.add_node(
            "mrf_out",
            Op::Emit {
                target: TARGET_MRF.to_string(),
            },
        );
        b.add_edge(mrf_filter.clone(), mrf_emit);
        output_branches.push(mrf_filter);
    }

    if config.autoscaling_failover_enabled && !config.autoscaling_failover_metrics.is_empty() {
        let af_filter = b.add_node(
            "af_metrics_gateway",
            Op::RouteFilter {
                kind: RouteFilterKind::SeriesAllowlist {
                    enabled: true,
                    metrics: config.autoscaling_failover_metrics.clone(),
                },
            },
        );
        let af_emit = b.add_node(
            "af_out",
            Op::Emit {
                target: TARGET_AUTOSCALING_FAILOVER.to_string(),
            },
        );
        b.add_edge(af_filter.clone(), af_emit);
        output_branches.push(af_filter);
    }

    if output_branches.len() == 1 {
        b.add_edge(shared_tail, output_branches.pop().expect("len is 1"));
    } else {
        let fanout = b.add_node("metrics_fanout", Op::Tee);
        b.add_edge(shared_tail, fanout.clone());
        for branch in output_branches {
            b.add_edge(fanout.clone(), branch);
        }
    }

    b.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::validate_plan;

    #[test]
    fn default_native_config_lowers_to_valid_plan() {
        let plan = lower_native_pipeline(&NativePipelineConfig::default());
        let validated = validate_plan(&plan).expect("default lowered plan must validate");
        assert_eq!(validated.stateful_node_count(), 1);
    }

    #[test]
    fn full_featured_native_config_lowers_to_valid_plan() {
        let config = NativePipelineConfig {
            dogstatsd_enabled: true,
            passthrough_streams: vec![
                STREAM_OTLP.to_string(),
                STREAM_CHECKS.to_string(),
                STREAM_LIVENESS.to_string(),
            ],
            client_telemetry_enabled: true,
            host_tags_enabled: true,
            routing_policies: vec![RoutingPolicySpec {
                target: "routing_0".to_string(),
                metric_allowlist: vec!["special.metric".to_string()],
                metric_prefix_allowlist: vec!["team_a.".to_string()],
            }],
            mrf_branch_configured: true,
            mrf_enabled: true,
            mrf_mirroring: MetricMirroring {
                enabled: true,
                allowlist: vec!["critical.metric".to_string()],
            },
            autoscaling_failover_enabled: true,
            autoscaling_failover_metrics: vec!["container.cpu.usage".to_string()],
            ..NativePipelineConfig::default()
        };

        let plan = lower_native_pipeline(&config);
        let validated = validate_plan(&plan).expect("full-featured lowered plan must validate");
        assert_eq!(validated.stateful_node_count(), 1);
    }
}
