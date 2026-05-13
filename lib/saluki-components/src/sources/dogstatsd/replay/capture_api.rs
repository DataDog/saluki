//! HTTP API handler for the DogStatsD capture control surface.
//!
//! Exposes two routes on the privileged API:
//! - `POST /dogstatsd/capture/trigger` — start a capture session
//! - `POST /dogstatsd/capture/tagger-state` — push a saved tagger snapshot into the running source
//!   for replay (currently stubbed)

use std::collections::HashMap;
use std::path::Path;

use saluki_api::{
    extract::State,
    routing::{post, Router},
    APIHandler, Json, StatusCode,
};
use saluki_config::parse_duration;
use serde::{Deserialize, Serialize};

use super::DogStatsDCaptureControl;

/// Request body for `POST /dogstatsd/capture/trigger`.
#[derive(Deserialize)]
pub struct CaptureTriggerBody {
    /// Duration of the capture, parsed by `parse_duration` (e.g. `"10s"`, `"500ms"`).
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

/// Request body for `POST /dogstatsd/capture/tagger-state`.
///
/// Mirrors the `TaggerState` proto used inside capture files: a snapshot of the PID→entity
/// mapping and the per-entity tag state, used during replay so that captured packets resolve
/// to their historical tags rather than whatever the live host's tagger thinks.
///
/// The `Entity` shape is currently `serde_json::Value`; it will be replaced with a typed
/// representation once the replay receive-side is implemented.
#[derive(Deserialize)]
pub struct SetTaggerStateBody {
    /// Map of entity ID to the entity's serialized state (tags, metadata, etc.).
    pub state: HashMap<String, serde_json::Value>,

    /// Map of PID to the entity ID that owned it at capture time.
    pub pid_map: HashMap<i32, String>,

    /// Duration of the original capture, in milliseconds.
    pub duration_ms: i64,
}

/// Response body for `POST /dogstatsd/capture/tagger-state`.
#[derive(Serialize)]
pub struct SetTaggerStateResponseBody {
    /// Whether the tagger state was successfully loaded into the running source.
    pub loaded: bool,
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

    async fn set_tagger_state_handler(
        State(_capture_control): State<DogStatsDCaptureControl>, Json(body): Json<SetTaggerStateBody>,
    ) -> Result<Json<SetTaggerStateResponseBody>, (StatusCode, String)> {
        Err((
            StatusCode::NOT_IMPLEMENTED,
            format!(
                "DogStatsD set-tagger-state is not implemented yet (received {} entities, {} pid mappings, {}ms duration).",
                body.state.len(),
                body.pid_map.len(),
                body.duration_ms,
            ),
        ))
    }
}

impl APIHandler for DogStatsDCaptureAPIHandler {
    type State = DogStatsDCaptureControl;

    fn generate_initial_state(&self) -> Self::State {
        self.capture_control.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new()
            .route("/dogstatsd/capture/trigger", post(Self::trigger_handler))
            .route("/dogstatsd/capture/tagger-state", post(Self::set_tagger_state_handler))
    }
}
