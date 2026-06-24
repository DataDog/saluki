//! Configuration for the DogStatsD source component.
//!
//! The types here hold the resolved, source-agnostic config values the DogStatsD source consumes at
//! runtime. They are plain data: no behavior, no runtime handles, and no `Deserialize`. The
//! translated-config system builds them field-by-field from Datadog keys; nothing deserializes into
//! them. See the `SourceConfig` docs for how this slice relates to `dogstatsd::Config` and
//! `DogStatsDConfiguration`.

use std::path::PathBuf;

use bytesize::ByteSize;
use saluki_context::origin::OriginTagCardinality;
use stringtheory::MetaString;

/// Configuration data for the DogStatsD *source* component: the resolved, source-agnostic values
/// the source consumes (listeners, parser and decoding options, interner sizing, capture settings).
///
/// Plain data only - no behavior, no runtime handles, and no `Deserialize`. The translated-config
/// system builds this field-by-field from Datadog keys; it is never deserialized into directly. It
/// derives `Serialize` for diagnostics.
///
/// It lives in this leaf crate so its two consumers can share it without depending on each other:
/// - `agent-data-plane-config`'s `dogstatsd::Config` embeds it as the `source` slice of the
///   DogStatsD domain family.
/// - `saluki-components`' `DogStatsDConfiguration` (the source's builder) embeds it by value and
///   adds the runtime handles plus the `build()` behavior.
///
/// It is named `SourceConfig`, not `Config`, because `dogstatsd::Config` is the larger domain group
/// that contains this slice.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SourceConfig {
    /// The size of the receive buffer, in bytes.
    ///
    /// Payloads larger than this are truncated, leading to discarded messages. Defaults to 8192.
    pub buffer_size: usize,

    /// The number of message buffers to allocate up front.
    ///
    /// Defaults to 128.
    pub buffer_count: usize,

    /// The maximum number of message buffers to allocate overall.
    ///
    /// The pool grows on demand from `buffer_count` up to this limit. A value below `buffer_count`
    /// is treated as equal to it. Defaults to 256.
    pub buffer_count_max: usize,

    /// The UDP port to listen on. If set to `0`, UDP is not used.
    ///
    /// Defaults to 8125.
    pub port: u16,

    /// The UDP/UDS socket receive buffer size, in bytes. If set to `0`, the OS default is used.
    ///
    /// Defaults to 0.
    pub socket_receive_buffer_size: usize,

    /// The TCP port to listen on. If set to `0`, TCP is not used.
    ///
    /// Defaults to 0.
    pub tcp_port: u16,

    /// The host to forward framed DogStatsD messages to over UDP.
    ///
    /// Forwarding is enabled only when this is non-empty and `statsd_forward_port` is non-zero.
    /// Defaults to unset.
    pub statsd_forward_host: Option<MetaString>,

    /// The port to forward framed DogStatsD messages to over UDP.
    ///
    /// Defaults to 0.
    pub statsd_forward_port: u16,

    /// The Unix domain socket path to listen on, in datagram mode. Defaults to unset.
    pub socket_path: Option<String>,

    /// The Unix domain socket path to listen on, in stream mode. Defaults to unset.
    pub socket_stream_path: Option<String>,

    /// Whether to log oversized DogStatsD stream frames.
    ///
    /// Defaults to `false`.
    pub stream_log_too_big: bool,

    /// Whether to lower DogStatsD parse-failure logs to debug level.
    ///
    /// Defaults to `false`.
    pub disable_verbose_logs: bool,

    /// Listener types that require DogStatsD messages to be newline-terminated.
    ///
    /// Valid values are `udp`, `uds`, and `named_pipe`. Defaults to empty.
    pub eol_required: Vec<String>,

    /// The host address to bind UDP and TCP listeners to.
    ///
    /// Ignored when `non_local_traffic` is `true`. Defaults to unset, which binds to `127.0.0.1`.
    pub bind_host: Option<String>,

    /// Whether to listen for non-local traffic in UDP mode.
    ///
    /// Defaults to `false`.
    pub non_local_traffic: bool,

    /// Whether to autoscale UDP stream handlers using `SO_REUSEPORT`.
    ///
    /// Has no effect on non-Linux platforms. Defaults to `false`.
    pub autoscale_udp_listeners: bool,

    /// Whether to allow heap allocations when resolving contexts.
    ///
    /// Defaults to `true`.
    pub allow_context_heap_allocations: bool,

    /// Whether to enable support for no-aggregation pipelines.
    ///
    /// Defaults to `true`.
    pub no_aggregation_pipeline_support: bool,

    /// Number of entries for the string interner, as interpreted by the Core Agent.
    ///
    /// Multiplied by 512 bytes per entry when `context_string_interner_size_bytes` is unset.
    /// Defaults to 4096 entries.
    pub context_string_interner_entry_count: u64,

    /// Total size of the string interner used for contexts, in bytes.
    ///
    /// When set, takes priority over `context_string_interner_entry_count`. Defaults to unset.
    pub context_string_interner_size_bytes: Option<ByteSize>,

    /// The maximum number of cached contexts to allow.
    ///
    /// Defaults to 500,000.
    pub cached_contexts_limit: usize,

    /// The maximum number of cached tagsets to allow.
    ///
    /// Defaults to 500,000.
    pub cached_tagsets_limit: usize,

    /// The number of seconds after which cached contexts expire.
    ///
    /// Defaults to 20 seconds.
    pub context_expiry_seconds: u64,

    /// Whether to enable permissive mode in the decoder.
    ///
    /// Defaults to `true`.
    pub permissive_decoding: bool,

    /// The minimum sample rate allowed for metrics.
    ///
    /// Sample rates lower than this are clamped. Defaults to `0.000000003845`.
    pub minimum_sample_rate: f64,

    /// Which payload types to forward to the backend.
    pub enable_payloads: EnablePayloadsConfiguration,

    /// Origin detection and enrichment configuration.
    pub origin_enrichment: OriginEnrichmentConfiguration,

    /// Additional tags to add to all metrics. Defaults to empty.
    pub additional_tags: Vec<String>,

    /// The directory where DogStatsD capture files are written by default.
    ///
    /// Defaults to empty.
    pub capture_path: PathBuf,

    /// The maximum number of captured packets that can be queued for persistence.
    ///
    /// Values below 1024 are raised to 1024. Defaults to 1024.
    pub capture_depth: usize,

    /// Provider kind tag appended to all metrics as `provider_kind:<value>`.
    ///
    /// When empty or absent, no tag is added. Defaults to empty.
    pub provider_kind: String,
}

impl Default for SourceConfig {
    // TODO: unify default strategy
    fn default() -> Self {
        Self {
            buffer_size: 8192,
            buffer_count: 128,
            buffer_count_max: 256,
            port: 8125,
            socket_receive_buffer_size: 0,
            tcp_port: 0,
            statsd_forward_host: None,
            statsd_forward_port: 0,
            socket_path: None,
            socket_stream_path: None,
            stream_log_too_big: false,
            disable_verbose_logs: false,
            eol_required: Vec::new(),
            bind_host: None,
            non_local_traffic: false,
            autoscale_udp_listeners: false,
            allow_context_heap_allocations: true,
            no_aggregation_pipeline_support: true,
            context_string_interner_entry_count: 4096,
            context_string_interner_size_bytes: None,
            cached_contexts_limit: 500_000,
            cached_tagsets_limit: 500_000,
            context_expiry_seconds: 20,
            permissive_decoding: true,
            minimum_sample_rate: 0.000000003845,
            enable_payloads: EnablePayloadsConfiguration::default(),
            origin_enrichment: OriginEnrichmentConfiguration::default(),
            additional_tags: Vec::new(),
            capture_path: PathBuf::new(),
            capture_depth: 1024,
            provider_kind: String::new(),
        }
    }
}

/// Which DogStatsD payload types to forward to the backend.
///
/// Mirrors `EnablePayloadsConfiguration` in `saluki-components`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct EnablePayloadsConfiguration {
    /// Whether to enable sending series (counter/gauge/rate) payloads.
    ///
    /// Defaults to `true`.
    pub series: bool,

    /// Whether to enable sending sketch (distribution) payloads.
    ///
    /// Defaults to `true`.
    pub sketches: bool,

    /// Whether to enable sending event payloads.
    ///
    /// Defaults to `true`.
    pub events: bool,

    /// Whether to enable sending service check payloads.
    ///
    /// Defaults to `true`.
    pub service_checks: bool,
}

impl Default for EnablePayloadsConfiguration {
    fn default() -> Self {
        Self {
            series: true,
            sketches: true,
            events: true,
            service_checks: true,
        }
    }
}

/// Origin detection and enrichment configuration for the DogStatsD source.
///
/// Mirrors `OriginEnrichmentConfiguration` in `saluki-components`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct OriginEnrichmentConfiguration {
    /// Whether origin detection is enabled.
    ///
    /// Defaults to `false`.
    pub enabled: bool,

    /// Whether a client-provided entity ID takes precedence over detected origin metadata.
    ///
    /// Defaults to `false`.
    pub entity_id_precedence: bool,

    /// The default cardinality of tags to enrich metrics with.
    ///
    /// Defaults to [`OriginTagCardinality::Low`].
    pub tag_cardinality: OriginTagCardinality,

    /// Whether to use the unified origin detection behavior.
    ///
    /// Defaults to `false`.
    pub origin_detection_unified: bool,

    /// Whether to opt out of origin detection for metrics that explicitly request `none` cardinality.
    ///
    /// Defaults to `true`.
    pub origin_detection_optout: bool,

    /// Whether to parse client-provided origin fields from DogStatsD payloads.
    ///
    /// Defaults to `false`.
    pub origin_detection_client: bool,
}

impl Default for OriginEnrichmentConfiguration {
    fn default() -> Self {
        Self {
            enabled: false,
            entity_id_precedence: false,
            tag_cardinality: OriginTagCardinality::Low,
            origin_detection_unified: false,
            origin_detection_optout: true,
            origin_detection_client: false,
        }
    }
}

/// Configuration data for the DogStatsD metric prefix and listener-side metric filter.
///
/// Plain data only: no behavior, no runtime handles, no `Deserialize`. The translated-config system
/// builds this field-by-field from Datadog keys; it is never deserialized into directly.
///
/// `metric_prefix_blocklist` keeps the existing component name (it is an exemption list for
/// prefixing, not a list of metrics to drop). Datadog defaults for this field, including the long
/// default list from `statsd_metric_namespace_blacklist`, come from the witness drive.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct PrefixFilterConfig {
    /// The metric namespace prefix prepended to every metric name.
    ///
    /// Defaults to empty (no prefix).
    pub metric_prefix: String,

    /// Metric name prefixes exempt from namespace prefixing.
    ///
    /// Defaults to empty; Datadog defaults come from the witness drive.
    pub metric_prefix_blocklist: Vec<String>,

    /// The metric allowlist. When non-empty, only metrics matching this list are forwarded.
    ///
    /// Defaults to empty (all metrics forwarded).
    pub metric_filterlist: Vec<String>,

    /// Whether `metric_filterlist` entries match as a prefix rather than exact.
    ///
    /// Defaults to `false`.
    pub metric_filterlist_match_prefix: bool,

    /// The metric blocklist. Metrics matching this list are dropped.
    ///
    /// Defaults to empty (no metrics dropped).
    pub metric_blocklist: Vec<String>,

    /// Whether `metric_blocklist` entries match as a prefix rather than exact.
    ///
    /// Defaults to `false`.
    pub metric_blocklist_match_prefix: bool,
}
