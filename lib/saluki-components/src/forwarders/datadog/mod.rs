use async_trait::async_trait;
use http::Uri;
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_component_config::forwarder::DatadogForwarderConfig;
use saluki_component_config::ScopedConfig;
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
    validation::ValidationReadiness,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT,
};

/// Datadog forwarder.
///
/// Forwards Datadog-specific payloads to the Datadog platform. Handles the standard Datadog Agent configuration,
/// in terms of specifying additional endpoints, adding the necessary HTTP request headers for authentication,
/// identification, and more.
///
/// This is a dynamic-capable component: it consumes a [`ScopedConfig<DatadogForwarderConfig>`] and
/// reads the latest configuration (notably rotated API keys) at request-build time via the live
/// handle threaded into the resolved endpoints.
pub struct DatadogForwarderConfiguration {
    /// The live forwarder configuration handle.
    config: ScopedConfig<DatadogForwarderConfig>,

    /// Optional endpoint override (for example, Multi-Region Failover).
    ///
    /// When set, the only endpoint built is this URL with this API key, and additional/OPW endpoints
    /// are dropped. The endpoint still refreshes its API key from the latest `endpoint.api_key` value
    /// of [`config`](Self::config), so the configuration system can rotate the override-region key by
    /// publishing it on the same slice.
    endpoint_override: Option<(String, String)>,
}

impl DatadogForwarderConfiguration {
    /// Creates a new `DatadogForwarderConfiguration` from the given native configuration handle.
    pub fn from_native(config: ScopedConfig<DatadogForwarderConfig>) -> Self {
        Self {
            config,
            endpoint_override: None,
        }
    }

    /// Overrides the default endpoint and uses the given API key.
    ///
    /// This is for override endpoints, such as Multi-Region Failover, whose endpoint and API key come
    /// from a separate configuration slice. The override is applied to the configuration built at
    /// `build()` time; the resulting endpoint continues to refresh its API key from the latest value
    /// of the live handle's `endpoint.api_key` field.
    pub fn with_endpoint_override(mut self, dd_url: String, api_key: String) -> Self {
        self.endpoint_override = Some((dd_url, api_key));

        self
    }

    /// Builds the static forwarder configuration snapshot used to size the I/O machinery, applying
    /// any endpoint override.
    fn build_forwarder_config(&self) -> ForwarderConfiguration {
        let mut forwarder_config = ForwarderConfiguration::from_native(&self.config.current());
        if let Some((dd_url, api_key)) = &self.endpoint_override {
            let endpoint = forwarder_config.endpoint_mut();
            endpoint.clear_additional_endpoints();
            endpoint.set_dd_url(dd_url.clone());
            endpoint.set_api_key(api_key.clone());
            forwarder_config.clear_opw_metrics_endpoint();
        }
        forwarder_config
    }
}

#[async_trait]
impl ForwarderBuilder for DatadogForwarderConfiguration {
    fn input_payload_type(&self) -> PayloadType {
        PayloadType::Http
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Forwarder + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let forwarder_config = self.build_forwarder_config();
        let forwarder = TransactionForwarder::from_config(
            context,
            forwarder_config,
            Some(self.config.clone()),
            get_dd_endpoint_name,
            telemetry.clone(),
            metrics_builder,
        )?;

        Ok(Box::new(Datadog { forwarder }))
    }
}

impl MemoryBounds for DatadogForwarderConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        let forwarder_config = self.build_forwarder_config();

        builder
            .minimum()
            .with_single_value::<Datadog>("component struct")
            .with_array::<Transaction<FrozenChunkedBytesBuffer>>("requests channel", 8);

        builder
            .firm()
            // TODO: This is a little wonky because we're accounting for the firm bound portion of connected encoders here, as this
            // is the only place where we can calculate how many requests we'll hold on to in memory, which is what ultimately influences
            // the firm usage.
            //
            // We're also cheating by knowing what the largest possible payload is that we'll potentially see, based on the limits on the
            // Datadog encoders. This won't necessarily hold up for future sources/encoders, but is good enough for now.
            .with_expr(UsageExpr::sum(
                "in-flight requests",
                UsageExpr::config(
                    "forwarder_retry_queue_payloads_max_size",
                    forwarder_config.retry().queue_max_size_bytes() as usize,
                ),
                UsageExpr::product(
                    "high priority queue",
                    UsageExpr::config(
                        "forwarder_high_prio_buffer_size",
                        forwarder_config.endpoint_buffer_size(),
                    ),
                    // TODO: The default compressed size limit just so happens to be the biggest one we currently default with on our side,
                    // but it's not clear that this will always be the case.
                    UsageExpr::constant("maximum compressed payload size", DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT),
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

        let mut validation = forwarder.api_key_validator().spawn();

        // Spawn our forwarder task to handle sending requests.
        let forwarder = forwarder.spawn().await;

        debug!("Datadog forwarder started.");

        loop {
            select! {
                _ = health.live() => continue,
                readiness = validation.wait_for_change() => match readiness {
                    ValidationReadiness::Ready => health.mark_ready(),
                    ValidationReadiness::NotReady => health.mark_not_ready(),
                },
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
        validation.abort();
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
    use saluki_component_config::forwarder::EndpointConfiguration;
    use tokio::sync::watch;

    use super::*;

    /// Builds an MRF-style forwarder slice whose `endpoint.api_key` carries the override-region key.
    fn mrf_slice(api_key: &str) -> DatadogForwarderConfig {
        DatadogForwarderConfig {
            endpoint: EndpointConfiguration {
                api_key: api_key.to_string(),
                dd_url: Some("http://mrf.example.test".to_string()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn endpoint_override_refreshes_from_mrf_api_key() {
        // The MRF forwarder consumes a live slice whose endpoint.api_key is the failover-region key.
        let initial = mrf_slice("mrf-api-key");
        let (tx, rx) = watch::channel(initial.clone());
        let live = ScopedConfig::live(initial, rx);

        let config = DatadogForwarderConfiguration::from_native(live)
            .with_endpoint_override("http://mrf.example.test".to_string(), "mrf-api-key".to_string());

        let forwarder_config = config.build_forwarder_config();
        let mut endpoints = forwarder_config
            .build_routable_endpoints(Some(config.config.clone()))
            .expect("endpoint should resolve");

        assert_eq!(endpoints.len(), 1);
        let (_, mut endpoint) = endpoints.pop().unwrap().into_parts();
        assert_eq!(endpoint.cached_api_key(), "mrf-api-key");
        assert!(endpoint.has_live_config());
        assert_eq!(endpoint.api_key(), "mrf-api-key");

        // Publishing a rotated MRF slice updates the endpoint's key on the next refresh.
        tx.send(mrf_slice("rotated-mrf-api-key")).expect("receiver alive");

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
