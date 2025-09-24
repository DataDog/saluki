use std::{
    fmt::{self, Display, Write},
    num::NonZeroUsize,
    sync::LazyLock,
};

use async_trait::async_trait;
use ddsketch_agent::DDSketch;
use indexmap::map::Entry;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::{collections::FastIndexMap, iter::ReusableDeduplicator, strings::StringBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::{tags::Tag, Context};
use saluki_core::components::{destinations::*, ComponentContext};
use saluki_core::data_model::event::{
    metric::{Histogram, MetricValues},
    EventType,
};
use saluki_error::GenericError;
use saluki_io::net::{listener::ConnectionOrientedListener, ListenAddress};
use serde::Deserialize;
use stringtheory::{interning::FixedSizeInterner, MetaString};
use tokio::{select, sync::mpsc};
use tracing::debug;

mod api;
use self::api::spawn_api_server;

const CONTEXT_LIMIT: usize = 10_000;
const PAYLOAD_BUFFER_SIZE_LIMIT_BYTES: usize = 1024 * 1024;
const SUBPAYLOAD_BUFFER_SIZE_LIMIT_BYTES: usize = 128 * 1024;
const TAGS_BUFFER_SIZE_LIMIT_BYTES: usize = 2048;
const NAME_NORMALIZATION_BUFFER_SIZE: usize = 512;

macro_rules! quantile_strs {
    ($($q:literal),*) => { &[$(($q, stringify!($q))),*] };
}

const HISTOGRAM_QUANTILES: &[(f64, &str); 6] = quantile_strs!(0.1, 0.25, 0.5, 0.95, 0.99, 0.999);
const SUFFIX_BUCKET: Option<&str> = Some("_bucket");
const SUFFIX_COUNT: Option<&str> = Some("_count");
const SUFFIX_SUM: Option<&str> = Some("_sum");

// Histogram-related constants and pre-calculated buckets.
const TIME_HISTOGRAM_BUCKET_COUNT: usize = 30;
static TIME_HISTOGRAM_BUCKETS: LazyLock<[(f64, &'static str); TIME_HISTOGRAM_BUCKET_COUNT]> =
    LazyLock::new(|| histogram_buckets::<TIME_HISTOGRAM_BUCKET_COUNT>(0.000000128, 4.0));

const NON_TIME_HISTOGRAM_BUCKET_COUNT: usize = 30;
static NON_TIME_HISTOGRAM_BUCKETS: LazyLock<[(f64, &'static str); NON_TIME_HISTOGRAM_BUCKET_COUNT]> =
    LazyLock::new(|| histogram_buckets::<NON_TIME_HISTOGRAM_BUCKET_COUNT>(1.0, 2.0));

// SAFETY: This is obviously not zero.
const STRING_INTERNER_BYTES: NonZeroUsize = NonZeroUsize::new(131_072).unwrap();

type InternedStringBuilder = StringBuilder<FixedSizeInterner<1>>;

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
    /// Creates a new `PrometheusConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Returns the listen address for the Prometheus scrape endpoint.
    pub fn listen_address(&self) -> &ListenAddress {
        &self.listen_addr
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
        }))
    }
}

impl MemoryBounds for PrometheusConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<Prometheus>("component struct");

        builder
            .firm()
            // Even though our context map is really the Prometheus context to a map of context/value pairs, we're just
            // simplifying things here because the ratio of true "contexts" to Prometheus contexts should be very high,
            // high enough to make this a reasonable approximation.
            .with_map::<Context, PrometheusValue>("state map", CONTEXT_LIMIT)
            .with_fixed_amount("latest payload", PAYLOAD_BUFFER_SIZE_LIMIT_BYTES)
            .with_fixed_amount("payload buffer size", PAYLOAD_BUFFER_SIZE_LIMIT_BYTES)
            .with_fixed_amount("subpayload buffer size", SUBPAYLOAD_BUFFER_SIZE_LIMIT_BYTES)
            .with_fixed_amount("name normalization buffer size", NAME_NORMALIZATION_BUFFER_SIZE)
            .with_fixed_amount("tags buffer", TAGS_BUFFER_SIZE_LIMIT_BYTES);
    }
}

struct Prometheus {
    listener: ConnectionOrientedListener,
}

#[async_trait]
impl Destination for Prometheus {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let Self { listener } = *self;

        let interner = FixedSizeInterner::new(STRING_INTERNER_BYTES);
        let mut name_builder =
            StringBuilder::with_limit(NAME_NORMALIZATION_BUFFER_SIZE).with_interner(interner.clone());
        let mut tags_builder = StringBuilder::with_limit(TAGS_BUFFER_SIZE_LIMIT_BYTES).with_interner(interner);

        let mut latest_payload = String::new();
        let mut payload_builder = StringBuilder::with_limit(PAYLOAD_BUFFER_SIZE_LIMIT_BYTES);
        let mut subpayload_builder = StringBuilder::with_limit(SUBPAYLOAD_BUFFER_SIZE_LIMIT_BYTES);
        let mut metrics = FastIndexMap::default();

        let mut health = context.take_health_handle();

        let (payload_req_tx, mut payload_req_rx) = mpsc::channel(2);
        let (http_shutdown, mut http_error) = spawn_api_server(listener, payload_req_tx);
        health.mark_ready();

        debug!("Prometheus destination started.");

        let mut contexts = 0;
        let mut tags_deduplicator = ReusableDeduplicator::new();
        let mut render_required = false;

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => {
                    let events = match maybe_events {
                        Some(events) => events,
                        None => break,
                    };

                    // Process each metric event in the batch, either merging it with the existing value or
                    // inserting it for the first time.
                    for event in events {
                        let (context, values, _) = match event.try_into_metric() {
                            Some(metric) => metric.into_parts(),
                            None => continue,
                        };

                        // We don't support all internal metric types through Prometheus.
                        let prom_type = match PrometheusType::try_from_values(&values) {
                            Some(prom_type) => prom_type,
                            None => continue,
                        };

                        // Create an entry for the context if we don't already have one, obeying our configured context limit.
                        let grouped_values = match metrics.entry(context.name().clone()) {
                            Entry::Occupied(entry) => entry.into_mut(),
                            Entry::Vacant(entry) => {
                                let prom_name = match try_context_to_prom_name(&context, &mut name_builder) {
                                    Some(prom_name) => prom_name,
                                    None => continue,
                                };

                                let new_grouped_values = GroupedValues::new(prom_name, prom_type);

                                entry.insert(new_grouped_values)
                            }
                        };
                        if !grouped_values.update(&context, &values) {
                            if contexts >= CONTEXT_LIMIT {
                                debug!("Prometheus destination reached context limit. Skipping metric '{}'.", context.name());
                                continue
                            }
                            let prom_tags = match try_context_to_prom_tags(&context, &mut tags_builder, &mut tags_deduplicator) {
                                Some(prom_tags) => prom_tags,
                                None => continue,
                            };

                            contexts += 1;

                            grouped_values.insert(context, prom_tags, values);
                        }

                        render_required = true;
                    }
                },
                Some(payload_resp_tx) = payload_req_rx.recv() => {
                    // Check if the payload needs to be (re)rendered first.
                    if render_required {
                        render_required = false;

                        render_payload(&metrics, &mut payload_builder, &mut subpayload_builder);

                        latest_payload.clear();
                        latest_payload.push_str(payload_builder.as_str());
                    }

                    let _ = payload_resp_tx.send(latest_payload.clone());
                }
                maybe_error = &mut http_error => {
                    if let Some(error) = maybe_error {
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

/// Prometheus metric type.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum PrometheusType {
    Counter,
    Gauge,
    Histogram,
    Summary,
}

impl PrometheusType {
    fn try_from_values(values: &MetricValues) -> Option<Self> {
        match values {
            MetricValues::Counter(_) => Some(Self::Counter),
            MetricValues::Gauge(_) | MetricValues::Set(_) => Some(Self::Gauge),
            MetricValues::Histogram(_) => Some(Self::Histogram),
            MetricValues::Distribution(_) => Some(Self::Summary),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
            Self::Summary => "summary",
        }
    }
}

/// Prometheus metric value.
///
/// Holds the value of a Prometheus metric of the same type.
enum PrometheusValue {
    Counter(f64),
    Gauge(f64),
    Histogram(PrometheusHistogram),
    Summary(DDSketch),
}

/// Per-context values of a Prometheus metric.
///
/// Holds the values attached to a specific context (metric name + tags), grouped by the _original_
/// `Context` from which the values were extracted. Allows for efficient lookups of existing contexts.
struct GroupedValues {
    prom_name: MetaString,
    prom_type: PrometheusType,
    groups: FastIndexMap<Context, (MetaString, PrometheusValue)>,
}

impl GroupedValues {
    fn new(prom_name: MetaString, prom_type: PrometheusType) -> Self {
        Self {
            prom_name,
            prom_type,
            groups: FastIndexMap::default(),
        }
    }

    fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    fn len(&self) -> usize {
        self.groups.len()
    }

    fn name(&self) -> &str {
        &self.prom_name
    }

    fn type_str(&self) -> &str {
        self.prom_type.as_str()
    }

    fn get_new_prom_value(&self) -> PrometheusValue {
        match self.prom_type {
            PrometheusType::Counter => PrometheusValue::Counter(0.0),
            PrometheusType::Gauge => PrometheusValue::Gauge(0.0),
            PrometheusType::Histogram => PrometheusValue::Histogram(PrometheusHistogram::new(&self.prom_name)),
            PrometheusType::Summary => PrometheusValue::Summary(DDSketch::default()),
        }
    }

    /// Inserts the given context and values.
    ///
    /// This overwrites the values if the context already exists.
    fn insert(&mut self, context: Context, prom_tags: MetaString, values: MetricValues) {
        let mut new_prom_values = self.get_new_prom_value();
        merge_metric_values_with_prom_value(&values, &mut new_prom_values);

        self.groups.insert(context, (prom_tags, new_prom_values));
    }

    /// Updates the given context by merging in the provided values.
    ///
    /// Returns `true` if the context exists and was updated, `false` otherwise.
    fn update(&mut self, context: &Context, values: &MetricValues) -> bool {
        match self.groups.get_mut(context) {
            Some((_, prom_values)) => {
                merge_metric_values_with_prom_value(values, prom_values);
                true
            }
            None => false,
        }
    }
}

fn render_payload(
    metrics: &FastIndexMap<MetaString, GroupedValues>, payload_builder: &mut StringBuilder,
    subpayload_builder: &mut StringBuilder,
) {
    payload_builder.clear();

    let mut metrics_written = 0;
    let metrics_total = metrics.len();

    for (metric_name, grouped_values) in metrics {
        // Write this single metric out to the subpayload builder, which will include all of the individual contexts/tagsets.
        if write_metrics(grouped_values, subpayload_builder).is_err() {
            debug!(
                contexts_len = grouped_values.len(),
                "Failed to render contexts for metric '{}'. Skipping.", metric_name,
            );
            continue;
        }

        // Push the subpayload builder's string into the payload builder.
        if payload_builder.push_str(subpayload_builder.as_str()).is_none() {
            debug!(
                metrics_written,
                metrics_total,
                payload_len = payload_builder.len(),
                "Failed to include metric '{}' in payload due to insufficient space. Skipping remaining metrics.",
                metric_name
            );
            break;
        }

        metrics_written += 1;
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

fn write_metrics(values: &GroupedValues, buf: &mut StringBuilder) -> fmt::Result {
    buf.clear();

    if values.is_empty() {
        debug!("No contexts for metric '{}'. Skipping.", values.name());
        return Ok(());
    }

    let metric_name = values.name();

    // Write HELP if available.
    if let Some(help_text) = get_help_text(metric_name) {
        writeln!(buf, "# HELP {} {}", metric_name, help_text)?;
    }

    // Write the metric header.
    writeln!(buf, "# TYPE {} {}", metric_name, values.type_str())?;

    for (_, (tags, value)) in &values.groups {
        // Write the metric value itself.
        match value {
            PrometheusValue::Counter(value) | PrometheusValue::Gauge(value) => {
                // No metric type-specific tags for counters or gauges, so just write them straight out.
                write_metric_line(buf, metric_name, None, tags, None, *value)?;
            }
            PrometheusValue::Histogram(hist) => {
                // Write the histogram buckets.
                for (le, count) in hist.buckets() {
                    write_metric_line(buf, metric_name, SUFFIX_BUCKET, tags, Some(("le", le)), count)?;
                }

                // Write the final bucket -- the +Inf bucket -- which is just equal to the count of the histogram.
                write_metric_line(buf, metric_name, SUFFIX_BUCKET, tags, Some(("le", "+Inf")), hist.count)?;

                // Write the histogram sum and count.
                write_metric_line(buf, metric_name, SUFFIX_SUM, tags, None, hist.sum)?;
                write_metric_line(buf, metric_name, SUFFIX_COUNT, tags, None, hist.count)?;
            }
            PrometheusValue::Summary(sketch) => {
                // We take a fixed set of quantiles from the sketch, which is hard-coded but should generally represent
                // the quantiles people generally care about.
                for (q, q_str) in HISTOGRAM_QUANTILES {
                    let q_value = sketch.quantile(*q).unwrap_or_default();
                    write_metric_line(buf, metric_name, None, tags, Some(("quantile", q_str)), q_value)?;
                }

                write_metric_line(buf, metric_name, SUFFIX_SUM, tags, None, sketch.sum().unwrap_or(0.0))?;
                write_metric_line(buf, metric_name, SUFFIX_COUNT, tags, None, sketch.count())?;
            }
        }
    }

    Ok(())
}

fn write_metric_line<N: Display>(
    builder: &mut StringBuilder, metric_name: &str, suffix: Option<&str>, primary_tags: &str,
    secondary_tag: Option<(&str, &str)>, value: N,
) -> fmt::Result {
    // We handle some different things here:
    // - the metric name can be suffixed (used for things like `_bucket`, `_count`, `_sum`,  in histograms and summaries)
    // - writing out the "primary" set of tags
    // - passing in a single tag key/value pair (used for `le` in histograms, or `quantile` in summaries)

    let has_tags = !primary_tags.is_empty() || secondary_tag.is_some();

    write!(builder, "{metric_name}")?;
    if let Some(suffix) = suffix {
        write!(builder, "{suffix}")?;
    }

    if has_tags {
        write!(builder, "{{{primary_tags}")?;

        if let Some((tag_key, tag_value)) = secondary_tag {
            if !primary_tags.is_empty() {
                builder.push(',').ok_or(fmt::Error)?;
            }

            write!(builder, "{tag_key}=\"{tag_value}\"")?;
        }

        write!(builder, "}}")?;
    }

    writeln!(builder, " {value}")
}

fn try_context_to_prom_name(context: &Context, builder: &mut InternedStringBuilder) -> Option<MetaString> {
    match normalize_metric_name(context, builder) {
        Some(name) => Some(name),
        None => {
            debug!(
                "Failed to normalize metric name '{}'. Name either too long or interner is full.",
                context.name()
            );
            None
        }
    }
}

fn try_context_to_prom_tags(
    context: &Context, builder: &mut InternedStringBuilder, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> Option<MetaString> {
    match format_tags(context, builder, tags_deduplicator) {
        Some(tags) => Some(tags),
        None => {
            debug!(
                "Failed to format tags for metric '{}'. Tags either too long or interner is full.",
                context.name()
            );
            None
        }
    }
}

fn merge_metric_values_with_prom_value(values: &MetricValues, prom_value: &mut PrometheusValue) {
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
                prom_histogram.merge_histogram(value);
            }
        }
        (MetricValues::Distribution(distribution_values), PrometheusValue::Summary(prom_summary)) => {
            for (_, value) in distribution_values {
                prom_summary.merge(value);
            }
        }
        _ => panic!("Mismatched metric types"),
    }
}

fn normalize_metric_name(context: &Context, builder: &mut InternedStringBuilder) -> Option<MetaString> {
    builder.clear();

    // Normalize the metric name to a valid Prometheus metric name.
    for (i, c) in context.name().chars().enumerate() {
        if i == 0 && is_valid_name_start_char(c) || i != 0 && is_valid_name_char(c) {
            builder.push(c)?;
        } else {
            // Convert periods to a set of two underscores, and anything else to a single underscore.
            //
            // This lets us ensure that the normal separators we use in metrics (periods) are converted in a way
            // where they can be distinguished on the collector side to potentially reconstitute them back to their
            // original form.
            builder.push_str(if c == '.' { "__" } else { "_" })?;
        }
    }

    builder.try_intern()
}

fn format_tags(
    context: &Context, builder: &mut InternedStringBuilder, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> Option<MetaString> {
    builder.clear();

    let chained_tags = context.tags().into_iter().chain(context.origin_tags());
    let deduplicated_tags = tags_deduplicator.deduplicated(chained_tags);

    for tag in deduplicated_tags {
        // If we're not the first tag to be written, add a comma to separate the tags.
        if !builder.is_empty() {
            builder.push(',')?;
        }

        let tag_name = tag.name();
        let tag_value = match tag.value() {
            Some(value) => value,
            None => {
                debug!("Skipping bare tag.");
                continue;
            }
        };

        builder.push_str(tag_name)?;
        builder.push_str("=\"")?;
        builder.push_str(tag_value)?;
        builder.push('"')?;
    }

    builder.try_intern()
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
