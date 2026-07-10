//! Protocol version types for Datadog payloads.

use std::collections::HashMap;

use facet::Facet;
use serde::{Deserialize, Serialize};

use super::METRICS_SERIES_V3_BETA_PATH;

fn default_v3_beta_series_route() -> String {
    METRICS_SERIES_V3_BETA_PATH.to_owned()
}

const fn default_v3_series_shadow_sample_rate() -> f64 {
    0.0
}

fn default_v3_series_shadow_sites() -> Vec<String> {
    vec!["datadoghq.com".to_string()]
}

fn default_use_v3_api_series_enabled() -> String {
    "true".to_string()
}

#[derive(Deserialize)]
#[serde(untagged)]
enum V3SeriesModeValue {
    String(String),
    Bool(bool),
}

impl V3SeriesModeValue {
    fn into_string(self) -> String {
        match self {
            Self::String(value) => value,
            Self::Bool(value) => value.to_string(),
        }
    }
}

/// Deserializes an Agent V3 series mode value.
///
/// The Datadog Agent accepts string values such as `true`, `false`, and `datadog_only`, while YAML values may also be
/// written as booleans. Normalize those supported forms into the string form used by the evaluator.
pub fn deserialize_v3_series_mode<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    V3SeriesModeValue::deserialize(deserializer).map(V3SeriesModeValue::into_string)
}

/// Deserializes per-endpoint Agent V3 series mode overrides.
pub fn deserialize_v3_series_endpoint_modes<'de, D>(deserializer: D) -> Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum EndpointModes {
        JsonString(String),
        Map(HashMap<String, V3SeriesModeValue>),
    }

    let values = match EndpointModes::deserialize(deserializer)? {
        EndpointModes::JsonString(raw) => {
            serde_json::from_str::<HashMap<String, V3SeriesModeValue>>(&raw).map_err(serde::de::Error::custom)?
        }
        EndpointModes::Map(values) => values,
    };

    Ok(values
        .into_iter()
        .map(|(endpoint, value)| (endpoint, value.into_string()))
        .collect())
}

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

    /// Whether this payload is part of sampled V3 shadow validation.
    #[serde(default)]
    pub shadow: bool,
}

impl MetricsPayloadInfo {
    /// Creates a new V2 series payload info.
    pub const fn v2_series() -> Self {
        Self {
            version: MetricsProtocolVersion::V2,
            payload_type: MetricsPayloadType::Series,
            shadow: false,
        }
    }

    /// Creates a new V2 sketches payload info.
    pub const fn v2_sketches() -> Self {
        Self {
            version: MetricsProtocolVersion::V2,
            payload_type: MetricsPayloadType::Sketches,
            shadow: false,
        }
    }

    /// Creates a new V3 series payload info.
    pub const fn v3_series() -> Self {
        Self {
            version: MetricsProtocolVersion::V3,
            payload_type: MetricsPayloadType::Series,
            shadow: false,
        }
    }

    /// Creates a new V3 sketches payload info.
    pub const fn v3_sketches() -> Self {
        Self {
            version: MetricsProtocolVersion::V3,
            payload_type: MetricsPayloadType::Sketches,
            shadow: false,
        }
    }

    /// Creates a new V2 series payload info for sampled shadow validation.
    pub const fn v2_shadow_series() -> Self {
        Self {
            version: MetricsProtocolVersion::V2,
            payload_type: MetricsPayloadType::Series,
            shadow: true,
        }
    }

    /// Creates a new V3 series payload info for sampled shadow validation.
    pub const fn v3_shadow_series() -> Self {
        Self {
            version: MetricsProtocolVersion::V3,
            payload_type: MetricsPayloadType::Series,
            shadow: true,
        }
    }

    /// Returns true if this is a sketch payload.
    pub const fn is_sketch(&self) -> bool {
        matches!(self.payload_type, MetricsPayloadType::Sketches)
    }

    /// Returns true if this payload is part of sampled shadow validation.
    pub const fn is_shadow(&self) -> bool {
        self.shadow
    }
}

/// V3 API settings for a specific metric type (series or sketches).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Facet)]
pub struct V3ApiSettings {
    /// Endpoints that should receive V3 payloads for this metric type.
    ///
    /// Each entry should be a configured endpoint name, such as `https://app.datadoghq.com`.
    /// If empty, no V3 payloads are generated for this metric type.
    #[serde(default)]
    pub endpoints: Vec<String>,

    /// Whether to also send V2 payloads to V3-enabled endpoints (validation mode).
    ///
    /// When true, endpoints in the `endpoints` list receive both V2 and V3 payloads.
    /// When false, endpoints in the `endpoints` list receive only V3 payloads.
    #[serde(default)]
    pub validate: bool,

    /// Whether to use the beta V3 route for this metric type.
    ///
    /// This only applies to series metrics. Sketches always use the standard V3 sketches route.
    #[serde(default)]
    pub use_beta: bool,

    /// Beta V3 route to use when `use_beta` is enabled for series metrics.
    ///
    /// Defaults to `/api/intake/metrics/v3beta/series`.
    #[serde(default = "default_v3_beta_series_route")]
    pub beta_route: String,

    /// Per-flush probability of sending a sampled V3 beta shadow payload.
    ///
    /// This only applies to series metrics when V3 is not authoritative.
    ///
    /// Defaults to `0`.
    #[serde(default = "default_v3_series_shadow_sample_rate")]
    pub shadow_sample_rate: f64,

    /// Datadog sites eligible for sampled V3 beta shadow payloads.
    ///
    /// This only applies to series metrics when V3 is not authoritative.
    ///
    /// Defaults to `["datadoghq.com"]`.
    #[serde(default = "default_v3_series_shadow_sites")]
    pub shadow_sites: Vec<String>,
}

impl Default for V3ApiSettings {
    fn default() -> Self {
        Self {
            endpoints: Vec::new(),
            validate: false,
            use_beta: false,
            beta_route: default_v3_beta_series_route(),
            shadow_sample_rate: default_v3_series_shadow_sample_rate(),
            shadow_sites: default_v3_series_shadow_sites(),
        }
    }
}

impl V3ApiSettings {
    /// Returns true if V3 is enabled for any endpoint.
    pub fn is_enabled(&self) -> bool {
        !self.endpoints.is_empty()
    }
}

/// V3 API configuration for per-endpoint V3 support.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, Facet)]
pub struct V3ApiConfig {
    /// V3 settings for series metrics (counters, gauges, rates, sets).
    #[serde(default)]
    pub series: V3ApiSettings,

    /// V3 settings for sketch metrics (histograms, distributions).
    #[serde(default)]
    pub sketches: V3ApiSettings,

    /// Override compression level for V3 payloads.
    ///
    /// Defaults to `0`, which uses the normal serializer compression level.
    #[serde(default)]
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

/// Agent-compatible `use_v3_api.series` configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Facet)]
pub struct UseV3ApiSeriesConfig {
    /// Global V3 series mode.
    ///
    /// Valid Agent values are truthy strings, falsy strings, and `datadog_only`. Invalid values are treated as false by
    /// the evaluator.
    #[serde(
        default = "default_use_v3_api_series_enabled",
        deserialize_with = "deserialize_v3_series_mode",
        rename = "use_v3_api_series_enabled"
    )]
    pub enabled: String,

    /// Per-endpoint V3 series mode overrides.
    #[serde(
        default,
        deserialize_with = "deserialize_v3_series_endpoint_modes",
        rename = "use_v3_api_series_endpoints"
    )]
    pub endpoints: HashMap<String, String>,
}

impl Default for UseV3ApiSeriesConfig {
    fn default() -> Self {
        Self {
            enabled: default_use_v3_api_series_enabled(),
            endpoints: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::deserialize_v3_series_mode;

    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(deserialize_with = "deserialize_v3_series_mode")]
        mode: String,
    }

    fn parse_mode(value: serde_json::Value) -> String {
        serde_json::from_value::<Wrapper>(serde_json::json!({ "mode": value }))
            .expect("mode should deserialize")
            .mode
    }

    #[test]
    fn deserialize_v3_series_mode_normalizes_accepted_forms() {
        // Documented accepted forms: the string values `true`, `false`, and `datadog_only` pass through
        // unchanged, and YAML/JSON booleans are normalized to their string form for the evaluator.
        let cases = [
            (serde_json::json!("true"), "true"),
            (serde_json::json!("false"), "false"),
            (serde_json::json!("datadog_only"), "datadog_only"),
            (serde_json::json!(true), "true"),
            (serde_json::json!(false), "false"),
        ];

        for (input, expected) in cases {
            assert_eq!(parse_mode(input.clone()), expected, "{input}");
        }
    }
}
