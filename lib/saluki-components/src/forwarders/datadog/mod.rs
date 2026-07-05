use agent_data_plane_config::{shared::SharedConfiguration, Live, SalukiConfiguration};
use async_trait::async_trait;
use http::Uri;
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_core::{
    components::{forwarders::*, ComponentContext},
    data_model::payload::{PayloadMetadata, PayloadType},
    observability::ComponentMetricsExt as _,
};
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::select;
use tracing::debug;

use crate::common::datadog::{
    config::ForwarderConfiguration,
    endpoints::ApiKeyRefresh,
    io::TransactionForwarder,
    protocol::MetricsPayloadInfo,
    telemetry::ComponentTelemetry,
    transaction::{Metadata, Transaction},
    validation::ValidationReadiness,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, METRICS_SERIES_V3_BETA_PATH, METRICS_SERIES_V3_PATH,
    METRICS_SKETCHES_V3_PATH,
};

/// Datadog forwarder.
///
/// Forwards Datadog-specific payloads to the Datadog platform. Handles the standard Datadog Agent configuration,
/// in terms of specifying additional endpoints, adding the necessary HTTP request headers for authentication,
/// identification, and more.
pub struct DatadogForwarderConfiguration {
    /// Forwarder configuration settings.
    ///
    /// See [`ForwarderConfiguration`] for more information about the available settings.
    forwarder_config: ForwarderConfiguration,

    live: Option<Live<SalukiConfiguration>>,
}

impl DatadogForwarderConfiguration {
    /// Creates a new `DatadogForwarderConfiguration` from the shared configuration model.
    ///
    /// The optional live view lets resolved endpoints refresh their API keys at runtime.
    pub fn from_model(
        shared: &SharedConfiguration, live: Option<Live<SalukiConfiguration>>,
    ) -> Result<Self, GenericError> {
        let forwarder_config = ForwarderConfiguration::from_model(shared)?;
        Ok(Self { forwarder_config, live })
    }

    /// Overrides the default endpoint with the Multi-Region Failover endpoint, refreshing its API
    /// key from the Multi-Region Failover model field rather than the shared primary `api_key`.
    pub fn with_multi_region_failover_endpoint_override(mut self, dd_url: String, api_key: String) -> Self {
        self.apply_endpoint_override(dd_url, api_key, ApiKeyRefresh::MultiRegionFailover);

        self
    }

    fn apply_endpoint_override(&mut self, dd_url: String, api_key: String, api_key_refresh: ApiKeyRefresh) {
        // Clear any existing additional endpoints, and set the new DD URL and API key.
        //
        // This ensures that the only endpoint we'll send to is this one.
        let endpoint = self.forwarder_config.endpoint_mut();
        endpoint.clear_additional_endpoints();
        endpoint.set_dd_url(dd_url);
        endpoint.set_api_key(api_key);
        endpoint.set_api_key_refresh(api_key_refresh);
        self.forwarder_config.clear_opw_metrics_endpoint();
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
        let forwarder = TransactionForwarder::from_config(
            context,
            self.forwarder_config.clone(),
            self.live.clone(),
            get_dd_endpoint_name,
            telemetry.clone(),
            metrics_builder,
        )?;

        Ok(Box::new(Datadog { forwarder }))
    }
}

impl MemoryBounds for DatadogForwarderConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
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
                    self.forwarder_config.retry().queue_max_size_bytes() as usize,
                ),
                UsageExpr::product(
                    "high priority queue",
                    UsageExpr::config(
                        "forwarder_high_prio_buffer_size",
                        self.forwarder_config.endpoint_buffer_size(),
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
                        let transaction_meta = transaction_metadata_from_payload_metadata(&payload_meta);
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

fn transaction_metadata_from_payload_metadata(payload_meta: &PayloadMetadata) -> Metadata {
    let mut transaction_meta =
        Metadata::from_event_and_data_point_count(payload_meta.event_count(), payload_meta.data_point_count());
    transaction_meta.payload_info = payload_meta.get::<MetricsPayloadInfo>().copied();
    transaction_meta
}

fn get_dd_endpoint_name(uri: &Uri) -> Option<MetaString> {
    match uri.path() {
        "/api/v2/logs" => Some(MetaString::from_static("logs_v2")),
        "/api/v1/series" => Some(MetaString::from_static("series_v1")),
        "/api/v2/series" => Some(MetaString::from_static("series_v2")),
        METRICS_SERIES_V3_PATH => Some(MetaString::from_static("series_v3")),
        METRICS_SERIES_V3_BETA_PATH => Some(MetaString::from_static("series_v3beta")),
        "/api/beta/sketches" => Some(MetaString::from_static("sketches_v2")),
        METRICS_SKETCHES_V3_PATH => Some(MetaString::from_static("sketches_v3")),
        "/api/v1/check_run" => Some(MetaString::from_static("check_run_v1")),
        "/api/v1/events_batch" => Some(MetaString::from_static("events_batch_v1")),
        "/api/v0.2/traces" => Some(MetaString::from_static("traces_v0.2")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use tokio::sync::watch;

    use super::*;

    fn config_with_keys(primary: &str, mrf: &str) -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        config.shared.endpoints.api_key = primary.to_string();
        config.domains.multi_region_failover.api_key = Some(mrf.to_string());
        config
    }

    #[tokio::test]
    async fn endpoint_override_refreshes_from_mrf_api_key() {
        let initial = config_with_keys("primary-api-key", "mrf-api-key");
        let cell = Arc::new(ArcSwap::from_pointee(initial.clone()));
        let (tx, rx) = watch::channel(());
        let live = Live::dynamic(Arc::clone(&cell), rx, |c| c);

        let config = DatadogForwarderConfiguration::from_model(&initial.shared, Some(live.clone()))
            .expect("DatadogForwarderConfiguration should build")
            .with_multi_region_failover_endpoint_override(
                "http://mrf.example.test".to_string(),
                "mrf-api-key".to_string(),
            );

        let mut endpoints = config
            .forwarder_config
            .build_routable_endpoints(Some(live))
            .expect("endpoint should resolve");

        assert_eq!(endpoints.len(), 1);
        let (_, mut endpoint) = endpoints.pop().unwrap().into_parts();
        assert_eq!(endpoint.cached_api_key(), "mrf-api-key");
        assert!(endpoint.has_configuration());
        assert_eq!(endpoint.api_key(), "mrf-api-key");

        // Rotating the primary key must not affect the override; rotating the MRF key must.
        cell.store(Arc::new(config_with_keys(
            "rotated-primary-api-key",
            "rotated-mrf-api-key",
        )));
        tx.send(()).expect("live cell should have a receiver");

        assert_eq!(endpoint.api_key(), "rotated-mrf-api-key");
    }
}
