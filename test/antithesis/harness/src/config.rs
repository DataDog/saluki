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

use crate::payload::dogstatsd::DATAGRAM_BYTE_LIMIT;
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

/// Compressor both targets serialize metric payloads with.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CompressorKind {
    /// Deflate. Disables the v3 series intake on both targets.
    Zlib,
    /// Zstandard.
    Zstd,
    /// Gzip.
    Gzip,
    /// No compression, the Agent's `NoneKind`.
    None,
    /// A codec neither target implements, so each falls back its own way and the two lanes disagree
    /// on the wire from one config value.
    Snappy,
}

impl Distribution<CompressorKind> for StandardUniform {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> CompressorKind {
        match rng.random_range(0..5u8) {
            0 => CompressorKind::Zlib,
            1 => CompressorKind::Zstd,
            2 => CompressorKind::Gzip,
            3 => CompressorKind::None,
            _ => CompressorKind::Snappy,
        }
    }
}

/// The Agent's nested `use_v3_api.series` switch between the v2 and v3 series
/// intake.
#[derive(Debug, Serialize)]
pub(crate) struct UseV3ApiConfig {
    /// The series sub-tree.
    series: V3SeriesConfig,
}

/// The `enabled` leaf under a v3 series key.
#[derive(Debug, Serialize)]
pub(crate) struct V3SeriesConfig {
    /// Whether the series intake is v3 rather than v2. A string because both the Agent and ADP read
    /// this leaf as one, and ADP's typed model rejects a YAML boolean outright.
    enabled: &'static str,
}

/// Cloud provider kinds sampled into `provider_kind`. The empty string emits no tag.
const PROVIDER_KINDS: &[&str] = &["", "gke-autopilot", "gke-gdc"];

/// Agent-facing config. `hostname`, `api_key`, `dd_url`, and the socket are
/// supplied by the environment; `log_level`, the series intake API, and the
/// `DogStatsD` options are sampled per branch. The static flags are appended by
/// [`Self::to_yaml`], not fields here.
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
    /// Series intake API for this timeline.
    use_v3_api: UseV3ApiConfig,
    /// Compressor for metric payloads. Sampled independently of the series API.
    serializer_compressor_kind: CompressorKind,
    /// ADP's safety gate for authoritative v3 series, which the Agent has no counterpart for.
    /// Sampled with [`Self::use_v3_api`] so ADP and the Agent never split encodings in a timeline.
    data_plane_metrics_v3_series_enabled: bool,
    /// Cloud provider kind, sampled per timeline from [`PROVIDER_KINDS`].
    ///
    /// A non-empty value makes the Agent append `provider_kind:<value>` to every `DogStatsD` metric as a
    /// static tag, `comp/dogstatsd/server/impl/server.go:263` and `pkg/util/tags/static_tags.go:84`. In a
    /// v3 payload that tag becomes a shared prefix tagset, and a metric carrying both a prefix tagset and
    /// its own tags is the only thing that makes the Agent emit the tagset back-reference Pyld54 asserts
    /// on. Without it the rig never reaches that assertion.
    ///
    /// The empty string is one of the three sampled values and emits no tag, matching the Agent's own
    /// default. So one timeline in three carries no prefix and exercises the back-reference-free path.
    ///
    /// This is a test input rather than an operator knob. Widen [`PROVIDER_KINDS`] to cover more prefix
    /// tagsets.
    #[serde(skip_serializing_if = "String::is_empty")]
    provider_kind: String,
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
        let series_v3 = rng.random();
        Self {
            hostname: hostname.to_owned(),
            api_key: api_key.to_owned(),
            dd_url: dd_url.to_owned(),
            log_level: LogLevel::Error,
            use_v3_api: UseV3ApiConfig {
                series: V3SeriesConfig {
                    enabled: if series_v3 { "true" } else { "false" },
                },
            },
            serializer_compressor_kind: rng.random(),
            data_plane_metrics_v3_series_enabled: series_v3,
            provider_kind: PROVIDER_KINDS[rng.random_range(0..PROVIDER_KINDS.len())].to_owned(),
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
inventories_enabled: false
enable_metadata_collection: false
cloud_provider_metadata: []
";

/// Upper bound on datagrams one driver invocation ships in a timeline.
const MAX_DATAGRAMS: usize = 10_000;

/// Upper bound on the working set one driver invocation fetches from the shared context pool.
const MAX_WORKING_SET: u64 = 1_024;

/// The intake's ceiling on one `/contexts` request. A `context_count` past it is rejected there, so a
/// config carrying one is rejected here instead.
const MAX_CONTEXTS_PER_REQUEST: usize = 65_536;

/// Config a load generator reads to shape its output to this timeline's SUT.
/// `first_sample_config` samples it beside `datadog.yaml` from one draw, so the
/// generator and the SUT are driven together.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DriverConfig {
    /// Max bytes a generator packs into one datagram, the smaller of the SUT's
    /// sampled receive buffer and [`DATAGRAM_BYTE_LIMIT`]. A datagram this size
    /// fits one read, so the SUT never truncates a line mid-token.
    pub datagram_byte_limit: usize,
    /// Datagrams a driver invocation ships this timeline.
    pub datagram_count: usize,
    /// Distinct contexts a driver invocation fetches from the shared pool as its working set.
    ///
    /// Sampled boundary-biased log-uniform in `1..=1_024`. Valid values run `1..=65_536`, the intake's
    /// per-request ceiling. Outside that the intake rejects every `/contexts` request, the driver waits
    /// out its fetch budget and ships nothing, so [`Self::read`] rejects such a config rather than
    /// letting a timeline generate no load. Every context in a pull gets a line in every datagram that
    /// has room, so a larger pull makes fatter datagrams over more identities rather than thinner
    /// series, and the trade is against how often any one identity recurs.
    pub context_count: usize,
}

impl DriverConfig {
    /// Sample the driver knobs for a SUT whose receive buffer is `buffer_size`.
    fn sample<R: Rng + ?Sized>(rng: &mut R, buffer_size: u64) -> Self {
        // The min is at most DATAGRAM_BYTE_LIMIT, so a buffer wider than usize
        // caps to the ceiling like any other oversized buffer.
        let datagram_byte_limit = match usize::try_from(buffer_size.min(DATAGRAM_BYTE_LIMIT as u64)) {
            Ok(bytes) => bytes,
            Err(_) => DATAGRAM_BYTE_LIMIT,
        };
        Self {
            datagram_byte_limit,
            datagram_count: rng.random_range(0..=MAX_DATAGRAMS),
            context_count: usize::try_from(Probe::new(1, MAX_WORKING_SET).sample(rng)).unwrap_or(usize::MAX),
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
    /// integer `datagram_byte_limit`.
    pub fn read(config_dir: &Path) -> anyhow::Result<Self> {
        let path = config_dir.join("driver.yaml");
        let yaml = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let config: Self =
            serde_yaml::from_str(&yaml).with_context(|| format!("parse driver config from {}", path.display()))?;
        anyhow::ensure!(
            (1..=MAX_CONTEXTS_PER_REQUEST).contains(&config.context_count),
            "context_count {} outside 1..={} in {}, the intake would reject every fetch and the driver would ship nothing",
            config.context_count,
            MAX_CONTEXTS_PER_REQUEST,
            path.display()
        );
        Ok(config)
    }
}

/// Upper bound on the distinct contexts a shared pool holds across every kind. The pool retains each
/// minted context, so the ceiling belongs to the total rather than to any one kind.
const MAX_CONTEXTS_TOTAL: u64 = 1_000_000;

/// The per-kind caps a timeline's shared context pool fills to before it recurs existing contexts.
/// `first_sample_config` samples this beside `datadog.yaml` so cardinality varies per timeline. Each
/// cap is drawn against the budget the earlier draws left, so a kind's cardinality still varies at
/// random while the three together stay under [`MAX_CONTEXTS_TOTAL`].
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ContextSourceConfig {
    /// Bytes a rendered line of any pooled context must fit, this timeline's real datagram budget
    /// rather than the protocol ceiling. Mint builds identities against it, so every served context
    /// has a rendering the driver can pack. Defaults to the sampled `datagram_byte_limit`, which is the
    /// smaller of the SUT's receive buffer and [`DATAGRAM_BYTE_LIMIT`].
    pub datagram_byte_limit: usize,
    /// Distinct metric contexts the pool holds before it recurs the ones it has.
    ///
    /// Sampled in `2..=MAX_CONTEXTS_TOTAL` minus what the other kinds took. A larger cap explores more
    /// identities and puts fewer points in each, and costs memory: the pool retains every context it
    /// mints for the life of the run. Two is the floor because the pool holds one context carrying an
    /// invalid UTF-8 byte per kind alongside the rest, and a kind capped at one could hold only one of
    /// the two.
    pub metric_contexts: usize,
    /// Distinct event contexts the pool holds. Same range and trade as [`Self::metric_contexts`].
    pub event_contexts: usize,
    /// Distinct service-check contexts the pool holds. Same range and trade as
    /// [`Self::metric_contexts`].
    pub service_check_contexts: usize,
}

impl ContextSourceConfig {
    /// Sample the per-kind caps, each boundary-biased log-uniform against the budget still free.
    ///
    /// The draws run in order and each spends from one shared ceiling, so the total is bounded by
    /// construction rather than by scaling three independent draws afterwards. Every kind keeps at
    /// least two contexts, one of which carries an invalid UTF-8 byte.
    #[must_use]
    pub fn sample<R: Rng + ?Sized>(rng: &mut R, datagram_byte_limit: usize) -> Self {
        // Four contexts held back so the two later kinds can each keep their two.
        let metric_contexts = sample_cap(rng, MAX_CONTEXTS_TOTAL - 4);
        let free = MAX_CONTEXTS_TOTAL - metric_contexts as u64;
        let event_contexts = sample_cap(rng, free - 2);
        let free = free - event_contexts as u64;
        Self {
            datagram_byte_limit,
            metric_contexts,
            event_contexts,
            service_check_contexts: sample_cap(rng, free),
        }
    }

    /// Render `self` as a `context_source.yaml` string.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_yaml(&self) -> anyhow::Result<String> {
        serde_yaml::to_string(self).context("serialize context_source.yaml")
    }

    /// Read the context-source config from the `context_source.yaml` that `first_sample_config` wrote
    /// to `config_dir`.
    ///
    /// # Errors
    ///
    /// Returns an error if the config is unreadable, is not valid YAML, or caps a kind below two. The
    /// pool seeds every kind with one context carrying an invalid UTF-8 byte and one without, so a cap of
    /// one cannot hold both and the pool would exceed its own cap assertion. [`Self::sample`] never draws
    /// below two, and this rejects a config that reached the pool by another route rather than letting it
    /// redden a run.
    pub fn read(config_dir: &Path) -> anyhow::Result<Self> {
        let path = config_dir.join("context_source.yaml");
        let yaml = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let config: Self = serde_yaml::from_str(&yaml)
            .with_context(|| format!("parse context source config from {}", path.display()))?;
        for (kind, cap) in [
            ("metric_contexts", config.metric_contexts),
            ("event_contexts", config.event_contexts),
            ("service_check_contexts", config.service_check_contexts),
        ] {
            anyhow::ensure!(
                cap >= MIN_CONTEXTS_PER_KIND,
                "{kind} is {cap}, which is below the {MIN_CONTEXTS_PER_KIND} the pool seeds"
            );
        }
        Ok(config)
    }
}

/// The fewest contexts a kind may be capped at. The pool seeds every kind with one context carrying an
/// invalid UTF-8 byte and one without, and both count against the cap.
pub const MIN_CONTEXTS_PER_KIND: usize = 2;

/// A single per-kind cap in `2..=ceiling`. The ceiling is well within `usize` on every supported
/// target, so the saturating conversion is unreachable in practice.
fn sample_cap<R: Rng + ?Sized>(rng: &mut R, ceiling: u64) -> usize {
    usize::try_from(Probe::new(2, ceiling.max(2)).sample(rng)).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
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

    // A cap below what the pool seeds would make the pool exceed its own cap assertion and redden the
    // run on a config value. Rejected at the boundary, as the sibling driver config is.
    #[test]
    fn context_source_read_rejects_a_cap_below_the_seeded_minimum() {
        let dir = std::env::temp_dir().join(format!("ctxcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("context_source.yaml"),
            "datagram_byte_limit: 8192\nmetric_contexts: 1\nevent_contexts: 4\nservice_check_contexts: 4\n",
        )
        .expect("write config");
        let err = ContextSourceConfig::read(&dir).expect_err("a cap of 1 must be rejected");
        assert!(err.to_string().contains("metric_contexts"), "{err}");
    }

    #[test]
    fn driver_config_caps_payload_to_the_smaller_bound() {
        assert_eq!(DriverConfig::sample(&mut SeqRng(0), 512).datagram_byte_limit, 512);
        assert_eq!(
            DriverConfig::sample(&mut SeqRng(0), 1 << 30).datagram_byte_limit,
            DATAGRAM_BYTE_LIMIT
        );
        assert_eq!(DriverConfig::sample(&mut SeqRng(0), 0).datagram_byte_limit, 0);
    }

    #[test]
    fn log_level_is_always_an_unambiguous_scalar() {
        assert!(has_key(&render(0), "log_level"));
        assert!(render(0).contains("log_level: error"));
    }

    /// The Agent's nested switch and ADP's safety gate, as a timeline renders them.
    fn series_api(seed: u64) -> (bool, bool) {
        let yaml = render(seed);
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("parse rendered yaml");
        // A string, not a YAML boolean: ADP's typed model reads this leaf as `String`, so an unquoted
        // boolean fails deserialization and the target never boots.
        let agent = match parsed["use_v3_api"]["series"]["enabled"]
            .as_str()
            .expect("use_v3_api.series.enabled is a string")
        {
            "true" => true,
            "false" => false,
            other => panic!("unexpected series mode {other}"),
        };
        let adp = parsed["data_plane_metrics_v3_series_enabled"]
            .as_bool()
            .expect("data_plane_metrics_v3_series_enabled");
        (agent, adp)
    }

    #[test]
    fn both_lanes_share_one_series_api() {
        for seed in 0..16 {
            let (agent, adp) = series_api(seed);
            assert_eq!(agent, adp, "seed {seed}");
        }
    }

    #[test]
    fn series_api_samples_both_intakes() {
        let mut seen = [false, false];
        for seed in 0..16 {
            seen[usize::from(series_api(seed).0)] = true;
        }
        assert_eq!(seen, [true, true]);
    }

    // The pool holds every minted context, so the ceiling is on the total across kinds rather than on
    // each kind alone. Three independent draws at the ceiling would retain three million.
    #[test]
    fn context_caps_sum_within_the_total_ceiling() {
        for seed in 0..64 {
            let caps = ContextSourceConfig::sample(&mut SeqRng(seed), 8_192);
            let total = (caps.metric_contexts + caps.event_contexts + caps.service_check_contexts) as u64;
            assert!(total <= MAX_CONTEXTS_TOTAL, "seed {seed} sampled {total}");
            // The pool seeds every kind with one context carrying an invalid UTF-8 byte and one without,
            // and both count against the cap, so a kind capped below two makes the pool exceed its own
            // cap assertion.
            assert!(
                caps.metric_contexts >= MIN_CONTEXTS_PER_KIND
                    && caps.event_contexts >= MIN_CONTEXTS_PER_KIND
                    && caps.service_check_contexts >= MIN_CONTEXTS_PER_KIND
            );
        }
    }

    // Randomness still drives each kind rather than the total being split evenly.
    #[test]
    fn context_caps_vary_per_kind() {
        let spread: BTreeSet<usize> = (0..64)
            .map(|seed| ContextSourceConfig::sample(&mut SeqRng(seed), 8_192).metric_contexts)
            .collect();
        assert!(spread.len() > 8, "metric cap barely varies: {spread:?}");
    }

    #[test]
    fn compressor_samples_every_kind() {
        let mut seen = BTreeSet::new();
        for seed in 0..64 {
            let yaml = render(seed);
            let kind = yaml
                .lines()
                .find_map(|line| line.strip_prefix("serializer_compressor_kind: "))
                .expect("serializer_compressor_kind")
                .to_owned();
            seen.insert(kind);
        }
        let want = ["gzip", "none", "snappy", "zlib", "zstd"]
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        assert_eq!(seen, want);
    }
}
