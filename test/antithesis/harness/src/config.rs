//! Configuration model and rendering for Datadog Agent configuration.
//!
//! Primary focus is currently `DogStatsD` but this is, hopefully, easy to expand
//! in the future.
//!
//! Two variations are sampled, selected by [`ConfigProfile`]. `General` samples
//! the full feral surface, including keys the Datadog Agent rejects or
//! interprets differently from ADP, which is correct when ADP runs alone.
//! `Differential` samples only keys both ADP and the Datadog Agent honor
//! identically, over value ranges both accept, so the A/B scenario compares two
//! targets that were actually asked to behave the same way.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use clap::ValueEnum;
use rand::distr::{Distribution, StandardUniform};
use rand::{Rng, RngExt};
use serde::{Serialize, Serializer};

use crate::rand::Probe;

/// Which `datadog.yaml` variation to sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ConfigProfile {
    /// ADP-only scenario. Sample the full feral surface, including keys the
    /// Datadog Agent rejects or interprets differently. Correct when ADP is the
    /// only target under test.
    General,
    /// A/B scenario. Sample only keys both ADP and the Datadog Agent honor
    /// identically, over value ranges both accept, so any divergence in their
    /// output is a real difference and not a config the two were never asked to
    /// share.
    Differential,
}

impl ConfigProfile {
    /// Returns `true` for the full feral surface, `false` for the shared subset.
    fn is_general(self) -> bool {
        matches!(self, ConfigProfile::General)
    }
}

/// A Go `time.Duration`, rendered as a Go duration string (for example `100ms`)
/// — the form the Agent's duration config keys parse.
#[derive(Debug, Clone, Copy)]
struct GoDuration(Duration);

impl Serialize for GoDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("{}ms", self.0.as_millis()))
    }
}

/// A duration the Agent reads as a plain integer number of seconds (`GetInt`),
/// rendered as that integer.
#[derive(Debug, Clone, Copy)]
struct DurationSeconds(Duration);

impl Serialize for DurationSeconds {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0.as_secs())
    }
}

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
/// by the environment; the rest are sampled.
///
/// Numeric fields are sampled with [`Probe`] over a per-field range. The
/// `Option` fields are keys the Datadog Agent rejects (`Incompatible`) or honors
/// differently from ADP (`Partial`); they are sampled only for [`ConfigProfile::General`]
/// and omitted from the rendered config otherwise.
#[allow(clippy::struct_field_names, clippy::struct_excessive_bools)]
#[derive(Debug, Serialize)]
pub(crate) struct DogStatsdConfig {
    /// Unix socket the server listens on. Supplied by the environment.
    dogstatsd_socket: PathBuf,
    /// Buffer used to receive statsd packets, in bytes.
    dogstatsd_buffer_size: u64,
    /// Bytes for the socket receive buffer (`POSIX`); `0` keeps the OS default.
    dogstatsd_so_rcvbuf: u64,
    /// Packets buffered before flushing to the processing queue. ADP rejects
    /// this key, so it is general-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    dogstatsd_packet_buffer_size: Option<u64>,
    /// Maximum time packets sit in the packet buffer before a flush. ADP rejects
    /// this key, so it is general-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    dogstatsd_packet_buffer_flush_timeout: Option<GoDuration>,
    /// Internal queue size of the server. ADP rejects this key, so it is
    /// general-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    dogstatsd_queue_size: Option<u64>,
    /// Number of processing pipelines. ADP rejects this key, so it is
    /// general-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    dogstatsd_pipeline_count: Option<u64>,
    /// Worker count processing packets. ADP rejects this key, so it is
    /// general-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    dogstatsd_workers_count: Option<u64>,
    /// Seconds a counter is sampled to `0` after its last value before expiring.
    /// ADP maps this onto its Saluki `counter_expiry_seconds` rather than the
    /// Agent's context expiry, so it is general-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    dogstatsd_expiry_seconds: Option<DurationSeconds>,
    /// Seconds a metric context is kept in memory after its last sample. The
    /// Core Agent folds this into counter zero-emission alongside
    /// `dogstatsd_expiry_seconds` while ADP holds counter expiry at its default.
    #[serde(skip_serializing_if = "Option::is_none")]
    dogstatsd_context_expiry_seconds: Option<DurationSeconds>,
    /// Maximum entries in the string interner cache.
    dogstatsd_string_interner_size: u64,
    /// Max number of metric-mapping results cached by the mapper. ADP differs on
    /// this key, so it is general-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    dogstatsd_mapper_cache_size: Option<u64>,
    /// Max metrics per payload from the no-aggregation pipeline. ADP rejects this
    /// key, so it is general-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    dogstatsd_no_aggregation_pipeline_batch_size: Option<u64>,
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
    /// Collect basic per-metric statistics (count / last seen). ADP differs on
    /// this key, so it is general-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    dogstatsd_metrics_stats_enable: Option<bool>,
    /// When an `Entity-ID` is set, skip origin-detection tag enrichment.
    dogstatsd_entity_id_precedence: bool,
    /// Enable the no-aggregation pipeline (forward timestamped metrics with
    /// tagging only).
    dogstatsd_no_aggregation_pipeline: bool,
    /// Flush incomplete metric time buckets on shutdown.
    dogstatsd_flush_incomplete_buckets: bool,
    /// Automatically adjust the number of processing pipelines. ADP rejects this
    /// key, so it is general-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    dogstatsd_pipeline_autoadjust: Option<bool>,
    /// Publish `DogStatsD` internal stats as Go expvars. ADP rejects this key, so
    /// it is general-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    dogstatsd_stats_enable: Option<bool>,
}

impl DogStatsdConfig {
    /// Sample the `DogStatsD` options from `rng` for `profile`, taking the socket
    /// from the environment.
    fn sample<R: Rng + ?Sized>(rng: &mut R, dogstatsd_socket: &Path, profile: ConfigProfile) -> Self {
        let general = profile.is_general();
        Self {
            dogstatsd_socket: dogstatsd_socket.to_path_buf(),
            dogstatsd_buffer_size: sample_buffer_size(rng, profile),
            dogstatsd_so_rcvbuf: Probe::new(0, 34_359_738_368).sample(rng),
            dogstatsd_packet_buffer_size: general.then(|| Probe::new(1, 10_000_000).sample(rng)),
            dogstatsd_packet_buffer_flush_timeout: general
                .then(|| GoDuration(Duration::from_millis(Probe::new(1, 86_400_000).sample(rng)))),
            dogstatsd_queue_size: general.then(|| Probe::new(1, 10_000_000).sample(rng)),
            dogstatsd_pipeline_count: general.then(|| Probe::new(1, 1_000_000).sample(rng)),
            dogstatsd_workers_count: general.then(|| Probe::new(0, 1_000_000).sample(rng)),
            dogstatsd_expiry_seconds: general
                .then(|| DurationSeconds(Duration::from_secs(Probe::new(0, 31_536_000_000).sample(rng)))),
            dogstatsd_context_expiry_seconds: general
                .then(|| DurationSeconds(Duration::from_secs(Probe::new(0, 31_536_000_000).sample(rng)))),
            dogstatsd_string_interner_size: Probe::new(1, MAX_STRING_INTERNER_ENTRIES).sample(rng),
            dogstatsd_mapper_cache_size: general.then(|| Probe::new(0, 100_000_000).sample(rng)),
            dogstatsd_no_aggregation_pipeline_batch_size: general.then(|| Probe::new(1, 10_000_000).sample(rng)),
            dogstatsd_tag_cardinality: rng.random(),
            dogstatsd_non_local_traffic: rng.random(),
            dogstatsd_origin_detection: rng.random(),
            dogstatsd_origin_detection_client: rng.random(),
            dogstatsd_origin_optout_enabled: rng.random(),
            dogstatsd_metrics_stats_enable: general.then(|| rng.random()),
            dogstatsd_entity_id_precedence: rng.random(),
            dogstatsd_no_aggregation_pipeline: rng.random(),
            dogstatsd_flush_incomplete_buckets: rng.random(),
            dogstatsd_pipeline_autoadjust: general.then(|| rng.random()),
            dogstatsd_stats_enable: general.then(|| rng.random()),
        }
    }
}

/// Entry-count ceiling for `dogstatsd_string_interner_size`.
///
/// ADP and the Core Agent both preallocate the interner at boot, multiplying
/// the entry count by 512 bytes when `dogstatsd_string_interner_size_bytes` is
/// unset. The current value will cap out at 4 GiB.
const MAX_STRING_INTERNER_ENTRIES: u64 = 8_388_608;

/// Receive-buffer size in bytes.
///
/// For [`ConfigProfile::Differential`] the value stays in a realistic range so
/// lines actually arrive on both targets. For [`ConfigProfile::General`] it is
/// usually realistic but rarely tiny or wild to probe the truncation edge. A
/// sampled `0` leaves ADP no room past the 4-byte length prefix, so it drops
/// every packet before decode — useful when ADP runs alone, but it would make
/// the differential targets diverge for a reason the oracle is not testing.
fn sample_buffer_size<R: Rng + ?Sized>(rng: &mut R, profile: ConfigProfile) -> u64 {
    if profile.is_general() && rng.random_ratio(1, 16) {
        Probe::new(0, 536_870_912).sample(rng)
    } else {
        rng.random_range(128..=65_536)
    }
}

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
    /// Aggregate transform context cap, the `aggregate_context_limit` key. This
    /// is a Saluki-only key with no Datadog Agent counterpart: ADP enforces it
    /// as a hard per-flush context drop while the Agent ignores it. Sampled only
    /// for [`ConfigProfile::General`]; omitted otherwise so the differential
    /// targets are not asked to honor a limit only one of them implements.
    #[serde(skip_serializing_if = "Option::is_none")]
    aggregate_context_limit: Option<u64>,
    /// `DogStatsD` options, flattened to top-level `dogstatsd_*` keys.
    #[serde(flatten)]
    dogstatsd: DogStatsdConfig,
}

impl DatadogConfig {
    /// Generate a config for `profile`: the environmental fields come from the
    /// caller, the rest are sampled from `rng`. With an Antithesis-backed rng,
    /// each call after the snapshot yields an independent draw per replay branch.
    #[must_use]
    pub fn sample<R: Rng + ?Sized>(
        rng: &mut R, hostname: &str, api_key: &str, dd_url: &str, dogstatsd_socket: &Path, profile: ConfigProfile,
    ) -> Self {
        Self {
            hostname: hostname.to_owned(),
            api_key: api_key.to_owned(),
            dd_url: dd_url.to_owned(),
            log_level: LogLevel::Error,
            aggregate_context_limit: profile.is_general().then(|| Probe::new(1, 100_000_000).sample(rng)),
            dogstatsd: DogStatsdConfig::sample(rng, dogstatsd_socket, profile),
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
}

/// Yaml flags the Agent reads at boot that never vary.
const STATIC_YAML_TAIL: &str = "use_dogstatsd: true
use_v2_api_series: true
inventories_enabled: false
enable_metadata_collection: false
cloud_provider_metadata: []
";

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use rand::rand_core::TryRng;

    use super::*;

    /// A trivial deterministic `SplitMix64` generator. Implementing `TryRng` with
    /// an infallible error gives a blanket [`rand::Rng`]. Key presence in the
    /// rendered config is gated by [`ConfigProfile`], not by sampled values, so
    /// any source of bits suffices.
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

    /// Keys the Datadog Agent rejects (`Incompatible`), honors differently
    /// (`Partial`), or does not define (Saluki-only). The differential config
    /// must omit every one; the general config must keep every one.
    const GENERAL_ONLY_KEYS: &[&str] = &[
        "aggregate_context_limit",
        "dogstatsd_packet_buffer_size",
        "dogstatsd_packet_buffer_flush_timeout",
        "dogstatsd_queue_size",
        "dogstatsd_pipeline_count",
        "dogstatsd_workers_count",
        "dogstatsd_expiry_seconds",
        "dogstatsd_context_expiry_seconds",
        "dogstatsd_mapper_cache_size",
        "dogstatsd_no_aggregation_pipeline_batch_size",
        "dogstatsd_metrics_stats_enable",
        "dogstatsd_pipeline_autoadjust",
        "dogstatsd_stats_enable",
    ];

    fn render(profile: ConfigProfile) -> String {
        let mut rng = SeqRng(0);
        DatadogConfig::sample(&mut rng, "h", "k", "http://intake:2049", Path::new("/s.sock"), profile)
            .to_yaml()
            .expect("render yaml")
    }

    #[test]
    fn differential_omits_keys_the_agent_does_not_share() {
        let yaml = render(ConfigProfile::Differential);
        for key in GENERAL_ONLY_KEYS {
            assert!(!yaml.contains(key), "differential config must omit `{key}`:\n{yaml}");
        }
    }

    #[test]
    fn general_keeps_the_full_feral_surface() {
        let yaml = render(ConfigProfile::General);
        for key in GENERAL_ONLY_KEYS {
            assert!(yaml.contains(key), "general config must include `{key}`:\n{yaml}");
        }
    }

    #[test]
    fn differential_keeps_keys_both_targets_share() {
        let yaml = render(ConfigProfile::Differential);
        for key in [
            "dogstatsd_string_interner_size",
            "dogstatsd_non_local_traffic",
            "dogstatsd_no_aggregation_pipeline",
        ] {
            assert!(
                yaml.contains(key),
                "differential config dropped shared key `{key}`:\n{yaml}"
            );
        }
    }

    #[test]
    fn log_level_is_an_unambiguous_scalar_for_both_profiles() {
        for profile in [ConfigProfile::General, ConfigProfile::Differential] {
            let yaml = render(profile);
            assert!(
                yaml.contains("log_level: error"),
                "missing `log_level: error` in:\n{yaml}"
            );
        }
    }
}
