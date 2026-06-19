//! HTTP API handler for the DogStatsD capture control surface.
//!
//! Exposes `POST /dogstatsd/capture/trigger` on the privileged API to start a capture session.

use std::path::Path;

use saluki_api::{
    extract::State,
    routing::{post, Router},
    APIHandler, Json, StatusCode,
};
use saluki_config_tools::parse_duration;
use serde::{Deserialize, Serialize};

use super::DogStatsDCaptureControl;

/// Request body for `POST /dogstatsd/capture/trigger`.
#[derive(Deserialize)]
pub struct CaptureTriggerBody {
    /// Duration of the capture, parsed by `parse_duration` (for example, `"10s"`, `"500ms"`).
    pub duration: String,

    /// Optional override for the capture output directory. When omitted, the source's
    /// configured default directory is used.
    #[serde(default)]
    pub path: Option<String>,

    /// Whether the capture file should be zstd-compressed.
    #[serde(default)]
    pub compressed: bool,
}

/// Response body for `POST /dogstatsd/capture/trigger`.
#[derive(Serialize)]
pub struct CaptureTriggerResponseBody {
    /// Absolute path the capture is being written to.
    pub path: String,
}

/// API handler for the DogStatsD capture control surface.
#[derive(Clone)]
pub struct DogStatsDCaptureAPIHandler {
    capture_control: DogStatsDCaptureControl,
}

impl DogStatsDCaptureAPIHandler {
    /// Creates a new handler bound to the given capture control.
    pub fn new(capture_control: DogStatsDCaptureControl) -> Self {
        Self { capture_control }
    }

    async fn trigger_handler(
        State(capture_control): State<DogStatsDCaptureControl>, Json(body): Json<CaptureTriggerBody>,
    ) -> Result<Json<CaptureTriggerResponseBody>, (StatusCode, String)> {
        let duration = parse_duration(&body.duration).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        let requested_dir = body.path.as_deref().map(Path::new);

        let capture_path = capture_control
            .start_capture(requested_dir, duration, body.compressed)
            .map_err(|e| (StatusCode::PRECONDITION_FAILED, e.to_string()))?;

        Ok(Json(CaptureTriggerResponseBody {
            path: capture_path.display().to_string(),
        }))
    }
}

impl APIHandler for DogStatsDCaptureAPIHandler {
    type State = DogStatsDCaptureControl;

    fn generate_initial_state(&self) -> Self::State {
        self.capture_control.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route("/dogstatsd/capture/trigger", post(Self::trigger_handler))
    }
}
