//! HTTP API handler for the DogStatsD capture control surface.
//!
//! Exposes `POST /dogstatsd/capture/trigger` on the privileged API to start a capture session.

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

#[cfg(test)]
mod tests {
    use super::super::test_support::{unique_dir, wait_until_inactive};
    use super::*;
    use crate::sources::dogstatsd::replay::TrafficCapture;

    fn trigger_body(duration: &str) -> CaptureTriggerBody {
        CaptureTriggerBody {
            duration: duration.to_string(),
            path: None,
            compressed: false,
        }
    }

    #[tokio::test]
    async fn trigger_rejects_unbound_control() {
        // With no capture runtime bound to the control, the handler surfaces the "capture control unavailable" error
        // as a precondition failure.
        let control = DogStatsDCaptureControl::new();

        let err = DogStatsDCaptureAPIHandler::trigger_handler(State(control), Json(trigger_body("50ms")))
            .await
            .err()
            .expect("unbound control should fail");

        assert_eq!(err.0, StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn trigger_rejects_invalid_duration() {
        // A malformed duration is rejected up front (BAD_REQUEST), before the capture runtime is consulted.
        let control = DogStatsDCaptureControl::new();
        control.bind(TrafficCapture::new(unique_dir("capture-api-bad-duration"), 1));

        let err = DogStatsDCaptureAPIHandler::trigger_handler(State(control), Json(trigger_body("not-a-duration")))
            .await
            .err()
            .expect("invalid duration should fail");

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn trigger_starts_capture_and_returns_path() {
        let target_dir = unique_dir("capture-api-start");
        let control = DogStatsDCaptureControl::new();
        control.bind(TrafficCapture::new(target_dir.clone(), 1));

        let Json(response) =
            DogStatsDCaptureAPIHandler::trigger_handler(State(control.clone()), Json(trigger_body("100ms")))
                .await
                .expect("capture should start");
        assert!(
            response
                .path
                .starts_with(target_dir.to_str().expect("path should be utf-8")),
            "returned path {:?} should be inside the configured directory {:?}",
            response.path,
            target_dir
        );
        assert!(response.path.contains("datadog-capture"));
        assert!(control.is_ongoing().expect("control should be bound"));

        wait_until_inactive(|| control.is_ongoing().expect("control should be bound"));
        let _ = std::fs::remove_dir_all(target_dir);
    }

    #[tokio::test]
    async fn trigger_rejects_concurrent_capture() {
        let target_dir = unique_dir("capture-api-concurrent");
        let control = DogStatsDCaptureControl::new();
        control.bind(TrafficCapture::new(target_dir.clone(), 1));

        let _ = DogStatsDCaptureAPIHandler::trigger_handler(State(control.clone()), Json(trigger_body("1s")))
            .await
            .expect("first capture should start");

        let err = DogStatsDCaptureAPIHandler::trigger_handler(State(control.clone()), Json(trigger_body("1s")))
            .await
            .err()
            .expect("second concurrent capture should fail");
        assert_eq!(err.0, StatusCode::PRECONDITION_FAILED);
        assert!(
            err.1.contains("capture already in progress"),
            "unexpected error: {}",
            err.1
        );

        control.stop_capture().expect("stop should succeed");
        wait_until_inactive(|| control.is_ongoing().expect("control should be bound"));
        let _ = std::fs::remove_dir_all(target_dir);
    }
}
