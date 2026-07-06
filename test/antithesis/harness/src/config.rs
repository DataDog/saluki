//! Configuration model and rendering for Datadog Agent configuration.
//!
//! Primary focus is currently `DogStatsD` but this is, hopefully, easy to expand
//! in the future.
//!
//! One `datadog.yaml` is sampled per timeline. Only keys ADP and the Datadog
//! Agent both fully support are emitted, and their values vary per timeline, so a
//! divergence between the two targets is a finding rather than a config artifact.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use rand::distr::{Distribution, StandardUniform};
use rand::{Rng, RngExt};
use serde::{Deserialize, Serialize};

use crate::payload::dogstatsd::PAYLOAD_BYTE_LIMIT;
use crate::rand::Probe;

/// Agent log level.
///
/// Pinned to `error`: the quietest level that still logs and a value both
/// targets parse identically. Louder levels blow Antithesis's per-hour
/// log-output budget. `off` is intentionally absent — `serde_yaml` renders it as
/// the bare scalar `off`, which a YAML 1.1 reader decodes as the boolean
/// `false`, and the Datadog Agent then rejects the level and refuses to boot.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogLevel {
    /// Errors only — the quietest level that still logs.
    Error,
}

/// Tag granularity for origin-detected `DogStatsD` tags.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TagCardinality {
    /// Low-cardinality objects: clusters, hosts, deployments, images. Agent
    /// default.
    Low,
    /// Orchestrator-level: pod (Kubernetes) or task (ECS/Mesos) cardinality.
    Orchestrator,
    /// High-cardinality objects: individual containers, request user IDs, etc.
    High,
}

impl Distribution<TagCardinality> for StandardUniform {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> TagCardinality {
        match rng.random_range(0..3u8) {
            0 => TagCardinality::Low,
            1 => TagCardinality::Orchestrator,
            _ => TagCardinality::High,
        }
    }
}

/// The Agent's `DogStatsD` configuration surface. `dogstatsd_socket` is supplied
/// by the environment, the rest are sampled.
///
/// Numeric fields are sampled per field, the wide ones with [`Probe`].
#[allow(clippy::struct_field_names, clippy::struct_excessive_bools)]
#[derive(Debug, Serialize)]
pub(crate) struct DogStatsdConfig {
    /// Unix socket the server listens on. Supplied by the environment.
    dogstatsd_socket: PathBuf,
    /// Buffer used to receive statsd packets, in bytes.
    dogstatsd_buffer_size: u64,
    /// Bytes for the socket receive buffer (`POSIX`); `0` keeps the OS default.
    dogstatsd_so_rcvbuf: u64,
    /// Maximum entries in the string interner cache.
    dogstatsd_string_interner_size: u64,
    /// Tag granularity for origin-detected tags.
    dogstatsd_tag_cardinality: TagCardinality,
    /// Listen for non-local UDP traffic (binds `0.0.0.0`).
    dogstatsd_non_local_traffic: bool,
    /// Tag metrics with container metadata from the Unix socket peer.
    dogstatsd_origin_detection: bool,
    /// Use a client-provided container ID to enrich metrics.
    dogstatsd_origin_detection_client: bool,
    /// Let clients opt out of origin detection via cardinality `none`.
    dogstatsd_origin_optout_enabled: bool,
    /// When an `Entity-ID` is set, skip origin-detection tag enrichment.
    dogstatsd_entity_id_precedence: bool,
    /// Enable the no-aggregation pipeline (forward timestamped metrics with
    /// tagging only).
    dogstatsd_no_aggregation_pipeline: bool,
    /// Flush incomplete metric time buckets on shutdown.
    dogstatsd_flush_incomplete_buckets: bool,
}

impl DogStatsdConfig {
    /// Sample the `DogStatsD` options from `rng`, taking the socket from the
    /// environment.
    fn sample<R: Rng + ?Sized>(rng: &mut R, dogstatsd_socket: &Path) -> Self {
        Self {
            dogstatsd_socket: dogstatsd_socket.to_path_buf(),
            dogstatsd_buffer_size: rng.random_range(128..=65_536),
            dogstatsd_so_rcvbuf: Probe::new(0, 25_165_824).sample(rng),
            dogstatsd_string_interner_size: Probe::new(1, MAX_STRING_INTERNER_ENTRIES).sample(rng),
            dogstatsd_tag_cardinality: rng.random(),
            dogstatsd_non_local_traffic: rng.random(),
            dogstatsd_origin_detection: rng.random(),
            dogstatsd_origin_detection_client: rng.random(),
            dogstatsd_origin_optout_enabled: rng.random(),
            dogstatsd_entity_id_precedence: rng.random(),
            dogstatsd_no_aggregation_pipeline: rng.random(),
            dogstatsd_flush_incomplete_buckets: rng.random(),
        }
    }
}

/// Entry-count ceiling for `dogstatsd_string_interner_size`.
///
/// ADP and the Core Agent both preallocate the interner at boot, multiplying
/// the entry count by 512 bytes when `dogstatsd_string_interner_size_bytes` is
/// unset. The current value caps the preallocation at 512 MiB.
const MAX_STRING_INTERNER_ENTRIES: u64 = 1_048_576;

/// Agent-facing config. `hostname`, `api_key`, `dd_url`, and the socket are
/// supplied by the environment; `log_level` and the `DogStatsD` options are
/// sampled per branch. The static flags are appended by [`Self::to_yaml`], not
/// fields here.
#[derive(Debug, Serialize)]
pub struct DatadogConfig {
    /// Agent hostname. Supplied by the environment. ADP requires it
    /// (`FixedHostProvider`); absent it refuses to boot.
    hostname: String,
    /// Agent API key. Supplied by the environment.
    api_key: String,
    /// Metrics intake base URL. Supplied by the environment.
    dd_url: String,
    /// Agent log verbosity. Pinned to `error` (see [`LogLevel`]).
    log_level: LogLevel,
    /// `DogStatsD` options, flattened to top-level `dogstatsd_*` keys.
    #[serde(flatten)]
    dogstatsd: DogStatsdConfig,
}

impl DatadogConfig {
    /// Generate a config: the environmental fields come from the caller, the rest
    /// are sampled from `rng`. With an Antithesis-backed rng, each call after the
    /// snapshot yields an independent draw per replay branch.
    #[must_use]
    pub fn sample<R: Rng + ?Sized>(
        rng: &mut R, hostname: &str, api_key: &str, dd_url: &str, dogstatsd_socket: &Path,
    ) -> Self {
        Self {
            hostname: hostname.to_owned(),
            api_key: api_key.to_owned(),
            dd_url: dd_url.to_owned(),
            log_level: LogLevel::Error,
            dogstatsd: DogStatsdConfig::sample(rng, dogstatsd_socket),
        }
    }

    /// Render `self` as a `datadog.yaml` string, followed by the static-tail
    /// flags.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_yaml(&self) -> anyhow::Result<String> {
        let mut yaml = serde_yaml::to_string(self).context("serialize datadog.yaml")?;
        yaml.push_str(STATIC_YAML_TAIL);
        Ok(yaml)
    }

    /// Derive the [`DriverConfig`] a load generator reads to match this timeline's
    /// SUT, sampling its knobs from `rng` so they land with the SUT config and the
    /// two cannot disagree.
    #[must_use]
    pub fn driver_config<R: Rng + ?Sized>(&self, rng: &mut R) -> DriverConfig {
        DriverConfig::sample(rng, self.dogstatsd.dogstatsd_buffer_size)
    }
}

/// Yaml flags the Agent reads at boot that never vary.
const STATIC_YAML_TAIL: &str = "use_dogstatsd: true
use_v2_api_series: true
inventories_enabled: false
enable_metadata_collection: false
cloud_provider_metadata: []
";

/// Upper bound on datagrams one driver invocation ships in a timeline.
const MAX_DATAGRAMS: usize = 10_000;

/// Config a load generator reads to shape its output to this timeline's SUT.
/// `first_sample_config` samples it beside `datadog.yaml` from one draw, so the
/// generator and the SUT are driven together.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DriverConfig {
    /// Max bytes a generator packs into one datagram, the smaller of the SUT's
    /// sampled receive buffer and [`PAYLOAD_BYTE_LIMIT`]. A datagram this size
    /// fits one read, so the SUT never truncates a line mid-token.
    pub payload_byte_limit: usize,
    /// Datagrams a driver invocation ships this timeline.
    pub datagram_count: usize,
}

impl DriverConfig {
    /// Sample the driver knobs for a SUT whose receive buffer is `buffer_size`.
    fn sample<R: Rng + ?Sized>(rng: &mut R, buffer_size: u64) -> Self {
        // The min is at most PAYLOAD_BYTE_LIMIT, so a buffer wider than usize
        // caps to the ceiling like any other oversized buffer.
        let payload_byte_limit = match usize::try_from(buffer_size.min(PAYLOAD_BYTE_LIMIT as u64)) {
            Ok(bytes) => bytes,
            Err(_) => PAYLOAD_BYTE_LIMIT,
        };
        Self {
            payload_byte_limit,
            datagram_count: rng.random_range(0..=MAX_DATAGRAMS),
        }
    }

    /// Render `self` as a `driver.yaml` string.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_yaml(&self) -> anyhow::Result<String> {
        serde_yaml::to_string(self).context("serialize driver.yaml")
    }

    /// Read the driver config from the `driver.yaml` that `first_sample_config`
    /// wrote to `config_dir`.
    ///
    /// # Errors
    ///
    /// Returns an error if the config is unreadable or is not valid YAML with an
    /// integer `payload_byte_limit`.
    pub fn read(config_dir: &Path) -> anyhow::Result<Self> {
        let path = config_dir.join("driver.yaml");
        let yaml = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        serde_yaml::from_str(&yaml).with_context(|| format!("parse driver config from {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use rand::rand_core::TryRng;

    use super::*;

    /// A trivial deterministic `SplitMix64` generator. Implementing `TryRng` with
    /// an infallible error gives a blanket [`rand::Rng`].
    #[derive(Debug)]
    struct SeqRng(u64);

    impl TryRng for SeqRng {
        type Error = Infallible;

        fn try_next_u32(&mut self) -> Result<u32, Infallible> {
            let bytes = self.try_next_u64()?.to_le_bytes();
            Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }

        fn try_next_u64(&mut self) -> Result<u64, Infallible> {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            Ok(z ^ (z >> 31))
        }

        fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
            for chunk in dst.chunks_mut(8) {
                let bytes = self.try_next_u64()?.to_le_bytes();
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
            Ok(())
        }
    }

    fn render(seed: u64) -> String {
        let mut rng = SeqRng(seed);
        DatadogConfig::sample(&mut rng, "h", "k", "http://intake:2049", Path::new("/s.sock"))
            .to_yaml()
            .expect("render yaml")
    }

    /// A rendered top-level key, matched at the start of a line to avoid a
    /// prefix collision between related keys.
    fn has_key(yaml: &str, key: &str) -> bool {
        yaml.lines().any(|line| line.starts_with(&format!("{key}:")))
    }

    #[test]
    fn driver_config_caps_payload_to_the_smaller_bound() {
        assert_eq!(DriverConfig::sample(&mut SeqRng(0), 512).payload_byte_limit, 512);
        assert_eq!(
            DriverConfig::sample(&mut SeqRng(0), 1 << 30).payload_byte_limit,
            PAYLOAD_BYTE_LIMIT
        );
        assert_eq!(DriverConfig::sample(&mut SeqRng(0), 0).payload_byte_limit, 0);
    }

    #[test]
    fn log_level_is_always_an_unambiguous_scalar() {
        assert!(has_key(&render(0), "log_level"));
        assert!(render(0).contains("log_level: error"));
    }
}
