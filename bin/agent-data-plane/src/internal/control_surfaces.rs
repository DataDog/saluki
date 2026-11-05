use async_trait::async_trait;
use saluki_api::{APIHandler, DynamicRoute, EndpointType};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_components::{
    destinations::DogStatsDStatsAPIHandler,
    sources::{DogStatsDCaptureAPIHandler, DogStatsDReplayAPIHandler},
};
use saluki_core::runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture};
use saluki_error::generic_error;

use crate::dogstatsd_contexts::DogStatsDContextDumpAPIHandler;

/// A worker for exposing the DogStatsD control surfaces.
///
/// When running, the worker asserts the routes of each DogStatsD API handler on the privileged API endpoint.
pub struct DogStatsDAPIWorker {
    /// API handler for the `/dogstatsd/stats` endpoint.
    pub(crate) stats_api_handler: DogStatsDStatsAPIHandler,
    /// API handler for the `/dogstatsd/capture/trigger` endpoint.
    pub(crate) capture_api_handler: DogStatsDCaptureAPIHandler,
    /// API handler for the `/dogstatsd/replay/session` endpoints.
    pub(crate) replay_api_handler: DogStatsDReplayAPIHandler,
    /// API handler for the Agent-compatible `/agent/dogstatsd-contexts-dump` endpoint.
    pub(crate) context_dump_api_handler: DogStatsDContextDumpAPIHandler,
}

#[async_trait]
impl Supervisable for DogStatsDAPIWorker {
    fn name(&self) -> &str {
        "dsd-control-surfaces"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let routes = [
            ("dsd-stats-api", privileged_route(&self.stats_api_handler)),
            ("dsd-capture-api", privileged_route(&self.capture_api_handler)),
            ("dsd-replay-api", privileged_route(&self.replay_api_handler)),
            ("dsd-ctx-dump-api", privileged_route(&self.context_dump_api_handler)),
        ];

        Ok(Box::pin(async move {
            let dataspace =
                DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;

            for (id, route) in routes {
                dataspace.assert(route, id);
            }

            process_shutdown.await;
            Ok(())
        }))
    }
}

fn privileged_route<H: APIHandler>(handler: H) -> DynamicRoute {
    DynamicRoute::http(EndpointType::Privileged, handler)
}
