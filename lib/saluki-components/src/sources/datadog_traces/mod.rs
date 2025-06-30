use async_trait::async_trait;
use axum::body::Body;
use axum::extract::Request;
use axum::routing::post;
use axum::Router;
use futures::TryStreamExt;
use hyper::body::Incoming;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::data_model::event::EventType;
use saluki_core::{
    components::{sources::*, ComponentContext},
    topology::OutputDefinition,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::net::listener::ConnectionOrientedListener;
use saluki_io::net::server::http::HttpServer;
use saluki_io::net::util::hyper::TowerToHyperService;
use saluki_io::net::ListenAddress;
use serde::Deserialize;
use tokio::select;
use tower::ServiceBuilder;
use tracing::{debug, error, info};

/// Datadog Traces source.
#[derive(Deserialize)]
pub struct DatadogTracesConfiguration;

impl DatadogTracesConfiguration {
    /// Creates a new `DatadogTracesConfiguration` from the given configuration.
    pub fn from_configuration(_config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(Self)
    }
}

#[async_trait]
impl SourceBuilder for DatadogTracesConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(DatadogTraces))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: [OutputDefinition; 1] = [OutputDefinition::default_output(EventType::Trace)];
        &OUTPUTS
    }
}

impl MemoryBounds for DatadogTracesConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<DatadogTraces>("source struct");
    }
}

pub struct DatadogTraces;

#[async_trait]
impl Source for DatadogTraces {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();

        health.mark_ready();
        info!("Datadog Traces source started.");

        // Create our listener and then our HTTP server to handle traces.
        let listen_address = ListenAddress::Tcp(([127, 0, 0, 1], 8127).into());
        let listener = ConnectionOrientedListener::from_listen_address(listen_address)
            .await
            .error_context("Failed to create listener.")?;

        let service = ServiceBuilder::new()
            .layer_fn(TowerToHyperService::new)
            .map_request(map_hyper_body_to_axum_body)
            .service(build_service());

        let http_server = HttpServer::from_listener(listener, service);
        let (http_shutdown_handle, mut http_error_handle) = http_server.listen();

        // Wait for the global shutdown signal, then notify listener to shutdown.
        //
        // We also handle liveness here, which doesn't really matter for _this_ task, since the real work is happening
        // in the listener, but we need to satisfy the health checker.
        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break
                },
                Some(e) = &mut http_error_handle => {
                    error!("HTTP server error: {}", e);
                    break
                },
                _ = health.live() => continue,
            }
        }

        // Tell the HTTP server to shutdown.
        http_shutdown_handle.shutdown();

        info!("Datadog Traces source stopped.");

        Ok(())
    }
}

fn build_service() -> Router {
    Router::new().route("/v0.7/traces", post(handle_post_v07_traces))
}

fn map_hyper_body_to_axum_body(req: Request<Incoming>) -> Request {
    req.map(Body::new)
}

#[axum::debug_handler]
async fn handle_post_v07_traces(req: Request) {
    let (parts, body) = req.into_parts();

    let maybe_body_len = body
        .into_data_stream()
        .try_fold(0, |acc, chunk| async move { Ok(acc + chunk.len()) })
        .await;
    let body_len = match maybe_body_len {
        Ok(len) => len,
        Err(e) => {
            error!("Failed to read body: {}", e);
            return;
        }
    };

    info!(body_len, headers = ?parts.headers, "Received v0.7 traces payload.");
}
