//! The legacy metrics path: the native components that built the metrics path before the plan-driven topology.
//!
//! The plan-driven topology replaces the chains of synchronous DogStatsD and enrichment transforms, and the gateways
//! in front of the metrics outputs (the endpoint routing filters and both failover gateways), with regions that run
//! the default program, and builds everything else exactly as these functions do. They are kept, with the components
//! only they use, as a differential reference: tests compare the structure and behavior of the plan-driven topology
//! with those of the topology they build.

use agent_data_plane_config::{shared::SharedConfiguration, SalukiConfiguration};
use agent_data_plane_config_system::ConfigurationSystem;
use datadog_agent_commons::ipc::config::RemoteAgentClientConfiguration;
use saluki_components::{
    config::{
        AutoscalingFailoverConfiguration, ClusterAgentConfiguration, MetricsEndpointRoutingConfiguration,
        MrfConfiguration,
    },
    destinations::DogStatsDClientTelemetryConfiguration,
    transforms::{
        AggregateContextSnapshotHandle, AutoscalingFailoverGatewayConfiguration, ChainedConfiguration,
        HostEnrichmentConfiguration,
    },
};
use saluki_core::topology::TopologyBlueprint;
use saluki_error::{generic_error, ErrorContext as _, GenericError};

use super::{
    autoscaling_failover_branch, dogstatsd_aggregate_configuration, dogstatsd_mapper_configuration,
    main_metrics_encoder, mrf_metrics_branch, routing_group_encoder,
};
use crate::{
    components::{
        dogstatsd_no_agg_split::DogStatsDNoAggSplitConfiguration,
        dogstatsd_post_aggregate_filter::DogStatsDPostAggregateFilterConfiguration,
        dogstatsd_prefix_filter::DogStatsDPrefixFilterConfiguration, host_tags::HostTagsConfiguration,
        metric_filter::MetricFilterConfiguration, tag_filterlist::TagFilterlistConfiguration,
    },
    config::DataPlaneConfiguration,
    internal::env::ADPEnvironmentProvider,
};

/// The component every legacy metrics source is connected to.
pub(super) const METRICS_DESTINATION: &str = "metrics_enrich";

/// What the legacy metrics path needs beyond what every pipeline gets.
pub(super) struct LegacyMetrics<'a> {
    config_system: &'a ConfigurationSystem,
    remote_agent_client_config: Option<&'a RemoteAgentClientConfiguration>,
    env_provider: &'a ADPEnvironmentProvider,
}

impl<'a> LegacyMetrics<'a> {
    pub(super) fn new(
        config_system: &'a ConfigurationSystem, remote_agent_client_config: Option<&'a RemoteAgentClientConfiguration>,
        env_provider: &'a ADPEnvironmentProvider,
    ) -> Self {
        Self {
            config_system,
            remote_agent_client_config,
            env_provider,
        }
    }

    /// Adds metrics enrichment and the metrics outputs.
    pub(super) async fn add_baseline(
        &mut self, blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem, shared: &SharedConfiguration,
    ) -> Result<(), GenericError> {
        // Create the back half of the metrics processing pipeline.
        let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(self.env_provider.clone());
        let mut metrics_enrich_config =
            ChainedConfiguration::default().with_transform_builder("host_enrichment", host_enrichment_config);

        let config = config_system.config();
        let dp = DataPlaneConfiguration::from_configuration(&config);
        if !dp.standalone_mode() {
            let client_config = self
                .remote_agent_client_config
                .ok_or_else(|| generic_error!("Remote Agent client configuration is required in connected mode."))?;
            let host_tags_config =
                HostTagsConfiguration::new(client_config.clone(), shared.tags.expected_tags_duration);
            if host_tags_config.enabled() {
                metrics_enrich_config = metrics_enrich_config.with_transform_builder("host_tags", host_tags_config);
            }
        }

        let endpoint_routing = MetricsEndpointRoutingConfiguration::from_configuration(
            &config.domains.metrics_endpoint_routing.metric_allowlists,
            &config.domains.metrics_endpoint_routing.metric_prefix_allowlists,
            &shared.endpoints,
        )?;
        blueprint.add_transform(METRICS_DESTINATION, metrics_enrich_config)?;

        add_metrics_output_pipelines_to_blueprint(blueprint, shared, &endpoint_routing)?;
        add_mrf_metrics_pipeline_to_blueprint(blueprint, config_system, shared, &config)?;
        add_autoscaling_failover_metrics_pipeline_to_blueprint(blueprint, shared)?;

        Ok(())
    }

    /// Adds the DogStatsD metric transforms, aggregation, and the client telemetry tap, connected to the DogStatsD
    /// source and to metrics enrichment, and returns the handle of aggregation's context snapshots.
    pub(super) fn add_dogstatsd(
        &mut self, blueprint: &mut TopologyBlueprint, typed: &SalukiConfiguration,
    ) -> Result<AggregateContextSnapshotHandle, GenericError> {
        let config_system = self.config_system;
        let prefix_filter = &typed.domains.dogstatsd.prefix_filter;
        let dsd_prefix_filter_configuration = DogStatsDPrefixFilterConfiguration::new(
            prefix_filter.metric_namespace.clone(),
            prefix_filter.metric_namespace_blocklist.clone(),
            config_system.live(|config| &config.domains.dogstatsd.metric_filter),
        );
        let dsd_mapper_config = dogstatsd_mapper_configuration(&typed.domains.dogstatsd.mapper);
        let dsd_enrich_config =
            ChainedConfiguration::default().with_transform_builder("dogstatsd_mapper", dsd_mapper_config);
        let dsd_tag_filterlist_config = TagFilterlistConfiguration::new(
            config_system.live(|config| &config.domains.dogstatsd.tag_filterlist),
            &typed.domains.dogstatsd.tag_value_allowlist,
            typed.domains.dogstatsd.aggregation.aggregator_tag_filter_cache_capacity,
        )
        .error_context("Failed to configure metric tag filterlist transform.")?;
        let (dsd_agg_config, dsd_context_snapshot_handle) = dogstatsd_aggregate_configuration(typed)?;
        let aggregation = &typed.domains.dogstatsd.aggregation;
        let histogram = &typed.shared.metrics_encoding.histogram;
        let dsd_no_agg_split_config = DogStatsDNoAggSplitConfiguration::new(
            aggregation.passthrough_idle_flush_timeout,
            aggregation.window_duration_seconds,
        );
        let dsd_post_agg_filter_config = DogStatsDPostAggregateFilterConfiguration::new(
            config_system.live(|config| &config.domains.dogstatsd.metric_filter),
            &histogram.aggregates,
            &histogram.percentiles,
        )
        .error_context("Failed to configure DogStatsD post-aggregate filter transform.")?;

        blueprint
            .add_transform("dsd_prefix_filter", dsd_prefix_filter_configuration)?
            .add_transform("dsd_enrich", dsd_enrich_config)?
            .add_transform("dsd_tag_filterlist", dsd_tag_filterlist_config)?
            .add_transform("dsd_agg", dsd_agg_config)?
            .add_transform("dsd_post_agg_filter", dsd_post_agg_filter_config)?
            .add_destination("dsd_client_telemetry_out", DogStatsDClientTelemetryConfiguration)?
            // Post-aggregation client telemetry for RAR/COAT.
            .connect_components("dsd_post_agg_filter", "dsd_client_telemetry_out")?;

        // `dsd_no_agg_split` only earns a place in the pipeline when the no-aggregation pipeline is enabled. With it
        // disabled, every metric is aggregated, so the split would have nothing to route out of the aggregation path
        // and would sit in the hottest part of the process as a pure forwarder.
        if aggregation.no_aggregation_pipeline {
            blueprint
                .add_transform("dsd_no_agg_split", dsd_no_agg_split_config)?
                .connect_components_in_order([
                    "dsd_in.metrics",
                    "dsd_enrich",
                    "dsd_prefix_filter",
                    "dsd_no_agg_split",
                    "dsd_tag_filterlist",
                    "dsd_agg",
                    "dsd_post_agg_filter",
                    METRICS_DESTINATION,
                ])?
                // Timestamped metrics skip tag filtering and aggregation, rejoining the pipeline after the aggregate
                // transform.
                .connect_components("dsd_no_agg_split.passthrough", "dsd_post_agg_filter")?;
        } else {
            blueprint.connect_components_in_order([
                "dsd_in.metrics",
                "dsd_enrich",
                "dsd_prefix_filter",
                "dsd_tag_filterlist",
                "dsd_agg",
                "dsd_post_agg_filter",
                METRICS_DESTINATION,
            ])?;
        }

        Ok(dsd_context_snapshot_handle)
    }
}

fn add_mrf_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem, shared: &SharedConfiguration,
    config: &SalukiConfiguration,
) -> Result<(), GenericError> {
    let mrf = &config.domains.multi_region_failover;
    let Some(branch) = mrf_metrics_branch(config_system, shared, mrf) else {
        return Ok(());
    };
    let gateway = MetricFilterConfiguration::for_mrf(
        MrfConfiguration::from_configuration(mrf).is_enabled(),
        config_system.live(|config| &config.domains.multi_region_failover.metric_mirroring),
    );

    blueprint
        .add_transform("mrf_metrics_gateway", gateway)?
        .add_encoder("mrf_metrics_encode", branch.encoder)?
        .add_forwarder("mrf_dd_out", branch.forwarder)?
        .connect_components_in_order([
            METRICS_DESTINATION,
            "mrf_metrics_gateway",
            "mrf_metrics_encode",
            "mrf_dd_out",
        ])?;

    Ok(())
}

// Build both sides of metric filtering together: the ordinary stream excludes selected endpoints, and each
// filtered stream targets only its policy's endpoints. This applies to both series and sketches.
fn add_metrics_output_pipelines_to_blueprint(
    blueprint: &mut TopologyBlueprint, shared: &SharedConfiguration, routing: &MetricsEndpointRoutingConfiguration,
) -> Result<(), GenericError> {
    blueprint
        .add_encoder("dd_metrics_encode", main_metrics_encoder(shared, routing))?
        .connect_components_in_order([METRICS_DESTINATION, "dd_metrics_encode", "dd_out"])?;

    // Each policy group needs unique component IDs; indices keep endpoint names out of those IDs.
    for (index, policy) in routing.policy_groups().iter().enumerate() {
        let filter_id = format!("metrics_routing_filter_{index}");
        let encoder_id = format!("metrics_routing_encode_{index}");
        let filter_config = MetricFilterConfiguration::for_allowlist(
            policy.metric_allowlist.clone(),
            policy.metric_prefix_allowlist.clone(),
        );
        let metrics_config = routing_group_encoder(shared, policy);

        blueprint
            .add_transform(filter_id.as_str(), filter_config)?
            .add_encoder(encoder_id.as_str(), metrics_config)?
            .connect_components_in_order([METRICS_DESTINATION, filter_id.as_str(), encoder_id.as_str(), "dd_out"])?;
    }

    Ok(())
}

fn add_autoscaling_failover_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, shared: &SharedConfiguration,
) -> Result<(), GenericError> {
    let cluster_agent = &shared.cluster_agent;
    let ca_config = ClusterAgentConfiguration {
        enabled: cluster_agent.enabled,
        url: cluster_agent.url.clone(),
        auth_token: cluster_agent.auth_token.clone(),
        kubernetes_service_name: cluster_agent.kubernetes_service_name.clone(),
    };
    let Some(branch) = autoscaling_failover_branch(shared, ca_config.endpoint_and_token())? else {
        return Ok(());
    };
    let gateway = AutoscalingFailoverGatewayConfiguration::new(AutoscalingFailoverConfiguration::new(
        shared.autoscaling_failover.enabled,
        shared.autoscaling_failover.metrics.clone(),
    ));

    blueprint
        .add_transform("af_metrics_gateway", gateway)?
        .add_encoder("af_metrics_encode", branch.encoder)?
        .add_forwarder("cluster_agent_out", branch.forwarder)?
        .connect_components_in_order([
            METRICS_DESTINATION,
            "af_metrics_gateway",
            "af_metrics_encode",
            "cluster_agent_out",
        ])?;

    Ok(())
}
