//! The sampled `datadog.yaml` both targets booted under, as Pyld60 reads it.

use std::fs;
use std::path::Path;

use serde::Deserialize;
use tracing::warn;

/// The sampled config fields Pyld60 reads.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct SutConfig {
    /// The switch between the v2 and v3 series intake.
    use_v3_api: UseV3Api,
    /// Carried for assertion details, since zlib is the likely cause of a v3 timeline shipping v2.
    serializer_compressor_kind: String,
}

/// The `use_v3_api` sub-tree.
#[derive(Debug, Deserialize, PartialEq, Eq)]
struct UseV3Api {
    /// The `series` sub-tree.
    series: V3Series,
}

/// The `enabled` leaf under `use_v3_api.series`.
#[derive(Debug, Deserialize, PartialEq, Eq)]
struct V3Series {
    /// The mode the config asked for. A string because that is the shape both targets read it as.
    enabled: String,
}

impl SutConfig {
    /// Read the sampled config from `dir`.
    ///
    /// Returns `None` before `first_sample_config` writes the file, and on a parse failure, logged.
    pub(crate) fn load(dir: &Path) -> Option<Self> {
        let path = dir.join("datadog.yaml");
        let yaml = fs::read_to_string(&path).ok()?;
        match serde_yaml::from_str(&yaml) {
            Ok(config) => Some(config),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Cannot read the sampled config, skipping Pyld60.");
                None
            }
        }
    }

    /// The series API a target must use. The compressor does not enter it: both targets silently
    /// downgrade a v3 timeline to v2 under zlib, and that downgrade is the fault Pyld60 exists to catch.
    ///
    /// The Agent accepts `true`, `false`, and `datadog_only`. Only `true` means v3 here, since the
    /// rig's `dd_url` is not a Datadog URL and `datadog_only` resolves against it to v2.
    pub(crate) fn expected_series_v3(&self) -> bool {
        self.use_v3_api.series.enabled == "true"
    }

    /// The configured compressor, for assertion details.
    pub(crate) fn compressor(&self) -> &str {
        &self.serializer_compressor_kind
    }
}

#[cfg(test)]
mod tests {
    use super::SutConfig;

    fn config(series_v3: bool, compressor: &str) -> SutConfig {
        let yaml = format!(
            "hostname: h\nuse_v3_api:\n  series:\n    enabled: \"{series_v3}\"\nserializer_compressor_kind: {compressor}\ndogstatsd_buffer_size: 8192\n"
        );
        serde_yaml::from_str(&yaml).expect("parse config")
    }

    #[test]
    fn only_the_series_switch_sets_the_expected_api() {
        assert!(config(true, "zlib").expected_series_v3());
        assert!(config(true, "zstd").expected_series_v3());
        assert!(config(true, "gzip").expected_series_v3());
        assert!(config(true, "none").expected_series_v3());
        assert!(config(true, "snappy").expected_series_v3());
        assert!(!config(false, "zstd").expected_series_v3());
        // datadog_only resolves against the rig's non-Datadog dd_url, so it means v2.
        let datadog_only: SutConfig = serde_yaml::from_str(
            "use_v3_api:\n  series:\n    enabled: \"datadog_only\"\nserializer_compressor_kind: zstd\n",
        )
        .expect("parse");
        assert!(!datadog_only.expected_series_v3());
    }
}
