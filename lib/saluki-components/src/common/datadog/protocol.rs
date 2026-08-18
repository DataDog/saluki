//! Protocol version types for Datadog payloads.

use std::collections::HashMap;

use agent_data_plane_config::shared::{
    MetricsEncoding as TypedMetricsEncoding, V3ApiEncoding as TypedV3ApiEncoding, V3ApiSettings as TypedV3ApiSettings,
    V3SeriesMode,
};
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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct V3ApiSettings {
    /// Endpoints that should receive V3 payloads for this metric type.
    ///
    /// Each entry should be a configured endpoint name, such as `https://app.datadoghq.com`.
    /// If empty, no V3 payloads are generated for this metric type.
    pub endpoints: Vec<String>,
}

impl V3ApiSettings {
    /// Returns true if V3 is enabled for any endpoint.
    pub fn is_enabled(&self) -> bool {
        !self.endpoints.is_empty()
    }
}

/// V3 API configuration for per-endpoint V3 support.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct V3ApiConfig {
    /// V3 settings for series metrics (counters, gauges, rates, sets).
    pub series: V3ApiSettings,

    /// V3 settings for sketch metrics (histograms, distributions).
    pub sketches: V3ApiSettings,

    /// Override compression level for V3 payloads.
    ///
    /// A value of `0` uses the normal serializer compression level.
    pub compression_level: i32,
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
}

impl From<&TypedV3ApiSettings> for V3ApiSettings {
    fn from(settings: &TypedV3ApiSettings) -> Self {
        Self {
            endpoints: settings.endpoints.clone(),
        }
    }
}

impl From<&TypedV3ApiEncoding> for V3ApiConfig {
    fn from(config: &TypedV3ApiEncoding) -> Self {
        Self {
            series: (&config.series).into(),
            sketches: (&config.sketches).into(),
            compression_level: config.compression_level,
        }
    }
}

/// The Datadog Agent's `use_v3_api` configuration section.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UseV3ApiConfig {
    /// Agent-compatible V3 API configuration for series metrics.
    pub series: UseV3ApiSeriesConfig,
}

/// Agent-compatible `use_v3_api.series` configuration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UseV3ApiSeriesConfig {
    /// Global V3 series mode.
    pub enabled: V3SeriesMode,

    /// Per-endpoint V3 series mode overrides.
    pub endpoints: HashMap<String, V3SeriesMode>,
}

impl From<&TypedMetricsEncoding> for UseV3ApiSeriesConfig {
    fn from(config: &TypedMetricsEncoding) -> Self {
        Self {
            enabled: config.v3_series_mode,
            endpoints: config.v3_series_endpoint_modes.clone(),
        }
    }
}
