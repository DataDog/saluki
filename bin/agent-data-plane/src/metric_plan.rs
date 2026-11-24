//! Lowers ADP's typed [`SalukiConfiguration`] into a [`MetricPlan`] and renders `explain` output.

use std::sync::Arc;

use agent_data_plane_config::SalukiConfiguration;
use saluki_components::{
    config::{AutoscalingFailoverConfiguration, ClusterAgentConfiguration, MetricsEndpointRoutingConfiguration},
    transforms::{DogStatsDMapperProfile, DogStatsDMetricMapping, MetricRegionConfiguration},
};
use saluki_core::data_model::event::metric::{Metric, MetricOrigin};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metric_plan::{
    explain_plan,
    lowering::{
        lower_native_pipeline, NativePipelineConfig, RoutingPolicySpec, STREAM_CHECKS, STREAM_DOGSTATSD,
        STREAM_LIVENESS, STREAM_OTLP, STREAM_TRACE_SAMPLER,
    },
    validate_plan, MetricPlan, RuntimeBindings, TelemetryContract,
};

use crate::{
    component_inputs::{
        metric_mirroring, metric_name_filter, mrf_configuration, tag_filter_entries, tag_value_allowlist,
    },
    config::DataPlaneConfiguration,
};

/// Translates `config` into a model-independent [`NativePipelineConfig`].
pub fn native_pipeline_config(config: &SalukiConfiguration) -> Result<NativePipelineConfig, GenericError> {
    let dp = DataPlaneConfiguration::from_configuration(config);
    let shared = &config.shared;
    let dsd = &config.domains.dogstatsd;
    let aggregation = &dsd.aggregation;
    let histogram = &shared.metrics_encoding.histogram;

    let mut passthrough_streams = Vec::new();
    if !dp.standalone_mode() {
        passthrough_streams.push(STREAM_LIVENESS.to_string());
    }
    if dp.checks_enabled() {
        passthrough_streams.push(STREAM_CHECKS.to_string());
    }
    if dp.otlp_enabled() && !dp.otlp_proxy_enabled() {
        passthrough_streams.push(STREAM_OTLP.to_string());
    }
    if dp.traces_pipeline_required() {
        passthrough_streams.push(STREAM_TRACE_SAMPLER.to_string());
    }

    let mapper_profiles = dsd
        .mapper
        .profiles
        .iter()
        .map(|profile| DogStatsDMapperProfile {
            name: profile.name.clone(),
            prefix: profile.prefix.clone(),
            mappings: profile
                .mappings
                .iter()
                .map(|mapping| DogStatsDMetricMapping {
                    metric_match: mapping.metric_match.clone(),
                    match_type: mapping.match_type.clone(),
                    name: mapping.name.clone(),
                    tags: mapping.tags.clone(),
                })
                .collect(),
        })
        .collect();

    let endpoint_routing = MetricsEndpointRoutingConfiguration::new(
        &config.domains.metrics_endpoint_routing.metric_allowlists,
        &config.domains.metrics_endpoint_routing.metric_prefix_allowlists,
        &shared.endpoints.primary_endpoint(),
        &shared.endpoints.additional_endpoints,
    )?;

    let routing_policies = endpoint_routing
        .policy_groups()
        .iter()
        .enumerate()
        .map(|(index, group)| RoutingPolicySpec {
            target: format!("routing_{index}"),
            metric_allowlist: group.metric_allowlist.clone(),
            metric_prefix_allowlist: group.metric_prefix_allowlist.clone(),
        })
        .collect();

    let mrf_cfg = mrf_configuration(&config.domains.multi_region_failover);
    let mrf_branch_configured = mrf_cfg.metrics_endpoint_override().is_some();

    let af_cfg = AutoscalingFailoverConfiguration::new(
        shared.autoscaling_failover.enabled,
        shared.autoscaling_failover.metrics.clone(),
    );
    let ca_cfg = ClusterAgentConfiguration {
        enabled: shared.cluster_agent.enabled,
        url: shared.cluster_agent.url.clone(),
        auth_token: shared.cluster_agent.auth_token.clone(),
        kubernetes_service_name: shared.cluster_agent.kubernetes_service_name.clone(),
    };
    let autoscaling_failover_enabled = af_cfg.is_branch_requested() && ca_cfg.endpoint_and_token().is_some();

    Ok(NativePipelineConfig {
        dogstatsd_enabled: dp.dogstatsd_enabled(),
        passthrough_streams,
        mapper_profiles,
        metric_prefix: dsd.prefix_filter.metric_namespace.clone(),
        metric_prefix_blocklist: dsd.prefix_filter.metric_namespace_blocklist.clone(),
        metric_filter: metric_name_filter(&dsd.metric_filter),
        no_aggregation_pipeline: aggregation.no_aggregation_pipeline,
        tag_filter_entries: tag_filter_entries(&dsd.tag_filterlist),
        tag_value_allowlists: tag_value_allowlist(&dsd.tag_value_allowlist),
        aggregate_window_duration_secs: aggregation.window_duration_seconds,
        aggregate_context_limit: aggregation.context_limit,
        aggregate_counter_expiry_secs: aggregation.counter_expiry_seconds.unwrap_or(0),
        aggregate_flush_open_windows: aggregation.flush_open_windows,
        histogram_aggregates: histogram.aggregates.clone(),
        histogram_percentiles: histogram.percentiles.clone(),
        histogram_copy_to_distribution: histogram.copy_to_distribution,
        histogram_copy_to_distribution_prefix: histogram.copy_to_distribution_prefix.clone(),
        client_telemetry_enabled: dp.dogstatsd_enabled(),
        host_tags_enabled: !dp.standalone_mode() && !shared.tags.expected_tags_duration.is_zero(),
        routing_policies,
        mrf_branch_configured,
        mrf_enabled: mrf_cfg.is_enabled(),
        mrf_mirroring: metric_mirroring(&config.domains.multi_region_failover.metric_mirroring),
        autoscaling_failover_enabled,
        autoscaling_failover_metrics: af_cfg.metrics().to_vec(),
    })
}

/// Lowers `config` into a complete, standalone [`MetricPlan`].
pub fn lower_from_saluki_config(config: &SalukiConfiguration) -> Result<MetricPlan, GenericError> {
    let native = native_pipeline_config(config)?;
    Ok(lower_native_pipeline(&native))
}

/// Renders either the human-readable `explain` report or pretty-printed JSON for the lowered metric plan.
pub fn render_plan_explanation(config: &SalukiConfiguration, as_json: bool) -> Result<String, GenericError> {
    let plan = lower_from_saluki_config(config)?;
    if as_json {
        plan.to_json_pretty()
            .error_context("Failed to serialize lowered metric plan as JSON.")
    } else {
        Ok(explain_plan(&plan, &[TelemetryContract::default_internal_telemetry()]))
    }
}

/// Lowers and validates `config` into a boot-time [`MetricRegionConfiguration`] hosting the single fused region.
pub fn build_metric_region_configuration(
    config: &SalukiConfiguration, bindings: RuntimeBindings,
) -> Result<MetricRegionConfiguration, GenericError> {
    let native = native_pipeline_config(config)?;
    let plan = lower_native_pipeline(&native);
    let validated = validate_plan(&plan).map_err(|errors| {
        let messages: Vec<String> = errors.into_iter().map(|e| e.to_string()).collect();
        generic_error!(
            "Failed to validate lowered programmable metrics plan: {}",
            messages.join("; ")
        )
    })?;

    let default_stream = if native.dogstatsd_enabled {
        STREAM_DOGSTATSD.to_string()
    } else {
        native
            .passthrough_streams
            .first()
            .cloned()
            .unwrap_or_else(|| "empty".to_string())
    };

    let mut region = MetricRegionConfiguration::new(
        validated,
        default_stream,
        config.domains.dogstatsd.aggregation.flush_interval,
    )
    .with_bindings(bindings);

    // When both DogStatsD and passthrough sources (OTLP, checks, liveness, trace sampler) feed the single fused
    // region, route DogStatsD-origin metrics into the aggregation branch and all other metrics into the shared
    // passthrough enrichment branch.
    if native.dogstatsd_enabled && !native.passthrough_streams.is_empty() {
        let passthrough_stream: &'static str = match native.passthrough_streams[0].as_str() {
            STREAM_LIVENESS => STREAM_LIVENESS,
            STREAM_CHECKS => STREAM_CHECKS,
            STREAM_OTLP => STREAM_OTLP,
            STREAM_TRACE_SAMPLER => STREAM_TRACE_SAMPLER,
            _ => STREAM_DOGSTATSD,
        };
        region = region.with_stream_classifier(Arc::new(move |metric: &Metric| {
            if metric.metadata().origin().is_some_and(MetricOrigin::is_dogstatsd) {
                STREAM_DOGSTATSD
            } else {
                passthrough_stream
            }
        }));
    }

    Ok(region)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, num::NonZeroUsize, time::Duration};

    use agent_data_plane_config::domains::dogstatsd::{
        FilterAction as ConfigFilterAction, MapperProfile, MetricMapping, MetricPrefixRule as ConfigMetricPrefixRule,
        MetricTagFilterEntry as ConfigTagFilterEntry, MetricTagValueAllowlistEntry as ConfigTagValueAllowlistEntry,
        TagValueMismatchAction as ConfigTagValueMismatchAction,
    };
    use saluki_common::sync::live::Live;
    use saluki_components::transforms::{
        aggregate_context_snapshot_channel, AggregateConfiguration, DogStatsDMapperConfiguration,
        DogStatsDMapperProfile, DogStatsDMetricMapping, HistogramConfiguration,
    };
    use saluki_core::{
        components::{
            test_util::{Output, TestComponentDriver},
            transforms::SynchronousTransformBuilder as _,
            BuildContext,
        },
        data_model::{
            event::{
                metric::{context::Context, Metric, MetricValues},
                Event,
            },
            tags::{SharedTagSet, Tag},
        },
        topology::EventsBuffer,
    };
    use saluki_metric_plan::{
        lowering::{HOST_TAGS_BINDING, MRF_SIGNAL_BINDING, STREAM_DOGSTATSD, TARGET_PRIMARY},
        validate_plan, ReferenceInterpreter, RuntimeBindings,
    };

    use super::*;
    use crate::components::{
        dogstatsd_no_agg_split::DogStatsDNoAggSplitConfiguration,
        dogstatsd_post_aggregate_filter::DogStatsDPostAggregateFilterConfiguration,
        dogstatsd_prefix_filter::DogStatsDPrefixFilterConfiguration, tag_filterlist::TagFilterlistConfiguration,
    };

    /// Runs a batch of metrics through the native DogStatsD transform components (`dsd_mapper` -> `dsd_prefix_filter`
    /// -> optional `dsd_no_agg_split` -> `dsd_tag_filterlist` -> `dsd_agg` -> `dsd_post_agg_filter`) and collects
    /// emitted metrics after flushing.
    async fn run_native_dogstatsd_chain(
        config: &SalukiConfiguration, input: Vec<Metric>,
    ) -> Vec<(String, Vec<String>, String)> {
        let dsd = &config.domains.dogstatsd;
        let histogram = &config.shared.metrics_encoding.histogram;

        // 1. Mapper (synchronous transform).
        let mapper_profiles = dsd
            .mapper
            .profiles
            .iter()
            .map(|p| DogStatsDMapperProfile {
                name: p.name.clone(),
                prefix: p.prefix.clone(),
                mappings: p
                    .mappings
                    .iter()
                    .map(|m| DogStatsDMetricMapping {
                        metric_match: m.metric_match.clone(),
                        match_type: m.match_type.clone(),
                        name: m.name.clone(),
                        tags: m.tags.clone(),
                    })
                    .collect(),
            })
            .collect();
        let mapper_cfg =
            DogStatsDMapperConfiguration::new(NonZeroUsize::new(64 * 1024).unwrap(), 1024, mapper_profiles);
        let mut mapper = mapper_cfg
            .build(BuildContext::test_transform("dsd_mapper"))
            .await
            .expect("mapper build");

        let mut buf = EventsBuffer::default();
        for m in input {
            assert!(buf.try_push(Event::Metric(m)).is_none());
        }
        mapper.transform_buffer(&mut buf);

        // 2. Prefix + listener filter.
        let prefix_cfg = DogStatsDPrefixFilterConfiguration::new(
            dsd.prefix_filter.metric_namespace.clone(),
            dsd.prefix_filter.metric_namespace_blocklist.clone(),
            Live::new_fixed(metric_name_filter(&dsd.metric_filter)),
        );
        let (control, mut outputs) = TestComponentDriver::options()
            .with_component_id("dsd_prefix_filter")
            .transform(prefix_cfg)
            .await
            .expect("prefix driver");
        control.send(buf).await;
        control.shutdown().await.expect("prefix shutdown");
        let after_prefix = collect_all_metrics(outputs.default_output()).await;

        // 3. Optional no-agg split.
        let (to_agg, passthrough) = if dsd.aggregation.no_aggregation_pipeline {
            let split_cfg = DogStatsDNoAggSplitConfiguration::new(Duration::from_millis(50));
            let (control, mut outputs) = TestComponentDriver::options()
                .with_component_id("dsd_no_agg_split")
                .transform(split_cfg)
                .await
                .expect("split driver");
            control.send(buffer_from_metrics(after_prefix)).await;
            control.shutdown().await.expect("split shutdown");
            let agg_metrics = collect_all_metrics(outputs.default_output()).await;
            let pass_metrics = collect_all_metrics(outputs.named_output("passthrough")).await;
            (agg_metrics, pass_metrics)
        } else {
            (after_prefix, Vec::new())
        };

        // 4. Tag filterlist.
        let tag_cfg = TagFilterlistConfiguration::new(
            Live::new_fixed(tag_filter_entries(&dsd.tag_filterlist)),
            &tag_value_allowlist(&dsd.tag_value_allowlist),
            1024,
        );
        let (control, mut outputs) = TestComponentDriver::options()
            .with_component_id("dsd_tag_filterlist")
            .transform(tag_cfg)
            .await
            .expect("tag driver");
        control.send(buffer_from_metrics(to_agg)).await;
        control.shutdown().await.expect("tag shutdown");
        let after_tags = collect_all_metrics(outputs.default_output()).await;

        // 5. Aggregate (flush open windows on shutdown so all samples emit deterministically).
        let (_snap_handle, snap_rx) = aggregate_context_snapshot_channel();
        let agg_cfg = AggregateConfiguration {
            window_duration_seconds: dsd.aggregation.window_duration_seconds,
            primary_flush_interval: Duration::from_secs(60),
            context_limit: dsd.aggregation.context_limit,
            flush_open_windows: true,
            counter_expiry_seconds: None,
            hist_config: HistogramConfiguration::new(
                &histogram.aggregates,
                &histogram.percentiles,
                histogram.copy_to_distribution,
                histogram.copy_to_distribution_prefix.clone(),
            ),
            origin_telemetry_enabled: false,
            context_snapshot_receiver: snap_rx,
        };
        let (control, mut outputs) = TestComponentDriver::options()
            .with_component_id("dsd_agg")
            .transform(agg_cfg)
            .await
            .expect("agg driver");
        control.send(buffer_from_metrics(after_tags)).await;
        control.shutdown().await.expect("agg shutdown");
        let mut after_agg = collect_all_metrics(outputs.default_output()).await;
        after_agg.extend(passthrough);

        // 6. Post-aggregate filter.
        let post_cfg = DogStatsDPostAggregateFilterConfiguration::new(
            Live::new_fixed(metric_name_filter(&dsd.metric_filter)),
            &histogram.aggregates,
            &histogram.percentiles,
        );
        let (control, mut outputs) = TestComponentDriver::options()
            .with_component_id("dsd_post_agg_filter")
            .transform(post_cfg)
            .await
            .expect("post driver");
        control.send(buffer_from_metrics(after_agg)).await;
        control.shutdown().await.expect("post shutdown");
        let final_metrics = collect_all_metrics(outputs.default_output()).await;

        canonicalize_metrics(final_metrics)
    }

    fn buffer_from_metrics(metrics: Vec<Metric>) -> EventsBuffer {
        let mut buf = EventsBuffer::default();
        for metric in metrics {
            assert!(buf.try_push(Event::Metric(metric)).is_none());
        }
        buf
    }

    async fn collect_all_metrics(output: &mut Output<EventsBuffer>) -> Vec<Metric> {
        output
            .collect_events()
            .await
            .into_iter()
            .filter_map(Event::try_into_metric)
            .collect()
    }

    fn canonicalize_metrics(metrics: Vec<Metric>) -> Vec<(String, Vec<String>, String)> {
        let mut rows: Vec<(String, Vec<String>, String)> = metrics
            .into_iter()
            .map(|m| {
                let name = m.context().name().to_string();
                let mut tags: Vec<String> = m.context().tags().into_iter().map(|t| t.as_str().to_string()).collect();
                tags.sort();
                let kind = match m.values() {
                    MetricValues::Counter(_) => "counter",
                    MetricValues::Rate(_, _) => "rate",
                    MetricValues::Gauge(_) => "gauge",
                    MetricValues::Set(_) => "set",
                    MetricValues::Histogram(_) => "histogram",
                    MetricValues::Distribution(_) => "distribution",
                }
                .to_string();
                (name, tags, kind)
            })
            .collect();
        rows.sort();
        rows
    }

    /// Differential test: lowered `SalukiConfiguration` executed on `ReferenceInterpreter` matches the native
    /// DogStatsD transform chain across mapper, namespace prefix, filterlist (with histogram exception), timestamp
    /// split, tag filterlist, tag value allowlist, aggregation, and post-aggregate filtering.
    #[tokio::test]
    async fn differential_lowered_plan_matches_native_dogstatsd_chain() {
        let mut config = SalukiConfiguration::default();
        config.control.dogstatsd = true;
        config.domains.dogstatsd.prefix_filter.metric_namespace = "acme".to_string();
        config.domains.dogstatsd.prefix_filter.metric_namespace_blocklist = vec!["datadog.".to_string()];
        config.domains.dogstatsd.mapper.profiles = vec![MapperProfile {
            name: "jobs".to_string(),
            prefix: "raw.job.".to_string(),
            mappings: vec![MetricMapping {
                metric_match: "raw.job.*.count".to_string(),
                match_type: "wildcard".to_string(),
                name: "mapped.job.count".to_string(),
                tags: HashMap::from([("job".to_string(), "$1".to_string())]),
            }],
        }];
        config.domains.dogstatsd.metric_filter.values = vec!["acme.blocked".to_string()];
        config.domains.dogstatsd.metric_filter.prefix_rules = vec![ConfigMetricPrefixRule {
            prefix: "acme.hist.".to_string(),
            except_exact: vec!["acme.hist.latency.avg".to_string()],
            except_prefix: vec![],
        }];
        config.domains.dogstatsd.tag_filterlist = vec![ConfigTagFilterEntry {
            metric_name: "acme.mapped.job.count".to_string(),
            action: ConfigFilterAction::Exclude,
            tags: vec!["drop_tag".to_string()],
        }];
        config.domains.dogstatsd.tag_value_allowlist = vec![ConfigTagValueAllowlistEntry {
            metric_prefix: "acme.mapped.".to_string(),
            tag_name: "job".to_string(),
            values: vec!["billing".to_string()],
            on_miss: ConfigTagValueMismatchAction::Replace,
            replacement: "other".to_string(),
        }];
        config.domains.dogstatsd.aggregation.no_aggregation_pipeline = true;
        config.domains.dogstatsd.aggregation.flush_open_windows = true;
        config.domains.dogstatsd.aggregation.counter_expiry_seconds = None;
        config.shared.metrics_encoding.histogram.aggregates = vec!["avg".to_string(), "max".to_string()];
        config.shared.metrics_encoding.histogram.percentiles = vec![];

        let input = vec![
            // Mapped + prefixed + tag-filtered + value-allowlisted counter.
            Metric::counter(
                Context::from_static_parts("raw.job.billing.count", &["drop_tag:1", "env:prod"]),
                2.0,
            ),
            Metric::counter(
                Context::from_static_parts("raw.job.unknown.count", &["drop_tag:2", "env:prod"]),
                4.0,
            ),
            // Blocked by metric filterlist after prefixing.
            Metric::gauge("blocked", 1.0),
            // Exempted from namespace prefixing.
            Metric::gauge("datadog.agent.running", 1.0),
            // Histogram with descendant exception: only `acme.hist.latency.avg` survives post-aggregate filtering!
            Metric::histogram("hist.latency", [10.0, 20.0]),
            // Timestamped counter bypasses tag filter & aggregation and becomes a rate.
            Metric::counter(
                Context::from_static_parts("ts.events", &["env:prod"]),
                (1_700_000_000, 5.0),
            ),
        ];

        let input_for_region = input.clone();

        // 1. Run native component chain.
        let native_out = run_native_dogstatsd_chain(&config, input.clone()).await;

        // 2. Lower `SalukiConfiguration` -> `MetricPlan` and run via `ReferenceInterpreter`.
        let plan = lower_from_saluki_config(&config).expect("lowering should succeed");
        let validated = validate_plan(&plan).expect("lowered plan must validate");
        let mut interp = ReferenceInterpreter::new(&validated, RuntimeBindings::default()).expect("interpreter init");

        let mut plan_metrics = Vec::new();
        let immediate = interp
            .ingest(STREAM_DOGSTATSD, 1_700_000_012, input)
            .expect("ingest should succeed");
        if let Some(m) = immediate.get(TARGET_PRIMARY) {
            plan_metrics.extend(m.iter().cloned());
        }
        let flushed = interp.flush(1_700_000_012, true);
        if let Some(m) = flushed.get(TARGET_PRIMARY) {
            plan_metrics.extend(m.iter().cloned());
        }
        let plan_out = canonicalize_metrics(plan_metrics);

        assert_eq!(
            plan_out, native_out,
            "lowered MetricPlan on ReferenceInterpreter must match native DogStatsD transform chain"
        );
        // Sanity check that non-trivial behaviors happened (histogram descendant exception, tag rewrite, etc.).
        assert!(plan_out.iter().any(|(n, _, _)| n == "acme.hist.latency.avg"));
        assert!(!plan_out.iter().any(|(n, _, _)| n == "acme.hist.latency.max"));
        assert!(!plan_out.iter().any(|(n, _, _)| n == "acme.blocked"));
        // 3. Also run the exact same lowered plan through `MetricRegionTransform` (single fused Tokio task).
        let mut standalone_config = config.clone();
        standalone_config.control.standalone_mode = true;
        let region_cfg = build_metric_region_configuration(&standalone_config, RuntimeBindings::default())
            .expect("metric region config");
        let (control, mut outputs) = TestComponentDriver::options()
            .with_component_id("metrics_region")
            .transform(region_cfg)
            .await
            .expect("region driver");
        control.send(buffer_from_metrics(input_for_region)).await;
        control.shutdown().await.expect("region shutdown");
        let region_metrics = collect_all_metrics(outputs.default_output()).await;
        let region_out = canonicalize_metrics(region_metrics);

        assert_eq!(
            region_out, native_out,
            "single-region MetricRegionTransform must match native DogStatsD transform chain"
        );
    }

    #[test]
    fn explain_renders_default_saluki_configuration() {
        let mut config = SalukiConfiguration::default();
        config.control.dogstatsd = true;
        let rendered = render_plan_explanation(&config, false).expect("explain should succeed");
        assert!(rendered.contains("Status: VALID"));
        assert!(rendered.contains("dsd_agg"));

        let json = render_plan_explanation(&config, true).expect("json explain should succeed");
        let parsed = MetricPlan::from_json(&json).expect("json roundtrip");
        assert!(validate_plan(&parsed).is_ok());

        // Exercise runtime binding constants for completeness.
        let bindings = RuntimeBindings::default()
            .with_host_tags(HOST_TAGS_BINDING, SharedTagSet::from_iter([Tag::from("host:a")]))
            .with_mirroring_signal(MRF_SIGNAL_BINDING, Default::default());
        assert!(bindings.host_tags.contains_key(HOST_TAGS_BINDING));
    }
}
