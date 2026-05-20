//! HTTP API handler for the DogStatsD replay control surface.
//!
//! Exposes `POST /dogstatsd/replay/trigger` on the privileged API. The body identifies the capture file to replay and
//! optionally overrides the loop count.

use std::path::PathBuf;

use saluki_api::{
    extract::State,
    routing::{post, Router},
    APIHandler, Json, StatusCode,
};
use serde::{Deserialize, Serialize};

use super::{DogStatsDReplayControl, ReplayOptions, DEFAULT_REPLAY_LOOPS};

/// Request body for `POST /dogstatsd/replay/trigger`.
#[derive(Deserialize)]
pub struct ReplayTriggerBody {
    /// Absolute path to the `.dog` or `.dog.zstd` capture file to replay.
    pub path: String,

    /// Number of times to replay the capture. `0` means replay forever until cancelled. Defaults to `1`.
    #[serde(default = "default_loops")]
    pub loops: u32,
}

fn default_loops() -> u32 {
    DEFAULT_REPLAY_LOOPS
}

/// Response body for `POST /dogstatsd/replay/trigger`.
#[derive(Serialize)]
pub struct ReplayTriggerResponseBody {
    /// Path the replay is reading from. Echoes back what the server accepted.
    pub path: String,
}

/// API handler for the DogStatsD replay control surface.
#[derive(Clone)]
pub struct DogStatsDReplayAPIHandler {
    replay_control: DogStatsDReplayControl,
}

impl DogStatsDReplayAPIHandler {
    /// Creates a new handler bound to the given replay control.
    pub fn new(replay_control: DogStatsDReplayControl) -> Self {
        Self { replay_control }
    }

    async fn trigger_handler(
        State(replay_control): State<DogStatsDReplayControl>, Json(body): Json<ReplayTriggerBody>,
    ) -> Result<Json<ReplayTriggerResponseBody>, (StatusCode, String)> {
        let opts = ReplayOptions::new(PathBuf::from(body.path)).with_loops(body.loops);

        let handle = replay_control.start_replay(opts).map_err(|e| {
            let message = e.to_string();
            // A concurrent replay is a conflict (409) rather than a precondition failure: the request is well-formed
            // but the system is currently in a state that rejects it.
            let status = if message.contains("replay already in progress") {
                StatusCode::CONFLICT
            } else {
                StatusCode::PRECONDITION_FAILED
            };
            (status, message)
        })?;

        Ok(Json(ReplayTriggerResponseBody {
            path: handle.path.display().to_string(),
        }))
    }
}

impl DogStatsDReplayAPIHandler {
    /// Stops the currently in-flight replay, if any.
    ///
    /// Returns `200` with `{ "stopped": true }` if a replay was running and has now been signaled to cancel; `200`
    /// with `{ "stopped": false }` if no replay was active. Never an error — "stop with nothing running" is benign.
    async fn stop_handler(
        State(replay_control): State<DogStatsDReplayControl>,
    ) -> Result<Json<ReplayStopResponseBody>, (StatusCode, String)> {
        let was_ongoing = replay_control
            .is_ongoing()
            .map_err(|e| (StatusCode::PRECONDITION_FAILED, e.to_string()))?;

        replay_control
            .stop_replay()
            .map_err(|e| (StatusCode::PRECONDITION_FAILED, e.to_string()))?;

        Ok(Json(ReplayStopResponseBody { stopped: was_ongoing }))
    }
}

/// Response body for `POST /dogstatsd/replay/stop`.
#[derive(Serialize)]
pub struct ReplayStopResponseBody {
    /// `true` if a replay was running when stop was called; `false` if no replay was active.
    pub stopped: bool,
}

impl APIHandler for DogStatsDReplayAPIHandler {
    type State = DogStatsDReplayControl;

    fn generate_initial_state(&self) -> Self::State {
        self.replay_control.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new()
            .route("/dogstatsd/replay/trigger", post(Self::trigger_handler))
            .route("/dogstatsd/replay/stop", post(Self::stop_handler))
    }
}
