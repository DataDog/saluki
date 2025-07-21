use serde::Deserialize;

/// Configuration for DogStatsD internal statistics API.
#[derive(Deserialize)]
pub struct DogStatsDInternalStatisticsConfiguration {
    /// Whether to enable the internal statistics API.
    ///
    /// When enabled, exposes an API endpoint that provides internal statistics about the DogStatsD source,
    /// including metrics received, errors, and performance data.
    ///
    /// Defaults to `false`.
    #[serde(rename = "dogstatsd_internal_statistics_enabled", default)]
    enabled: bool,
}

impl Default for DogStatsDInternalStatisticsConfiguration {
    fn default() -> Self {
        Self { enabled: false }
    }
}

impl DogStatsDInternalStatisticsConfiguration {
    /// Returns an API handler for DogStatsD internal statistics.
    pub fn api_handler() -> Option<()> {
        None
    }
}