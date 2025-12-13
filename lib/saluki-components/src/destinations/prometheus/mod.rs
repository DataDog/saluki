use std::{
    convert::Infallible,
    fmt::Write as _,
    num::NonZeroUsize,
    sync::{Arc, LazyLock},
};

use async_trait::async_trait;
use ddsketch_agent::DDSketch;
use http::{Request, Response};
use hyper::{body::Incoming, service::service_fn};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::{collections::FastIndexMap, iter::ReusableDeduplicator};
use saluki_context::{tags::Tag, Context};
use saluki_core::components::{destinations::*, ComponentContext};
use saluki_core::data_model::event::{
    metric::{Histogram, Metric, MetricValues},
    EventType,
};
use saluki_error::GenericError;
use saluki_io::net::{
    listener::ConnectionOrientedListener,
    server::http::{ErrorHandle, HttpServer, ShutdownHandle},
    ListenAddress,
};
use serde::Deserialize;
use stringtheory::{
    interning::{FixedSizeInterner, Interner as _},
    MetaString,
};
use tokio::{select, sync::RwLock};
use tracing::debug;

const CONTEXT_LIMIT: usize = 10_000;
const PAYLOAD_SIZE_LIMIT_BYTES: usize = 1024 * 1024;
const PAYLOAD_BUFFER_SIZE_LIMIT_BYTES: usize = 128 * 1024;
const TAGS_BUFFER_SIZE_LIMIT_BYTES: usize = 2048;
const NAME_NORMALIZATION_BUFFER_SIZE: usize = 512;

// Histogram-related constants and pre-calculated buckets.
const TIME_HISTOGRAM_BUCKET_COUNT: usize = 30;
static TIME_HISTOGRAM_BUCKETS: LazyLock<[(f64, &'static str); TIME_HISTOGRAM_BUCKET_COUNT]> =
    LazyLock::new(|| histogram_buckets::<TIME_HISTOGRAM_BUCKET_COUNT>(0.000000128, 4.0));

const NON_TIME_HISTOGRAM_BUCKET_COUNT: usize = 30;
static NON_TIME_HISTOGRAM_BUCKETS: LazyLock<[(f64, &'static str); NON_TIME_HISTOGRAM_BUCKET_COUNT]> =
    LazyLock::new(|| histogram_buckets::<NON_TIME_HISTOGRAM_BUCKET_COUNT>(1.0, 2.0));

// SAFETY: This is obviously not zero.
const METRIC_NAME_STRING_INTERNER_BYTES: NonZeroUsize = NonZeroUsize::new(65536).unwrap();

/// Prometheus destination.
///
/// Exposes a Prometheus scrape endpoint that emits metrics in the Prometheus exposition format.
///
/// # Limits
///
/// - Number of contexts (unique series) is limited to 10,000.
/// - Maximum size of scrape payload response is ~1MiB.
///
/// # Missing
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
    /// Creates a new `PrometheusConfiguration` for the given listen address.
    pub fn from_listen_address(listen_addr: ListenAddress) -> Self {
        Self { listen_addr }
    }
}

#[async_trait]
impl DestinationBuilder for PrometheusConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(Prometheus {
            listener: ConnectionOrientedListener::from_listen_address(self.listen_addr.clone()).await?,
            metrics: FastIndexMap::default(),
            payload: Arc::new(RwLock::new(String::new())),
            payload_buffer: String::with_capacity(PAYLOAD_BUFFER_SIZE_LIMIT_BYTES),
            tags_buffer: String::with_capacity(TAGS_BUFFER_SIZE_LIMIT_BYTES),
            interner: FixedSizeInterner::new(METRIC_NAME_STRING_INTERNER_BYTES),
        }))
    }
}

impl MemoryBounds for PrometheusConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<Prometheus>("component struct")
            // This isn't _really_ bounded since the string buffer could definitely grow larger if the metric name was
            // larger, but the default buffer size is far beyond any typical metric name that it should almost never
            // grow beyond this initially allocated size.
            .with_fixed_amount("name normalization buffer size", NAME_NORMALIZATION_BUFFER_SIZE);

        builder
            .firm()
            // Even though our context map is really the Prometheus context to a map of context/value pairs, we're just
            // simplifying things here because the ratio of true "contexts" to Prometheus contexts should be very high,
            // high enough to make this a reasonable approximation.
            .with_map::<Context, PrometheusValue>("state map", CONTEXT_LIMIT)
            .with_fixed_amount("payload size", PAYLOAD_SIZE_LIMIT_BYTES)
            .with_fixed_amount("payload buffer", PAYLOAD_BUFFER_SIZE_LIMIT_BYTES)
            .with_fixed_amount("tags buffer", TAGS_BUFFER_SIZE_LIMIT_BYTES);
    }
}

struct Prometheus {
    listener: ConnectionOrientedListener,
    metrics: FastIndexMap<PrometheusContext, FastIndexMap<Context, PrometheusValue>>,
    payload: Arc<RwLock<String>>,
    payload_buffer: String,
    tags_buffer: String,
    interner: FixedSizeInterner<1>,
}

#[async_trait]
impl Destination for Prometheus {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let Self {
            listener,
            mut metrics,
            payload,
            mut payload_buffer,
            mut tags_buffer,
            interner,
        } = *self;

        let mut health = context.take_health_handle();

        let (http_shutdown, mut http_error) = spawn_prom_scrape_service(listener, Arc::clone(&payload));
        health.mark_ready();

        debug!("Prometheus destination started.");

        let mut contexts = 0;
        let mut name_buf = String::with_capacity(NAME_NORMALIZATION_BUFFER_SIZE);
        let mut tags_deduplicator = ReusableDeduplicator::new();

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        // Process each metric event in the batch, either merging it with the existing value or
                        // inserting it for the first time.
                        for event in events {
                            if let Some(metric) = event.try_into_metric() {
                                // Break apart our metric into its constituent parts, and then normalize it for
                                // Prometheus: adjust the name if necessary, figuring out the equivalent Prometheus
                                // metric type, and so on.
                                let prom_context = match into_prometheus_metric(&metric, &mut name_buf, &interner) {
                                    Some(prom_context) => prom_context,
                                    None => continue,
                                };

                                let (context, values, _) = metric.into_parts();

                                // Create an entry for the context if we don't already have one, obeying our configured context limit.
                                let existing_contexts = metrics.entry(prom_context.clone()).or_default();
                                match existing_contexts.get_mut(&context) {
                                    Some(existing_prom_value) => merge_metric_values_with_prom_value(values, existing_prom_value),
                                    None => {
                                        if contexts >= CONTEXT_LIMIT {
                                            debug!("Prometheus destination reached context limit. Skipping metric '{}'.", context.name());
                                            continue
                                        }

                                        let mut new_prom_value = get_prom_value_for_prom_context(&prom_context);
                                        merge_metric_values_with_prom_value(values, &mut new_prom_value);

                                        existing_contexts.insert(context, new_prom_value);
                                        contexts += 1;
                                    }
                                }
                            }
                        }

                        // Regenerate the scrape payload.
                        regenerate_payload(&metrics, &payload, &mut payload_buffer, &mut tags_buffer, &mut tags_deduplicator).await;
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
    metrics: &FastIndexMap<PrometheusContext, FastIndexMap<Context, PrometheusValue>>, payload: &Arc<RwLock<String>>,
    payload_buffer: &mut String, tags_buffer: &mut String, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) {
    let mut payload = payload.write().await;
    payload.clear();

    let mut metrics_written = 0;
    let metrics_total = metrics.len();

    for (prom_context, contexts) in metrics {
        if write_metrics(payload_buffer, tags_buffer, prom_context, contexts, tags_deduplicator) {
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

fn get_help_text(metric_name: &str) -> Option<&'static str> {
    // The HELP text for overlapped metrics MUST match the agent's HELP text exactly or else an error will occur on the
    // agent's side when parsing the metrics.
    match metric_name {
        "no_aggregation__flush" => Some("Count the number of flushes done by the no-aggregation pipeline worker"),
        "no_aggregation__processed" => {
            Some("Count the number of samples processed by the no-aggregation pipeline worker")
        }
        "aggregator__dogstatsd_contexts_by_mtype" => {
            Some("Count the number of dogstatsd contexts in the aggregator, by metric type")
        }
        "aggregator__flush" => Some("Number of metrics/service checks/events flushed"),
        "aggregator__dogstatsd_contexts_bytes_by_mtype" => {
            Some("Estimated count of bytes taken by contexts in the aggregator, by metric type")
        }
        "aggregator__dogstatsd_contexts" => Some("Count the number of dogstatsd contexts in the aggregator"),
        "aggregator__processed" => Some("Amount of metrics/services_checks/events processed by the aggregator"),
        "dogstatsd__processed" => Some("Count of service checks/events/metrics processed by dogstatsd"),
        "dogstatsd__packet_pool_get" => Some("Count of get done in the packet pool"),
        "dogstatsd__packet_pool_put" => Some("Count of put done in the packet pool"),
        "dogstatsd__packet_pool" => Some("Usage of the packet pool in dogstatsd"),
        _ => None,
    }
}

fn write_metrics(
    payload_buffer: &mut String, tags_buffer: &mut String, prom_context: &PrometheusContext,
    contexts: &FastIndexMap<Context, PrometheusValue>, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> bool {
    if contexts.is_empty() {
        debug!("No contexts for metric '{}'. Skipping.", prom_context.metric_name);
        return true;
    }

    payload_buffer.clear();

    // Write HELP if available
    if let Some(help_text) = get_help_text(prom_context.metric_name.as_ref()) {
        writeln!(payload_buffer, "# HELP {} {}", prom_context.metric_name, help_text).unwrap();
    }
    // Write the metric header.
    writeln!(
        payload_buffer,
        "# TYPE {} {}",
        prom_context.metric_name,
        prom_context.metric_type.as_str()
    )
    .unwrap();

    for (context, values) in contexts {
        if payload_buffer.len() > PAYLOAD_BUFFER_SIZE_LIMIT_BYTES {
            debug!("Payload buffer size limit exceeded. Additional contexts for this metric will be truncated.");
            break;
        }

        tags_buffer.clear();

        // Format/encode the tags.
        if !format_tags(tags_buffer, context, tags_deduplicator) {
            return false;
        }

        // Write the metric value itself.
        match values {
            PrometheusValue::Counter(value) | PrometheusValue::Gauge(value) => {
                // No metric type-specific tags for counters or gauges, so just write them straight out.
                payload_buffer.push_str(&prom_context.metric_name);
                if !tags_buffer.is_empty() {
                    payload_buffer.push('{');
                    payload_buffer.push_str(tags_buffer);
                    payload_buffer.push('}');
                }
                writeln!(payload_buffer, " {}", value).unwrap();
            }
            PrometheusValue::Histogram(histogram) => {
                // Write the histogram buckets.
                for (upper_bound_str, count) in histogram.buckets() {
                    write!(payload_buffer, "{}_bucket{{{}", &prom_context.metric_name, tags_buffer).unwrap();
                    if !tags_buffer.is_empty() {
                        payload_buffer.push(',');
                    }
                    writeln!(payload_buffer, "le=\"{}\"}} {}", upper_bound_str, count).unwrap();
                }

                // Write the final bucket -- the +Inf bucket -- which is just equal to the count of the histogram.
                write!(payload_buffer, "{}_bucket{{{}", &prom_context.metric_name, tags_buffer).unwrap();
                if !tags_buffer.is_empty() {
                    payload_buffer.push(',');
                }
                writeln!(payload_buffer, "le=\"+Inf\"}} {}", histogram.count).unwrap();

                // Write the histogram sum and count.
                write!(payload_buffer, "{}_sum", &prom_context.metric_name).unwrap();
                if !tags_buffer.is_empty() {
                    payload_buffer.push('{');
                    payload_buffer.push_str(tags_buffer);
                    payload_buffer.push('}');
                }
                writeln!(payload_buffer, " {}", histogram.sum).unwrap();

                write!(payload_buffer, "{}_count", &prom_context.metric_name).unwrap();
                if !tags_buffer.is_empty() {
                    payload_buffer.push('{');
                    payload_buffer.push_str(tags_buffer);
                    payload_buffer.push('}');
                }
                writeln!(payload_buffer, " {}", histogram.count).unwrap();
            }
            PrometheusValue::Summary(sketch) => {
                // We take a fixed set of quantiles from the sketch, which is hard-coded but should generally represent
                // the quantiles people generally care about.
                for quantile in [0.1, 0.25, 0.5, 0.95, 0.99, 0.999] {
                    let q_value = sketch.quantile(quantile).unwrap_or_default();

                    write!(payload_buffer, "{}{{{}", &prom_context.metric_name, tags_buffer).unwrap();
                    if !tags_buffer.is_empty() {
                        payload_buffer.push(',');
                    }
                    writeln!(payload_buffer, "quantile=\"{}\"}} {}", quantile, q_value).unwrap();
                }

                write!(payload_buffer, "{}_sum", &prom_context.metric_name).unwrap();
                if !tags_buffer.is_empty() {
                    payload_buffer.push('{');
                    payload_buffer.push_str(tags_buffer);
                    payload_buffer.push('}');
                }
                writeln!(payload_buffer, " {}", sketch.sum().unwrap_or_default()).unwrap();

                write!(payload_buffer, "{}_count", &prom_context.metric_name).unwrap();
                if !tags_buffer.is_empty() {
                    payload_buffer.push('{');
                    payload_buffer.push_str(tags_buffer);
                    payload_buffer.push('}');
                }
                writeln!(payload_buffer, " {}", sketch.count()).unwrap();
            }
        }
    }

    true
}

fn format_tags(tags_buffer: &mut String, context: &Context, tags_deduplicator: &mut ReusableDeduplicator<Tag>) -> bool {
    let mut has_tags = false;

    let chained_tags = context.tags().into_iter().chain(context.origin_tags());
    let deduplicated_tags = tags_deduplicator.deduplicated(chained_tags);

    for tag in deduplicated_tags {
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
            debug!("Tags buffer size limit exceeded. Tags may be missing from this metric.");
            return false;
        }

        write!(tags_buffer, "{}=\"{}\"", tag_name, tag_value).unwrap();
    }

    true
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum PrometheusType {
    Counter,
    Gauge,
    Histogram,
    Summary,
}

impl PrometheusType {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
            Self::Summary => "summary",
        }
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct PrometheusContext {
    metric_name: MetaString,
    metric_type: PrometheusType,
}

enum PrometheusValue {
    Counter(f64),
    Gauge(f64),
    Histogram(PrometheusHistogram),
    Summary(DDSketch),
}

fn into_prometheus_metric(
    metric: &Metric, name_buf: &mut String, interner: &FixedSizeInterner<1>,
) -> Option<PrometheusContext> {
    // Normalize the metric name first, since we might fail due to the interner being full.
    let metric_name = match normalize_metric_name(metric.context().name(), name_buf, interner) {
        Some(name) => name,
        None => {
            debug!(
                "Failed to intern normalized metric name. Skipping metric '{}'.",
                metric.context().name()
            );
            return None;
        }
    };

    let metric_type = match metric.values() {
        MetricValues::Counter(_) => PrometheusType::Counter,
        MetricValues::Gauge(_) | MetricValues::Set(_) => PrometheusType::Gauge,
        MetricValues::Histogram(_) => PrometheusType::Histogram,
        MetricValues::Distribution(_) => PrometheusType::Summary,
        _ => return None,
    };

    Some(PrometheusContext {
        metric_name,
        metric_type,
    })
}

fn get_prom_value_for_prom_context(prom_context: &PrometheusContext) -> PrometheusValue {
    match prom_context.metric_type {
        PrometheusType::Counter => PrometheusValue::Counter(0.0),
        PrometheusType::Gauge => PrometheusValue::Gauge(0.0),
        PrometheusType::Histogram => PrometheusValue::Histogram(PrometheusHistogram::new(&prom_context.metric_name)),
        PrometheusType::Summary => PrometheusValue::Summary(DDSketch::default()),
    }
}

fn merge_metric_values_with_prom_value(values: MetricValues, prom_value: &mut PrometheusValue) {
    match (values, prom_value) {
        (MetricValues::Counter(counter_values), PrometheusValue::Counter(prom_counter)) => {
            for (_, value) in counter_values {
                *prom_counter += value;
            }
        }
        (MetricValues::Gauge(gauge_values), PrometheusValue::Gauge(prom_gauge)) => {
            let latest_value = gauge_values
                .into_iter()
                .max_by_key(|(ts, _)| ts.map(|v| v.get()).unwrap_or_default())
                .map(|(_, value)| value)
                .unwrap_or_default();
            *prom_gauge = latest_value;
        }
        (MetricValues::Set(set_values), PrometheusValue::Gauge(prom_gauge)) => {
            let latest_value = set_values
                .into_iter()
                .max_by_key(|(ts, _)| ts.map(|v| v.get()).unwrap_or_default())
                .map(|(_, value)| value)
                .unwrap_or_default();
            *prom_gauge = latest_value;
        }
        (MetricValues::Histogram(histogram_values), PrometheusValue::Histogram(prom_histogram)) => {
            for (_, value) in histogram_values {
                prom_histogram.merge_histogram(&value);
            }
        }
        (MetricValues::Distribution(distribution_values), PrometheusValue::Summary(prom_summary)) => {
            for (_, value) in distribution_values {
                prom_summary.merge(&value);
            }
        }
        _ => panic!("Mismatched metric types"),
    }
}

fn normalize_metric_name(name: &str, name_buf: &mut String, interner: &FixedSizeInterner<1>) -> Option<MetaString> {
    name_buf.clear();

    // Normalize the metric name to a valid Prometheus metric name.
    for (i, c) in name.chars().enumerate() {
        if i == 0 && is_valid_name_start_char(c) || i != 0 && is_valid_name_char(c) {
            name_buf.push(c);
        } else {
            // Convert periods to a set of two underscores, and anything else to a single underscore.
            //
            // This lets us ensure that the normal separators we use in metrics (periods) are converted in a way
            // where they can be distinguished on the collector side to potentially reconstitute them back to their
            // original form.
            name_buf.push_str(if c == '.' { "__" } else { "_" });
        }
    }

    // Now try and intern the normalized name.
    interner.try_intern(name_buf).map(MetaString::from)
}

#[inline]
fn is_valid_name_start_char(c: char) -> bool {
    // Matches a regular expression of [a-zA-Z_:].
    c.is_ascii_alphabetic() || c == '_' || c == ':'
}

#[inline]
fn is_valid_name_char(c: char) -> bool {
    // Matches a regular expression of [a-zA-Z0-9_:].
    c.is_ascii_alphanumeric() || c == '_' || c == ':'
}

#[derive(Clone)]
struct PrometheusHistogram {
    sum: f64,
    count: u64,
    buckets: Vec<(f64, &'static str, u64)>,
}

impl PrometheusHistogram {
    fn new(metric_name: &str) -> Self {
        // Super hacky but effective way to decide when to switch to the time-oriented buckets.
        let base_buckets = if metric_name.ends_with("_seconds") {
            &TIME_HISTOGRAM_BUCKETS[..]
        } else {
            &NON_TIME_HISTOGRAM_BUCKETS[..]
        };

        let buckets = base_buckets
            .iter()
            .map(|(upper_bound, upper_bound_str)| (*upper_bound, *upper_bound_str, 0))
            .collect();

        Self {
            sum: 0.0,
            count: 0,
            buckets,
        }
    }

    fn merge_histogram(&mut self, histogram: &Histogram) {
        for sample in histogram.samples() {
            self.add_sample(sample.value.into_inner(), sample.weight);
        }
    }

    fn add_sample(&mut self, value: f64, weight: u64) {
        self.sum += value * weight as f64;
        self.count += weight;

        // Add the value to each bucket that it falls into, up to the maximum number of buckets.
        for (upper_bound, _, count) in &mut self.buckets {
            if value <= *upper_bound {
                *count += weight;
            }
        }
    }

    fn buckets(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        self.buckets
            .iter()
            .map(|(_, upper_bound_str, count)| (*upper_bound_str, *count))
    }
}

fn histogram_buckets<const N: usize>(base: f64, scale: f64) -> [(f64, &'static str); N] {
    // We generate a set of "log-linear" buckets: logarithmically spaced values which are then subdivided linearly.
    //
    // As an example, with base=2 and scale=4, we would get: 2, 5, 8, 20, 32, 80, 128, 320, 512, and so on.
    //
    // We calculate buckets in pairs, where the n-th pair is `i` and `j`, such that `i` is `base * scale^n` and `j` is
    // the midpoint between `i` and the next `i` (`base * scale^(n+1)`).

    let mut buckets = [(0.0, ""); N];

    let log_linear_buckets = std::iter::repeat(base).enumerate().flat_map(|(i, base)| {
        let pow = scale.powf(i as f64);
        let value = base * pow;

        let next_pow = scale.powf((i + 1) as f64);
        let next_value = base * next_pow;
        let midpoint = (value + next_value) / 2.0;

        [value, midpoint]
    });

    for (i, current_le) in log_linear_buckets.enumerate().take(N) {
        let (bucket_le, bucket_le_str) = &mut buckets[i];
        let current_le_str = format!("{}", current_le);

        *bucket_le = current_le;
        *bucket_le_str = current_le_str.leak();
    }

    buckets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_print() {
        println!("time buckets: {:?}", *TIME_HISTOGRAM_BUCKETS);
        println!("non-time buckets: {:?}", *NON_TIME_HISTOGRAM_BUCKETS);
    }

    #[test]
    fn prom_histogram_add_sample() {
        let sample1 = (0.25, 1);
        let sample2 = (1.0, 2);
        let sample3 = (2.0, 3);

        let mut histogram = PrometheusHistogram::new("time_metric_seconds");
        histogram.add_sample(sample1.0, sample1.1);
        histogram.add_sample(sample2.0, sample2.1);
        histogram.add_sample(sample3.0, sample3.1);

        let sample1_weighted_value = sample1.0 * sample1.1 as f64;
        let sample2_weighted_value = sample2.0 * sample2.1 as f64;
        let sample3_weighted_value = sample3.0 * sample3.1 as f64;
        let expected_sum = sample1_weighted_value + sample2_weighted_value + sample3_weighted_value;
        let expected_count = sample1.1 + sample2.1 + sample3.1;
        assert_eq!(histogram.sum, expected_sum);
        assert_eq!(histogram.count, expected_count);

        // Go through and make sure we have things in the right buckets.
        let mut expected_bucket_count = 0;
        for sample in [sample1, sample2, sample3] {
            for bucket in &histogram.buckets {
                // If we've finally hit a bucket that includes our sample value, it's count should be equal to or
                // greater than our expected bucket count when we account for the current sample.
                if sample.0 <= bucket.0 {
                    assert!(bucket.2 >= expected_bucket_count + sample.1);
                }
            }

            // Adjust the expected bucket count to fully account for the current sample before moving on.
            expected_bucket_count += sample.1;
        }
    }

    #[test]
    fn prom_get_help_text() {
        // Ensure that we catch when the help text changes for these metrics.
        assert_eq!(
            get_help_text("no_aggregation__flush"),
            Some("Count the number of flushes done by the no-aggregation pipeline worker")
        );
        assert_eq!(
            get_help_text("no_aggregation__processed"),
            Some("Count the number of samples processed by the no-aggregation pipeline worker")
        );
        assert_eq!(
            get_help_text("aggregator__dogstatsd_contexts_by_mtype"),
            Some("Count the number of dogstatsd contexts in the aggregator, by metric type")
        );
        assert_eq!(
            get_help_text("aggregator__flush"),
            Some("Number of metrics/service checks/events flushed")
        );
        assert_eq!(
            get_help_text("aggregator__dogstatsd_contexts_bytes_by_mtype"),
            Some("Estimated count of bytes taken by contexts in the aggregator, by metric type")
        );
        assert_eq!(
            get_help_text("aggregator__dogstatsd_contexts"),
            Some("Count the number of dogstatsd contexts in the aggregator")
        );
        assert_eq!(
            get_help_text("aggregator__processed"),
            Some("Amount of metrics/services_checks/events processed by the aggregator")
        );
    }
}
