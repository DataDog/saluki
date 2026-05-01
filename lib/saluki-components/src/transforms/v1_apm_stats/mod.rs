//! V1 APM stats transform.
//!
//! V1 counterpart to [`ApmStatsTransformConfiguration`][super::apm_stats::ApmStatsTransformConfiguration].
//! Aggregates `Event::V1Trace` events into time-bucketed statistics using the same
//! `SpanConcentrator` as the OTLP path, producing `Event::TraceStats` events.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::tags::TagSet;
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::{
        trace::v1::{V1AnyValue, V1KeyValue, V1Trace},
        trace_stats::{ClientStatsPayload, TraceStats},
        Event, EventType,
    },
    topology::OutputDefinition,
};
use saluki_env::{host::providers::BoxedHostProvider, EnvironmentProvider, HostProvider};
use saluki_error::{ErrorContext as _, GenericError};
use stringtheory::MetaString;
use tokio::{select, time::interval};
use tracing::{debug, error};

use crate::common::datadog::apm::ApmConfig;
use crate::transforms::apm_stats::{process_tags_hash, PayloadAggregationKey};
use crate::transforms::apm_stats::{InfraTags, SpanConcentrator};

/// Default flush interval for the V1 APM stats transform.
const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(10);

/// Tag key for process tags in span attributes.
const TAG_PROCESS_TAGS: &str = "_dd.tags.process";

/// Maximum number of `ClientGroupedStats` entries per `TraceStats` event.
const MAX_STATS_GROUPS_PER_EVENT: usize = 4000;

/// V1 APM stats transform configuration.
pub struct V1ApmStatsTransformConfiguration {
    apm_config: ApmConfig,
    default_hostname: Option<String>,
}

impl V1ApmStatsTransformConfiguration {
    /// Creates a new `V1ApmStatsTransformConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let apm_config = ApmConfig::from_configuration(config)?;
        Ok(Self {
            apm_config,
            default_hostname: None,
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
}

#[async_trait]
impl TransformBuilder for V1ApmStatsTransformConfiguration {
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

        Ok(Box::new(V1ApmStats {
            concentrator,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            agent_env: apm_config.default_env().clone(),
            agent_hostname: apm_config.hostname().clone(),
        }))
    }

    fn input_event_type(&self) -> EventType {
        EventType::V1Trace
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] =
            &[OutputDefinition::default_output(EventType::TraceStats)];
        OUTPUTS
    }
}

impl MemoryBounds for V1ApmStatsTransformConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<V1ApmStats>("component struct");
    }
}

struct V1ApmStats {
    concentrator: SpanConcentrator,
    flush_interval: Duration,
    agent_env: MetaString,
    agent_hostname: MetaString,
}

impl V1ApmStats {
    fn process_trace(&mut self, trace: &V1Trace) {
        let root_span = trace
            .chunk
            .spans
            .iter()
            .find(|s| s.parent_id == 0)
            .or_else(|| trace.chunk.spans.first());

        let trace_weight = root_span.map(v1_weight).unwrap_or(1.0);

        let process_tags = extract_v1_process_tags(trace);
        let payload_key = self.build_payload_key(trace, &process_tags);
        let infra_tags = build_infra_tags(trace, &process_tags);

        let origin = trace.chunk.origin.as_ref();

        for span in &trace.chunk.spans {
            self.concentrator
                .add_v1_span_if_eligible(span, trace_weight, &payload_key, &infra_tags, origin);
        }
    }

    fn build_payload_key(&self, trace: &V1Trace, process_tags: &str) -> PayloadAggregationKey {
        let root_span = trace
            .chunk
            .spans
            .iter()
            .find(|s| s.parent_id == 0)
            .or_else(|| trace.chunk.spans.first());

        // Span-level env overrides payload-level env which overrides agent default.
        let env = root_span
            .and_then(|s| get_v1_str_attr(&s.attributes, "env").filter(|s| !s.is_empty()))
            .map(MetaString::from)
            .unwrap_or_else(|| {
                if !trace.env.is_empty() {
                    trace.env.clone()
                } else {
                    self.agent_env.clone()
                }
            });

        let hostname = root_span
            .and_then(|s| get_v1_str_attr(&s.attributes, "_dd.hostname").filter(|s| !s.is_empty()))
            .map(MetaString::from)
            .unwrap_or_else(|| {
                if !trace.hostname.is_empty() {
                    trace.hostname.clone()
                } else {
                    self.agent_hostname.clone()
                }
            });

        let version = if !trace.app_version.is_empty() {
            trace.app_version.clone()
        } else {
            root_span
                .and_then(|s| get_v1_str_attr(&s.attributes, "version").filter(|s| !s.is_empty()))
                .map(MetaString::from)
                .unwrap_or_default()
        };

        let container_id = trace.container_id.clone();

        let git_commit_sha = root_span
            .and_then(|s| get_v1_str_attr(&s.attributes, "_dd.git.commit.sha").filter(|s| !s.is_empty()))
            .map(MetaString::from)
            .unwrap_or_default();

        let image_tag = root_span
            .and_then(|s| get_v1_str_attr(&s.attributes, "_dd.image_tag").filter(|s| !s.is_empty()))
            .map(MetaString::from)
            .unwrap_or_default();

        let lang = trace.language_name.clone();

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
impl Transform for V1ApmStats {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();

        let mut flush_ticker = interval(self.flush_interval);
        flush_ticker.tick().await;

        let mut final_flush = false;

        health.mark_ready();
        debug!("V1 APM Stats transform started.");

        loop {
            select! {
                _ = health.live() => continue,

                _ = flush_ticker.tick() => {
                    let stats_payloads = self.concentrator.flush(now_nanos(), final_flush);
                    if !stats_payloads.is_empty() {
                        debug!(stats_payloads = stats_payloads.len(), "Flushing V1 APM stats.");

                        let events = split_into_trace_stats(stats_payloads, MAX_STATS_GROUPS_PER_EVENT);
                        let dispatcher = context
                            .dispatcher()
                            .buffered()
                            .error_context("Default output should be available.")?;

                        if let Err(e) = dispatcher.send_all(events.into_iter().map(Event::TraceStats)).await {
                            error!(error = %e, "Failed to dispatch V1 APM stats events.");
                        }
                    }

                    if final_flush {
                        debug!("Final V1 APM stats flush complete.");
                        break;
                    }
                },

                maybe_events = context.events().next(), if !final_flush => {
                    match maybe_events {
                        Some(events) => {
                            for event in events {
                                if let Event::V1Trace(trace) = event {
                                    self.process_trace(&trace);
                                }
                            }
                        }
                        None => {
                            final_flush = true;
                            flush_ticker.reset_immediately();
                            debug!("V1 APM Stats transform stopping, triggering final flush...");
                        }
                    }
                },
            }
        }

        debug!("V1 APM Stats transform stopped.");
        Ok(())
    }
}

fn build_infra_tags(trace: &V1Trace, process_tags: &str) -> InfraTags {
    InfraTags::new(trace.container_id.clone(), TagSet::default(), process_tags)
}

fn extract_v1_process_tags(trace: &V1Trace) -> MetaString {
    // Check root span attributes first, then payload attributes.
    let root_span = trace
        .chunk
        .spans
        .iter()
        .find(|s| s.parent_id == 0)
        .or_else(|| trace.chunk.spans.first());

    if let Some(span) = root_span {
        if let Some(tags) = get_v1_str_attr(&span.attributes, TAG_PROCESS_TAGS) {
            if !tags.is_empty() {
                return MetaString::from(tags);
            }
        }
    }

    if let Some(tags) = get_v1_str_attr(&trace.payload_attributes, TAG_PROCESS_TAGS) {
        if !tags.is_empty() {
            return MetaString::from(tags);
        }
    }

    MetaString::empty()
}

fn v1_weight(span: &saluki_core::data_model::event::trace::v1::V1Span) -> f64 {
    const KEY_SAMPLING_RATE: &str = "_sample_rate";
    if let Some(rate) = span
        .attributes
        .iter()
        .find(|kv| kv.key.as_ref() == KEY_SAMPLING_RATE)
        .and_then(|kv| match &kv.value {
            V1AnyValue::Double(f) => Some(*f),
            V1AnyValue::Int(i) => Some(*i as f64),
            _ => None,
        })
    {
        if rate > 0.0 && rate <= 1.0 {
            return 1.0 / rate;
        }
    }
    1.0
}

fn get_v1_str_attr<'a>(attrs: &'a [V1KeyValue], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|kv| kv.key.as_ref() == key)
        .and_then(|kv| match &kv.value {
            V1AnyValue::String(s) => Some(s.as_ref()),
            _ => None,
        })
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn split_into_trace_stats(client_payloads: Vec<ClientStatsPayload>, max_entries_per_event: usize) -> Vec<TraceStats> {
    if client_payloads.is_empty() {
        return Vec::new();
    }

    let total = client_payloads
        .iter()
        .map(|p| p.stats().iter().map(|b| b.stats().len()).sum::<usize>())
        .sum::<usize>();
    if total <= max_entries_per_event {
        return vec![TraceStats::new(client_payloads)];
    }

    let mut events = Vec::new();
    let mut current_client_payloads = Vec::new();
    let mut current_event_len = 0;

    for mut client_payload in client_payloads {
        let client_payload_len = client_payload.stats().iter().map(|b| b.stats().len()).sum::<usize>();
        if current_event_len + client_payload_len <= max_entries_per_event {
            current_client_payloads.push(client_payload);
            current_event_len += client_payload_len;
            continue;
        }

        let mut current_client_stats_buckets = Vec::new();
        for mut client_stats_bucket in client_payload.take_stats() {
            let bucket_len = client_stats_bucket.stats().len();
            if current_event_len + bucket_len <= max_entries_per_event {
                current_client_stats_buckets.push(client_stats_bucket);
                current_event_len += bucket_len;
                continue;
            }

            let mut bucket_entries = client_stats_bucket.take_stats();
            while current_event_len + bucket_entries.len() > max_entries_per_event {
                let split_amount = max_entries_per_event - current_event_len;
                let split_point = bucket_entries.len() - split_amount;
                let split_entries = bucket_entries.split_off(split_point);

                let split_bucket = client_stats_bucket.clone().with_stats(split_entries);
                current_client_stats_buckets.push(split_bucket);

                let split_client_payload = client_payload
                    .clone()
                    .with_stats(std::mem::take(&mut current_client_stats_buckets));
                current_client_payloads.push(split_client_payload);

                events.push(TraceStats::new(std::mem::take(&mut current_client_payloads)));
                current_event_len = 0;
            }

            if !bucket_entries.is_empty() {
                current_event_len += bucket_entries.len();
                current_client_stats_buckets.push(client_stats_bucket.with_stats(bucket_entries));
            }
        }

        if !current_client_stats_buckets.is_empty() {
            current_client_payloads.push(client_payload.with_stats(current_client_stats_buckets));
        }
    }

    if !current_client_payloads.is_empty() {
        events.push(TraceStats::new(std::mem::take(&mut current_client_payloads)));
    }

    events
}

// Suppress the unused import warning for Arc — it's needed for TransformBuilder
// impls that may use workload providers in the future.
const _: Option<Arc<()>> = None;

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::trace::v1::{V1AnyValue, V1KeyValue, V1Span, V1Trace, V1TraceChunk};
    use stringtheory::MetaString;

    use crate::transforms::apm_stats::{SpanConcentrator, BUCKET_DURATION_NS};

    use super::*;

    fn make_v1_span(service: &str, resource: &str, parent_id: u64, is_top_level: bool) -> V1Span {
        let mut attributes = Vec::new();
        if is_top_level {
            attributes.push(V1KeyValue {
                key: MetaString::from("_top_level"),
                value: V1AnyValue::Double(1.0),
            });
        }
        V1Span {
            service: MetaString::from(service),
            name: MetaString::from("op"),
            resource: MetaString::from(resource),
            span_id: 1,
            parent_id,
            start: 1_000_000_000,
            duration: 100_000_000,
            error: false,
            attributes,
            span_type: MetaString::from("web"),
            links: Vec::new(),
            events: Vec::new(),
            env: MetaString::default(),
            version: MetaString::default(),
            component: MetaString::default(),
            kind: 0,
        }
    }

    fn make_v1_trace(spans: Vec<V1Span>) -> V1Trace {
        V1Trace {
            chunk: V1TraceChunk {
                priority: 1,
                origin: MetaString::default(),
                attributes: Vec::new(),
                spans,
                dropped_trace: false,
                trace_id_high: 0,
                trace_id_low: 1,
                sampling_mechanism: 0,
            },
            container_id: MetaString::default(),
            language_name: MetaString::from("rust"),
            language_version: MetaString::default(),
            tracer_version: MetaString::default(),
            runtime_id: MetaString::default(),
            env: MetaString::from("prod"),
            hostname: MetaString::from("test-host"),
            app_version: MetaString::from("1.0.0"),
            payload_attributes: Vec::new(),
            client_dropped_p0s_weight: 0.0,
        }
    }

    #[test]
    fn test_v1_process_trace_creates_stats() {
        let now = now_nanos();
        let concentrator = SpanConcentrator::new(true, true, &[], now);
        let mut transform = V1ApmStats {
            concentrator,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            agent_env: MetaString::from("none"),
            agent_hostname: MetaString::default(),
        };

        let span = make_v1_span("test-service", "test-resource", 0, true);
        let trace = make_v1_trace(vec![span]);

        transform.process_trace(&trace);

        let stats = transform.concentrator.flush(now + BUCKET_DURATION_NS * 2, true);
        assert!(!stats.is_empty(), "Expected stats to be produced for V1 trace");
    }

    #[test]
    fn test_v1_non_eligible_span_produces_no_stats() {
        let now = now_nanos();
        let concentrator = SpanConcentrator::new(false, false, &[], now);
        let mut transform = V1ApmStats {
            concentrator,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            agent_env: MetaString::from("none"),
            agent_hostname: MetaString::default(),
        };

        // Span with no _top_level, no _dd.measured, no span.kind, no compute_stats_by_span_kind
        let span = make_v1_span("test-service", "test-resource", 0, false);
        let trace = make_v1_trace(vec![span]);

        transform.process_trace(&trace);

        let stats = transform.concentrator.flush(now + BUCKET_DURATION_NS * 2, true);
        assert!(stats.is_empty(), "Non-eligible V1 span should produce no stats");
    }

    #[test]
    fn test_v1_payload_key_uses_trace_metadata() {
        let now = now_nanos();
        let concentrator = SpanConcentrator::new(true, true, &[], now);
        let transform = V1ApmStats {
            concentrator,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            agent_env: MetaString::from("agent-env"),
            agent_hostname: MetaString::from("agent-host"),
        };

        let span = make_v1_span("svc", "res", 0, true);
        let trace = make_v1_trace(vec![span]);

        let process_tags = "";
        let key = transform.build_payload_key(&trace, process_tags);

        assert_eq!(key.env.as_ref(), "prod");
        assert_eq!(key.hostname.as_ref(), "test-host");
        assert_eq!(key.version.as_ref(), "1.0.0");
        assert_eq!(key.lang.as_ref(), "rust");
    }
}
