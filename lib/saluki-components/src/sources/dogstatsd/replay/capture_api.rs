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

/// Largest duration, in nanoseconds, representable as a non-negative `i64`.
const MAX_NANOS_U64: u64 = i64::MAX as u64;

/// Error returned when a capture duration value can't be parsed.
#[derive(Debug)]
enum ParseDurationError {
    /// The value was syntactically invalid.
    Invalid {
        /// The original input string.
        input: String,
        /// Reason the input was rejected.
        reason: String,
    },
    /// The value parsed to a negative duration.
    Negative,
    /// The value exceeds the range of [`std::time::Duration`] as nanoseconds.
    Overflow,
}

impl std::fmt::Display for ParseDurationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseDurationError::Invalid { input, reason } => {
                write!(f, "invalid duration '{}': {}", input, reason)
            }
            ParseDurationError::Negative => write!(f, "negative durations are not supported"),
            ParseDurationError::Overflow => write!(f, "duration value exceeds supported range"),
        }
    }
}

fn invalid(input: &str, reason: impl Into<String>) -> ParseDurationError {
    ParseDurationError::Invalid {
        input: input.to_string(),
        reason: reason.into(),
    }
}

/// Parses a string in the exact format accepted by Go's `time.ParseDuration`, restricted to non-negative values
/// (since [`std::time::Duration`] can't represent negatives).
///
/// This is a runtime parse of the HTTP request body's duration string (for example, `"10s"`, `"500ms"`); it is not
/// configuration parsing.
fn parse_duration(s: &str) -> Result<Duration, ParseDurationError> {
    let orig = s;
    let mut rest = s;
    let mut total_ns: u128 = 0;
    let mut negative = false;

    if let Some(c) = rest.chars().next() {
        if c == '+' || c == '-' {
            negative = c == '-';
            rest = &rest[1..];
        }
    }

    // Special case: "0" alone (possibly after a sign) is zero.
    if rest == "0" {
        return Ok(Duration::ZERO);
    }
    if rest.is_empty() {
        return Err(invalid(orig, "empty duration"));
    }

    while !rest.is_empty() {
        let (int_part, after_int) = consume_digits(rest);
        let had_int = !int_part.is_empty();

        let (frac_part, after_frac) = if let Some(stripped) = after_int.strip_prefix('.') {
            consume_digits(stripped)
        } else {
            ("", after_int)
        };
        let consumed_dot = after_int.starts_with('.');
        let had_frac = consumed_dot && !frac_part.is_empty();

        if !had_int && !had_frac {
            return Err(invalid(orig, "expected digits"));
        }

        rest = after_frac;

        let unit_str = consume_unit(rest);
        if unit_str.is_empty() {
            return Err(invalid(orig, "missing unit"));
        }
        rest = &rest[unit_str.len()..];

        let unit_ns: u128 = match unit_str {
            "ns" => 1,
            "us" | "µs" | "μs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60 * 1_000_000_000,
            "h" => 3_600 * 1_000_000_000,
            other => return Err(invalid(orig, format!("unknown unit '{}'", other))),
        };

        let int_val: u128 = if int_part.is_empty() {
            0
        } else {
            int_part
                .parse::<u128>()
                .map_err(|_| invalid(orig, "integer overflow"))?
        };

        let mut ns = int_val.checked_mul(unit_ns).ok_or_else(|| invalid(orig, "overflow"))?;

        if !frac_part.is_empty() {
            // Truncate the fraction to at most 18 digits to keep the intermediate u128 math well within range. 18
            // decimal digits of precision is well beyond nanoseconds for every supported unit.
            let keep = frac_part.len().min(18);
            let frac_digits = &frac_part[..keep];
            let mut scale: u128 = 1;
            for _ in 0..keep {
                scale *= 10;
            }
            let f: u128 = frac_digits
                .parse::<u128>()
                .map_err(|_| invalid(orig, "invalid fractional"))?;
            let frac_ns = f.checked_mul(unit_ns).ok_or_else(|| invalid(orig, "overflow"))? / scale;
            ns = ns.checked_add(frac_ns).ok_or_else(|| invalid(orig, "overflow"))?;
        }

        total_ns = total_ns.checked_add(ns).ok_or_else(|| invalid(orig, "overflow"))?;
    }

    if negative && total_ns != 0 {
        return Err(ParseDurationError::Negative);
    }

    if total_ns > MAX_NANOS_U64 as u128 {
        return Err(ParseDurationError::Overflow);
    }
    Ok(Duration::from_nanos(total_ns as u64))
}

fn consume_digits(s: &str) -> (&str, &str) {
    let end = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    s.split_at(end)
}

fn consume_unit(s: &str) -> &str {
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if c.is_ascii_alphabetic() || c == 'µ' || c == 'μ' {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    &s[..end]
}
