//! Datadog forwarder component.
//!
//! # Missing
//!
//! - Account for connected encoder firm bounds in the encoder components once they can calculate request memory.
//! - Replace the fixed maximum compressed payload size if future sources or encoders use a larger default.

use async_trait::async_trait;
use http::Uri;
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{forwarders::*, ComponentContext},
    data_model::payload::PayloadType,
    observability::ComponentMetricsExt as _,
};
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::select;
use tracing::debug;

use crate::common::datadog::{
    config::ForwarderConfiguration,
    io::TransactionForwarder,
    telemetry::ComponentTelemetry,
    transaction::{Metadata, Transaction},
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT,
};

/// Datadog forwarder.
///
/// Forwards Datadog-specific payloads to the Datadog platform. Handles the standard Datadog Agent configuration,
/// in terms of specifying additional endpoints, adding the necessary HTTP request headers for authentication,
/// identification, and more.
pub struct DatadogConfiguration {
    /// Forwarder configuration settings.
    ///
    /// See [`ForwarderConfiguration`] for more information about the available settings.
    forwarder_config: ForwarderConfiguration,

    configuration: Option<GenericConfiguration>,
}

impl DatadogConfiguration {
    /// Creates a new `DatadogConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let forwarder_config = ForwarderConfiguration::from_configuration(config)?;
        Ok(Self {
            forwarder_config,
            configuration: Some(config.clone()),
        })
    }

    /// Overrides the default endpoint and refreshes its API key from the given config path.
    ///
    /// This is for override endpoints whose API key does not refresh from the top-level `api_key`
    /// config path, such as Multi-Region Failover.
    pub fn with_endpoint_override_and_api_key_refresh_config_path(
        mut self, dd_url: String, api_key: String, api_key_refresh_config_path: &'static str,
    ) -> Self {
        self.apply_endpoint_override(dd_url, api_key, api_key_refresh_config_path);

        self
    }

    fn apply_endpoint_override(&mut self, dd_url: String, api_key: String, api_key_refresh_config_path: &'static str) {
        // Clear any existing additional endpoints, and set the new DD URL and API key.
        //
        // This ensures that the only endpoint we'll send to is this one.
        let endpoint = self.forwarder_config.endpoint_mut();
        endpoint.clear_additional_endpoints();
        endpoint.set_dd_url(dd_url);
        endpoint.set_api_key(api_key);
        endpoint.set_api_key_refresh_config_path(api_key_refresh_config_path);
        self.forwarder_config.clear_opw_metrics_endpoint();
    }
}

#[async_trait]
impl ForwarderBuilder for DatadogConfiguration {
    fn input_payload_type(&self) -> PayloadType {
        PayloadType::Http
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Forwarder + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let forwarder = TransactionForwarder::from_config(
            context,
            self.forwarder_config.clone(),
            self.configuration.clone(),
            get_dd_endpoint_name,
            telemetry.clone(),
            metrics_builder,
        )?;

        Ok(Box::new(Datadog { forwarder }))
    }
}

impl MemoryBounds for DatadogConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<Datadog>("component struct")
            .with_array::<Transaction<FrozenChunkedBytesBuffer>>("requests channel", 8);

        builder
            .firm()
            // This accounts for connected encoder firm bounds here because request retention is calculated by the
            // forwarder.
            //
            // The maximum compressed payload size comes from the largest Datadog encoder default currently in use.
            .with_expr(UsageExpr::sum(
                "in-flight requests",
                UsageExpr::config(
                    "forwarder_retry_queue_payloads_max_size",
                    self.forwarder_config.retry().queue_max_size_bytes() as usize,
                ),
                UsageExpr::sum(
                    "priority queues",
                    UsageExpr::product(
                        "high priority queue",
                        UsageExpr::config(
                            "forwarder_high_prio_buffer_size",
                            self.forwarder_config.endpoint_buffer_size(),
                        ),
                        UsageExpr::constant("maximum compressed payload size", DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT),
                    ),
                    UsageExpr::product(
                        "low priority queue",
                        UsageExpr::config(
                            "forwarder_low_prio_buffer_size",
                            self.forwarder_config.low_priority_buffer_size(),
                        ),
                        UsageExpr::constant("maximum compressed payload size", DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT),
                    ),
                ),
            ));
    }
}

pub struct Datadog {
    forwarder: TransactionForwarder<FrozenChunkedBytesBuffer>,
}

#[async_trait]
impl Forwarder for Datadog {
    async fn run(mut self: Box<Self>, mut context: ForwarderContext) -> Result<(), GenericError> {
        let Self { forwarder } = *self;

        let mut health = context.take_health_handle();

        // Spawn our forwarder task to handle sending requests.
        let forwarder = forwarder.spawn().await;

        health.mark_ready();
        debug!("Datadog forwarder started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_payload = context.payloads().next() => match maybe_payload {
                    Some(payload) => if let Some(http_payload) = payload.try_into_http_payload() {
                        let (payload_meta, request) = http_payload.into_parts();
                        let transaction_meta = Metadata::from_event_and_data_point_count(
                            payload_meta.event_count(),
                            payload_meta.data_point_count(),
                        );
                        let transaction = Transaction::from_original(transaction_meta, request);

                        forwarder.send_transaction(transaction).await?;
                    }
                    None => break,
                },
            }
        }

        // Shutdown the forwarder gracefully.
        forwarder.shutdown().await;

        debug!("Datadog forwarder stopped.");

        Ok(())
    }
}

fn get_dd_endpoint_name(uri: &Uri) -> Option<MetaString> {
    match uri.path() {
        "/api/v2/logs" => Some(MetaString::from_static("logs_v2")),
        "/api/v1/series" => Some(MetaString::from_static("series_v1")),
        "/api/v2/series" => Some(MetaString::from_static("series_v2")),
        "/api/beta/sketches" => Some(MetaString::from_static("sketches_v2")),
        "/api/v1/check_run" => Some(MetaString::from_static("check_run_v1")),
        "/api/v1/events_batch" => Some(MetaString::from_static("events_batch_v1")),
        "/api/v0.2/traces" => Some(MetaString::from_static("traces_v0.2")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use saluki_config::ConfigurationLoader;
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn endpoint_override_refreshes_from_mrf_api_key() {
        let (generic_config, sender) = ConfigurationLoader::for_tests(
            Some(json!({
                "api_key": "primary-api-key",
                "multi_region_failover": {
                    "api_key": "mrf-api-key"
                }
            })),
            None,
            true,
        )
        .await;
        let sender = sender.expect("dynamic sender should exist");
        sender
            .send(saluki_config::dynamic::ConfigUpdate::Snapshot(json!({})))
            .await
            .expect("initial dynamic snapshot should be sent");
        generic_config.ready().await;

        let config = DatadogConfiguration::from_configuration(&generic_config)
            .expect("DatadogConfiguration should parse")
            .with_endpoint_override_and_api_key_refresh_config_path(
                "http://mrf.example.test".to_string(),
                "mrf-api-key".to_string(),
                "multi_region_failover.api_key",
            );

        let mut endpoints = config
            .forwarder_config
            .build_routable_endpoints(config.configuration.clone())
            .expect("endpoint should resolve");

        assert_eq!(endpoints.len(), 1);
        let (_, mut endpoint) = endpoints.pop().unwrap().into_parts();
        assert_eq!(endpoint.cached_api_key(), "mrf-api-key");
        assert!(endpoint.has_configuration());
        assert_eq!(endpoint.api_key(), "mrf-api-key");

        sender
            .send(saluki_config::dynamic::ConfigUpdate::Partial {
                key: "api_key".to_string(),
                value: json!("rotated-primary-api-key"),
            })
            .await
            .expect("primary API key update should be sent");
        sender
            .send(saluki_config::dynamic::ConfigUpdate::Partial {
                key: "multi_region_failover.api_key".to_string(),
                value: json!("rotated-mrf-api-key"),
            })
            .await
            .expect("MRF API key update should be sent");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if endpoint.api_key() == "rotated-mrf-api-key" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for endpoint override to refresh from MRF API key"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}
