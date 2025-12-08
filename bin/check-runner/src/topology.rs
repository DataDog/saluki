//! Topology creation for the check-runner binary.
//!
//! This module creates a minimal topology for running checks and forwarding their results.

use memory_accounting::ComponentRegistry;
use saluki_components::{
    encoders::{
        BufferedIncrementalConfiguration, DatadogEventsConfiguration, DatadogMetricsConfiguration,
        DatadogServiceChecksConfiguration,
    },
    forwarders::DatadogConfiguration,
    sources::ChecksConfiguration,
    transforms::{AggregateConfiguration, ChainedConfiguration, HostEnrichmentConfiguration, HostTagsConfiguration},
};
use saluki_config::GenericConfiguration;
use saluki_core::topology::TopologyBlueprint;
use saluki_error::{ErrorContext as _, GenericError};

use crate::config::ExecutionMode;
use crate::env_provider::CheckRunnerEnvironmentProvider;

/// Creates the check-runner topology.
///
/// This creates a minimal pipeline for running checks:
/// - ChecksSource for executing checks
/// - Aggregation transform for metrics
/// - Host enrichment transform
/// - Encoders for metrics, events, and service checks
/// - Datadog forwarder for sending results
pub async fn create_topology(
    configuration: &GenericConfiguration,
    env_provider: &CheckRunnerEnvironmentProvider,
    component_registry: &ComponentRegistry,
    execution_mode: ExecutionMode,
) -> Result<TopologyBlueprint, GenericError> {
    // Configure aggregation transform
    // For "once" mode, we need to flush open windows on shutdown
    let agg_config = if execution_mode == ExecutionMode::Once {
        // Create config that will flush open windows
        // We'll set this via the configuration or override
        AggregateConfiguration::from_configuration(configuration)
            .error_context("Failed to configure aggregate transform.")?
    } else {
        AggregateConfiguration::from_configuration(configuration)
            .error_context("Failed to configure aggregate transform.")?
    };

    // Configure host enrichment
    let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(env_provider.clone());
    let host_tags_config = HostTagsConfiguration::from_configuration(configuration).await?;
    let enrich_config = ChainedConfiguration::default()
        .with_transform_builder("host_enrichment", host_enrichment_config)
        .with_transform_builder("host_tags", host_tags_config);

    // Configure encoders
    let dd_metrics_config = DatadogMetricsConfiguration::from_configuration(configuration)
        .error_context("Failed to configure Datadog Metrics encoder.")?;
    let dd_events_config = DatadogEventsConfiguration::from_configuration(configuration)
        .map(BufferedIncrementalConfiguration::from_encoder_builder)
        .error_context("Failed to configure Datadog Events encoder.")?;
    let dd_service_checks_config = DatadogServiceChecksConfiguration::from_configuration(configuration)
        .map(BufferedIncrementalConfiguration::from_encoder_builder)
        .error_context("Failed to configure Datadog Service Checks encoder.")?;

    // Configure forwarder
    let dd_forwarder_config = DatadogConfiguration::from_configuration(configuration)
        .error_context("Failed to configure Datadog forwarder.")?;

    // Configure checks source
    let checks_config = ChecksConfiguration::from_configuration(configuration)
        .error_context("Failed to configure checks source.")?
        .with_environment_provider(env_provider.clone());

    // Build the topology blueprint
    let mut blueprint = TopologyBlueprint::new("check-runner", component_registry);

    blueprint
        // Components
        .add_source("checks_in", checks_config)?
        .add_transform("agg", agg_config)?
        .add_transform("enrich", enrich_config)?
        .add_encoder("dd_metrics_encode", dd_metrics_config)?
        .add_encoder("dd_events_encode", dd_events_config)?
        .add_encoder("dd_service_checks_encode", dd_service_checks_config)?
        .add_forwarder("dd_out", dd_forwarder_config)?
        // Metrics pipeline: checks -> aggregation -> enrichment -> encoder -> forwarder
        .connect_component("agg", ["checks_in.metrics"])?
        .connect_component("enrich", ["agg"])?
        .connect_component("dd_metrics_encode", ["enrich"])?
        // Events pipeline: checks -> encoder -> forwarder
        .connect_component("dd_events_encode", ["checks_in.events"])?
        // Service checks pipeline: checks -> encoder -> forwarder
        .connect_component("dd_service_checks_encode", ["checks_in.service_checks"])?
        // All encoders feed into the forwarder
        .connect_component(
            "dd_out",
            ["dd_metrics_encode", "dd_events_encode", "dd_service_checks_encode"],
        )?;

    Ok(blueprint)
}
