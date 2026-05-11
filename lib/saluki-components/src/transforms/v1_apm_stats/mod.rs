//! V1 APM stats transform.
//!
//! Aggregates APM `Event::Trace` events (produced by the APM receiver source) into
//! time-bucketed statistics using the same `SpanConcentrator` as the OTLP path,
//! producing `Event::TraceStats` events.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::tags::TagSet;
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::{
        trace::Trace,
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
        EventType::Trace
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
    fn process_trace(&mut self, trace: &Trace) {
        let root_span = trace.spans().iter().find(|s| s.parent_id() == 0).or_else(|| trace.spans().first());

        let trace_weight = root_span.map(weight).unwrap_or(1.0);

        let process_tags = extract_process_tags(trace);
        let payload_key = self.build_payload_key(trace, &process_tags);
        let infra_tags = build_infra_tags(trace, &process_tags);

        let origin = trace.origin.as_ref();

        for span in trace.spans() {
            self.concentrator
                .add_span_if_eligible(span, trace_weight, &payload_key, &infra_tags, origin);
        }
    }

    fn build_payload_key(&self, trace: &Trace, process_tags: &str) -> PayloadAggregationKey {
        let root_span = trace.spans().iter().find(|s| s.parent_id() == 0).or_else(|| trace.spans().first());

        // Span-level env overrides payload-level env which overrides agent default.
        let env = root_span
            .and_then(|s| s.meta().get("env").filter(|s| !s.is_empty()))
            .map(|s| s.clone())
            .unwrap_or_else(|| {
                if !trace.env.is_empty() {
                    trace.env.clone()
                } else {
                    self.agent_env.clone()
                }
            });

        let hostname = root_span
            .and_then(|s| s.meta().get("_dd.hostname").filter(|s| !s.is_empty()))
            .map(|s| s.clone())
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
                .and_then(|s| s.meta().get("version").filter(|s| !s.is_empty()))
                .map(|s| s.clone())
                .unwrap_or_default()
        };

        let container_id = trace.container_id.clone();

        let git_commit_sha = root_span
            .and_then(|s| s.meta().get("_dd.git.commit.sha").filter(|s| !s.is_empty()))
            .map(|s| s.clone())
            .unwrap_or_default();

        let image_tag = root_span
            .and_then(|s| s.meta().get("_dd.image_tag").filter(|s| !s.is_empty()))
            .map(|s| s.clone())
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
                            let mut count = 0u32;
                            for event in events {
                                if let Event::Trace(trace) = event {
                                    count += 1;
                                    self.process_trace(&trace);
                                }
                            }
                            if count > 0 {
                                debug!(traces = count, "V1 APM stats processed buffer.");
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

fn build_infra_tags(trace: &Trace, process_tags: &str) -> InfraTags {
    InfraTags::new(trace.container_id.clone(), TagSet::default(), process_tags)
}

fn extract_process_tags(trace: &Trace) -> MetaString {
    let root_span = trace.spans().iter().find(|s| s.parent_id() == 0).or_else(|| trace.spans().first());

    if let Some(span) = root_span {
        if let Some(tags) = span.meta().get(TAG_PROCESS_TAGS) {
            if !tags.is_empty() {
                return tags.clone();
            }
        }
    }

    // Check trace-level attributes (merged from payload_attributes during APM source conversion).
    if let Some(saluki_core::data_model::event::trace::AttributeValue::String(tags)) =
        trace.attributes.get(TAG_PROCESS_TAGS)
    {
        if !tags.is_empty() {
            return tags.clone();
        }
    }

    MetaString::empty()
}

fn weight(span: &saluki_core::data_model::event::trace::Span) -> f64 {
    const KEY_SAMPLING_RATE: &str = "_sample_rate";
    if let Some(&rate) = span.metrics().get(KEY_SAMPLING_RATE) {
        if rate > 0.0 && rate <= 1.0 {
            return 1.0 / rate;
        }
    }
    1.0
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

#[cfg(test)]
mod tests {
    use saluki_common::collections::FastHashMap;
    use saluki_context::tags::TagSet;
    use saluki_core::data_model::event::trace::{Span, Trace};
    use stringtheory::MetaString;

    use crate::transforms::apm_stats::{SpanConcentrator, BUCKET_DURATION_NS};

    use super::*;

    fn make_span(service: &str, resource: &str, parent_id: u64, is_top_level: bool) -> Span {
        let mut metrics = FastHashMap::default();
        if is_top_level {
            metrics.insert(MetaString::from("_top_level"), 1.0);
        }
        Span::new(service, "op", resource, "web", 0, 1, parent_id, 1_000_000_000, 100_000_000, 0)
            .with_metrics(Some(metrics))
    }

    fn make_trace(spans: Vec<Span>) -> Trace {
        let mut trace = Trace::new(spans, TagSet::default());
        trace.priority = Some(1);
        trace.language_name = MetaString::from("rust");
        trace.env = MetaString::from("prod");
        trace.hostname = MetaString::from("test-host");
        trace.app_version = MetaString::from("1.0.0");
        trace
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

        let span = make_span("test-service", "test-resource", 0, true);
        let trace = make_trace(vec![span]);

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

        let span = make_span("test-service", "test-resource", 0, false);
        let trace = make_trace(vec![span]);

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

        let span = make_span("svc", "res", 0, true);
        let trace = make_trace(vec![span]);

        let process_tags = "";
        let key = transform.build_payload_key(&trace, process_tags);

        assert_eq!(key.env.as_ref(), "prod");
        assert_eq!(key.hostname.as_ref(), "test-host");
        assert_eq!(key.version.as_ref(), "1.0.0");
        assert_eq!(key.lang.as_ref(), "rust");
    }
}
