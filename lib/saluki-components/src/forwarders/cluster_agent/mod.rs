//! Cluster Agent forwarder.

use async_trait::async_trait;
use http::{
    header::AUTHORIZATION,
    uri::{Authority, Scheme},
    HeaderName, HeaderValue, Request, Uri,
};
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_config_tools::GenericConfiguration;
use saluki_core::{
    components::{forwarders::*, ComponentContext},
    data_model::payload::PayloadType,
    observability::ComponentMetricsExt as _,
};
use saluki_error::{generic_error, GenericError};
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::select;
use tracing::debug;

use crate::common::datadog::{
    config::ForwarderConfiguration,
    endpoints::ResolvedEndpoint,
    io::{EndpointRequestMapper, EndpointRequestMapperFactory, TransactionForwarder},
    telemetry::ComponentTelemetry,
    transaction::{Metadata, Transaction, TransactionBody},
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT,
};

const CLUSTER_AGENT_SERIES_PATH: &str = "/series";

static DD_API_KEY_HEADER: HeaderName = HeaderName::from_static("dd-api-key");

/// Cluster Agent forwarder configuration.
///
/// This forwarder sends encoded metric series payloads to the Cluster Agent `/series` endpoint using bearer-token
/// authorization. It intentionally does not use Datadog intake endpoint configuration or `DD-Api-Key` auth.
pub struct ClusterAgentForwarderConfiguration {
    forwarder_config: ForwarderConfiguration,
    auth_header_value: HeaderValue,
}

impl ClusterAgentForwarderConfiguration {
    /// Creates a new `ClusterAgentForwarderConfiguration` from the given Cluster Agent endpoint and bearer token.
    pub fn from_configuration(
        config: &GenericConfiguration, endpoint_url: String, auth_token: String,
    ) -> Result<Self, GenericError> {
        let auth_header_value = bearer_auth_header_value(&auth_token)?;
        let mut forwarder_config = ForwarderConfiguration::from_configuration(config)?.with_allow_arbitrary_tags(false);

        let endpoint = forwarder_config.endpoint_mut();
        endpoint.clear_additional_endpoints();
        endpoint.set_dd_url(endpoint_url);
        endpoint.set_api_key(auth_token);
        forwarder_config.clear_opw_metrics_endpoint();

        Ok(Self {
            forwarder_config,
            auth_header_value,
        })
    }
}

#[async_trait]
impl ForwarderBuilder for ClusterAgentForwarderConfiguration {
    fn input_payload_type(&self) -> PayloadType {
        PayloadType::Http
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Forwarder + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let endpoint_request_mapper_factory = cluster_agent_request_mapper_factory(self.auth_header_value.clone());
        let forwarder = TransactionForwarder::from_config_with_endpoint_request_mapper(
            context,
            self.forwarder_config.clone(),
            None,
            get_cluster_agent_endpoint_name,
            telemetry.clone(),
            metrics_builder,
            endpoint_request_mapper_factory,
        )?;

        Ok(Box::new(ClusterAgentForwarder { forwarder }))
    }
}

impl MemoryBounds for ClusterAgentForwarderConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<ClusterAgentForwarder>("component struct")
            .with_array::<Transaction<FrozenChunkedBytesBuffer>>("requests channel", 8);

        builder.firm().with_expr(UsageExpr::sum(
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
                UsageExpr::constant("maximum compressed payload size", DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT),
            ),
        ));
    }
}

/// Cluster Agent forwarder.
pub struct ClusterAgentForwarder {
    forwarder: TransactionForwarder<FrozenChunkedBytesBuffer>,
}

#[async_trait]
impl Forwarder for ClusterAgentForwarder {
    async fn run(mut self: Box<Self>, mut context: ForwarderContext) -> Result<(), GenericError> {
        let Self { forwarder } = *self;

        let mut health = context.take_health_handle();
        let forwarder = forwarder.spawn().await;

        health.mark_ready();
        debug!("Cluster Agent forwarder started.");

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

        forwarder.shutdown().await;

        debug!("Cluster Agent forwarder stopped.");

        Ok(())
    }
}

fn bearer_auth_header_value(auth_token: &str) -> Result<HeaderValue, GenericError> {
    let raw_value = format!("Bearer {auth_token}");
    HeaderValue::from_str(&raw_value)
        .map_err(|_| generic_error!("cluster_agent.auth_token contains characters that are invalid in HTTP headers."))
}

fn cluster_agent_request_mapper_factory<B>(auth_header_value: HeaderValue) -> EndpointRequestMapperFactory<B>
where
    B: 'static,
{
    std::sync::Arc::new(move |endpoint| cluster_agent_request_mapper(endpoint, auth_header_value.clone()))
}

fn cluster_agent_request_mapper<B>(
    endpoint: ResolvedEndpoint, auth_header_value: HeaderValue,
) -> EndpointRequestMapper<B> {
    let new_uri_authority = Authority::try_from(endpoint.endpoint().authority())
        .expect("should not fail to construct new endpoint authority");
    let new_uri_scheme =
        Scheme::try_from(endpoint.endpoint().scheme()).expect("should not fail to construct new endpoint scheme");

    Box::new(move |mut request: Request<TransactionBody<B>>| {
        let new_uri = Uri::builder()
            .scheme(new_uri_scheme.clone())
            .authority(new_uri_authority.clone())
            .path_and_query(CLUSTER_AGENT_SERIES_PATH)
            .build()
            .expect("should not fail to construct Cluster Agent URI");
        *request.uri_mut() = new_uri;
        request.headers_mut().remove(&DD_API_KEY_HEADER);
        request.headers_mut().insert(AUTHORIZATION, auth_header_value.clone());

        request
    })
}

fn get_cluster_agent_endpoint_name(uri: &Uri) -> Option<MetaString> {
    match uri.path() {
        CLUSTER_AGENT_SERIES_PATH => Some(MetaString::from_static("cluster_agent_series")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use http::Method;
    use saluki_config_tools::ConfigurationLoader;
    use serde_json::json;

    use super::*;
    use crate::common::datadog::endpoints::EndpointRoute;

    #[test]
    fn request_mapper_targets_cluster_agent_series_endpoint_with_bearer_auth() {
        let auth_header_value = bearer_auth_header_value("secret-token").expect("auth header should be valid");
        let endpoint = ResolvedEndpoint::from_raw_endpoint("https://cluster-agent.example.com:5005", "secret-token")
            .expect("endpoint should resolve");
        let mut mapper = cluster_agent_request_mapper::<()>(endpoint, auth_header_value);
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/v2/series")
            .header("dd-api-key", "primary-api-key")
            .body(TransactionBody::<()>::Rehydrated(None))
            .expect("request should build");

        let request = mapper(request);

        assert_eq!(
            request.uri().to_string(),
            "https://cluster-agent.example.com:5005/series"
        );
        assert_eq!(request.headers().get(AUTHORIZATION).unwrap(), "Bearer secret-token");
        assert!(request.headers().get("dd-api-key").is_none());
    }

    #[test]
    fn bearer_auth_header_rejects_invalid_token() {
        assert!(bearer_auth_header_value("bad\ntoken").is_err());
    }

    #[tokio::test]
    async fn configuration_uses_only_cluster_agent_endpoint() {
        let (config, _) = ConfigurationLoader::for_tests(
            Some(json!({
                "api_key": "primary-api-key",
                "dd_url": "https://app.datadoghq.com",
                "additional_endpoints": {
                    "https://additional.example.com": ["additional-api-key"]
                },
                "observability_pipelines_worker": {
                    "metrics": {
                        "enabled": true,
                        "url": "https://opw.example.com"
                    }
                }
            })),
            None,
            false,
        )
        .await;

        let config = ClusterAgentForwarderConfiguration::from_configuration(
            &config,
            "https://cluster-agent.example.com".to_string(),
            "secret-token".to_string(),
        )
        .expect("Cluster Agent forwarder configuration should parse");
        let endpoints = config
            .forwarder_config
            .build_routable_endpoints(None)
            .expect("endpoint should resolve");

        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].route(), EndpointRoute::Primary);
        assert_eq!(
            endpoints[0].endpoint().endpoint().as_str(),
            "https://cluster-agent.example.com/"
        );
        assert_eq!(endpoints[0].endpoint().cached_api_key(), "secret-token");
    }
}
