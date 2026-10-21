//! Bucket-tolerant comparison of APM stats payloads.

use std::{collections::BTreeMap, fmt};

use datadog_protos::{
    sketches::DDSketch as ProtoDDSketch,
    traces::{ClientGroupedStats, ClientStatsBucket, ClientStatsPayload, StatsPayload, Trilean},
};
use ddsketch::canonical::DDSketch;
use protobuf::{EnumOrUnknown, Message as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;
use stele::EncodedApmStatsDDSketch;

use super::{
    protojson::{base64_bytes, i64_str, u64_str},
    CompareOptions, Mismatches,
};

// --- Golden `protojson` model -------------------------------------------------------------------------------------

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct GoldenStatsPayload {
    agent_hostname: String,
    agent_env: String,
    stats: Vec<GoldenClientStatsPayload>,
    agent_version: String,
    client_computed: bool,
    split_payload: bool,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct GoldenClientStatsPayload {
    hostname: String,
    env: String,
    version: String,
    stats: Vec<GoldenBucket>,
    lang: String,
    tracer_version: String,
    #[serde(rename = "runtimeID")]
    runtime_id: String,
    #[serde(deserialize_with = "u64_str")]
    sequence: u64,
    agent_aggregation: String,
    service: String,
    #[serde(rename = "containerID")]
    container_id: String,
    tags: Vec<String>,
    git_commit_sha: String,
    image_tag: String,
    #[serde(deserialize_with = "u64_str")]
    process_tags_hash: u64,
    process_tags: String,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct GoldenBucket {
    /// Bucket start; the golden files replace it with the `"<bucket>"` placeholder, and it is ignored.
    #[serde(rename = "start")]
    _start: serde_json::Value,
    #[serde(deserialize_with = "u64_str")]
    duration: u64,
    stats: Vec<GoldenGroupedStats>,
    #[serde(deserialize_with = "i64_str")]
    agent_time_shift: i64,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct GoldenGroupedStats {
    service: String,
    name: String,
    resource: String,
    #[serde(rename = "HTTPStatusCode")]
    http_status_code: u32,
    #[serde(rename = "type")]
    span_type: String,
    #[serde(rename = "DBType")]
    db_type: String,
    #[serde(deserialize_with = "u64_str")]
    hits: u64,
    #[serde(deserialize_with = "u64_str")]
    errors: u64,
    #[serde(deserialize_with = "u64_str")]
    duration: u64,
    #[serde(deserialize_with = "base64_bytes")]
    ok_summary: Vec<u8>,
    #[serde(deserialize_with = "base64_bytes")]
    error_summary: Vec<u8>,
    synthetics: bool,
    #[serde(deserialize_with = "u64_str")]
    top_level_hits: u64,
    span_kind: String,
    peer_tags: Vec<String>,
    is_trace_root: Option<String>,
    #[serde(rename = "GRPCStatusCode")]
    grpc_status_code: String,
    #[serde(rename = "HTTPMethod")]
    http_method: String,
    #[serde(rename = "HTTPEndpoint")]
    http_endpoint: String,
    service_source: String,
    span_derived_primary_tags: Vec<String>,
    additional_metric_tags: Vec<String>,
}

fn trilean_from_name(name: Option<&str>) -> Result<Trilean, GenericError> {
    match name.unwrap_or("NOT_SET") {
        "NOT_SET" => Ok(Trilean::NOT_SET),
        "TRUE" => Ok(Trilean::TRUE),
        "FALSE" => Ok(Trilean::FALSE),
        other => Err(generic_error!("Unknown isTraceRoot value '{}'.", other)),
    }
}

impl GoldenStatsPayload {
    /// Converts the golden `protojson` model into the protobuf message. Bucket starts are set to `0`.
    pub(super) fn into_proto(self) -> Result<StatsPayload, GenericError> {
        let mut p = StatsPayload::new();
        p.agentHostname = self.agent_hostname;
        p.agentEnv = self.agent_env;
        p.agentVersion = self.agent_version;
        p.clientComputed = self.client_computed;
        p.splitPayload = self.split_payload;
        for c in self.stats {
            let mut cp = ClientStatsPayload::new();
            cp.hostname = c.hostname;
            cp.env = c.env;
            cp.version = c.version;
            cp.lang = c.lang;
            cp.tracerVersion = c.tracer_version;
            cp.runtimeID = c.runtime_id;
            cp.sequence = c.sequence;
            cp.agentAggregation = c.agent_aggregation;
            cp.service = c.service;
            cp.containerID = c.container_id;
            cp.tags = c.tags;
            cp.git_commit_sha = c.git_commit_sha;
            cp.image_tag = c.image_tag;
            cp.process_tags_hash = c.process_tags_hash;
            cp.process_tags = c.process_tags;
            for b in c.stats {
                let mut bucket = ClientStatsBucket::new();
                bucket.duration = b.duration;
                bucket.agentTimeShift = b.agent_time_shift;
                for g in b.stats {
                    let mut gs = ClientGroupedStats::new();
                    gs.service = g.service;
                    gs.name = g.name;
                    gs.resource = g.resource;
                    gs.HTTP_status_code = g.http_status_code;
                    gs.type_ = g.span_type;
                    gs.DB_type = g.db_type;
                    gs.hits = g.hits;
                    gs.errors = g.errors;
                    gs.duration = g.duration;
                    gs.okSummary = g.ok_summary;
                    gs.errorSummary = g.error_summary;
                    gs.synthetics = g.synthetics;
                    gs.topLevelHits = g.top_level_hits;
                    gs.span_kind = g.span_kind;
                    gs.peer_tags = g.peer_tags;
                    gs.is_trace_root = EnumOrUnknown::new(trilean_from_name(g.is_trace_root.as_deref())?);
                    gs.GRPC_status_code = g.grpc_status_code;
                    gs.HTTP_method = g.http_method;
                    gs.HTTP_endpoint = g.http_endpoint;
                    gs.service_source = g.service_source;
                    gs.span_derived_primary_tags = g.span_derived_primary_tags;
                    gs.additional_metric_tags = g.additional_metric_tags;
                    bucket.stats.push(gs);
                }
                cp.stats.push(bucket);
            }
            p.stats.push(cp);
        }
        Ok(p)
    }
}

// --- Semantic model ---------------------------------------------------------------------------------------------

/// The dimensions that identify one aggregated stats group.
///
/// Holds every agent-level, client-payload-level, and group-level dimension of a stats group, plus the bucket
/// duration. The bucket start, `agentTimeShift`, and `sequence` are deliberately excluded, so a group's values may be
/// spread over any number of buckets and payloads.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GroupKey {
    /// `StatsPayload.agentHostname`.
    pub agent_hostname: String,
    /// `StatsPayload.agentEnv`.
    pub agent_env: String,
    /// `StatsPayload.agentVersion` (blanked when ignored).
    pub agent_version: String,
    /// `StatsPayload.clientComputed`.
    pub client_computed: bool,
    /// `StatsPayload.splitPayload`.
    pub split_payload: bool,

    /// `ClientStatsPayload.hostname`.
    pub hostname: String,
    /// `ClientStatsPayload.env`.
    pub env: String,
    /// `ClientStatsPayload.version`.
    pub version: String,
    /// `ClientStatsPayload.lang`.
    pub lang: String,
    /// `ClientStatsPayload.tracerVersion`.
    pub tracer_version: String,
    /// `ClientStatsPayload.runtimeID`.
    pub runtime_id: String,
    /// `ClientStatsPayload.service`.
    pub client_service: String,
    /// `ClientStatsPayload.agentAggregation`.
    pub agent_aggregation: String,
    /// `ClientStatsPayload.containerID`.
    pub container_id: String,
    /// `ClientStatsPayload.tags`, sorted.
    pub tags: Vec<String>,
    /// `ClientStatsPayload.git_commit_sha`.
    pub git_commit_sha: String,
    /// `ClientStatsPayload.image_tag`.
    pub image_tag: String,
    /// `ClientStatsPayload.process_tags_hash`.
    pub process_tags_hash: u64,
    /// `ClientStatsPayload.process_tags`.
    pub process_tags: String,

    /// `ClientStatsBucket.duration`.
    pub bucket_duration: u64,

    /// `ClientGroupedStats.service`.
    pub service: String,
    /// `ClientGroupedStats.name`.
    pub name: String,
    /// `ClientGroupedStats.resource`.
    pub resource: String,
    /// `ClientGroupedStats.type`.
    pub span_type: String,
    /// `ClientGroupedStats.HTTP_status_code`.
    pub http_status_code: u32,
    /// `ClientGroupedStats.HTTP_method`.
    pub http_method: String,
    /// `ClientGroupedStats.HTTP_endpoint`.
    pub http_endpoint: String,
    /// `ClientGroupedStats.GRPC_status_code`.
    pub grpc_status_code: String,
    /// `ClientGroupedStats.DB_type`.
    pub db_type: String,
    /// `ClientGroupedStats.span_kind`.
    pub span_kind: String,
    /// `ClientGroupedStats.synthetics`.
    pub synthetics: bool,
    /// `ClientGroupedStats.is_trace_root`, as its enum name (`NOT_SET`, `TRUE`, or `FALSE`).
    pub is_trace_root: String,
    /// `ClientGroupedStats.service_source`.
    pub service_source: String,
    /// `ClientGroupedStats.peer_tags`, sorted.
    pub peer_tags: Vec<String>,
    /// `ClientGroupedStats.span_derived_primary_tags`, sorted.
    pub span_derived_primary_tags: Vec<String>,
    /// `ClientGroupedStats.additional_metric_tags`, sorted.
    pub additional_metric_tags: Vec<String>,
}

fn sorted(v: &[String]) -> Vec<String> {
    let mut v = v.to_vec();
    v.sort();
    v
}

fn trilean_name(v: &EnumOrUnknown<Trilean>) -> String {
    match v.enum_value() {
        Ok(Trilean::NOT_SET) => "NOT_SET".into(),
        Ok(Trilean::TRUE) => "TRUE".into(),
        Ok(Trilean::FALSE) => "FALSE".into(),
        Err(n) => n.to_string(),
    }
}

impl GroupKey {
    fn new(p: &StatsPayload, c: &ClientStatsPayload, b: &ClientStatsBucket, g: &ClientGroupedStats) -> Self {
        Self {
            agent_hostname: p.agentHostname.clone(),
            agent_env: p.agentEnv.clone(),
            agent_version: p.agentVersion.clone(),
            client_computed: p.clientComputed,
            split_payload: p.splitPayload,
            hostname: c.hostname.clone(),
            env: c.env.clone(),
            version: c.version.clone(),
            lang: c.lang.clone(),
            tracer_version: c.tracerVersion.clone(),
            runtime_id: c.runtimeID.clone(),
            client_service: c.service.clone(),
            agent_aggregation: c.agentAggregation.clone(),
            container_id: c.containerID.clone(),
            tags: sorted(&c.tags),
            git_commit_sha: c.git_commit_sha.clone(),
            image_tag: c.image_tag.clone(),
            process_tags_hash: c.process_tags_hash,
            process_tags: c.process_tags.clone(),
            bucket_duration: b.duration,
            service: g.service.clone(),
            name: g.name.clone(),
            resource: g.resource.clone(),
            span_type: g.type_.clone(),
            http_status_code: g.HTTP_status_code,
            http_method: g.HTTP_method.clone(),
            http_endpoint: g.HTTP_endpoint.clone(),
            grpc_status_code: g.GRPC_status_code.clone(),
            db_type: g.DB_type.clone(),
            span_kind: g.span_kind.clone(),
            synthetics: g.synthetics,
            is_trace_root: trilean_name(&g.is_trace_root),
            service_source: g.service_source.clone(),
            peer_tags: sorted(&g.peer_tags),
            span_derived_primary_tags: sorted(&g.span_derived_primary_tags),
            additional_metric_tags: sorted(&g.additional_metric_tags),
        }
    }
}

impl fmt::Display for GroupKey {
    /// Renders the group's main dimensions first, followed by every other non-default dimension.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "group{{service={:?} name={:?} resource={:?} type={:?}",
            self.service, self.name, self.resource, self.span_type
        )?;
        let strs = [
            ("http_status_code", self.http_status_code.to_string(), "0"),
            ("http_method", self.http_method.clone(), ""),
            ("http_endpoint", self.http_endpoint.clone(), ""),
            ("grpc_status_code", self.grpc_status_code.clone(), ""),
            ("db_type", self.db_type.clone(), ""),
            ("span_kind", self.span_kind.clone(), ""),
            ("synthetics", self.synthetics.to_string(), "false"),
            ("is_trace_root", self.is_trace_root.clone(), ""),
            ("service_source", self.service_source.clone(), ""),
            ("peer_tags", format!("{:?}", self.peer_tags), "[]"),
            (
                "span_derived_primary_tags",
                format!("{:?}", self.span_derived_primary_tags),
                "[]",
            ),
            (
                "additional_metric_tags",
                format!("{:?}", self.additional_metric_tags),
                "[]",
            ),
            ("client.hostname", self.hostname.clone(), ""),
            ("client.env", self.env.clone(), ""),
            ("client.version", self.version.clone(), ""),
            ("client.lang", self.lang.clone(), ""),
            ("client.tracer_version", self.tracer_version.clone(), ""),
            ("client.runtime_id", self.runtime_id.clone(), ""),
            ("client.service", self.client_service.clone(), ""),
            ("client.agent_aggregation", self.agent_aggregation.clone(), ""),
            ("client.container_id", self.container_id.clone(), ""),
            ("client.tags", format!("{:?}", self.tags), "[]"),
            ("client.git_commit_sha", self.git_commit_sha.clone(), ""),
            ("client.image_tag", self.image_tag.clone(), ""),
            ("client.process_tags_hash", self.process_tags_hash.to_string(), "0"),
            ("client.process_tags", self.process_tags.clone(), ""),
            ("bucket_duration", self.bucket_duration.to_string(), "0"),
            ("agent.hostname", self.agent_hostname.clone(), ""),
            ("agent.env", self.agent_env.clone(), ""),
            ("agent.version", self.agent_version.clone(), ""),
            ("agent.client_computed", self.client_computed.to_string(), "false"),
            ("agent.split_payload", self.split_payload.to_string(), "false"),
        ];
        for (name, value, default) in strs {
            if value != default {
                write!(f, " {}={:?}", name, value)?;
            }
        }
        write!(f, "}}")
    }
}

/// The aggregated values of one stats group.
#[derive(Clone, Debug, Default)]
pub struct Totals {
    /// Total hits.
    pub hits: u64,
    /// Total errors.
    pub errors: u64,
    /// Total duration, in nanoseconds.
    pub duration: u64,
    /// Total top-level hits.
    pub top_level_hits: u64,
    /// Merged latency sketch of successful spans, if any sketch was present.
    pub ok_summary: Option<DDSketch>,
    /// Merged latency sketch of error spans, if any sketch was present.
    pub error_summary: Option<DDSketch>,
}

fn decode_sketch(bytes: &[u8]) -> Result<Option<DDSketch>, GenericError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let proto = ProtoDDSketch::parse_from_bytes(bytes).error_context("Failed to decode stats sketch.")?;
    Ok(Some(EncodedApmStatsDDSketch::try_from(proto)?.inner().clone()))
}

fn merge_sketch(into: &mut Option<DDSketch>, other: Option<DDSketch>) {
    match (into.as_mut(), other) {
        (Some(a), Some(b)) => a.merge(&b),
        (None, Some(b)) => *into = Some(b),
        (_, None) => {}
    }
}

impl Totals {
    fn merge(&mut self, other: Totals) {
        self.hits += other.hits;
        self.errors += other.errors;
        self.duration += other.duration;
        self.top_level_hits += other.top_level_hits;
        merge_sketch(&mut self.ok_summary, other.ok_summary);
        merge_sketch(&mut self.error_summary, other.error_summary);
    }
}

/// The semantic form of a set of stats payloads: every group, aggregated across payloads and buckets.
///
/// Empty payloads contribute nothing, so a run that flushed one empty payload is equal to a run that sent no stats.
#[derive(Clone, Debug, Default)]
pub struct StatsSummary {
    /// Aggregated values, keyed by group dimensions.
    pub groups: BTreeMap<GroupKey, Totals>,
}

impl StatsSummary {
    /// Builds a summary from actual stats payloads.
    ///
    /// # Errors
    ///
    /// Returns an error if a latency sketch can't be decoded.
    pub fn from_payloads(payloads: &[StatsPayload]) -> Result<Self, GenericError> {
        let mut summary = Self::default();
        for p in payloads {
            for c in &p.stats {
                for b in &c.stats {
                    for g in &b.stats {
                        let totals = Totals {
                            hits: g.hits,
                            errors: g.errors,
                            duration: g.duration,
                            top_level_hits: g.topLevelHits,
                            ok_summary: decode_sketch(&g.okSummary)?,
                            error_summary: decode_sketch(&g.errorSummary)?,
                        };
                        summary.add(GroupKey::new(p, c, b, g), totals);
                    }
                }
            }
        }
        Ok(summary)
    }

    /// Builds a summary from a golden stats file (`v1_<case>.json`): a JSON array of `protojson` `StatsPayload`s.
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON doesn't match the expected `protojson` shape, or if a sketch can't be decoded.
    pub fn from_golden_json(json: &str) -> Result<Self, GenericError> {
        let goldens: Vec<GoldenStatsPayload> =
            serde_json::from_str(json).error_context("Failed to parse golden stats payloads.")?;
        Self::from_golden_payloads(goldens)
    }

    pub(super) fn from_golden_payloads(goldens: Vec<GoldenStatsPayload>) -> Result<Self, GenericError> {
        let payloads = goldens
            .into_iter()
            .map(GoldenStatsPayload::into_proto)
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_payloads(&payloads)
    }

    fn add(&mut self, key: GroupKey, totals: Totals) {
        self.groups.entry(key).or_default().merge(totals);
    }

    fn normalized(&self, opts: &CompareOptions) -> Self {
        let mut out = Self::default();
        for (key, totals) in &self.groups {
            let mut key = key.clone();
            if opts.ignore_agent_version {
                key.agent_version.clear();
            }
            out.add(key, totals.clone());
        }
        out
    }
}

const SKETCH_QUANTILES: [(&str, f64); 5] = [("min", 0.0), ("p50", 0.5), ("p95", 0.95), ("p99", 0.99), ("max", 1.0)];

fn compare_sketch(
    out: &mut Mismatches, path: &str, expected: &Option<DDSketch>, actual: &Option<DDSketch>, opts: &CompareOptions,
) {
    let count = |s: &Option<DDSketch>| s.as_ref().map_or(0, DDSketch::count);
    let (ec, ac) = (count(expected), count(actual));
    if ec != ac {
        out.push(format!("{}.count", path), ec.to_string(), ac.to_string());
        return;
    }
    let (Some(e), Some(a)) = (expected, actual) else {
        return;
    };
    if ec == 0 {
        return;
    }
    for (name, q) in SKETCH_QUANTILES {
        let (Some(ev), Some(av)) = (e.quantile(q), a.quantile(q)) else {
            continue;
        };
        let scale = ev.abs().max(av.abs());
        if (ev - av).abs() > opts.sketch_relative_tolerance * scale {
            out.push(format!("{}.{}", path, name), ev.to_string(), av.to_string());
        }
    }
}

/// Compares two stats summaries.
///
/// Every expected group must be present in the actual output and vice versa. For each group, hits, errors, duration,
/// and top-level hits must match exactly; sketch counts must match exactly, and sketch quantiles must match within
/// [`CompareOptions::sketch_relative_tolerance`].
///
/// # Errors
///
/// Returns every difference found.
pub fn compare_stats(expected: &StatsSummary, actual: &StatsSummary, opts: &CompareOptions) -> Result<(), Mismatches> {
    let expected = expected.normalized(opts);
    let actual = actual.normalized(opts);
    let mut out = Mismatches::default();
    for (key, e) in &expected.groups {
        let Some(a) = actual.groups.get(key) else {
            out.push(key.to_string(), "group present", "group missing");
            continue;
        };
        let path = key.to_string();
        out.check(format!("{}.hits", path), &e.hits, &a.hits);
        out.check(format!("{}.errors", path), &e.errors, &a.errors);
        out.check(format!("{}.duration", path), &e.duration, &a.duration);
        out.check(format!("{}.top_level_hits", path), &e.top_level_hits, &a.top_level_hits);
        compare_sketch(
            &mut out,
            &format!("{}.ok_summary", path),
            &e.ok_summary,
            &a.ok_summary,
            opts,
        );
        compare_sketch(
            &mut out,
            &format!("{}.error_summary", path),
            &e.error_summary,
            &a.error_summary,
            opts,
        );
    }
    for key in actual.groups.keys() {
        if !expected.groups.contains_key(key) {
            out.push(key.to_string(), "no group", "unexpected group");
        }
    }
    out.into_result()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{compare::WireGolden, golden_stats_path, golden_wire_path};

    fn golden_names() -> Vec<String> {
        crate::cases::load_bundled_cases()
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect()
    }

    fn golden(case: &str) -> StatsSummary {
        StatsSummary::from_golden_json(&std::fs::read_to_string(golden_stats_path(case)).unwrap()).unwrap()
    }

    fn sketch_bytes(values: &[f64]) -> Vec<u8> {
        let mut s = DDSketch::with_relative_accuracy(0.01).unwrap();
        for v in values {
            s.add(*v);
        }
        s.to_proto().write_to_bytes().unwrap()
    }

    fn group(resource: &str, hits: u64, durations: &[f64]) -> ClientGroupedStats {
        let mut g = ClientGroupedStats::new();
        g.service = "web".into();
        g.name = "web.request".into();
        g.resource = resource.into();
        g.type_ = "web".into();
        g.hits = hits;
        g.topLevelHits = hits;
        g.duration = durations.iter().sum::<f64>() as u64;
        g.okSummary = sketch_bytes(durations);
        g.errorSummary = sketch_bytes(&[]);
        g.is_trace_root = EnumOrUnknown::new(Trilean::TRUE);
        g
    }

    fn payload(buckets: Vec<(u64, Vec<ClientGroupedStats>)>) -> StatsPayload {
        let mut p = StatsPayload::new();
        p.agentHostname = "agent-host".into();
        p.agentEnv = "none".into();
        let mut c = ClientStatsPayload::new();
        c.env = "prod".into();
        for (start, groups) in buckets {
            let mut b = ClientStatsBucket::new();
            b.start = start;
            b.duration = 10_000_000_000;
            b.stats = groups;
            c.stats.push(b);
        }
        p.stats.push(c);
        p
    }

    fn b64(s: &str) -> Vec<u8> {
        crate::compare::protojson::decode_base64(s).unwrap()
    }

    /// Hand-builds the `default` case's stats as a real proto, independently of the golden parser, with a real bucket
    /// start, a non-zero sequence, and a real agent version.
    fn default_actual() -> StatsPayload {
        const EMPTY_SKETCH: &str = "CgkJ/UqBWr9S8D8SABoA";
        let mut root = ClientGroupedStats::new();
        root.service = "web".into();
        root.name = "web.request".into();
        root.resource = "GET /home".into();
        root.type_ = "web".into();
        root.HTTP_status_code = 200;
        root.hits = 1;
        root.topLevelHits = 1;
        root.duration = 100_000_000;
        root.okSummary = b64("CgkJ/UqBWr9S8D8SDRIIAAAAAAAA8D8YsA4aAA==");
        root.errorSummary = b64(EMPTY_SKETCH);
        root.span_kind = "unknown".into();
        root.is_trace_root = EnumOrUnknown::new(Trilean::TRUE);

        let mut child = ClientGroupedStats::new();
        child.service = "web-db".into();
        child.name = "sql.query".into();
        child.resource = "SELECT * FROM users".into();
        child.type_ = "sql".into();
        child.hits = 1;
        child.topLevelHits = 1;
        child.duration = 50_000_000;
        child.okSummary = b64("CgkJ/UqBWr9S8D8SDRIIAAAAAAAA8D8Y7A0aAA==");
        child.errorSummary = b64(EMPTY_SKETCH);
        child.span_kind = "unknown".into();
        child.is_trace_root = EnumOrUnknown::new(Trilean::FALSE);

        let mut bucket = ClientStatsBucket::new();
        bucket.start = 1_700_000_000_000_000_000;
        bucket.duration = 10_000_000_000;
        bucket.stats = vec![root, child];

        let mut c = ClientStatsPayload::new();
        c.hostname = "agent-host".into();
        c.env = "prod".into();
        c.version = "1.2.3".into();
        c.lang = "python".into();
        c.sequence = 42;
        c.stats.push(bucket);

        let mut p = StatsPayload::new();
        p.agentHostname = "agent-host".into();
        p.agentEnv = "none".into();
        p.agentVersion = "7.99.0".into();
        p.stats.push(c);
        p
    }

    fn expect_mismatch(result: Result<(), Mismatches>, needle: &str) {
        let err = result.expect_err("comparison should fail");
        assert!(
            err.0.iter().any(|m| m.to_string().contains(needle)),
            "no mismatch containing {:?} in:\n{}",
            needle,
            err
        );
    }

    #[test]
    fn golden_stats_match_wire_stats_for_every_case() {
        for case in golden_names() {
            let from_stats = golden(&case);
            let wire = WireGolden::from_json(&std::fs::read_to_string(golden_wire_path(&case)).unwrap()).unwrap();
            let from_wire = wire.stats_summary().unwrap();
            compare_stats(&from_stats, &from_wire, &CompareOptions::default())
                .unwrap_or_else(|e| panic!("case {}: {}", case, e));
        }
    }

    #[test]
    fn default_case_proto_matches_golden() {
        let actual = StatsSummary::from_payloads(&[default_actual()]).unwrap();
        compare_stats(&golden("default"), &actual, &CompareOptions::default()).unwrap();
    }

    #[test]
    fn agent_version_is_compared_when_not_ignored() {
        let actual = StatsSummary::from_payloads(&[default_actual()]).unwrap();
        let opts = CompareOptions {
            ignore_agent_version: false,
            ..Default::default()
        };
        expect_mismatch(compare_stats(&golden("default"), &actual, &opts), "group missing");
    }

    #[test]
    fn values_split_across_buckets_and_payloads_match() {
        let expected =
            StatsSummary::from_payloads(&[payload(vec![(0, vec![group("GET /", 2, &[1e8, 2e8])])])]).unwrap();
        let split_buckets = StatsSummary::from_payloads(&[payload(vec![
            (10_000_000_000, vec![group("GET /", 1, &[1e8])]),
            (20_000_000_000, vec![group("GET /", 1, &[2e8])]),
        ])])
        .unwrap();
        compare_stats(&expected, &split_buckets, &CompareOptions::default()).unwrap();

        let split_payloads = StatsSummary::from_payloads(&[
            payload(vec![(10_000_000_000, vec![group("GET /", 1, &[1e8])])]),
            payload(vec![(30_000_000_000, vec![group("GET /", 1, &[2e8])])]),
        ])
        .unwrap();
        compare_stats(&expected, &split_payloads, &CompareOptions::default()).unwrap();
    }

    #[test]
    fn changed_totals_are_reported() {
        let expected = golden("default");
        for (mutate, needle) in [
            (
                (|g: &mut ClientGroupedStats| g.hits += 1) as fn(&mut ClientGroupedStats),
                ".hits",
            ),
            (|g| g.errors += 1, ".errors"),
            (|g| g.duration += 1, ".duration"),
            (|g| g.topLevelHits = 0, ".top_level_hits"),
            (|g| g.okSummary = sketch_bytes(&[1e8, 1e8]), "ok_summary.count"),
            (|g| g.okSummary = sketch_bytes(&[5e8]), "ok_summary.p50"),
        ] {
            let mut p = default_actual();
            mutate(&mut p.stats[0].stats[0].stats[0]);
            let actual = StatsSummary::from_payloads(&[p]).unwrap();
            expect_mismatch(compare_stats(&expected, &actual, &CompareOptions::default()), needle);
        }
    }

    #[test]
    fn changed_dimensions_are_reported_as_missing_and_extra_groups() {
        let expected = golden("default");
        for mutate in [
            (|g: &mut ClientGroupedStats| g.resource = "GET /other".into()) as fn(&mut ClientGroupedStats),
            |g| g.HTTP_status_code = 500,
            |g| g.peer_tags = vec!["peer.service:db".into()],
        ] {
            let mut p = default_actual();
            mutate(&mut p.stats[0].stats[0].stats[0]);
            let actual = StatsSummary::from_payloads(&[p]).unwrap();
            let err = compare_stats(&expected, &actual, &CompareOptions::default()).unwrap_err();
            assert!(err.0.iter().any(|m| m.actual == "group missing"), "{}", err);
            assert!(err.0.iter().any(|m| m.actual == "unexpected group"), "{}", err);
        }
    }

    #[test]
    fn missing_and_extra_groups_are_reported() {
        let expected = golden("default");

        let mut p = default_actual();
        p.stats[0].stats[0].stats.pop();
        let actual = StatsSummary::from_payloads(&[p]).unwrap();
        expect_mismatch(
            compare_stats(&expected, &actual, &CompareOptions::default()),
            "resource=\"SELECT * FROM users\"",
        );

        let mut p = default_actual();
        p.stats[0].stats[0].stats.push(group("GET /extra", 1, &[1e8]));
        let actual = StatsSummary::from_payloads(&[p]).unwrap();
        expect_mismatch(
            compare_stats(&expected, &actual, &CompareOptions::default()),
            "unexpected group",
        );
    }

    #[test]
    fn empty_stats_payload_equals_no_stats() {
        let summary = golden("client_computed_stats");
        assert!(summary.groups.is_empty());
        compare_stats(&summary, &StatsSummary::default(), &CompareOptions::default()).unwrap();
    }
}
