use saluki_app::dynamic_api::DynamicAPIBuilder;
use saluki_components::{
    destinations::DogStatsDStatsAPIHandler,
    sources::{DogStatsDCaptureAPIHandler, DogStatsDReplayAPIHandler},
};

/// Combined set of control surfaces to expose from the privileged API endpoint.
#[derive(Default)]
pub struct TopologyControlSurfaces {
    dogstatsd: Option<DogStatsDControlSurface>,
}

impl TopologyControlSurfaces {
    /// Attaches the DogStatsD-specific control surfaces to this set of topology control surfaces.
    pub fn attach_dogstatsd(&mut self, dsd_control_surface: DogStatsDControlSurface) {
        self.dogstatsd = Some(dsd_control_surface);
    }

    /// Registers all configured control surfaces with the given API builder.
    pub fn register_control_surfaces(self, mut builder: DynamicAPIBuilder) -> DynamicAPIBuilder {
        if let Some(dogstatsd) = self.dogstatsd {
            builder = dogstatsd.register_control_surfaces(builder);
        }

        builder
    }
}

/// DogStatsD-specific control surfaces.
pub struct DogStatsDControlSurface {
    /// API handler for the `/dogstatsd/stats` endpoint.
    pub(crate) stats_api_handler: DogStatsDStatsAPIHandler,
    /// API handler for the `/dogstatsd/capture/trigger` endpoint.
    pub(crate) capture_api_handler: DogStatsDCaptureAPIHandler,
    /// API handler for the `/dogstatsd/replay/session` endpoints.
    pub(crate) replay_api_handler: DogStatsDReplayAPIHandler,
}

impl DogStatsDControlSurface {
    fn register_control_surfaces(self, builder: DynamicAPIBuilder) -> DynamicAPIBuilder {
        builder
            .with_handler(self.stats_api_handler)
            .with_handler(self.capture_api_handler)
            .with_handler(self.replay_api_handler)
    }
}
