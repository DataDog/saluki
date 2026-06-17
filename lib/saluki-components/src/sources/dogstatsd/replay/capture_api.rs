//! HTTP API handler for the DogStatsD capture control surface.
//!
//! Exposes `POST /dogstatsd/capture/trigger` on the privileged API to start a capture session.

use std::{path::Path, time::Duration};

use saluki_api::{
    extract::State,
    routing::{post, Router},
    APIHandler, Json, StatusCode,
};
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
        let duration = parse_capture_duration(&body.duration).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        let requested_dir = body.path.as_deref().map(Path::new);

        let capture_path = capture_control
            .start_capture(requested_dir, duration, body.compressed)
            .map_err(|e| (StatusCode::PRECONDITION_FAILED, e.to_string()))?;

        Ok(Json(CaptureTriggerResponseBody {
            path: capture_path.display().to_string(),
        }))
    }
}

fn parse_capture_duration(input: &str) -> Result<Duration, String> {
    let original = input;
    let mut rest = input.trim();
    let mut total_ns = 0u128;

    if let Some(stripped) = rest.strip_prefix('+') {
        rest = stripped;
    } else if rest.starts_with('-') {
        return Err("duration cannot be negative".to_string());
    }

    if rest == "0" {
        return Ok(Duration::ZERO);
    }
    if rest.is_empty() {
        return Err("duration is empty".to_string());
    }

    while !rest.is_empty() {
        let int_len = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let int_part = &rest[..int_len];
        rest = &rest[int_len..];

        let frac_part = if let Some(after_dot) = rest.strip_prefix('.') {
            let frac_len = after_dot.find(|c: char| !c.is_ascii_digit()).unwrap_or(after_dot.len());
            let frac_part = &after_dot[..frac_len];
            if frac_part.is_empty() {
                return Err(format!(
                    "invalid duration '{original}': expected digits after decimal point"
                ));
            }
            rest = &after_dot[frac_len..];
            frac_part
        } else {
            ""
        };

        if int_part.is_empty() && frac_part.is_empty() {
            return Err(format!("invalid duration '{original}': expected digits"));
        }

        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let unit = &rest[..unit_len];
        if unit.is_empty() {
            return Err(format!("invalid duration '{original}': missing unit"));
        }
        rest = &rest[unit_len..];

        let unit_ns = match unit {
            "ns" => 1u128,
            "us" | "µs" | "μs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60 * 1_000_000_000,
            "h" => 3_600 * 1_000_000_000,
            other => return Err(format!("invalid duration '{original}': unknown unit '{other}'")),
        };

        let whole = if int_part.is_empty() {
            0
        } else {
            int_part
                .parse::<u128>()
                .map_err(|_| format!("invalid duration '{original}': integer overflow"))?
        };
        total_ns = total_ns
            .checked_add(
                whole
                    .checked_mul(unit_ns)
                    .ok_or_else(|| format!("invalid duration '{original}': duration overflow"))?,
            )
            .ok_or_else(|| format!("invalid duration '{original}': duration overflow"))?;

        if !frac_part.is_empty() {
            let frac = frac_part
                .parse::<u128>()
                .map_err(|_| format!("invalid duration '{original}': fractional overflow"))?;
            let scale = 10u128
                .checked_pow(frac_part.len() as u32)
                .ok_or_else(|| format!("invalid duration '{original}': fractional precision overflow"))?;
            total_ns = total_ns
                .checked_add(frac.saturating_mul(unit_ns) / scale)
                .ok_or_else(|| format!("invalid duration '{original}': duration overflow"))?;
        }
    }

    let nanos = u64::try_from(total_ns).map_err(|_| format!("invalid duration '{original}': duration overflow"))?;
    Ok(Duration::from_nanos(nanos))
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
