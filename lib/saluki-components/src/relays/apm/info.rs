//! The trace-agent's `/info` discovery endpoint.
//!
//! Tracers read `/info` to learn which endpoints and features the agent supports, and how it is configured. The relay
//! builds the payload from ADP's configuration, as the trace-agent's `makeInfoHandler` builds it from its own.
//!
//! Every response the trace-agent sends carries `Datadog-Agent-State`, the SHA-256 of the `/info` body, so that a
//! tracer re-fetches `/info` when the hash changes. The relay's body can't match the trace-agent's byte for byte: Go
//! orders `feature_flags` and `span_kinds_stats_computed` by map iteration, which is random. So the relay owns the
//! header instead, and replaces the trace-agent's on every response it proxies.
//!
//! The payload has changed across Agent versions, so the relay renders the shape of the Agent it runs alongside: see
//! [`PayloadShape`].

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use datadog_agent_apm::semantics::Registry;
use datadog_agent_apm::semantics::SemanticRegistryProvider;
use datadog_agent_commons::agent_version::{self, AgentVersion};
use datadog_agent_commons::ipc::client::RunningAgentVersion;
use http::{header, HeaderMap, HeaderName, HeaderValue};
use serde::{Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use stringtheory::MetaString;

use crate::transforms::apm_stats::peer_tags::PeerTagKeys;

/// Header carrying the SHA-256 of the `/info` body.
pub(super) const AGENT_STATE_HEADER: HeaderName = HeaderName::from_static("datadog-agent-state");

/// Header carrying the version of the Agent.
pub(super) const AGENT_VERSION_HEADER: HeaderName = HeaderName::from_static("datadog-agent-version");

/// The `obfuscation_version` of the trace-agent's obfuscator (`obfuscate.Version`).
const OBFUSCATION_VERSION: u32 = 1;

/// The `obfuscation_version` reported when the effective SQL obfuscation differs from the tracers' defaults, which
/// tells tracers not to obfuscate the stats they compute themselves.
const DIVERGENT_SQL_OBFUSCATION_VERSION: u32 = 2;

/// Headers the trace-agent's EVP proxy forwards (`EvpProxyAllowedHeaders`).
///
/// The last two were added in Agent 7.84.0.
const EVP_PROXY_ALLOWED_HEADERS: &[&str] = &[
    "Content-Type",
    "Accept-Encoding",
    "Content-Encoding",
    "User-Agent",
    "DD-CI-PROVIDER-NAME",
    "DD-EVP-ORIGIN",
    "DD-EVP-ORIGIN-VERSION",
];

/// The number of [`EVP_PROXY_ALLOWED_HEADERS`] before Agent 7.84.0.
const EVP_PROXY_ALLOWED_HEADERS_BEFORE_7_84: usize = 5;

/// The parts of the payload that depend on the version of the Agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PayloadShape {
    /// Whether the EVP proxy forwards `DD-EVP-ORIGIN` and `DD-EVP-ORIGIN-VERSION` (Agent 7.84.0).
    evp_origin_headers: bool,

    /// Whether the obfuscation settings include the tag replacement rules, the credit card and SQL settings, and the
    /// JSON query settings, and whether `obfuscation_version` follows the effective SQL obfuscation (Agent 7.85.0).
    detailed_obfuscation: bool,
}

impl PayloadShape {
    /// Returns the shape for the version of the running Agent, or, while that is unknown, the version ADP was built
    /// against. With neither, the payload has the most recent shape.
    fn of(agent: &RunningAgentVersion) -> Self {
        let version = AgentVersion::parse(&agent.version).or_else(agent_version::version);
        let meets = |major, minor, patch| version.is_none_or(|version| version.meets(major, minor, patch));
        Self {
            evp_origin_headers: meets(7, 84, 0),
            detailed_obfuscation: meets(7, 85, 0),
        }
    }
}

/// Serializes a float as Go's `encoding/json` does where the two differ: an integral value has no fractional part,
/// so `10.0` is written `10`.
fn serialize_go_float<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    // Go writes integral values below 1e21 without an exponent; past `i64`'s range, the float form is close enough.
    if value.fract() == 0.0 && value.abs() < 9e18 {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}

/// A float serialized as Go's `encoding/json` does; see [`serialize_go_float`].
struct GoFloat(f64);

impl Serialize for GoFloat {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_go_float(&self.0, serializer)
    }
}

/// Span kinds the trace-agent computes stats for when stats are computed by span kind (`stats.KindsComputed`), sorted.
const SPAN_KINDS_STATS_COMPUTED: &[&str] = &["client", "consumer", "producer", "server"];

/// The endpoints the trace-agent lists, in its order (`endpoints` in `pkg/trace/api/endpoints.go`), and whether
/// each needs a setting.
///
/// Hidden endpoints, which the trace-agent serves but doesn't list, are left out.
const ENDPOINTS: &[(&str, Requires)] = &[
    ("/v0.3/traces", Requires::Nothing),
    ("/v0.3/services", Requires::Nothing),
    ("/v0.4/traces", Requires::Nothing),
    ("/v0.4/services", Requires::Nothing),
    ("/v0.5/traces", Requires::Nothing),
    ("/v0.7/traces", Requires::Nothing),
    ("/v1.0/traces", Requires::Nothing),
    ("/profiling/v1/input", Requires::Nothing),
    ("/telemetry/proxy/", Requires::Telemetry),
    ("/v0.6/stats", Requires::Nothing),
    ("/v0.1/pipeline_stats", Requires::Nothing),
    ("/openlineage/api/v1/lineage", Requires::Nothing),
    ("/evp_proxy/v1/", Requires::Nothing),
    ("/evp_proxy/v2/", Requires::Nothing),
    ("/evp_proxy/v3/", Requires::Nothing),
    ("/evp_proxy/v4/", Requires::Nothing),
    ("/debugger/v1/input", Requires::Nothing),
    ("/debugger/v1/diagnostics", Requires::Nothing),
    ("/debugger/v2/input", Requires::Nothing),
    ("/symdb/v1/input", Requires::Nothing),
    ("/dogstatsd/v1/proxy", Requires::Nothing),
    ("/dogstatsd/v2/proxy", Requires::Nothing),
    ("/tracer_flare/v1", Requires::Nothing),
    // The trace-agent attaches these two at startup, after its own list.
    ("/v0.7/config", Requires::RemoteConfiguration),
    ("/config/set", Requires::Nothing),
];

/// The setting an endpoint needs to be listed.
#[derive(Clone, Copy)]
enum Requires {
    Nothing,
    Telemetry,
    RemoteConfiguration,
}

/// The trace-agent settings `/info` reports, as the trace-agent reads them.
#[derive(Clone, Debug, Default)]
pub struct InfoConfiguration {
    /// Default trace environment, normalized (`config.default_env`).
    pub default_env: String,

    /// Target traces sampled per second (`config.target_tps`).
    pub target_traces_per_second: f64,

    /// Maximum events sampled per second (`config.max_eps`).
    pub max_events_per_second: f64,

    /// TCP port the receiver listens on, `0` when disabled (`config.receiver_port`).
    pub receiver_port: u16,

    /// Unix domain socket path the receiver listens on, empty when disabled (`config.receiver_socket`).
    pub receiver_socket: String,

    /// Receiver connection limit, `0` for none (`config.connection_limit`).
    pub connection_limit: u32,

    /// Receiver request timeout, in seconds, `0` for the default (`config.receiver_timeout`).
    pub receiver_timeout: u32,

    /// Maximum receiver request body size, in bytes (`config.max_request_bytes`).
    pub max_request_bytes: u64,

    /// DogStatsD port (`config.statsd_port`).
    pub statsd_port: u16,

    /// Memory, in bytes, the trace-agent aims to stay under, `0` for no limit (`config.max_memory`).
    pub max_memory: f64,

    /// CPU, as a fraction of a core, the trace-agent aims to stay under, `0` for no limit (`config.max_cpu`).
    pub max_cpu: f64,

    /// Event extraction rates by service and operation name (`config.analyzed_spans_by_service`).
    pub analyzed_spans_by_service: HashMap<String, HashMap<String, f64>>,

    /// Beta APM feature flags, in configuration order (`feature_flags`).
    pub features: Vec<String>,

    /// Whether the probabilistic sampler is enabled, in which case tracers must keep every trace (`client_drop_p0s`).
    pub probabilistic_sampler_enabled: bool,

    /// Whether stats are computed by span kind (`span_kinds_stats_computed`).
    pub compute_stats_by_span_kind: bool,

    /// Whether stats are aggregated by peer tags (`peer_tags`).
    pub peer_tags_aggregation: bool,

    /// Peer tags configured on top of the semantic registry's (`peer_tags`).
    pub custom_peer_tags: Vec<String>,

    /// Whether the trace-agent proxies tracer telemetry (`endpoints`).
    pub telemetry_enabled: bool,

    /// Whether Remote Configuration is enabled, in which case the trace-agent serves tracers' configuration
    /// (`endpoints`).
    pub remote_configuration_enabled: bool,

    /// Obfuscation settings (`config.obfuscation` and `obfuscation_version`).
    pub obfuscation: InfoObfuscation,

    /// Tag replacement rules (`config.obfuscation.tag_replace_rules`).
    pub replace_tags: Vec<InfoReplaceRule>,

    /// Tag rules traces are kept or dropped by (`filter_tags`).
    pub filter_tags: InfoTagFilters,

    /// Tag rules whose values are regular expressions (`filter_tags_regex`).
    pub filter_tags_regex: InfoTagFilters,

    /// Patterns for the resources whose traces are dropped (`ignore_resources`).
    pub ignore_resources: Vec<String>,
}

/// The obfuscation settings `/info` reports.
#[derive(Clone, Debug, Default)]
pub struct InfoObfuscation {
    /// Elasticsearch query obfuscation.
    pub elasticsearch: InfoJsonObfuscation,

    /// OpenSearch query obfuscation.
    pub opensearch: InfoJsonObfuscation,

    /// MongoDB query obfuscation.
    pub mongodb: InfoJsonObfuscation,

    /// Whether SQL execution plans are obfuscated.
    pub sql_exec_plan: bool,

    /// Whether SQL execution plans are normalized.
    pub sql_exec_plan_normalize: bool,

    /// SQL obfuscator mode, empty for the default obfuscator.
    pub sql_obfuscation_mode: String,

    /// Whether the query string is removed from HTTP URLs.
    pub http_remove_query_string: bool,

    /// Whether path segments containing digits are removed from HTTP URLs.
    pub http_remove_paths_with_digits: bool,

    /// Whether stack traces are removed from spans.
    pub remove_stack_traces: bool,

    /// Redis command obfuscation.
    pub redis: InfoCacheObfuscation,

    /// Valkey command obfuscation.
    pub valkey: InfoCacheObfuscation,

    /// Whether Memcached commands are obfuscated.
    pub memcached_enabled: bool,

    /// Whether the Memcached command verb is kept.
    pub memcached_keep_command: bool,

    /// Whether credit-card numbers are obfuscated.
    pub credit_cards_enabled: bool,

    /// Whether a Luhn check is applied before a value is treated as a card number.
    pub credit_cards_luhn: bool,

    /// Tag keys whose values are never treated as card numbers.
    pub credit_cards_keep_values: Vec<String>,
}

/// JSON query obfuscation settings.
#[derive(Clone, Debug, Default)]
pub struct InfoJsonObfuscation {
    /// Whether queries are obfuscated.
    pub enabled: bool,

    /// Keys whose values are kept.
    pub keep_values: Vec<String>,
}

/// Cache command obfuscation settings.
#[derive(Clone, Debug, Default)]
pub struct InfoCacheObfuscation {
    /// Whether commands are obfuscated.
    pub enabled: bool,

    /// Whether every command argument is removed.
    pub remove_all_args: bool,
}

/// A tag replacement rule.
#[derive(Clone, Debug, Default)]
pub struct InfoReplaceRule {
    /// Tag the rule targets.
    pub name: String,

    /// Pattern the rule matches.
    pub pattern: String,

    /// Replacement for each match.
    pub repl: String,
}

/// Tag rules that keep or drop a trace.
#[derive(Clone, Debug, Default)]
pub struct InfoTagFilters {
    /// Rules a trace must match to be kept.
    pub require: Vec<InfoTagFilter>,

    /// Rules that drop a trace that matches them.
    pub reject: Vec<InfoTagFilter>,
}

/// A tag rule.
#[derive(Clone, Debug, Default)]
pub struct InfoTagFilter {
    /// Tag key the rule matches.
    pub key: String,

    /// Value, or value pattern, the tag must have; `None` matches any value.
    pub value: Option<String>,
}

impl InfoTagFilter {
    /// Formats the rule as the trace-agent reports it: `key:value`, or `key` with no value.
    fn render(&self) -> String {
        match &self.value {
            Some(value) => format!("{}:{value}", self.key),
            None => self.key.clone(),
        }
    }
}

/// A rendered `/info` body, with the headers derived from it.
pub(super) struct RenderedInfo {
    body: Bytes,
    state: HeaderValue,
    version: HeaderValue,
}

impl RenderedInfo {
    fn render(config: &InfoConfiguration, registry: &Registry, agent: &RunningAgentVersion) -> Self {
        let payload = InfoPayload::new(config, registry, agent, PayloadShape::of(agent));

        // The trace-agent indents with tabs; only the hash, not the layout, matters to tracers.
        let mut body = Vec::new();
        let mut serializer =
            serde_json::Serializer::with_formatter(&mut body, serde_json::ser::PrettyFormatter::with_indent(b"\t"));
        payload
            .serialize(&mut serializer)
            .expect("the info payload only holds serializable values");

        let state = faster_hex::hex_string(&Sha256::digest(&body));
        Self {
            body: Bytes::from(body),
            state: HeaderValue::from_str(&state).expect("a hex digest is a valid header value"),
            version: HeaderValue::from_str(&agent.version).unwrap_or_else(|_| HeaderValue::from_static("")),
        }
    }
}

/// The `/info` endpoint: its payload, and the headers every other response carries.
///
/// Clones share the payload.
#[derive(Clone)]
pub(super) struct Info {
    config: Arc<InfoConfiguration>,
    semantic_registry: SemanticRegistryProvider,
    agent: Arc<ArcSwap<RunningAgentVersion>>,
    rendered: Arc<ArcSwap<RenderedInfo>>,
}

impl Info {
    /// Renders the payload of `config` with the current semantic registry, before the Agent version is known.
    pub(super) fn new(config: InfoConfiguration, semantic_registry: SemanticRegistryProvider) -> Self {
        let agent = RunningAgentVersion::default();
        let rendered = RenderedInfo::render(&config, &semantic_registry.snapshot(), &agent);
        Self {
            config: Arc::new(config),
            semantic_registry,
            agent: Arc::new(ArcSwap::from_pointee(agent)),
            rendered: Arc::new(ArcSwap::from_pointee(rendered)),
        }
    }

    /// Re-renders the payload with the version of the Agent.
    pub(super) fn set_agent_version(&self, agent: RunningAgentVersion) {
        self.agent.store(Arc::new(agent));
        self.rerender();
    }

    fn rerender(&self) {
        let agent = self.agent.load();
        let rendered = RenderedInfo::render(&self.config, &self.semantic_registry.snapshot(), &agent);
        self.rendered.store(Arc::new(rendered));
    }

    /// Answers `GET /info` with the current payload.
    ///
    /// As in the trace-agent, the response doesn't carry `Datadog-Agent-State` itself.
    pub(super) fn response(&self) -> Response {
        let rendered = self.rendered.load();
        ([(header::CONTENT_TYPE, "application/json")], rendered.body.clone()).into_response()
    }

    /// Sets `Datadog-Agent-State` and `Datadog-Agent-Version` on a response the relay served itself.
    pub(super) fn set_headers(&self, headers: &mut HeaderMap) {
        let rendered = self.rendered.load();
        headers.insert(AGENT_STATE_HEADER, rendered.state.clone());
        headers.insert(AGENT_VERSION_HEADER, rendered.version.clone());
    }

    /// Replaces `Datadog-Agent-State` and `Datadog-Agent-Version` on a response proxied from the trace-agent.
    ///
    /// Only a header the trace-agent set is replaced, so a response the trace-agent sent without them, such as for an
    /// endpoint it doesn't serve, stays without them.
    pub(super) fn replace_headers(&self, headers: &mut HeaderMap) {
        let rendered = self.rendered.load();
        if headers.contains_key(AGENT_STATE_HEADER) {
            headers.insert(AGENT_STATE_HEADER, rendered.state.clone());
        }
        if headers.contains_key(AGENT_VERSION_HEADER) {
            headers.insert(AGENT_VERSION_HEADER, rendered.version.clone());
        }
    }
}

/// The `/info` payload, with the trace-agent's field names and order (`infoPayload`).
#[derive(Serialize)]
struct InfoPayload<'a> {
    version: &'a str,
    git_commit: &'a str,
    endpoints: Vec<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    feature_flags: Vec<&'a str>,
    client_drop_p0s: bool,
    span_meta_structs: bool,
    long_running_spans: bool,
    span_events: bool,
    evp_proxy_allowed_headers: &'static [&'static str],
    config: ReducedConfig<'a>,
    peer_tags: Option<Vec<String>>,
    span_kinds_stats_computed: Option<&'static [&'static str]>,
    obfuscation_version: u32,
    filter_tags: RenderedTagFilters,
    filter_tags_regex: RenderedTagFilters,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    ignore_resources: &'a [String],
}

impl<'a> InfoPayload<'a> {
    fn new(
        config: &'a InfoConfiguration, registry: &Registry, agent: &'a RunningAgentVersion, shape: PayloadShape,
    ) -> Self {
        let endpoints: Vec<&'static str> = ENDPOINTS
            .iter()
            .filter(|(_, requires)| match requires {
                Requires::Nothing => true,
                Requires::Telemetry => config.telemetry_enabled,
                Requires::RemoteConfiguration => config.remote_configuration_enabled,
            })
            .map(|(pattern, _)| *pattern)
            .collect();

        // The trace-agent reads the features into a set.
        let mut feature_flags: Vec<&str> = Vec::with_capacity(config.features.len());
        for feature in &config.features {
            if !feature_flags.contains(&feature.as_str()) {
                feature_flags.push(feature);
            }
        }

        let sql = EffectiveSql::new(config);
        let obfuscation_version = if shape.detailed_obfuscation && sql.diverges_from_tracers() {
            DIVERGENT_SQL_OBFUSCATION_VERSION
        } else {
            OBFUSCATION_VERSION
        };

        let peer_tags = config.peer_tags_aggregation.then(|| {
            let custom: Vec<MetaString> = config
                .custom_peer_tags
                .iter()
                .map(|tag| MetaString::from(tag.as_str()))
                .collect();
            let keys = PeerTagKeys::build(registry, &custom);
            keys.keys().iter().map(|key| key.to_string()).collect::<Vec<_>>()
        });

        Self {
            version: &agent.version,
            git_commit: &agent.commit,
            // The trace-agent lets tracers drop unsampled traces when it computes stats, unless the probabilistic
            // sampler needs to see every trace.
            client_drop_p0s: !config.probabilistic_sampler_enabled && endpoints.contains(&"/v0.6/stats"),
            endpoints,
            feature_flags,
            span_meta_structs: true,
            long_running_spans: true,
            span_events: true,
            evp_proxy_allowed_headers: if shape.evp_origin_headers {
                EVP_PROXY_ALLOWED_HEADERS
            } else {
                &EVP_PROXY_ALLOWED_HEADERS[..EVP_PROXY_ALLOWED_HEADERS_BEFORE_7_84]
            },
            config: ReducedConfig::new(config, sql, shape),
            // The trace-agent reports no peer tags as `null`, not as an empty list.
            peer_tags: peer_tags.filter(|tags| !tags.is_empty()),
            span_kinds_stats_computed: config.compute_stats_by_span_kind.then_some(SPAN_KINDS_STATS_COMPUTED),
            obfuscation_version,
            filter_tags: RenderedTagFilters::new(&config.filter_tags),
            filter_tags_regex: RenderedTagFilters::new(&config.filter_tags_regex),
            ignore_resources: &config.ignore_resources,
        }
    }
}

/// The SQL obfuscation the trace-agent applies (`EffectiveSQLConfig`), which beta feature flags select rather than
/// the `apm_config.obfuscation.sql` settings.
struct EffectiveSql {
    replace_digits: bool,
    keep_sql_alias: bool,
    dollar_quoted_func: bool,
    obfuscation_mode: String,
}

impl EffectiveSql {
    fn new(config: &InfoConfiguration) -> Self {
        let has = |feature: &str| config.features.iter().any(|enabled| enabled == feature);
        // An explicit mode wins; otherwise the `sqllexer` feature selects the `sqllexer` obfuscator.
        let obfuscation_mode = if !config.obfuscation.sql_obfuscation_mode.is_empty() {
            config.obfuscation.sql_obfuscation_mode.clone()
        } else if has("sqllexer") {
            "obfuscate_only".to_string()
        } else {
            String::new()
        };
        Self {
            replace_digits: has("quantize_sql_tables") || has("replace_sql_digits"),
            keep_sql_alias: has("keep_sql_alias"),
            dollar_quoted_func: has("dollar_quoted_func"),
            obfuscation_mode,
        }
    }

    /// Returns whether this SQL obfuscation changes the obfuscated queries from those tracers produce, which the
    /// trace-agent signals with `obfuscation_version`.
    ///
    /// Table name collection is left out: it only collects metadata, which stats don't keep.
    fn diverges_from_tracers(&self) -> bool {
        self.replace_digits || self.keep_sql_alias || self.dollar_quoted_func || !self.obfuscation_mode.is_empty()
    }
}

/// The trace-agent's `reducedConfig`.
#[derive(Serialize)]
struct ReducedConfig<'a> {
    default_env: &'a str,
    #[serde(serialize_with = "serialize_go_float")]
    target_tps: f64,
    #[serde(serialize_with = "serialize_go_float")]
    max_eps: f64,
    receiver_port: u16,
    receiver_socket: &'a str,
    connection_limit: u32,
    receiver_timeout: u32,
    max_request_bytes: u64,
    statsd_port: u16,
    #[serde(serialize_with = "serialize_go_float")]
    max_memory: f64,
    #[serde(serialize_with = "serialize_go_float")]
    max_cpu: f64,
    // Go sorts map keys, and a stable order keeps the hash stable.
    analyzed_spans_by_service: BTreeMap<&'a str, BTreeMap<&'a str, GoFloat>>,
    obfuscation: ReducedObfuscation<'a>,
}

impl<'a> ReducedConfig<'a> {
    fn new(config: &'a InfoConfiguration, sql: EffectiveSql, shape: PayloadShape) -> Self {
        Self {
            default_env: &config.default_env,
            target_tps: config.target_traces_per_second,
            max_eps: config.max_events_per_second,
            receiver_port: config.receiver_port,
            receiver_socket: &config.receiver_socket,
            connection_limit: config.connection_limit,
            receiver_timeout: config.receiver_timeout,
            max_request_bytes: config.max_request_bytes,
            statsd_port: config.statsd_port,
            max_memory: config.max_memory,
            max_cpu: config.max_cpu,
            analyzed_spans_by_service: config
                .analyzed_spans_by_service
                .iter()
                .map(|(service, rates)| {
                    let rates = rates
                        .iter()
                        .map(|(operation, rate)| (operation.as_str(), GoFloat(*rate)))
                        .collect();
                    (service.as_str(), rates)
                })
                .collect(),
            obfuscation: ReducedObfuscation::new(config, sql, shape),
        }
    }
}

/// The trace-agent's `reducedObfuscationConfig`.
///
/// The fields that are `None` without [`PayloadShape::detailed_obfuscation`] are left out.
#[derive(Serialize)]
struct ReducedObfuscation<'a> {
    elastic_search: bool,
    mongo: bool,
    sql_exec_plan: bool,
    sql_exec_plan_normalize: bool,
    sql_obfuscation_mode: String,
    // No rules are reported as `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    tag_replace_rules: Option<Option<Vec<ReducedReplaceRule<'a>>>>,
    http: ReducedHttp,
    remove_stack_traces: bool,
    redis: ReducedCache,
    valkey: ReducedCache,
    memcached: ReducedMemcached,
    #[serde(skip_serializing_if = "Option::is_none")]
    credit_cards: Option<ReducedCreditCards<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sql: Option<ReducedSql>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elasticsearch: Option<ReducedJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    opensearch: Option<ReducedJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mongodb: Option<ReducedJson<'a>>,
}

impl<'a> ReducedObfuscation<'a> {
    fn new(config: &'a InfoConfiguration, sql: EffectiveSql, shape: PayloadShape) -> Self {
        let obfuscation = &config.obfuscation;
        let detailed = shape.detailed_obfuscation;
        let tag_replace_rules = (!config.replace_tags.is_empty()).then(|| {
            config
                .replace_tags
                .iter()
                .map(|rule| ReducedReplaceRule {
                    name: &rule.name,
                    pattern: &rule.pattern,
                    repl: &rule.repl,
                })
                .collect()
        });
        Self {
            elastic_search: obfuscation.elasticsearch.enabled,
            mongo: obfuscation.mongodb.enabled,
            sql_exec_plan: obfuscation.sql_exec_plan,
            sql_exec_plan_normalize: obfuscation.sql_exec_plan_normalize,
            sql_obfuscation_mode: sql.obfuscation_mode.clone(),
            tag_replace_rules: detailed.then_some(tag_replace_rules),
            http: ReducedHttp {
                remove_query_string: obfuscation.http_remove_query_string,
                remove_path_digits: obfuscation.http_remove_paths_with_digits,
            },
            remove_stack_traces: obfuscation.remove_stack_traces,
            redis: ReducedCache::new(&obfuscation.redis),
            valkey: ReducedCache::new(&obfuscation.valkey),
            memcached: ReducedMemcached {
                enabled: obfuscation.memcached_enabled,
                keep_command: obfuscation.memcached_keep_command,
            },
            credit_cards: detailed.then(|| ReducedCreditCards {
                enabled: obfuscation.credit_cards_enabled,
                luhn: obfuscation.credit_cards_luhn,
                keep_values: &obfuscation.credit_cards_keep_values,
            }),
            sql: detailed.then(|| ReducedSql::new(sql)),
            elasticsearch: detailed.then(|| ReducedJson::new(&obfuscation.elasticsearch)),
            opensearch: detailed.then(|| ReducedJson::new(&obfuscation.opensearch)),
            mongodb: detailed.then(|| ReducedJson::new(&obfuscation.mongodb)),
        }
    }
}

#[derive(Serialize)]
struct ReducedReplaceRule<'a> {
    name: &'a str,
    pattern: &'a str,
    repl: &'a str,
}

#[derive(Serialize)]
struct ReducedHttp {
    remove_query_string: bool,
    remove_path_digits: bool,
}

#[derive(Serialize)]
struct ReducedCache {
    enabled: bool,
    remove_all_args: bool,
}

impl ReducedCache {
    fn new(cache: &InfoCacheObfuscation) -> Self {
        Self {
            enabled: cache.enabled,
            remove_all_args: cache.remove_all_args,
        }
    }
}

#[derive(Serialize)]
struct ReducedMemcached {
    enabled: bool,
    keep_command: bool,
}

#[derive(Serialize)]
struct ReducedCreditCards<'a> {
    enabled: bool,
    luhn: bool,
    keep_values: &'a [String],
}

/// The trace-agent's `reducedSQLConfig`. The settings no feature flag selects are always off.
#[derive(Serialize)]
struct ReducedSql {
    replace_digits: bool,
    keep_sql_alias: bool,
    dollar_quoted_func: bool,
    keep_null: bool,
    keep_boolean: bool,
    keep_positional_parameter: bool,
    keep_trailing_semicolon: bool,
    keep_identifier_quotation: bool,
    replace_bind_parameter: bool,
    remove_space_between_parentheses: bool,
    keep_json_path: bool,
    obfuscation_mode: String,
}

impl ReducedSql {
    fn new(sql: EffectiveSql) -> Self {
        Self {
            replace_digits: sql.replace_digits,
            keep_sql_alias: sql.keep_sql_alias,
            dollar_quoted_func: sql.dollar_quoted_func,
            keep_null: false,
            keep_boolean: false,
            keep_positional_parameter: false,
            keep_trailing_semicolon: false,
            keep_identifier_quotation: false,
            replace_bind_parameter: false,
            remove_space_between_parentheses: false,
            keep_json_path: false,
            obfuscation_mode: sql.obfuscation_mode,
        }
    }
}

#[derive(Serialize)]
struct ReducedJson<'a> {
    enabled: bool,
    keep_keys: &'a [String],
}

impl<'a> ReducedJson<'a> {
    fn new(json: &'a InfoJsonObfuscation) -> Self {
        Self {
            enabled: json.enabled,
            keep_keys: &json.keep_values,
        }
    }
}

/// The trace-agent's `filterTags`.
#[derive(Serialize)]
struct RenderedTagFilters {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    require: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reject: Vec<String>,
}

impl RenderedTagFilters {
    fn new(filters: &InfoTagFilters) -> Self {
        Self {
            require: filters.require.iter().map(InfoTagFilter::render).collect(),
            reject: filters.reject.iter().map(InfoTagFilter::render).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use datadog_agent_apm::semantics::EMBEDDED_REGISTRY;
    use serde_json::{json, Value};

    use super::*;

    fn agent_at(version: &str) -> RunningAgentVersion {
        RunningAgentVersion {
            version: version.to_string(),
            commit: "abc1234".to_string(),
        }
    }

    fn agent() -> RunningAgentVersion {
        agent_at("7.85.0")
    }

    fn render_for(config: &InfoConfiguration, agent: &RunningAgentVersion) -> (Value, RenderedInfo) {
        let rendered = RenderedInfo::render(config, &EMBEDDED_REGISTRY, agent);
        let payload = serde_json::from_slice(&rendered.body).expect("the body should be JSON");
        (payload, rendered)
    }

    fn render(config: &InfoConfiguration) -> (Value, RenderedInfo) {
        render_for(config, &agent())
    }

    fn endpoints(payload: &Value) -> Vec<&str> {
        payload["endpoints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|endpoint| endpoint.as_str().unwrap())
            .collect()
    }

    #[test]
    fn the_state_is_the_hash_of_the_body_and_is_stable() {
        let config = InfoConfiguration {
            analyzed_spans_by_service: HashMap::from([
                (
                    "b".to_string(),
                    HashMap::from([("y".to_string(), 0.5), ("x".to_string(), 1.0)]),
                ),
                ("a".to_string(), HashMap::from([("z".to_string(), 0.1)])),
            ]),
            ..Default::default()
        };
        let (_, rendered) = render(&config);

        assert_eq!(
            rendered.state.to_str().unwrap(),
            faster_hex::hex_string(&Sha256::digest(&rendered.body))
        );
        assert_eq!(rendered.version, "7.85.0");
        // Map keys are sorted, so rendering the same settings again yields the same hash.
        for _ in 0..10 {
            assert_eq!(render(&config).1.state, rendered.state);
        }
    }

    #[test]
    fn reports_the_agent_and_the_fixed_capabilities() {
        let (payload, _) = render(&InfoConfiguration::default());

        assert_eq!(payload["version"], "7.85.0");
        assert_eq!(payload["git_commit"], "abc1234");
        assert_eq!(payload["span_meta_structs"], true);
        assert_eq!(payload["long_running_spans"], true);
        assert_eq!(payload["span_events"], true);
        assert_eq!(payload["evp_proxy_allowed_headers"][0], "Content-Type");
        assert_eq!(payload["obfuscation_version"], 1);
        // Empty lists the trace-agent omits, or reports as `null`.
        for omitted in ["feature_flags", "ignore_resources", "org_prop_marker"] {
            assert!(payload.get(omitted).is_none(), "{omitted}");
        }
        for null in ["peer_tags", "span_kinds_stats_computed"] {
            assert!(payload[null].is_null(), "{null}");
        }
        assert!(payload["config"]["obfuscation"]["tag_replace_rules"].is_null());
        assert_eq!(payload["filter_tags"], json!({}));
        assert_eq!(payload["filter_tags_regex"], json!({}));
    }

    #[test]
    fn the_payload_has_the_shape_of_the_agent_version() {
        let config = InfoConfiguration {
            features: vec!["keep_sql_alias".to_string()],
            ..Default::default()
        };

        // Agent 7.83: five EVP headers, no detailed obfuscation, and an `obfuscation_version` that never changes.
        let (payload, _) = render_for(&config, &agent_at("7.83.0"));
        assert_eq!(payload["evp_proxy_allowed_headers"].as_array().unwrap().len(), 5);
        let obfuscation = payload["config"]["obfuscation"].as_object().unwrap();
        for absent in [
            "tag_replace_rules",
            "credit_cards",
            "sql",
            "elasticsearch",
            "opensearch",
            "mongodb",
        ] {
            assert!(!obfuscation.contains_key(absent), "{absent}");
        }
        assert_eq!(payload["obfuscation_version"], 1);

        // Agent 7.84 adds the EVP origin headers.
        let (payload, _) = render_for(&config, &agent_at("7.84.1"));
        assert_eq!(payload["evp_proxy_allowed_headers"].as_array().unwrap().len(), 7);
        assert!(payload["config"]["obfuscation"].get("sql").is_none());

        // Agent 7.85, and development builds, add the detailed obfuscation.
        for version in ["7.85.0", "7.83.0-devel+git.1.abc1234"] {
            let (payload, _) = render_for(&config, &agent_at(version));
            let obfuscation = payload["config"]["obfuscation"].as_object().unwrap();
            assert!(obfuscation["tag_replace_rules"].is_null(), "{version}");
            assert!(obfuscation.contains_key("sql"), "{version}");
            assert_eq!(payload["obfuscation_version"], 2, "{version}");
        }
    }

    #[test]
    fn integral_floats_are_written_as_go_writes_them() {
        let (_, rendered) = render(&InfoConfiguration {
            target_traces_per_second: 10.0,
            max_memory: 5e8,
            max_cpu: 0.5,
            analyzed_spans_by_service: HashMap::from([("web".to_string(), HashMap::from([("op".to_string(), 1.0)]))]),
            ..Default::default()
        });
        let body = std::str::from_utf8(&rendered.body).unwrap();

        assert!(body.contains("\"target_tps\": 10,"), "{body}");
        assert!(body.contains("\"max_memory\": 500000000,"), "{body}");
        assert!(body.contains("\"max_cpu\": 0.5,"), "{body}");
        assert!(body.contains("\"op\": 1\n"), "{body}");
    }

    #[test]
    fn lists_endpoints_that_need_a_setting_only_with_it() {
        let (payload, _) = render(&InfoConfiguration::default());
        let listed = endpoints(&payload);
        assert!(listed.contains(&"/v0.4/traces"));
        assert!(listed.contains(&"/tracer_flare/v1"));
        assert!(!listed.contains(&"/telemetry/proxy/"));
        assert!(!listed.contains(&"/v0.7/config"));
        // Hidden endpoints are served but not listed.
        assert!(!listed.contains(&"/v0.2/traces"));

        let (payload, _) = render(&InfoConfiguration {
            telemetry_enabled: true,
            remote_configuration_enabled: true,
            ..Default::default()
        });
        let listed = endpoints(&payload);
        assert!(listed.contains(&"/telemetry/proxy/"));
        assert_eq!(listed[listed.len() - 2..], ["/v0.7/config", "/config/set"]);
    }

    #[test]
    fn tracers_may_drop_unsampled_traces_unless_the_probabilistic_sampler_runs() {
        let (payload, _) = render(&InfoConfiguration::default());
        assert_eq!(payload["client_drop_p0s"], true);

        let (payload, _) = render(&InfoConfiguration {
            probabilistic_sampler_enabled: true,
            ..Default::default()
        });
        assert_eq!(payload["client_drop_p0s"], false);
    }

    #[test]
    fn peer_tags_are_reported_with_peer_tags_aggregation() {
        let (payload, _) = render(&InfoConfiguration {
            peer_tags_aggregation: true,
            custom_peer_tags: vec!["zz.custom".to_string(), "peer.service".to_string()],
            ..Default::default()
        });

        let tags: Vec<&str> = payload["peer_tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tag| tag.as_str().unwrap())
            .collect();
        assert!(tags.contains(&"peer.service"));
        assert!(tags.contains(&"zz.custom"));
        let mut sorted = tags.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(tags, sorted);
    }

    #[test]
    fn span_kinds_are_reported_when_stats_are_computed_by_span_kind() {
        let (payload, _) = render(&InfoConfiguration {
            compute_stats_by_span_kind: true,
            ..Default::default()
        });
        assert_eq!(
            payload["span_kinds_stats_computed"],
            json!(["client", "consumer", "producer", "server"])
        );
    }

    #[test]
    fn feature_flags_select_the_reported_sql_obfuscation() {
        let (payload, _) = render(&InfoConfiguration {
            features: vec![
                "sqllexer".to_string(),
                "keep_sql_alias".to_string(),
                "sqllexer".to_string(),
                "table_names".to_string(),
            ],
            ..Default::default()
        });

        assert_eq!(
            payload["feature_flags"],
            json!(["sqllexer", "keep_sql_alias", "table_names"])
        );
        let obfuscation = &payload["config"]["obfuscation"];
        assert_eq!(obfuscation["sql_obfuscation_mode"], "obfuscate_only");
        assert_eq!(obfuscation["sql"]["obfuscation_mode"], "obfuscate_only");
        assert_eq!(obfuscation["sql"]["keep_sql_alias"], true);
        assert_eq!(obfuscation["sql"]["replace_digits"], false);
        assert_eq!(payload["obfuscation_version"], 2);

        // An explicit mode wins over `sqllexer`; table name collection alone keeps tracers' obfuscation.
        let (payload, _) = render(&InfoConfiguration {
            features: vec!["sqllexer".to_string()],
            obfuscation: InfoObfuscation {
                sql_obfuscation_mode: "normalize_only".to_string(),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(
            payload["config"]["obfuscation"]["sql_obfuscation_mode"],
            "normalize_only"
        );
        let (payload, _) = render(&InfoConfiguration {
            features: vec!["table_names".to_string()],
            ..Default::default()
        });
        assert_eq!(payload["obfuscation_version"], 1);
    }

    #[test]
    fn reports_the_reduced_configuration() {
        let (payload, _) = render(&InfoConfiguration {
            default_env: "prod".to_string(),
            target_traces_per_second: 10.0,
            max_events_per_second: 200.0,
            receiver_port: 8126,
            receiver_socket: "/var/run/datadog/apm.socket".to_string(),
            connection_limit: 2000,
            receiver_timeout: 5,
            max_request_bytes: 1024,
            statsd_port: 8125,
            max_memory: 5e8,
            max_cpu: 0.5,
            replace_tags: vec![InfoReplaceRule {
                name: "*".to_string(),
                pattern: "secret".to_string(),
                repl: "?".to_string(),
            }],
            obfuscation: InfoObfuscation {
                elasticsearch: InfoJsonObfuscation {
                    enabled: true,
                    keep_values: vec!["index".to_string()],
                },
                http_remove_paths_with_digits: true,
                memcached_keep_command: true,
                credit_cards_keep_values: vec!["card".to_string()],
                ..Default::default()
            },
            ..Default::default()
        });

        let config = &payload["config"];
        assert_eq!(config["default_env"], "prod");
        assert_eq!(config["target_tps"], 10.0);
        assert_eq!(config["max_eps"], 200.0);
        assert_eq!(config["receiver_port"], 8126);
        assert_eq!(config["receiver_socket"], "/var/run/datadog/apm.socket");
        assert_eq!(config["connection_limit"], 2000);
        assert_eq!(config["receiver_timeout"], 5);
        assert_eq!(config["max_request_bytes"], 1024);
        assert_eq!(config["statsd_port"], 8125);
        assert_eq!(config["max_memory"], 5e8);
        assert_eq!(config["max_cpu"], 0.5);
        let obfuscation = &config["obfuscation"];
        assert_eq!(
            obfuscation["tag_replace_rules"],
            json!([{ "name": "*", "pattern": "secret", "repl": "?" }])
        );
        assert_eq!(obfuscation["elastic_search"], true);
        assert_eq!(
            obfuscation["elasticsearch"],
            json!({ "enabled": true, "keep_keys": ["index"] })
        );
        assert_eq!(obfuscation["mongo"], false);
        assert_eq!(obfuscation["http"]["remove_path_digits"], true);
        assert_eq!(obfuscation["memcached"]["keep_command"], true);
        assert_eq!(obfuscation["credit_cards"]["keep_values"], json!(["card"]));
    }

    #[test]
    fn tag_filters_are_reported_as_the_trace_agent_formats_them() {
        let rule = |key: &str, value: Option<&str>| InfoTagFilter {
            key: key.to_string(),
            value: value.map(str::to_string),
        };
        let (payload, _) = render(&InfoConfiguration {
            filter_tags: InfoTagFilters {
                require: vec![rule("env", Some("prod")), rule("team", None)],
                reject: Vec::new(),
            },
            filter_tags_regex: InfoTagFilters {
                require: Vec::new(),
                reject: vec![rule("debug", Some("^t")), rule("any", Some(""))],
            },
            ignore_resources: vec!["GET /ping".to_string()],
            ..Default::default()
        });

        assert_eq!(payload["filter_tags"], json!({ "require": ["env:prod", "team"] }));
        assert_eq!(payload["filter_tags_regex"], json!({ "reject": ["debug:^t", "any:"] }));
        assert_eq!(payload["ignore_resources"], json!(["GET /ping"]));
    }

    #[test]
    fn the_agent_version_rerenders_the_payload_and_its_headers() {
        let info = Info::new(InfoConfiguration::default(), SemanticRegistryProvider::default());
        let mut before = HeaderMap::new();
        info.set_headers(&mut before);
        assert_eq!(before[AGENT_VERSION_HEADER], "");

        info.set_agent_version(agent());

        let mut after = HeaderMap::new();
        info.set_headers(&mut after);
        assert_eq!(after[AGENT_VERSION_HEADER], "7.85.0");
        assert_ne!(after[AGENT_STATE_HEADER], before[AGENT_STATE_HEADER]);
    }

    #[test]
    fn only_headers_the_trace_agent_set_are_replaced() {
        let info = Info::new(InfoConfiguration::default(), SemanticRegistryProvider::default());

        let mut headers = HeaderMap::new();
        info.replace_headers(&mut headers);
        assert!(headers.is_empty());

        headers.insert(AGENT_STATE_HEADER, HeaderValue::from_static("trace-agent-hash"));
        headers.insert(AGENT_VERSION_HEADER, HeaderValue::from_static("7.0.0"));
        info.replace_headers(&mut headers);
        let mut expected = HeaderMap::new();
        info.set_headers(&mut expected);
        assert_eq!(headers, expected);
    }
}
