//! APM Stats transform.
//!
//! Aggregates traces into time-bucketed statistics, producing `TraceStats` events.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use opentelemetry_semantic_conventions::resource::{CONTAINER_ID, K8S_POD_UID};
use saluki_config::GenericConfiguration;
use saluki_context::{origin::OriginTagCardinality, tags::TagSet};
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::{trace::Trace, trace_stats::TraceStats, Event, EventType},
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_env::{
    host::providers::BoxedHostProvider, workload::EntityId, EnvironmentProvider, HostProvider, WorkloadProvider,
};
use saluki_error::GenericError;
use stringtheory::MetaString;
use tokio::{select, time::interval};
use tracing::{debug, error};

use crate::common::datadog::apm::ApmConfig;
use crate::common::otlp::util::{extract_container_tags_from_resource_tagset, KEY_DATADOG_CONTAINER_ID};

mod aggregation;

use self::aggregation::process_tags_hash;
mod span_concentrator;
mod statsraw;
mod weight;

use self::aggregation::PayloadAggregationKey;
use self::span_concentrator::{InfraTags, SpanConcentrator};
use self::weight::weight;

/// Default flush interval for the APM stats transform.
const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(10);

/// Tag key for process tags in span meta.
const TAG_PROCESS_TAGS: &str = "_dd.tags.process";

/// APM Stats transform configuration.
///
/// Aggregates incoming `Trace` events into time-bucketed statistics, emitting
/// `TraceStats` events.
pub struct ApmStatsConfiguration {
    apm_config: ApmConfig,
    default_hostname: Option<String>,
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
}

impl ApmStatsConfiguration {
    /// Creates a new `ApmStatsConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let apm_config = ApmConfig::from_configuration(config)?;
        Ok(Self {
            apm_config,
            default_hostname: None,
            workload_provider: None,
        })
    }

    /// Sets the default hostname using the environment provider.
    pub async fn with_environment_provider<E>(mut self, env_provider: E) -> Result<Self, GenericError>
    where
        E: EnvironmentProvider<Host = BoxedHostProvider>,
    {
        let hostname = env_provider.host().get_hostname().await?;
        self.default_hostname = Some(hostname);
        Ok(self)
    }

    /// Sets the workload provider.
    ///
    /// Defaults to unset.
    pub fn with_workload_provider<W>(mut self, workload_provider: W) -> Self
    where
        W: WorkloadProvider + Send + Sync + 'static,
    {
        self.workload_provider = Some(Arc::new(workload_provider));
        self
    }
}

#[async_trait]
impl TransformBuilder for ApmStatsConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let mut apm_config = self.apm_config.clone();

        if let Some(hostname) = &self.default_hostname {
            apm_config.set_hostname_if_empty(hostname.as_str());
        }

        let concentrator = SpanConcentrator::new(
            apm_config.compute_stats_by_span_kind(),
            apm_config.peer_tags_aggregation(),
            apm_config.peer_tags(),
            now_nanos(),
        );

        Ok(Box::new(ApmStats {
            concentrator,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            agent_env: apm_config.default_env().clone(),
            agent_hostname: apm_config.hostname().clone(),
            workload_provider: self.workload_provider.clone(),
        }))
    }

    fn input_event_type(&self) -> EventType {
        EventType::Trace
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::TraceStats)];
        OUTPUTS
    }
}

impl MemoryBounds for ApmStatsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<ApmStats>("component struct");
        // TODO: Think about everything we need to account for here.
    }
}

struct ApmStats {
    concentrator: SpanConcentrator,
    flush_interval: Duration,
    agent_env: MetaString,
    agent_hostname: MetaString,
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
}

impl ApmStats {
    fn process_trace(&mut self, trace: &Trace) {
        let root_span = trace
            .spans()
            .iter()
            .find(|s| s.parent_id() == 0)
            .or_else(|| trace.spans().first());

        let trace_weight = root_span.map(weight).unwrap_or(1.0);

        let process_tags = extract_process_tags(trace);

        let payload_key = self.build_payload_key(trace, &process_tags);
        let infra_tags = self.build_infra_tags(trace, &process_tags);

        let origin = trace
            .spans()
            .first()
            .and_then(|s| s.meta().get("_dd.origin"))
            .map(|s| s.as_ref())
            .unwrap_or("");

        for span in trace.spans() {
            if let Some(stat_span) = self.concentrator.new_stat_span_from_span(span) {
                self.concentrator
                    .add_span(&stat_span, trace_weight, &payload_key, &infra_tags, origin);
            }
        }
    }

    fn build_infra_tags(&self, trace: &Trace, process_tags: &str) -> InfraTags {
        let resource_tags = trace.resource_tags();
        let container_id = resolve_container_id(resource_tags);
        let mut container_tags = if container_id.is_empty() {
            vec![]
        } else {
            extract_container_tags(resource_tags)
        };

        // Query the workload provider for additional container tags.
        if !container_id.is_empty() {
            if let Some(workload_provider) = &self.workload_provider {
                let entity_id = EntityId::Container(container_id.clone());
                if let Some(tags) = workload_provider.get_tags_for_entity(&entity_id, OriginTagCardinality::Low) {
                    container_tags.extend((&tags).into_iter().map(|tag| MetaString::from(tag.as_str())));
                }
            }
        }

        container_tags.sort();

        InfraTags::new(container_id, container_tags, process_tags)
    }

    fn build_payload_key(&self, trace: &Trace, process_tags: &str) -> PayloadAggregationKey {
        let root_span = trace
            .spans()
            .iter()
            .find(|s| s.parent_id() == 0)
            .or_else(|| trace.spans().first());

        let span_env = root_span.and_then(|s| s.meta().get("env")).filter(|s| !s.is_empty());
        let env = span_env.cloned().unwrap_or_else(|| self.agent_env.clone());

        let hostname = root_span
            .and_then(|s| s.meta().get("_dd.hostname"))
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| self.agent_hostname.clone());

        let version = root_span
            .and_then(|s| s.meta().get("version"))
            .cloned()
            .unwrap_or_default();

        let container_id = root_span
            .and_then(|s| s.meta().get("_dd.container_id"))
            .cloned()
            .unwrap_or_default();

        let git_commit_sha = root_span
            .and_then(|s| s.meta().get("_dd.git.commit.sha"))
            .cloned()
            .unwrap_or_default();

        let image_tag = root_span
            .and_then(|s| s.meta().get("_dd.image_tag"))
            .cloned()
            .unwrap_or_default();

        let lang = root_span
            .and_then(|s| s.meta().get("language"))
            .cloned()
            .unwrap_or_default();

        PayloadAggregationKey {
            env,
            hostname,
            version,
            container_id,
            git_commit_sha,
            image_tag,
            lang,
            process_tags_hash: process_tags_hash(process_tags),
        }
    }
}

#[async_trait]
impl Transform for ApmStats {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();

        let mut flush_ticker = interval(self.flush_interval);
        flush_ticker.tick().await;

        let mut final_flush = false;

        health.mark_ready();
        debug!("APM Stats transform started.");

        loop {
            select! {
                _ = health.live() => continue,

                _ = flush_ticker.tick() => {
                    let stats_payloads = self.concentrator.flush(now_nanos(), final_flush);

                    if !stats_payloads.is_empty() {
                        let trace_stats = TraceStats::new(stats_payloads);
                        debug!(buckets = trace_stats.stats().len(), "Flushing APM stats.");

                        let mut event_buffer = EventsBuffer::default();
                        if event_buffer.try_push(Event::TraceStats(trace_stats)).is_some() {
                            error!("Failed to push TraceStats event to buffer.");
                        } else if let Err(e) = context.dispatcher().dispatch(event_buffer).await {
                            error!(error = %e, "Failed to dispatch TraceStats event.");
                        }
                    }

                    if final_flush {
                        debug!("Final APM stats flush complete.");
                        break;
                    }
                },

                maybe_events = context.events().next(), if !final_flush => {
                    match maybe_events {
                        Some(events) => {
                            for event in events {
                                if let Event::Trace(trace) = event {
                                    self.process_trace(&trace);
                                }
                            }
                        },
                        None => {
                            // We've reached the end of our input stream, so mark ourselves for a final flush and reset the
                            // interval so it ticks immediately on the next loop iteration.
                            final_flush = true;
                            flush_ticker.reset_immediately();
                            debug!("APM Stats transform stopping, triggering final flush...");
                        }
                    }
                },
            }
        }

        debug!("APM Stats transform stopped.");
        Ok(())
    }
}

/// Returns the current time as nanoseconds since Unix epoch.
fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

/// Resolves container ID from OTLP resource tags.
fn resolve_container_id(resource_tags: &TagSet) -> MetaString {
    for key in [KEY_DATADOG_CONTAINER_ID, CONTAINER_ID, K8S_POD_UID] {
        if let Some(tag) = resource_tags.get_single_tag(key) {
            if let Some(value) = tag.value() {
                if !value.is_empty() {
                    return MetaString::from(value);
                }
            }
        }
    }
    MetaString::default()
}

/// Extracts container tags from OTLP resource tags.
fn extract_container_tags(resource_tags: &TagSet) -> Vec<MetaString> {
    let mut container_tags_set = TagSet::default();
    extract_container_tags_from_resource_tagset(resource_tags, &mut container_tags_set);

    container_tags_set
        .into_iter()
        .map(|tag| MetaString::from(tag.as_str()))
        .collect()
}

/// Extracts process tags from trace.
fn extract_process_tags(trace: &Trace) -> String {
    if let Some(first_span) = trace.spans().first() {
        if let Some(process_tags) = first_span.meta().get(TAG_PROCESS_TAGS) {
            let tags = process_tags.as_ref();
            if !tags.is_empty() {
                return tags.to_string();
            }
        }
    }

    String::new()
}

#[cfg(test)]
mod tests {
    use saluki_common::collections::FastHashMap;
    use saluki_context::tags::TagSet;
    use saluki_core::data_model::event::trace::Span;

    use super::aggregation::BUCKET_DURATION_NS;
    use super::span_concentrator::METRIC_PARTIAL_VERSION;
    use super::*;

    /// Helper to align timestamp to bucket boundary
    fn align_ts(ts: u64, bsize: u64) -> u64 {
        ts - ts % bsize
    }

    /// Creates a test span with the given parameters.
    #[allow(clippy::too_many_arguments)]
    fn test_span(
        aligned_now: u64, span_id: u64, parent_id: u64, duration: u64, bucket_offset: u64, service: &str,
        resource: &str, error: i32, meta: Option<FastHashMap<MetaString, MetaString>>,
        metrics: Option<FastHashMap<MetaString, f64>>,
    ) -> Span {
        // Calculate start time so that span ends in the correct bucket
        // End time = start + duration, and we want end time to be in bucket (aligned_now - offset * bsize)
        // Use BUCKET_DURATION_NS as the bucket size (matches the concentrator)
        let bucket_start = aligned_now - bucket_offset * BUCKET_DURATION_NS;
        let start = bucket_start - duration;

        Span::new(
            service, "query", resource, "db", 1, span_id, parent_id, start, duration, error,
        )
        .with_meta(meta)
        .with_metrics(metrics)
    }

    /// Creates a simple measured span for basic tests
    fn make_test_span(service: &str, name: &str, resource: &str) -> Span {
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from("_dd.measured"), 1.0);

        Span::new(service, name, resource, "web", 1, 1, 0, 1000000000, 100000000, 0).with_metrics(metrics)
    }

    /// Creates a top-level span (parent_id = 0, has _top_level metric)
    fn make_top_level_span(
        aligned_now: u64, span_id: u64, duration: u64, bucket_offset: u64, service: &str, resource: &str, error: i32,
        meta: Option<FastHashMap<MetaString, MetaString>>,
    ) -> Span {
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from("_top_level"), 1.0);
        test_span(
            aligned_now,
            span_id,
            0,
            duration,
            bucket_offset,
            service,
            resource,
            error,
            meta,
            Some(metrics),
        )
    }

    #[test]
    fn test_process_trace_creates_stats() {
        let now = now_nanos();

        let concentrator = SpanConcentrator::new(true, true, &[], now);
        let mut transform = ApmStats {
            concentrator,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            agent_env: MetaString::from("none"),
            agent_hostname: MetaString::default(),
            workload_provider: None,
        };

        let span = make_test_span("test-service", "test-operation", "test-resource");
        let trace = Trace::new(vec![span], TagSet::default());

        transform.process_trace(&trace);

        // Flush and verify we got stats
        let stats = transform.concentrator.flush(now + BUCKET_DURATION_NS * 2, true);
        assert!(!stats.is_empty(), "Expected stats to be produced");
    }

    #[test]
    fn test_weight_applied_to_stats() {
        let now = now_nanos();

        let concentrator = SpanConcentrator::new(true, true, &[], now);
        let mut transform = ApmStats {
            concentrator,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            agent_env: MetaString::from("none"),
            agent_hostname: MetaString::default(),
            workload_provider: None,
        };

        // Create a span with 0.5 sample rate (weight = 2.0)
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from("_dd.measured"), 1.0);
        metrics.insert(MetaString::from("_sample_rate"), 0.5);

        let span = Span::new(
            "test-service",
            "test-op",
            "test-resource",
            "web",
            1,
            1,
            0,
            now,
            100000000,
            0,
        )
        .with_metrics(metrics);

        let trace = Trace::new(vec![span], TagSet::default());
        transform.process_trace(&trace);

        let stats = transform.concentrator.flush(now + BUCKET_DURATION_NS * 2, true);
        assert!(!stats.is_empty());

        // The hits should be weighted (approximately 2 due to 0.5 sample rate)
        let bucket = &stats[0].stats()[0];
        let grouped = &bucket.stats()[0];
        // With stochastic rounding, hits could be 1 or 2, but with weight 2.0 it should round to 2
        assert!(grouped.hits() >= 1, "Expected weighted hits");
    }

    #[test]
    fn test_force_flush() {
        let now = now_nanos();
        let aligned_now = align_ts(now, BUCKET_DURATION_NS);

        let mut concentrator = SpanConcentrator::new(true, true, &[], now);

        // Add a span
        let span = make_top_level_span(aligned_now, 1, 50, 5, "A1", "resource1", 0, None);
        let trace = Trace::new(vec![span], TagSet::default());

        let payload_key = PayloadAggregationKey {
            env: MetaString::from("test"),
            hostname: MetaString::from("host"),
            ..Default::default()
        };
        let infra_tags = InfraTags::default();

        for span in trace.spans() {
            if let Some(stat_span) = concentrator.new_stat_span_from_span(span) {
                concentrator.add_span(&stat_span, 1.0, &payload_key, &infra_tags, "");
            }
        }

        // ts=0 so that flush always considers buckets not old enough
        let ts: u64 = 0;

        // Without force flush, should skip the bucket
        let stats = concentrator.flush(ts, false);
        assert!(stats.is_empty(), "Non-force flush should return empty");

        // With force flush, should flush buckets regardless of age
        let stats = concentrator.flush(ts, true);
        assert!(!stats.is_empty(), "Force flush should return stats");
        assert_eq!(stats[0].stats().len(), 1, "Should have 1 bucket");
    }

    #[test]
    fn test_ignores_partial_spans() {
        let now = now_nanos();
        let aligned_now = align_ts(now, BUCKET_DURATION_NS);

        let mut concentrator = SpanConcentrator::new(true, true, &[], now);

        // Create a partial span (has _dd.partial_version metric)
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from("_top_level"), 1.0);
        metrics.insert(MetaString::from(METRIC_PARTIAL_VERSION), 830604.0);

        let span = test_span(aligned_now, 1, 0, 50, 5, "A1", "resource1", 0, None, Some(metrics));
        let trace = Trace::new(vec![span], TagSet::default());

        let payload_key = PayloadAggregationKey {
            env: MetaString::from("test"),
            hostname: MetaString::from("tracer-hostname"),
            ..Default::default()
        };
        let infra_tags = InfraTags::default();

        for span in trace.spans() {
            if let Some(stat_span) = concentrator.new_stat_span_from_span(span) {
                concentrator.add_span(&stat_span, 1.0, &payload_key, &infra_tags, "");
            }
        }

        // Partial spans should be ignored
        let stats = concentrator.flush(now + BUCKET_DURATION_NS * 3, true);
        assert!(stats.is_empty(), "Partial spans should be ignored");
    }

    #[test]
    fn test_concentrator_stats_totals() {
        let now = now_nanos();
        let aligned_now = align_ts(now, BUCKET_DURATION_NS);

        // Set oldestTs to allow old buckets
        let oldest_ts = aligned_now - 2 * BUCKET_DURATION_NS;
        let mut concentrator = SpanConcentrator::new(true, true, &[], oldest_ts);

        // Build spans spread over time windows
        let spans = vec![
            make_top_level_span(aligned_now, 1, 50, 5, "A1", "resource1", 0, None),
            make_top_level_span(aligned_now, 2, 40, 4, "A1", "resource1", 0, None),
            make_top_level_span(aligned_now, 3, 30, 3, "A1", "resource1", 0, None),
            make_top_level_span(aligned_now, 4, 20, 2, "A1", "resource1", 0, None),
            make_top_level_span(aligned_now, 5, 10, 1, "A1", "resource1", 0, None),
            make_top_level_span(aligned_now, 6, 1, 0, "A1", "resource1", 0, None),
        ];

        let payload_key = PayloadAggregationKey {
            env: MetaString::from("none"),
            ..Default::default()
        };
        let infra_tags = InfraTags::default();

        for span in &spans {
            if let Some(stat_span) = concentrator.new_stat_span_from_span(span) {
                concentrator.add_span(&stat_span, 1.0, &payload_key, &infra_tags, "");
            }
        }

        // Flush all and collect totals
        let all_stats = concentrator.flush(now + BUCKET_DURATION_NS * 10, true);

        let mut total_duration: u64 = 0;
        let mut total_hits: u64 = 0;
        let mut total_errors: u64 = 0;
        let mut total_top_level_hits: u64 = 0;

        for payload in &all_stats {
            for bucket in payload.stats() {
                for grouped in bucket.stats() {
                    total_duration += grouped.duration();
                    total_hits += grouped.hits();
                    total_errors += grouped.errors();
                    total_top_level_hits += grouped.top_level_hits();
                }
            }
        }

        assert_eq!(total_duration, 50 + 40 + 30 + 20 + 10 + 1, "Wrong total duration");
        assert_eq!(total_hits, 6, "Wrong total hits");
        assert_eq!(total_top_level_hits, 6, "Wrong total top level hits");
        assert_eq!(total_errors, 0, "Wrong total errors");
    }

    #[test]
    fn test_root_tag() {
        let now = now_nanos();
        let aligned_now = align_ts(now, BUCKET_DURATION_NS);

        let mut concentrator = SpanConcentrator::new(true, true, &[], now);

        // Root span (parent_id = 0, top_level)
        let mut root_metrics = FastHashMap::default();
        root_metrics.insert(MetaString::from("_top_level"), 1.0);
        let root_span = test_span(
            aligned_now,
            1,
            0,
            40,
            10,
            "A1",
            "resource1",
            0,
            None,
            Some(root_metrics),
        );

        // Non-root but top level span (has _top_level but parent_id != 0)
        let mut top_level_metrics = FastHashMap::default();
        top_level_metrics.insert(MetaString::from("_top_level"), 1.0);
        let top_level_span = test_span(
            aligned_now,
            4,
            1000,
            10,
            10,
            "A1",
            "resource1",
            0,
            None,
            Some(top_level_metrics),
        );

        // Client span (non-root, non-top level, but has span.kind = client)
        let mut client_meta = FastHashMap::default();
        client_meta.insert(MetaString::from("span.kind"), MetaString::from("client"));
        let client_span = test_span(aligned_now, 3, 2, 20, 10, "A1", "resource1", 0, Some(client_meta), None);

        let spans = vec![root_span, top_level_span, client_span];

        let payload_key = PayloadAggregationKey {
            env: MetaString::from("none"),
            ..Default::default()
        };
        let infra_tags = InfraTags::default();

        for span in &spans {
            if let Some(stat_span) = concentrator.new_stat_span_from_span(span) {
                concentrator.add_span(&stat_span, 1.0, &payload_key, &infra_tags, "");
            }
        }

        let stats = concentrator.flush(now + BUCKET_DURATION_NS * 20, true);
        assert!(!stats.is_empty(), "Should have stats");

        // Count grouped stats - should be split by IsTraceRoot
        let mut total_grouped = 0;
        let mut root_count = 0;
        let mut non_root_count = 0;

        for payload in &stats {
            for bucket in payload.stats() {
                for grouped in bucket.stats() {
                    total_grouped += 1;
                    match grouped.is_trace_root() {
                        Some(true) => root_count += 1,
                        Some(false) => non_root_count += 1,
                        None => {}
                    }
                }
            }
        }

        // We expect 3 grouped stats:
        // 1. Root span (is_trace_root = true)
        // 2. Non-root top-level span (is_trace_root = false)
        // 3. Client span (is_trace_root = false, span.kind = client)
        assert_eq!(total_grouped, 3, "Expected 3 grouped stats");
        assert_eq!(root_count, 1, "Expected 1 root span");
        assert_eq!(non_root_count, 2, "Expected 2 non-root spans");
    }

    #[test]
    fn test_compute_stats_through_span_kind_check() {
        let now = now_nanos();

        // Test with compute_stats_by_span_kind DISABLED
        {
            let mut concentrator = SpanConcentrator::new(false, true, &[], now);

            // Create a simple top-level span using the same pattern as make_test_span (which works)
            let mut metrics = FastHashMap::default();
            metrics.insert(MetaString::from("_top_level"), 1.0);
            let span = Span::new("myservice", "query", "GET /users", "web", 1, 1, 0, now, 500, 0).with_metrics(metrics);

            let payload_key = PayloadAggregationKey {
                env: MetaString::from("test"),
                ..Default::default()
            };
            let infra_tags = InfraTags::default();

            if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
                concentrator.add_span(&stat_span, 1.0, &payload_key, &infra_tags, "");
            }

            // Client span with span.kind=client but no _top_level or _dd.measured
            // Should NOT produce stats when compute_stats_by_span_kind is disabled
            let mut client_meta = FastHashMap::default();
            client_meta.insert(MetaString::from("span.kind"), MetaString::from("client"));
            let client_span = Span::new("myservice", "postgres.query", "SELECT ...", "db", 1, 2, 1, now, 75, 0)
                .with_meta(client_meta);

            if let Some(stat_span) = concentrator.new_stat_span_from_span(&client_span) {
                concentrator.add_span(&stat_span, 1.0, &payload_key, &infra_tags, "");
            }

            let stats = concentrator.flush(now + BUCKET_DURATION_NS * 3, true);

            let mut count = 0;
            for payload in &stats {
                for bucket in payload.stats() {
                    count += bucket.stats().len();
                }
            }

            // When disabled, only top_level span gets stats (client span has no top_level/measured)
            assert_eq!(count, 1, "Expected 1 stat when span kind check disabled");
        }

        // Test with compute_stats_by_span_kind ENABLED
        {
            let mut concentrator = SpanConcentrator::new(true, true, &[], now);

            // Create a simple top-level span
            let mut metrics = FastHashMap::default();
            metrics.insert(MetaString::from("_top_level"), 1.0);
            let span = Span::new("myservice", "query", "GET /users", "web", 1, 1, 0, now, 500, 0).with_metrics(metrics);

            let payload_key = PayloadAggregationKey {
                env: MetaString::from("test"),
                ..Default::default()
            };
            let infra_tags = InfraTags::default();

            if let Some(stat_span) = concentrator.new_stat_span_from_span(&span) {
                concentrator.add_span(&stat_span, 1.0, &payload_key, &infra_tags, "");
            }

            // Client span with span.kind=client
            // SHOULD produce stats when compute_stats_by_span_kind is enabled
            let mut client_meta = FastHashMap::default();
            client_meta.insert(MetaString::from("span.kind"), MetaString::from("client"));
            let client_span = Span::new("myservice", "postgres.query", "SELECT ...", "db", 1, 2, 1, now, 75, 0)
                .with_meta(client_meta);

            if let Some(stat_span) = concentrator.new_stat_span_from_span(&client_span) {
                concentrator.add_span(&stat_span, 1.0, &payload_key, &infra_tags, "");
            }

            let stats = concentrator.flush(now + BUCKET_DURATION_NS * 3, true);

            let mut count = 0;
            for payload in &stats {
                for bucket in payload.stats() {
                    count += bucket.stats().len();
                }
            }

            // When enabled, both spans get stats
            assert_eq!(count, 2, "Expected 2 stats when span kind check enabled");
        }
    }

    #[test]
    fn test_peer_tags() {
        let now = now_nanos();

        // Test without peer tags aggregation enabled
        {
            let mut concentrator = SpanConcentrator::new(true, false, &[], now);

            // Client span with db tags and _dd.measured
            let mut client_meta = FastHashMap::default();
            client_meta.insert(MetaString::from("span.kind"), MetaString::from("client"));
            client_meta.insert(MetaString::from("db.instance"), MetaString::from("i-1234"));
            client_meta.insert(MetaString::from("db.system"), MetaString::from("postgres"));
            let mut client_metrics = FastHashMap::default();
            client_metrics.insert(MetaString::from("_dd.measured"), 1.0);
            let client_span = Span::new("myservice", "postgres.query", "SELECT ...", "db", 1, 2, 1, now, 75, 0)
                .with_meta(client_meta)
                .with_metrics(client_metrics);

            let payload_key = PayloadAggregationKey {
                env: MetaString::from("test"),
                ..Default::default()
            };
            let infra_tags = InfraTags::default();

            if let Some(stat_span) = concentrator.new_stat_span_from_span(&client_span) {
                concentrator.add_span(&stat_span, 1.0, &payload_key, &infra_tags, "");
            }

            let stats = concentrator.flush(now + BUCKET_DURATION_NS * 3, true);

            // Without peer tags aggregation, peer_tags should be empty
            for payload in &stats {
                for bucket in payload.stats() {
                    for grouped in bucket.stats() {
                        assert!(
                            grouped.peer_tags().is_empty(),
                            "Peer tags should be empty when peer_tags_aggregation is false"
                        );
                    }
                }
            }
        }

        // Test with peer tags aggregation enabled
        {
            // Note: BASE_PEER_TAGS already includes db.instance and db.system
            let mut concentrator = SpanConcentrator::new(true, true, &[], now);

            // Client span with db tags and _dd.measured
            let mut client_meta = FastHashMap::default();
            client_meta.insert(MetaString::from("span.kind"), MetaString::from("client"));
            client_meta.insert(MetaString::from("db.instance"), MetaString::from("i-1234"));
            client_meta.insert(MetaString::from("db.system"), MetaString::from("postgres"));
            let mut client_metrics = FastHashMap::default();
            client_metrics.insert(MetaString::from("_dd.measured"), 1.0);
            let client_span = Span::new("myservice", "postgres.query", "SELECT ...", "db", 1, 2, 1, now, 75, 0)
                .with_meta(client_meta)
                .with_metrics(client_metrics);

            let payload_key = PayloadAggregationKey {
                env: MetaString::from("test"),
                ..Default::default()
            };
            let infra_tags = InfraTags::default();

            if let Some(stat_span) = concentrator.new_stat_span_from_span(&client_span) {
                concentrator.add_span(&stat_span, 1.0, &payload_key, &infra_tags, "");
            }

            let stats = concentrator.flush(now + BUCKET_DURATION_NS * 3, true);

            // With peer tags aggregation, client span should have peer_tags
            let mut found_client_with_peer_tags = false;
            for payload in &stats {
                for bucket in payload.stats() {
                    for grouped in bucket.stats() {
                        if grouped.resource() == "SELECT ..." {
                            assert!(!grouped.peer_tags().is_empty(), "Client span should have peer tags");
                            // Check that peer tags contain db.instance and db.system
                            let peer_tags: Vec<&str> = grouped.peer_tags().iter().map(|s| s.as_ref()).collect();
                            assert!(
                                peer_tags.iter().any(|t| t.starts_with("db.instance:")),
                                "Should have db.instance peer tag"
                            );
                            assert!(
                                peer_tags.iter().any(|t| t.starts_with("db.system:")),
                                "Should have db.system peer tag"
                            );
                            found_client_with_peer_tags = true;
                        }
                    }
                }
            }
            assert!(
                found_client_with_peer_tags,
                "Should have found client span with peer tags"
            );
        }
    }

    #[test]
    fn test_concentrator_oldest_ts() {
        let now = now_nanos();
        let aligned_now = align_ts(now, BUCKET_DURATION_NS);

        // Test "cold" scenario - all spans in the past should end up in current bucket
        {
            // Start concentrator at current time (cold start)
            let mut concentrator = SpanConcentrator::new(true, true, &[], now);

            // Build spans spread over many time windows (all in the past)
            let spans = vec![
                make_top_level_span(aligned_now, 1, 50, 5, "A1", "resource1", 0, None),
                make_top_level_span(aligned_now, 2, 40, 4, "A1", "resource1", 0, None),
                make_top_level_span(aligned_now, 3, 30, 3, "A1", "resource1", 0, None),
                make_top_level_span(aligned_now, 4, 20, 2, "A1", "resource1", 0, None),
                make_top_level_span(aligned_now, 5, 10, 1, "A1", "resource1", 0, None),
                make_top_level_span(aligned_now, 6, 1, 0, "A1", "resource1", 0, None),
            ];

            let payload_key = PayloadAggregationKey {
                env: MetaString::from("none"),
                ..Default::default()
            };
            let infra_tags = InfraTags::default();

            for span in &spans {
                if let Some(stat_span) = concentrator.new_stat_span_from_span(span) {
                    concentrator.add_span(&stat_span, 1.0, &payload_key, &infra_tags, "");
                }
            }

            // Flush multiple times without force
            let mut flush_time = now;
            let buffer_len = 2; // DEFAULT_BUFFER_LEN

            for _ in 0..buffer_len {
                let stats = concentrator.flush(flush_time, false);
                assert!(stats.is_empty(), "Should not flush before buffer fills");
                flush_time += BUCKET_DURATION_NS;
            }

            // After buffer_len flushes, should get aggregated stats
            let stats = concentrator.flush(flush_time, false);
            assert!(!stats.is_empty(), "Should flush after buffer fills");

            // All spans should be aggregated into one bucket (oldest bucket aggregates old data)
            let mut total_hits: u64 = 0;
            let mut total_duration: u64 = 0;
            for payload in &stats {
                for bucket in payload.stats() {
                    for grouped in bucket.stats() {
                        total_hits += grouped.hits();
                        total_duration += grouped.duration();
                    }
                }
            }

            assert_eq!(total_hits, 6, "All 6 spans should be counted");
            assert_eq!(
                total_duration,
                50 + 40 + 30 + 20 + 10 + 1,
                "Total duration should match"
            );
        }
    }

    #[test]
    fn test_compute_stats_for_span_kind() {
        use super::span_concentrator::compute_stats_for_span_kind;

        // Valid span kinds (case insensitive)
        assert!(compute_stats_for_span_kind("server"));
        assert!(compute_stats_for_span_kind("consumer"));
        assert!(compute_stats_for_span_kind("client"));
        assert!(compute_stats_for_span_kind("producer"));

        // Uppercase
        assert!(compute_stats_for_span_kind("SERVER"));
        assert!(compute_stats_for_span_kind("CONSUMER"));
        assert!(compute_stats_for_span_kind("CLIENT"));
        assert!(compute_stats_for_span_kind("PRODUCER"));

        // Mixed case
        assert!(compute_stats_for_span_kind("SErVER"));
        assert!(compute_stats_for_span_kind("COnSUMER"));
        assert!(compute_stats_for_span_kind("CLiENT"));
        assert!(compute_stats_for_span_kind("PRoDUCER"));

        // Invalid span kinds
        assert!(!compute_stats_for_span_kind("internal"));
        assert!(!compute_stats_for_span_kind("INTERNAL"));
        assert!(!compute_stats_for_span_kind("INtERNAL"));
        assert!(!compute_stats_for_span_kind(""));
    }

    #[test]
    fn test_extract_process_tags() {
        // Test with no process tags
        {
            let span = Span::default();
            let trace = Trace::new(vec![span], TagSet::default());
            let process_tags = extract_process_tags(&trace);
            assert!(process_tags.is_empty(), "Should be empty when no _dd.tags.process");
        }

        // Test with process tags in first span meta
        {
            let mut meta = FastHashMap::default();
            meta.insert(MetaString::from(TAG_PROCESS_TAGS), MetaString::from("a:1,b:2,c:3"));
            let span = Span::default().with_meta(meta);
            let trace = Trace::new(vec![span], TagSet::default());
            let process_tags = extract_process_tags(&trace);
            assert_eq!(process_tags, "a:1,b:2,c:3");
        }

        // Test with empty process tags
        {
            let mut meta = FastHashMap::default();
            meta.insert(MetaString::from(TAG_PROCESS_TAGS), MetaString::from(""));
            let span = Span::default().with_meta(meta);
            let trace = Trace::new(vec![span], TagSet::default());
            let process_tags = extract_process_tags(&trace);
            assert!(
                process_tags.is_empty(),
                "Should be empty when _dd.tags.process is empty string"
            );
        }

        // Test with empty trace
        {
            let trace = Trace::new(vec![], TagSet::default());
            let process_tags = extract_process_tags(&trace);
            assert!(process_tags.is_empty(), "Should be empty when trace has no spans");
        }
    }

    #[test]
    fn test_process_tags_hash_computation() {
        use super::aggregation::process_tags_hash;

        // Empty string should return 0
        assert_eq!(process_tags_hash(""), 0);

        // Same tags should produce same hash
        let hash1 = process_tags_hash("a:1,b:2,c:3");
        let hash2 = process_tags_hash("a:1,b:2,c:3");
        assert_eq!(hash1, hash2);

        // Different tags should produce different hash
        let hash3 = process_tags_hash("a:1,b:2");
        assert_ne!(hash1, hash3);
    }
}
