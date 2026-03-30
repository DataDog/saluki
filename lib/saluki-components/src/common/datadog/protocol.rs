//! Protocol version types for Datadog payloads.

use serde::{Deserialize, Serialize};

/// The type of metrics payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricsPayloadType {
    /// Series metrics (counters, gauges, rates, sets).
    Series,

    /// Sketch metrics (histograms, distributions).
    Sketches,
}

/// Protocol version for metrics payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricsProtocolVersion {
    /// V2 protocol (legacy format).
    V2,

    /// V3 protocol (columnar format).
    V3,
}

/// Combined payload info for metrics, encoding both protocol version and metric type.
///
/// This is stored in `PayloadMetadata` and used by the I/O layer to filter payloads
/// based on endpoint V3 settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsPayloadInfo {
    /// The protocol version (V2 or V3).
    pub version: MetricsProtocolVersion,

    /// The type of metrics (series or sketches).
    pub payload_type: MetricsPayloadType,
}

impl MetricsPayloadInfo {
    /// Creates a new V2 series payload info.
    pub const fn v2_series() -> Self {
        Self {
            version: MetricsProtocolVersion::V2,
            payload_type: MetricsPayloadType::Series,
        }
    }

    /// Creates a new V2 sketches payload info.
    pub const fn v2_sketches() -> Self {
        Self {
            version: MetricsProtocolVersion::V2,
            payload_type: MetricsPayloadType::Sketches,
        }
    }

    /// Creates a new V3 series payload info.
    pub const fn v3_series() -> Self {
        Self {
            version: MetricsProtocolVersion::V3,
            payload_type: MetricsPayloadType::Series,
        }
    }

    /// Creates a new V3 sketches payload info.
    pub const fn v3_sketches() -> Self {
        Self {
            version: MetricsProtocolVersion::V3,
            payload_type: MetricsPayloadType::Sketches,
        }
    }

    /// Returns true if this is a sketch payload.
    pub const fn is_sketch(&self) -> bool {
        matches!(self.payload_type, MetricsPayloadType::Sketches)
    }
}

/// V3 API settings for a specific metric type (series or sketches).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct V3ApiSettings {
    /// Endpoints that should receive V3 payloads for this metric type.
    ///
    /// Each entry should be a URL or URL pattern that matches endpoint URLs.
    /// If empty, no V3 payloads are generated for this metric type.
    #[serde(default)]
    pub endpoints: Vec<String>,

    /// Whether to also send V2 payloads to V3-enabled endpoints (validation mode).
    ///
    /// When true, endpoints in the `endpoints` list receive both V2 and V3 payloads.
    /// When false, endpoints in the `endpoints` list receive only V3 payloads.
    #[serde(default)]
    pub validate: bool,
}

impl V3ApiSettings {
    /// Returns true if V3 is enabled for any endpoint.
    pub fn is_enabled(&self) -> bool {
        !self.endpoints.is_empty()
    }
}

/// V3 API configuration for per-endpoint V3 support.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct V3ApiConfig {
    /// V3 settings for series metrics (counters, gauges, rates, sets).
    #[serde(default)]
    pub series: V3ApiSettings,

    /// V3 settings for sketch metrics (histograms, distributions).
    #[serde(default)]
    pub sketches: V3ApiSettings,
}

impl V3ApiConfig {
    /// Returns true if V3 is enabled for series metrics.
    pub fn use_v3_series(&self) -> bool {
        self.series.is_enabled()
    }

    /// Returns true if V3 is enabled for sketch metrics.
    pub fn use_v3_sketches(&self) -> bool {
        self.sketches.is_enabled()
    }

    /// Returns true if validation mode is enabled for series metrics.
    pub fn use_v3_series_validate(&self) -> bool {
        self.series.is_enabled() && self.series.validate
    }

    /// Returns true if validation mode is enabled for sketch metrics.
    pub fn use_v3_sketches_validate(&self) -> bool {
        self.sketches.is_enabled() && self.sketches.validate
    }

    /// Returns true if any V3 encoding is enabled.
    pub fn any_v3_enabled(&self) -> bool {
        self.use_v3_series() || self.use_v3_sketches()
    }
}
