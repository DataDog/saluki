use std::{convert::Infallible, fmt::Write as _, sync::Arc};

use async_trait::async_trait;
use http::{Request, Response};
use hyper::{body::Incoming, service::service_fn};
use indexmap::IndexMap;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::{Context, TagSet};
use saluki_core::components::destinations::*;
use saluki_error::GenericError;
use saluki_event::{metric::MetricValue, DataType};
use saluki_io::net::{
    listener::ConnectionOrientedListener,
    server::http::{ErrorHandle, HttpServer, ShutdownHandle},
    ListenAddress,
};
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::{select, sync::RwLock};
use tracing::debug;

const CONTEXT_LIMIT: usize = 1000;
const PAYLOAD_SIZE_LIMIT_BYTES: usize = 512 * 1024;
const PAYLOAD_BUFFER_SIZE_LIMIT_BYTES: usize = 16384;
const TAGS_BUFFER_SIZE_LIMIT_BYTES: usize = 1024;

/// Prometheus destination.
///
/// Exposes a Prometheus scrape endpoint that emits metrics in the Prometheus exposition format.
///
/// ## Limits
///
/// - Number of contexts (unique series) is limited to 1000.
/// - Maximum size of scrape payload response is ~512KB.
///
/// ## Missing
///
/// - no support for expiring metrics (which we don't really need because the only use for this destination at the
///   moment is internal metrics, which aren't dynamic since we don't use dynamic tags or have dynamic topology support,
///   but... you know, we'll eventually need this)
/// - full support for distributions (we can't convert a distribution to an aggregated histogram, and native histogram
///   support is still too fresh for most clients, so we simply expose aggregated summaries as a stopgap)
///
#[derive(Deserialize)]
pub struct PrometheusConfiguration {
    #[serde(rename = "prometheus_listen_addr")]
    listen_addr: ListenAddress,
}

impl PrometheusConfiguration {
    /// Creates a new `PrometheusConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
}

#[async_trait]
impl DestinationBuilder for PrometheusConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::Metric
    }

    async fn build(&self) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(Prometheus {
            listener: ConnectionOrientedListener::from_listen_address(self.listen_addr.clone()).await?,
            metrics: IndexMap::new(),
            contexts: 0,
            payload: Arc::new(RwLock::new(String::new())),
            payload_buffer: String::with_capacity(PAYLOAD_BUFFER_SIZE_LIMIT_BYTES),
            tags_buffer: String::with_capacity(TAGS_BUFFER_SIZE_LIMIT_BYTES),
        }))
    }
}

impl MemoryBounds for PrometheusConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<Prometheus>();
        builder
            .firm()
            // Even though our context map is really the Prometheus contest to a map of context/value pairs, we're just
            // simplifying things here because the ratio of true "contexts" to Prometheus contexts should be very high,
            // high enough to make this a reasonable approximation.
            .with_map::<Context, MetricValue>(CONTEXT_LIMIT)
            .with_fixed_amount(PAYLOAD_SIZE_LIMIT_BYTES)
            .with_fixed_amount(PAYLOAD_BUFFER_SIZE_LIMIT_BYTES)
            .with_fixed_amount(TAGS_BUFFER_SIZE_LIMIT_BYTES);
    }
}

struct Prometheus {
    listener: ConnectionOrientedListener,
    metrics: IndexMap<PrometheusMetricContext, IndexMap<Context, MetricValue>>,
    contexts: usize,
    payload: Arc<RwLock<String>>,
    payload_buffer: String,
    tags_buffer: String,
}

#[async_trait]
impl Destination for Prometheus {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), ()> {
        let Self {
            listener,
            mut metrics,
            contexts,
            payload,
            mut payload_buffer,
            mut tags_buffer,
        } = *self;

        let mut health = context.take_health_handle();

        let (http_shutdown, mut http_error) = spawn_prom_scrape_service(listener, Arc::clone(&payload));
        health.mark_ready();

        debug!("Prometheus destination started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        // Process each metric event in the batch, either merging it with the existing value or
                        // inserting it for the first time.
                        for event in events {
                            if let Some(metric) = event.try_into_metric() {
                                let (context, mut values, _) = metric.into_parts();

                                for (_, value) in values.take_values() {
                                    // Skip any metric types we can't handle.
                                    let value = match value {
                                        // Skip sets and rates, because we can't convert them to a Prometheus-compatible type.
                                        MetricValue::Rate { .. } | MetricValue::Set { .. } => continue,
                                        // Everything else is fine as-is.
                                        value => value,
                                    };

                                    // Generate a Prometheus-specific context, which is essentially the metric name + metric
                                    // type.
                                    //
                                    // We need to do this because we have to group all contexts with the same name to generate
                                    // their payload output as a group, as it's required by the Prometheus exposition specification.
                                    let prom_context = PrometheusMetricContext {
                                        metric_name: context.name().clone(),
                                        metric_type: PrometheusMetricType::from_value(&value),
                                    };

                                    let existing_contexts = metrics.entry(prom_context).or_insert_with(IndexMap::new);
                                    match existing_contexts.get_mut(&context) {
                                        Some(existing_value) => {
                                            existing_value.merge(value);
                                        },
                                        None => {
                                            if contexts >= CONTEXT_LIMIT {
                                                debug!("Prometheus destination reached context limit. Skipping metric '{}'.", context.name());
                                                continue
                                            }

                                            existing_contexts.insert(context.clone(), value);
                                            self.contexts += 1;
                                        },
                                    }
                                }
                            }
                        }

                        // Regenerate the scrape payload.
                        regenerate_payload(&metrics, &payload, &mut payload_buffer, &mut tags_buffer).await;
                    },
                    None => break,
                },
                error = &mut http_error => {
                    if let Some(error) = error {
                        debug!(%error, "HTTP server error.");
                    }
                    break;
                },
            }
        }

        // TODO: This should really be `DynamicShutdownCoordinator`-based so we can trigger shutdown _and_ wait until
        // all HTTP connections and the listener have finished.
        http_shutdown.shutdown();

        debug!("Prometheus destination stopped.");

        Ok(())
    }
}

fn spawn_prom_scrape_service(
    listener: ConnectionOrientedListener, payload: Arc<RwLock<String>>,
) -> (ShutdownHandle, ErrorHandle) {
    let service = service_fn(move |_: Request<Incoming>| {
        let payload = Arc::clone(&payload);
        async move {
            let payload = payload.read().await;
            Ok::<Response<String>, Infallible>(Response::new(payload.to_string()))
        }
    });

    let http_server = HttpServer::from_listener(listener, service);
    http_server.listen()
}

#[allow(clippy::mutable_key_type)]
async fn regenerate_payload(
    metrics: &IndexMap<PrometheusMetricContext, IndexMap<Context, MetricValue>>, payload: &Arc<RwLock<String>>,
    payload_buffer: &mut String, tags_buffer: &mut String,
) {
    let mut payload = payload.write().await;
    payload.clear();

    let mut metrics_written = 0;
    let metrics_total = metrics.len();

    for (prom_context, contexts) in metrics {
        if write_metrics(payload_buffer, tags_buffer, prom_context, contexts) {
            if payload.len() + payload_buffer.len() > PAYLOAD_SIZE_LIMIT_BYTES {
                debug!(
                    metrics_written,
                    metrics_total,
                    payload_len = payload.len(),
                    "Writing additional metrics would exceed payload size limit. Skipping remaining metrics."
                );
                break;
            }

            // If we've already written some metrics, add a newline between each grouping.
            if metrics_written > 0 {
                payload.push('\n');
            }

            payload.push_str(payload_buffer);

            metrics_written += 1;
        } else {
            debug!("Failed to write metric to payload. Continuing...");
        }
    }
}

fn write_metrics(
    payload_buffer: &mut String, tags_buffer: &mut String, prom_context: &PrometheusMetricContext,
    contexts: &IndexMap<Context, MetricValue>,
) -> bool {
    if contexts.is_empty() {
        debug!("No contexts for metric '{}'. Skipping.", prom_context.metric_name);
        return true;
    }

    payload_buffer.clear();

    // Write the metric header.
    writeln!(
        payload_buffer,
        "# TYPE {} {}",
        prom_context.metric_name,
        prom_context.metric_type.as_str()
    )
    .unwrap();

    for (context, value) in contexts {
        if payload_buffer.len() > PAYLOAD_BUFFER_SIZE_LIMIT_BYTES {
            debug!("Payload buffer size limit exceeded. Additional contexts for this metric will be truncated.");
            break;
        }

        tags_buffer.clear();

        // Format/encode the tags.
        if !format_tags(tags_buffer, context.tags()) {
            return false;
        }

        // Write the metric value itself.
        match value {
            MetricValue::Counter { value } | MetricValue::Gauge { value } => {
                // No metric type-specific tags for counters or gauges, so just write them straight out.
                payload_buffer.push_str(context.name());
                if !tags_buffer.is_empty() {
                    payload_buffer.push('{');
                    payload_buffer.push_str(tags_buffer);
                    payload_buffer.push('}');
                }
                writeln!(payload_buffer, " {}", value).unwrap();
            }
            MetricValue::Distribution { sketch } => {
                // We take a fixed set of quantiles from the sketch, which is hard-coded but should generally represent
                // the quantiles people generally care about.
                for quantile in [0.1, 0.25, 0.5, 0.95, 0.99, 0.999] {
                    let q_value = sketch.quantile(quantile).unwrap_or_default();

                    write!(payload_buffer, "{}{{{}", context.name(), tags_buffer).unwrap();
                    if !tags_buffer.is_empty() {
                        payload_buffer.push(',');
                    }
                    writeln!(payload_buffer, "quantile=\"{}\"}} {}", quantile, q_value).unwrap();
                }

                write!(payload_buffer, "{}_sum", context.name(),).unwrap();
                if !tags_buffer.is_empty() {
                    payload_buffer.push('{');
                    payload_buffer.push_str(tags_buffer);
                    payload_buffer.push('}');
                }
                writeln!(payload_buffer, " {}", sketch.sum().unwrap_or_default()).unwrap();

                write!(payload_buffer, "{}_count", context.name(),).unwrap();
                if !tags_buffer.is_empty() {
                    payload_buffer.push('{');
                    payload_buffer.push_str(tags_buffer);
                    payload_buffer.push('}');
                }
                writeln!(payload_buffer, " {}", sketch.count()).unwrap();
            }
            _ => unreachable!(),
        }
    }

    true
}

fn format_tags(tags_buffer: &mut String, tags: &TagSet) -> bool {
    let mut has_tags = false;

    for tag in tags {
        // If we're not the first tag to be written, add a comma to separate the tags.
        if has_tags {
            tags_buffer.push(',');
        }

        let tag_name = tag.name();
        let tag_value = match tag.value() {
            Some(value) => value,
            None => {
                debug!("Skipping bare tag.");
                continue;
            }
        };

        has_tags = true;

        // Can't exceed the tags buffer size limit: we calculate the addition as tag name/value length plus three bytes
        // to account for having to format it as `name="value",`.
        if tags_buffer.len() + tag_name.len() + tag_value.len() + 4 > TAGS_BUFFER_SIZE_LIMIT_BYTES {
            debug!("Tags for metric would exceed tags buffer size limit.");
            return false;
        }

        write!(tags_buffer, "{}=\"{}\"", tag_name, tag_value).unwrap();
    }

    true
}

#[derive(Eq, Hash, Ord, PartialEq, PartialOrd)]
enum PrometheusMetricType {
    Counter,
    Gauge,
    Summary,
}

impl PrometheusMetricType {
    fn from_value(value: &MetricValue) -> Self {
        match value {
            MetricValue::Counter { .. } => Self::Counter,
            MetricValue::Gauge { .. } => Self::Gauge,
            MetricValue::Distribution { .. } => Self::Summary,
            _ => unreachable!(),
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Summary => "summary",
        }
    }
}

#[derive(Eq, Hash, Ord, PartialEq, PartialOrd)]
struct PrometheusMetricContext {
    metric_name: MetaString,
    metric_type: PrometheusMetricType,
}
