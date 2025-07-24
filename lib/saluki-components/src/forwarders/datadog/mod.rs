use async_trait::async_trait;
use http::Uri;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_config::{GenericConfiguration, RefreshableConfiguration};
use saluki_core::{
    components::{forwarders::*, ComponentContext},
    data_model::payload::PayloadType,
    observability::ComponentMetricsExt as _,
};
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
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
#[derive(Deserialize)]
pub struct DatadogConfiguration {
    /// Forwarder configuration settings.
    ///
    /// See [`ForwarderConfiguration`] for more information about the available settings.
    #[serde(flatten)]
    forwarder_config: ForwarderConfiguration,

    #[serde(skip)]
    config_refresher: Option<RefreshableConfiguration>,
}

impl DatadogConfiguration {
    /// Creates a new `DatadogConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Add option to retrieve configuration values from a `RefreshableConfiguration`.
    pub fn add_refreshable_configuration(&mut self, refresher: RefreshableConfiguration) {
        self.config_refresher = Some(refresher);
    }

    /// Overrides the default endpoint that payloads are sent to.
    ///
    /// This overrides any existing endpoint configuration, and manually sets the base endpoint (e.g.,
    /// `https://api.datad0g.com`) to be used for all payloads.
    ///
    /// This can be used to preserve other configuration settings (forwarder settings, retry, etc) while still allowing
    /// for overriding _where_ payloads are sent to.
    ///
    /// # Errors
    ///
    /// If the given request path is not valid, an error is returned.
    pub fn with_endpoint_override(mut self, dd_url: String, api_key: String) -> Self {
        // Clear any existing additional endpoints, and set the new DD URL and API key.
        //
        // This ensures that the only endpoint we'll send to is this one.
        let endpoint = self.forwarder_config.endpoint_mut();
        endpoint.clear_additional_endpoints();
        endpoint.set_dd_url(dd_url);
        endpoint.set_api_key(api_key);

        self
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
            self.config_refresher.clone(),
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
                        let transaction_meta = Metadata::from_event_count(payload_meta.event_count());
                        let transaction = Transaction::from_original(transaction_meta, request);

                        forwarder.send_transaction(transaction).await?;
                    },
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
        "/api/v2/series" => Some(MetaString::from_static("series_v2")),
        "/api/beta/sketches" => Some(MetaString::from_static("sketches_v2")),
        "/api/intake/pipelines/ddseries" => Some(MetaString::from_static("preaggregation")),
        "/api/v1/check_run" => Some(MetaString::from_static("check_run_v1")),
        "/api/v1/events_batch" => Some(MetaString::from_static("events_batch_v1")),
        _ => None,
    }
}
