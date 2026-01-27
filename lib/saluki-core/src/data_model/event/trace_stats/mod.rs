//! Trace stats.

use stringtheory::MetaString;

/// Trace statistics output from the APM Stats transform.
///
/// Contains pre-aggregated trace statistics grouped by client/tracer. The encoder wraps this
/// in a `StatsPayload` protobuf and adds agent-level metadata (agentHostname, agentEnv,
/// agentVersion, clientComputed, splitPayload) from ADP configuration.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct TraceStats {
    /// Multiple client payloads, one per PayloadAggregationKey (hostname/env/version/container).
    stats: Vec<ClientStatsPayload>,
}

impl TraceStats {
    /// Creates a new `TraceStats` with the given client stats payloads.
    pub fn new(stats: Vec<ClientStatsPayload>) -> Self {
        Self { stats }
    }

    /// Returns a reference to the client stats payloads.
    pub fn stats(&self) -> &[ClientStatsPayload] {
        &self.stats
    }

    /// Returns a mutable reference to the client stats payloads.
    pub fn stats_mut(&mut self) -> &mut Vec<ClientStatsPayload> {
        &mut self.stats
    }
}

/// Tracer-level stats payload.
///
/// Groups stats by tracer/container identity (hostname, env, version, container_id).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ClientStatsPayload {
    hostname: MetaString,
    env: MetaString,
    version: MetaString,
    stats: Vec<ClientStatsBucket>,
    lang: MetaString,
    tracer_version: MetaString,
    runtime_id: MetaString,
    sequence: u64,
    agent_aggregation: MetaString,
    service: MetaString,
    container_id: MetaString,
    tags: Vec<MetaString>,
    git_commit_sha: MetaString,
    image_tag: MetaString,
    process_tags_hash: u64,
    process_tags: MetaString,
}

impl ClientStatsPayload {
    /// Creates a new `ClientStatsPayload` with the required identity fields.
    pub fn new(hostname: impl Into<MetaString>, env: impl Into<MetaString>, version: impl Into<MetaString>) -> Self {
        Self {
            hostname: hostname.into(),
            env: env.into(),
            version: version.into(),
            ..Self::default()
        }
    }

    /// Sets the stats buckets.
    pub fn with_stats(mut self, stats: Vec<ClientStatsBucket>) -> Self {
        self.stats = stats;
        self
    }

    /// Sets the tracer language.
    pub fn with_lang(mut self, lang: impl Into<MetaString>) -> Self {
        self.lang = lang.into();
        self
    }

    /// Sets the tracer version.
    pub fn with_tracer_version(mut self, tracer_version: impl Into<MetaString>) -> Self {
        self.tracer_version = tracer_version.into();
        self
    }

    /// Sets the runtime identifier.
    pub fn with_runtime_id(mut self, runtime_id: impl Into<MetaString>) -> Self {
        self.runtime_id = runtime_id.into();
        self
    }

    /// Sets the message sequence number.
    pub fn with_sequence(mut self, sequence: u64) -> Self {
        self.sequence = sequence;
        self
    }

    /// Sets the agent aggregation key.
    pub fn with_agent_aggregation(mut self, agent_aggregation: impl Into<MetaString>) -> Self {
        self.agent_aggregation = agent_aggregation.into();
        self
    }

    /// Sets the main service name.
    pub fn with_service(mut self, service: impl Into<MetaString>) -> Self {
        self.service = service.into();
        self
    }

    /// Sets the container identifier.
    pub fn with_container_id(mut self, container_id: impl Into<MetaString>) -> Self {
        self.container_id = container_id.into();
        self
    }

    /// Sets the orchestrator tags.
    pub fn with_tags(mut self, tags: Vec<MetaString>) -> Self {
        self.tags = tags;
        self
    }

    /// Sets the git commit SHA.
    pub fn with_git_commit_sha(mut self, git_commit_sha: impl Into<MetaString>) -> Self {
        self.git_commit_sha = git_commit_sha.into();
        self
    }

    /// Sets the container image tag.
    pub fn with_image_tag(mut self, image_tag: impl Into<MetaString>) -> Self {
        self.image_tag = image_tag.into();
        self
    }

    /// Sets the process tags hash.
    pub fn with_process_tags_hash(mut self, process_tags_hash: u64) -> Self {
        self.process_tags_hash = process_tags_hash;
        self
    }

    /// Sets the process tags.
    pub fn with_process_tags(mut self, process_tags: impl Into<MetaString>) -> Self {
        self.process_tags = process_tags.into();
        self
    }

    /// Returns the hostname.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Returns the environment.
    pub fn env(&self) -> &str {
        &self.env
    }

    /// Returns the version.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Returns the stats buckets.
    pub fn stats(&self) -> &[ClientStatsBucket] {
        &self.stats
    }

    /// Returns the tracer language.
    pub fn lang(&self) -> &str {
        &self.lang
    }

    /// Returns the tracer version.
    pub fn tracer_version(&self) -> &str {
        &self.tracer_version
    }

    /// Returns the runtime identifier.
    pub fn runtime_id(&self) -> &str {
        &self.runtime_id
    }

    /// Returns the message sequence number.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the agent aggregation key.
    pub fn agent_aggregation(&self) -> &str {
        &self.agent_aggregation
    }

    /// Returns the main service name.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Returns the container identifier.
    pub fn container_id(&self) -> &str {
        &self.container_id
    }

    /// Returns the orchestrator tags.
    pub fn tags(&self) -> &[MetaString] {
        &self.tags
    }

    /// Returns the git commit SHA.
    pub fn git_commit_sha(&self) -> &str {
        &self.git_commit_sha
    }

    /// Returns the container image tag.
    pub fn image_tag(&self) -> &str {
        &self.image_tag
    }

    /// Returns the process tags hash.
    pub fn process_tags_hash(&self) -> u64 {
        self.process_tags_hash
    }

    /// Returns the process tags.
    pub fn process_tags(&self) -> &str {
        &self.process_tags
    }

    /// Adds a new client statistics bucket to this payload.
    pub fn add_stats(&mut self, stats: ClientStatsBucket) {
        self.stats.push(stats);
    }

    /// Consumes the statistics buckets and returns them.
    ///
    /// No statistics buckets will remain in `self`.
    pub fn take_stats(&mut self) -> Vec<ClientStatsBucket> {
        std::mem::take(&mut self.stats)
    }
}

/// A time bucket containing aggregated stats.
///
/// Stats are grouped into fixed-duration buckets (typically 10 seconds).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ClientStatsBucket {
    /// Bucket start timestamp in nanoseconds since Unix epoch.
    start: u64,
    /// Bucket duration in nanoseconds.
    duration: u64,
    /// Grouped stats within this bucket.
    stats: Vec<ClientGroupedStats>,
    /// Time shift applied by the agent.
    agent_time_shift: i64,
}

impl ClientStatsBucket {
    /// Creates a new `ClientStatsBucket` with the given time range and stats.
    pub fn new(start: u64, duration: u64, stats: Vec<ClientGroupedStats>) -> Self {
        Self {
            start,
            duration,
            stats,
            agent_time_shift: 0,
        }
    }

    /// Sets the grouped stats.
    pub fn with_stats(mut self, stats: Vec<ClientGroupedStats>) -> Self {
        self.stats = stats;
        self
    }

    /// Sets the agent time shift.
    pub fn with_agent_time_shift(mut self, agent_time_shift: i64) -> Self {
        self.agent_time_shift = agent_time_shift;
        self
    }

    /// Returns the bucket start timestamp in nanoseconds.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Returns the bucket duration in nanoseconds.
    pub fn duration(&self) -> u64 {
        self.duration
    }

    /// Returns the grouped stats within this bucket.
    pub fn stats(&self) -> &[ClientGroupedStats] {
        &self.stats
    }

    /// Returns a mutable reference to the grouped stats within this bucket.
    pub fn stats_mut(&mut self) -> &mut Vec<ClientGroupedStats> {
        &mut self.stats
    }

    /// Returns the agent time shift.
    pub fn agent_time_shift(&self) -> i64 {
        self.agent_time_shift
    }

    /// Consumes the grouped statistics and returns them.
    ///
    /// No statistics groups will remain in `self`.
    pub fn take_stats(&mut self) -> Vec<ClientGroupedStats> {
        std::mem::take(&mut self.stats)
    }
}

/// Aggregated stats for spans grouped by aggregation key.
///
/// Contains both the aggregation key fields (service, name, resource, etc.) and
/// the aggregated values (hits, errors, duration, latency distributions).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ClientGroupedStats {
    // Aggregation key fields
    service: MetaString,
    name: MetaString,
    resource: MetaString,
    http_status_code: u32,
    span_type: MetaString,
    db_type: MetaString,
    span_kind: MetaString,
    peer_tags: Vec<MetaString>,
    is_trace_root: Option<bool>,
    grpc_status_code: MetaString,
    http_method: MetaString,
    http_endpoint: MetaString,

    // Aggregated values
    hits: u64,
    errors: u64,
    duration: u64,
    ok_summary: Vec<u8>,
    error_summary: Vec<u8>,
    synthetics: bool,
    top_level_hits: u64,
}

impl ClientGroupedStats {
    /// Creates a new `ClientGroupedStats` with the required aggregation key fields.
    pub fn new(service: impl Into<MetaString>, name: impl Into<MetaString>, resource: impl Into<MetaString>) -> Self {
        Self {
            service: service.into(),
            name: name.into(),
            resource: resource.into(),
            ..Self::default()
        }
    }

    // Builder methods for aggregation key fields

    /// Sets the HTTP status code.
    pub fn with_http_status_code(mut self, http_status_code: u32) -> Self {
        self.http_status_code = http_status_code;
        self
    }

    /// Sets the span type.
    pub fn with_span_type(mut self, span_type: impl Into<MetaString>) -> Self {
        self.span_type = span_type.into();
        self
    }

    /// Sets the database type.
    pub fn with_db_type(mut self, db_type: impl Into<MetaString>) -> Self {
        self.db_type = db_type.into();
        self
    }

    /// Sets the span kind.
    pub fn with_span_kind(mut self, span_kind: impl Into<MetaString>) -> Self {
        self.span_kind = span_kind.into();
        self
    }

    /// Sets the peer tags.
    pub fn with_peer_tags(mut self, peer_tags: Vec<MetaString>) -> Self {
        self.peer_tags = peer_tags;
        self
    }

    /// Sets whether this is a trace root.
    pub fn with_is_trace_root(mut self, is_trace_root: Option<bool>) -> Self {
        self.is_trace_root = is_trace_root;
        self
    }

    /// Sets the gRPC status code.
    pub fn with_grpc_status_code(mut self, grpc_status_code: impl Into<MetaString>) -> Self {
        self.grpc_status_code = grpc_status_code.into();
        self
    }

    /// Sets the HTTP method.
    pub fn with_http_method(mut self, http_method: impl Into<MetaString>) -> Self {
        self.http_method = http_method.into();
        self
    }

    /// Sets the HTTP endpoint.
    pub fn with_http_endpoint(mut self, http_endpoint: impl Into<MetaString>) -> Self {
        self.http_endpoint = http_endpoint.into();
        self
    }

    // Builder methods for aggregated values

    /// Sets the hit count.
    pub fn with_hits(mut self, hits: u64) -> Self {
        self.hits = hits;
        self
    }

    /// Sets the error count.
    pub fn with_errors(mut self, errors: u64) -> Self {
        self.errors = errors;
        self
    }

    /// Sets the total duration in nanoseconds.
    pub fn with_duration(mut self, duration: u64) -> Self {
        self.duration = duration;
        self
    }

    /// Sets the DDSketch summary for successful spans.
    pub fn with_ok_summary(mut self, ok_summary: Vec<u8>) -> Self {
        self.ok_summary = ok_summary;
        self
    }

    /// Sets the DDSketch summary for error spans.
    pub fn with_error_summary(mut self, error_summary: Vec<u8>) -> Self {
        self.error_summary = error_summary;
        self
    }

    /// Sets the synthetics traffic flag.
    pub fn with_synthetics(mut self, synthetics: bool) -> Self {
        self.synthetics = synthetics;
        self
    }

    /// Sets the top-level hit count.
    pub fn with_top_level_hits(mut self, top_level_hits: u64) -> Self {
        self.top_level_hits = top_level_hits;
        self
    }

    // Getters for aggregation key fields

    /// Returns the service name.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Returns the operation name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the resource name.
    pub fn resource(&self) -> &str {
        &self.resource
    }

    /// Returns the HTTP status code.
    pub fn http_status_code(&self) -> u32 {
        self.http_status_code
    }

    /// Returns the span type.
    pub fn span_type(&self) -> &str {
        &self.span_type
    }

    /// Returns the database type.
    pub fn db_type(&self) -> &str {
        &self.db_type
    }

    /// Returns the span kind.
    pub fn span_kind(&self) -> &str {
        &self.span_kind
    }

    /// Returns the peer tags.
    pub fn peer_tags(&self) -> &[MetaString] {
        &self.peer_tags
    }

    /// Returns whether this is a trace root.
    pub fn is_trace_root(&self) -> Option<bool> {
        self.is_trace_root
    }

    /// Returns the gRPC status code.
    pub fn grpc_status_code(&self) -> &str {
        &self.grpc_status_code
    }

    /// Returns the HTTP method.
    pub fn http_method(&self) -> &str {
        &self.http_method
    }

    /// Returns the HTTP endpoint.
    pub fn http_endpoint(&self) -> &str {
        &self.http_endpoint
    }

    // Getters for aggregated values

    /// Returns the hit count.
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Returns the error count.
    pub fn errors(&self) -> u64 {
        self.errors
    }

    /// Returns the total duration in nanoseconds.
    pub fn duration(&self) -> u64 {
        self.duration
    }

    /// Returns the DDSketch summary for successful spans.
    pub fn ok_summary(&self) -> &[u8] {
        &self.ok_summary
    }

    /// Returns the DDSketch summary for error spans.
    pub fn error_summary(&self) -> &[u8] {
        &self.error_summary
    }

    /// Returns the synthetics traffic flag.
    pub fn synthetics(&self) -> bool {
        self.synthetics
    }

    /// Returns the top-level hit count.
    pub fn top_level_hits(&self) -> u64 {
        self.top_level_hits
    }
}
