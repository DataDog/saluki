//! Configuration model and rendering for Datadog Agent configuration.
//!
//! Primary focus is currently `DogStatsD` but this is, hopefully, easy to expand
//! in the future.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use harness::rand::Probe;
use rand::distr::{Distribution, StandardUniform};
use rand::{Rng, RngExt};
use serde::{Serialize, Serializer};

/// Yaml flags the Agent reads at boot that never vary.
const STATIC_YAML_TAIL: &str = "use_dogstatsd: true
use_v2_api_series: true
inventories_enabled: false
enable_metadata_collection: false
cloud_provider_metadata: []
";

/// A Go `time.Duration`, rendered as a Go duration string (for example `100ms`)
/// — the form the Agent's duration config keys parse.
#[derive(Debug, Clone, Copy)]
struct GoDuration(Duration);

impl Serialize for GoDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("{}ms", self.0.as_millis()))
    }
}

impl Distribution<GoDuration> for Probe {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> GoDuration {
        let millis: u64 = self.sample(rng);
        GoDuration(Duration::from_millis(millis))
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

impl Distribution<DurationSeconds> for Probe {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> DurationSeconds {
        let secs: u64 = self.sample(rng);
        DurationSeconds(Duration::from_secs(secs))
    }
}

/// Agent log level
///
/// Restricted to quiet levels on purpose. Antithesis enforces a per-hour
/// log-output budget per run and `info`/`debug`/`trace` is a whole awful lot of
/// logs.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogLevel {
    /// Errors only — the quietest level that still logs.
    Error,
    /// No logs at all — the floor of the log-output budget.
    Off,
}

impl Distribution<LogLevel> for StandardUniform {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> LogLevel {
        match rng.random_range(0..2u8) {
            0 => LogLevel::Error,
            _ => LogLevel::Off,
        }
    }
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

/// The Agent's `DogStatsD` configuration surface. `dogstatsd_socket` is
/// supplied by the environment; the rest are sampled.
///
/// Numeric fields are sampled with [`Probe`]: usually a typical value (so ADP
/// boots and runs), occasionally a boundary value to probe overflow and
/// wraparound.
#[allow(clippy::struct_field_names, clippy::struct_excessive_bools)]
#[derive(Debug, Serialize)]
pub(crate) struct DogStatsdConfig {
    /// Unix socket the server listens on. Supplied by the environment.
    dogstatsd_socket: PathBuf,
    /// Buffer used to receive statsd packets, in bytes.
    dogstatsd_buffer_size: u64,
    /// Bytes for the socket receive buffer (`POSIX`); `0` keeps the OS default.
    dogstatsd_so_rcvbuf: u64,
    /// Packets buffered before flushing to the processing queue.
    dogstatsd_packet_buffer_size: u64,
    /// Maximum time packets sit in the packet buffer before a flush.
    dogstatsd_packet_buffer_flush_timeout: GoDuration,
    /// Internal queue size of the server; smaller caps memory but risks packet
    /// drops.
    dogstatsd_queue_size: u64,
    /// Number of processing pipelines.
    dogstatsd_pipeline_count: u64,
    /// Worker count processing packets; `0` lets the Agent choose.
    dogstatsd_workers_count: u64,
    /// Seconds a counter is sampled to `0` after its last value before expiring.
    dogstatsd_expiry_seconds: DurationSeconds,
    /// Seconds a metric context is kept in memory after its last sample.
    dogstatsd_context_expiry_seconds: DurationSeconds,
    /// Maximum entries in the string interner cache.
    dogstatsd_string_interner_size: u64,
    /// Max number of metric-mapping results cached by the mapper.
    dogstatsd_mapper_cache_size: u64,
    /// Max metrics per payload from the no-aggregation pipeline.
    dogstatsd_no_aggregation_pipeline_batch_size: u64,
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
    /// Collect basic per-metric statistics (count / last seen).
    dogstatsd_metrics_stats_enable: bool,
    /// When an `Entity-ID` is set, skip origin-detection tag enrichment.
    dogstatsd_entity_id_precedence: bool,
    /// Enable the no-aggregation pipeline (forward timestamped metrics with
    /// tagging only).
    dogstatsd_no_aggregation_pipeline: bool,
    /// Flush incomplete metric time buckets on shutdown.
    dogstatsd_flush_incomplete_buckets: bool,
    /// Automatically adjust the number of processing pipelines.
    dogstatsd_pipeline_autoadjust: bool,
    /// Publish `DogStatsD` internal stats as Go expvars.
    dogstatsd_stats_enable: bool,
}

/// Receive-buffer size in bytes. Usually realistic so lines actually arrive,
/// rarely tiny or wild to probe the truncation edge. A sampled `0` leaves ADP
/// no room past the 4-byte length prefix, so it drops every packet before
/// decode and `finally_verify_delivery` sees nothing delivered end-to-end.
/// Keep `0` and sub-128 values rare.
fn sample_buffer_size<R: Rng + ?Sized>(rng: &mut R) -> u64 {
    if rng.random_ratio(1, 16) {
        Probe.sample(rng)
    } else {
        rng.random_range(128..=65_536)
    }
}

impl DogStatsdConfig {
    /// Sample the `DogStatsD` options from `rng`, taking the socket from the
    /// environment.
    fn sample<R: Rng + ?Sized>(rng: &mut R, dogstatsd_socket: &Path) -> Self {
        Self {
            dogstatsd_socket: dogstatsd_socket.to_path_buf(),
            dogstatsd_buffer_size: sample_buffer_size(rng),
            dogstatsd_so_rcvbuf: Probe.sample(rng),
            dogstatsd_packet_buffer_size: Probe.sample(rng),
            dogstatsd_packet_buffer_flush_timeout: Probe.sample(rng),
            dogstatsd_queue_size: Probe.sample(rng),
            dogstatsd_pipeline_count: Probe.sample(rng),
            dogstatsd_workers_count: Probe.sample(rng),
            dogstatsd_expiry_seconds: Probe.sample(rng),
            dogstatsd_context_expiry_seconds: Probe.sample(rng),
            dogstatsd_string_interner_size: Probe.sample(rng),
            dogstatsd_mapper_cache_size: Probe.sample(rng),
            dogstatsd_no_aggregation_pipeline_batch_size: Probe.sample(rng),
            dogstatsd_tag_cardinality: rng.random(),
            dogstatsd_non_local_traffic: rng.random(),
            dogstatsd_origin_detection: rng.random(),
            dogstatsd_origin_detection_client: rng.random(),
            dogstatsd_origin_optout_enabled: rng.random(),
            dogstatsd_metrics_stats_enable: rng.random(),
            dogstatsd_entity_id_precedence: rng.random(),
            dogstatsd_no_aggregation_pipeline: rng.random(),
            dogstatsd_flush_incomplete_buckets: rng.random(),
            dogstatsd_pipeline_autoadjust: rng.random(),
            dogstatsd_stats_enable: rng.random(),
        }
    }
}

/// Agent-facing config. `hostname`, `api_key`, `dd_url`, and the socket are
/// supplied by the environment; `log_level` and the `DogStatsD` options are
/// sampled per branch. The static flags are appended by [`Self::to_yaml`], not
/// fields here.
#[derive(Debug, Serialize)]
pub(crate) struct DatadogConfig {
    /// Agent hostname. Supplied by the environment. ADP requires it
    /// (`FixedHostProvider`); absent it refuses to boot.
    hostname: String,
    /// Agent API key. Supplied by the environment.
    api_key: String,
    /// Metrics intake base URL. Supplied by the environment.
    dd_url: String,
    /// Agent log verbosity. Sampled; restricted to quiet levels (see [`LogLevel`]).
    log_level: LogLevel,
    /// Aggregate transform context cap, the `aggregate_context_limit` key. ADP
    /// defaults to `1_000_000` contexts per flush window, far above what the
    /// workload reaches in a window, so the breach path never fired. Sampled
    /// with [`Probe`] so it often lands small enough for the workload to reach
    /// and exercise the cap, and occasionally large to probe the headroom.
    aggregate_context_limit: u64,
    /// `DogStatsD` options, flattened to top-level `dogstatsd_*` keys.
    #[serde(flatten)]
    dogstatsd: DogStatsdConfig,
}

impl DatadogConfig {
    /// Generate a config: the environmental fields come from the caller, the
    /// rest are sampled from `rng`. With an Antithesis-backed rng, each call after
    /// the snapshot yields an independent draw per replay branch.
    pub(crate) fn sample<R: Rng + ?Sized>(
        rng: &mut R, hostname: &str, api_key: &str, dd_url: &str, dogstatsd_socket: &Path,
    ) -> Self {
        Self {
            hostname: hostname.to_owned(),
            api_key: api_key.to_owned(),
            dd_url: dd_url.to_owned(),
            log_level: rng.random(),
            aggregate_context_limit: Probe.sample(rng),
            dogstatsd: DogStatsdConfig::sample(rng, dogstatsd_socket),
        }
    }

    /// Render `self` as a `datadog.yaml` string, followed by the static-tail
    /// flags.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub(crate) fn to_yaml(&self) -> anyhow::Result<String> {
        let mut yaml = serde_yaml::to_string(self).context("serialize datadog.yaml")?;
        yaml.push_str(STATIC_YAML_TAIL);
        Ok(yaml)
    }
}
