//! Prometheus destination.
//!
//! # Missing
//!
//! - Use `ShutdownCoordinator::shutdown_and_wait` so shutdown can be triggered and all HTTP connections can drain.

use std::{
    convert::Infallible,
    num::NonZeroUsize,
    sync::{Arc, LazyLock},
};

use async_trait::async_trait;
use ddsketch::DDSketch;
use http::{Request, Response, StatusCode};
use hyper::{body::Incoming, service::service_fn};
use prometheus_exposition::{MetricType, PrometheusRenderer};
use saluki_common::{collections::FastIndexMap, iter::ReusableDeduplicator, sync::shutdown::ShutdownCoordinator};
use saluki_context::{tags::Tag, Context};
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::components::{destinations::*, ComponentContext};
use saluki_core::data_model::event::{
    metric::{Histogram, Metric, MetricValues},
    EventType,
};
use saluki_error::GenericError;
use saluki_io::net::{
    listener::ConnectionOrientedListener,
    server::http::{ErrorHandle, HttpServer},
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
const TAGS_BUFFER_SIZE_LIMIT_BYTES: usize = 2048;
const RAW_METRICS_PATH: &str = "/metrics";
const LEGACY_RAW_METRICS_PATH: &str = "/";

// Histogram-related constants and pre-calculated buckets.
const TIME_HISTOGRAM_BUCKET_COUNT: usize = 30;
static TIME_HISTOGRAM_BUCKETS: LazyLock<[(f64, &'static str); TIME_HISTOGRAM_BUCKET_COUNT]> =
    LazyLock::new(|| histogram_buckets::<TIME_HISTOGRAM_BUCKET_COUNT>(0.000000128, 4.0));

const NON_TIME_HISTOGRAM_BUCKET_COUNT: usize = 30;
static NON_TIME_HISTOGRAM_BUCKETS: LazyLock<[(f64, &'static str); NON_TIME_HISTOGRAM_BUCKET_COUNT]> =
    LazyLock::new(|| histogram_buckets::<NON_TIME_HISTOGRAM_BUCKET_COUNT>(1.0, 2.0));

// SAFETY: This is obviously not zero.
const METRIC_NAME_STRING_INTERNER_BYTES: NonZeroUsize = NonZeroUsize::new(65536).unwrap();

/// Provides a Prometheus scrape payload for an additional route.
pub trait PrometheusPayloadProvider: Send + Sync {
    /// Renders the current Prometheus text payload.
    fn render_payload(&self) -> String;
}

impl<F> PrometheusPayloadProvider for F
where
    F: Fn() -> String + Send + Sync,
{
    fn render_payload(&self) -> String {
        self()
    }
}

#[derive(Clone)]
struct PrometheusAdditionalRoute {
    path: String,
    provider: Arc<dyn PrometheusPayloadProvider>,
}

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

    #[serde(skip)]
    additional_routes: Vec<PrometheusAdditionalRoute>,
}

impl PrometheusConfiguration {
    /// Creates a new `PrometheusConfiguration` for the given listen address.
    pub fn from_listen_address(listen_addr: ListenAddress) -> Self {
        Self {
            listen_addr,
            additional_routes: Vec::new(),
        }
    }

    /// Adds an additional scrape route backed by the given payload provider.
    pub fn with_additional_route(
        mut self, path: impl Into<String>, provider: Arc<dyn PrometheusPayloadProvider>,
    ) -> Self {
        self.additional_routes.push(PrometheusAdditionalRoute {
            path: path.into(),
            provider,
        });
        self
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
            additional_routes: self.additional_routes.clone(),
            metrics: FastIndexMap::default(),
            payload: Arc::new(RwLock::new(String::new())),
            renderer: PrometheusRenderer::new(),
            interner: FixedSizeInterner::new(METRIC_NAME_STRING_INTERNER_BYTES),
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
            .with_fixed_amount("payload size", PAYLOAD_SIZE_LIMIT_BYTES)
            .with_fixed_amount("tags buffer", TAGS_BUFFER_SIZE_LIMIT_BYTES);
    }
}

struct Prometheus {
    listener: ConnectionOrientedListener,
    additional_routes: Vec<PrometheusAdditionalRoute>,
    metrics: FastIndexMap<PrometheusContext, FastIndexMap<Context, PrometheusValue>>,
    payload: Arc<RwLock<String>>,
    renderer: PrometheusRenderer,
    interner: FixedSizeInterner<1>,
}

#[async_trait]
impl Destination for Prometheus {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let Self {
            listener,
            additional_routes,
            mut metrics,
            payload,
            mut renderer,
            interner,
        } = *self;

        let mut health = context.take_health_handle();

        let (http_shutdown, mut http_error) =
            spawn_prom_scrape_service(listener, Arc::clone(&payload), additional_routes);
        health.mark_ready();

        debug!("Prometheus destination started.");

        let mut contexts = 0;
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
                                let prom_context = match into_prometheus_metric(&metric, &mut renderer, &interner) {
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
                        regenerate_payload(&metrics, &payload, &mut renderer, &mut tags_deduplicator).await;
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

        // TODO: This should really use `ShutdownCoordinator::shutdown_and_wait` so we can trigger shutdown _and_ wait
        // until all HTTP connections and the listener have finished.
        http_shutdown.shutdown();

        debug!("Prometheus destination stopped.");

        Ok(())
    }
}

fn spawn_prom_scrape_service(
    listener: ConnectionOrientedListener, payload: Arc<RwLock<String>>,
    additional_routes: Vec<PrometheusAdditionalRoute>,
) -> (ShutdownCoordinator, ErrorHandle) {
    let additional_routes = Arc::new(additional_routes);
    let service = service_fn(move |req: Request<Incoming>| {
        let payload = Arc::clone(&payload);
        let additional_routes = Arc::clone(&additional_routes);
        async move {
            Ok::<_, Infallible>(build_scrape_response(req.uri().path(), &payload, additional_routes.as_ref()).await)
        }
    });

    let http_server = HttpServer::from_listener(listener, service);
    http_server.listen()
}

async fn build_scrape_response(
    path: &str, payload: &Arc<RwLock<String>>, additional_routes: &[PrometheusAdditionalRoute],
) -> Response<axum::body::Body> {
    if path == RAW_METRICS_PATH || path == LEGACY_RAW_METRICS_PATH {
        let payload = payload.read().await;
        return Response::new(axum::body::Body::from(payload.to_string()));
    }

    if let Some(route) = additional_routes.iter().find(|route| route.path == path) {
        return Response::new(axum::body::Body::from(route.provider.render_payload()));
    }

    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(axum::body::Body::empty())
        .expect("response builder should accept static status and empty body")
}

#[allow(clippy::mutable_key_type)]
async fn regenerate_payload(
    metrics: &FastIndexMap<PrometheusContext, FastIndexMap<Context, PrometheusValue>>, payload: &Arc<RwLock<String>>,
    renderer: &mut PrometheusRenderer, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) {
    renderer.clear();

    for (prom_context, contexts) in metrics {
        if !write_metrics(renderer, prom_context, contexts, tags_deduplicator) {
            debug!("Failed to write metric to payload. Continuing...");
            continue;
        }

        if renderer.output().len() > PAYLOAD_SIZE_LIMIT_BYTES {
            debug!(
                payload_len = renderer.output().len(),
                "Payload size limit exceeded. Skipping remaining metrics."
            );
            break;
        }
    }

    let mut payload = payload.write().await;
    payload.clear();
    payload.push_str(renderer.output());
}

fn write_metrics(
    renderer: &mut PrometheusRenderer, prom_context: &PrometheusContext,
    contexts: &FastIndexMap<Context, PrometheusValue>, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> bool {
    if contexts.is_empty() {
        debug!("No contexts for metric '{}'. Skipping.", prom_context.metric_name);
        return true;
    }

    renderer.begin_group(&prom_context.metric_name, prom_context.metric_type, None);

    for (context, values) in contexts {
        let labels = match collect_tags(context, tags_deduplicator) {
            Some(labels) => labels,
            None => return false,
        };

        match values {
            PrometheusValue::Counter(value) | PrometheusValue::Gauge(value) => {
                renderer.write_gauge_or_counter_series(labels, *value);
            }
            PrometheusValue::Histogram(histogram) => {
                renderer.write_histogram_series(labels, histogram.buckets(), histogram.sum, histogram.count);
            }
            PrometheusValue::Summary(sketch) => {
                let quantiles = [0.1, 0.25, 0.5, 0.95, 0.99, 0.999]
                    .into_iter()
                    .map(|q| (q, sketch.quantile(q).unwrap_or_default()));

                renderer.write_summary_series(labels, quantiles, sketch.sum().unwrap_or_default(), sketch.count());
            }
        }
    }

    renderer.finish_group();
    true
}

/// Collects tags from a context into key-value pairs suitable for the renderer.
fn collect_tags<'a>(
    context: &'a Context, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> Option<Vec<(&'a str, &'a str)>> {
    let mut labels = Vec::new();
    let mut total_bytes = 0;

    let chained_tags = context.tags().into_iter().chain(context.origin_tags());
    let deduplicated_tags = tags_deduplicator.deduplicated(chained_tags);

    for tag in deduplicated_tags {
        let tag_name = tag.name();
        let tag_value = match tag.value() {
            Some(value) => value,
            None => {
                debug!("Skipping bare tag.");
                continue;
            }
        };

        // Can't exceed the tags buffer size limit: we calculate the addition as tag name/value length plus three bytes
        // to account for having to format it as `name="value",`.
        total_bytes += tag_name.len() + tag_value.len() + 4;
        if total_bytes > TAGS_BUFFER_SIZE_LIMIT_BYTES {
            debug!("Tags buffer size limit exceeded. Tags may be missing from this metric.");
            return None;
        }

        labels.push((tag_name, tag_value));
    }

    Some(labels)
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct PrometheusContext {
    metric_name: MetaString,
    metric_type: MetricType,
}

enum PrometheusValue {
    Counter(f64),
    Gauge(f64),
    Histogram(PrometheusHistogram),
    Summary(DDSketch),
}

fn into_prometheus_metric(
    metric: &Metric, renderer: &mut PrometheusRenderer, interner: &FixedSizeInterner<1>,
) -> Option<PrometheusContext> {
    // Normalize the metric name using the renderer, then intern it.
    let normalized = renderer.normalize_metric_name(metric.context().name());
    let metric_name = match interner.try_intern(normalized).map(MetaString::from) {
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
        MetricValues::Counter(_) => MetricType::Counter,
        MetricValues::Gauge(_) | MetricValues::Set(_) => MetricType::Gauge,
        MetricValues::Histogram(_) => MetricType::Histogram,
        MetricValues::Distribution(_) => MetricType::Summary,
        _ => return None,
    };

    Some(PrometheusContext {
        metric_name,
        metric_type,
    })
}

fn get_prom_value_for_prom_context(prom_context: &PrometheusContext) -> PrometheusValue {
    match prom_context.metric_type {
        MetricType::Counter => PrometheusValue::Counter(0.0),
        MetricType::Gauge => PrometheusValue::Gauge(0.0),
        MetricType::Histogram => PrometheusValue::Histogram(PrometheusHistogram::new(&prom_context.metric_name)),
        MetricType::Summary => PrometheusValue::Summary(DDSketch::default()),
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
            self.add_sample(sample.value.into_inner(), sample.weight.0 as u64);
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
    use std::collections::BTreeSet;

    use http_body_util::BodyExt as _;
    use saluki_context::tags::TagSet;

    use super::*;

    fn prom_context(metric_type: MetricType) -> PrometheusContext {
        PrometheusContext {
            metric_name: MetaString::from("test.metric"),
            metric_type,
        }
    }

    #[test]
    fn histogram_buckets_are_monotonic_finite_and_labeled() {
        // The pre-computed log-linear bucket tables back every Prometheus histogram we expose, so their invariants are
        // load-bearing: exactly N upper bounds, strictly increasing, finite and positive, starting at the configured
        // base, each paired with a string label that renders (and parses back to) its own float value. A regression
        // producing NaN/inf bounds, non-monotonic spacing, or a mismatched label would silently corrupt the exposition
        // output, so assert the contract rather than just printing the tables.
        fn assert_bucket_table(buckets: &[(f64, &'static str)], base: f64) {
            assert!(!buckets.is_empty(), "bucket table must not be empty");
            assert_eq!(
                buckets[0].0, base,
                "first bucket upper bound must equal the configured base"
            );

            let mut previous = f64::NEG_INFINITY;
            for (upper_bound, label) in buckets {
                assert!(
                    upper_bound.is_finite(),
                    "bucket upper bound must be finite, got {upper_bound}"
                );
                assert!(
                    *upper_bound > 0.0,
                    "bucket upper bound must be positive, got {upper_bound}"
                );
                assert!(
                    *upper_bound > previous,
                    "bucket upper bounds must be strictly increasing ({upper_bound} !> {previous})"
                );
                previous = *upper_bound;

                let parsed: f64 = label.parse().expect("bucket label must parse as an f64");
                assert_eq!(
                    parsed, *upper_bound,
                    "bucket label {label:?} must render its own upper bound {upper_bound}"
                );
            }
        }

        assert_eq!(TIME_HISTOGRAM_BUCKETS.len(), TIME_HISTOGRAM_BUCKET_COUNT);
        assert_eq!(NON_TIME_HISTOGRAM_BUCKETS.len(), NON_TIME_HISTOGRAM_BUCKET_COUNT);
        assert_bucket_table(&TIME_HISTOGRAM_BUCKETS[..], 0.000000128);
        assert_bucket_table(&NON_TIME_HISTOGRAM_BUCKETS[..], 1.0);
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

    #[tokio::test]
    async fn scrape_routes_serve_raw_compat_and_404() {
        let payload = Arc::new(RwLock::new("raw".to_string()));
        let routes = vec![PrometheusAdditionalRoute {
            path: "/compat/metrics".to_string(),
            provider: Arc::new(|| "compat".to_string()),
        }];

        let raw_response = build_scrape_response("/metrics", &payload, &routes).await;
        assert_eq!(raw_response.status(), StatusCode::OK);
        let raw_body = raw_response
            .into_body()
            .collect()
            .await
            .expect("body should collect")
            .to_bytes();
        assert_eq!(&raw_body[..], b"raw");

        let legacy_response = build_scrape_response("/", &payload, &routes).await;
        assert_eq!(legacy_response.status(), StatusCode::OK);
        let legacy_body = legacy_response
            .into_body()
            .collect()
            .await
            .expect("body should collect")
            .to_bytes();
        assert_eq!(&legacy_body[..], b"raw");

        let compat_response = build_scrape_response("/compat/metrics", &payload, &routes).await;
        assert_eq!(compat_response.status(), StatusCode::OK);
        let compat_body = compat_response
            .into_body()
            .collect()
            .await
            .expect("body should collect")
            .to_bytes();
        assert_eq!(&compat_body[..], b"compat");

        let missing_response = build_scrape_response("/missing", &payload, &routes).await;
        assert_eq!(missing_response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn into_prometheus_metric_maps_value_type_and_normalizes_name() {
        let mut renderer = PrometheusRenderer::new();
        let interner = FixedSizeInterner::<1>::new(METRIC_NAME_STRING_INTERNER_BYTES);

        // The metric name is normalized to the Prometheus character set: the `.` separator is not a valid name
        // character, so it is replaced (the renderer yields `my__counter`).
        let counter = into_prometheus_metric(&Metric::counter("my.counter", 1.0), &mut renderer, &interner)
            .expect("counter should translate");
        assert_eq!("my__counter", counter.metric_name.as_ref());
        assert!(matches!(counter.metric_type, MetricType::Counter));

        // Gauges and sets both map to a Prometheus gauge.
        let gauge =
            into_prometheus_metric(&Metric::gauge("g", 1.0), &mut renderer, &interner).expect("gauge should translate");
        assert!(matches!(gauge.metric_type, MetricType::Gauge));
        let set =
            into_prometheus_metric(&Metric::set("s", "a"), &mut renderer, &interner).expect("set should translate");
        assert!(matches!(set.metric_type, MetricType::Gauge));

        // Histograms map to a native Prometheus histogram.
        let histogram = into_prometheus_metric(&Metric::histogram("h", [1.0]), &mut renderer, &interner)
            .expect("histogram should translate");
        assert!(matches!(histogram.metric_type, MetricType::Histogram));

        // Distributions are exposed as a DDSketch-backed summary: the documented stopgap, since a distribution can't
        // be converted to an aggregated histogram.
        let distribution = into_prometheus_metric(&Metric::distribution("d", [1.0]), &mut renderer, &interner)
            .expect("distribution should translate");
        assert!(matches!(distribution.metric_type, MetricType::Summary));
    }

    #[test]
    fn merge_counter_sums_all_points() {
        let mut value = get_prom_value_for_prom_context(&prom_context(MetricType::Counter));
        let (_, values, _) = Metric::counter("c", [(1, 1.0), (2, 2.0), (3, 4.0)]).into_parts();
        merge_metric_values_with_prom_value(values, &mut value);
        match value {
            PrometheusValue::Counter(sum) => assert_eq!(7.0, sum),
            _ => panic!("counter values should stay a counter"),
        }
    }

    #[test]
    fn merge_gauge_keeps_latest_by_timestamp() {
        let mut value = get_prom_value_for_prom_context(&prom_context(MetricType::Gauge));
        // Points are supplied out of timestamp order; the highest timestamp's value wins.
        let (_, values, _) = Metric::gauge("g", [(10, 1.0), (30, 3.0), (20, 2.0)]).into_parts();
        merge_metric_values_with_prom_value(values, &mut value);
        match value {
            PrometheusValue::Gauge(latest) => assert_eq!(3.0, latest),
            _ => panic!("gauge values should stay a gauge"),
        }
    }

    #[test]
    fn merge_set_maps_cardinality_into_gauge() {
        let mut value = get_prom_value_for_prom_context(&prom_context(MetricType::Gauge));
        let (_, values, _) = Metric::set("s", "a").into_parts();
        merge_metric_values_with_prom_value(values, &mut value);
        match value {
            PrometheusValue::Gauge(cardinality) => assert_eq!(1.0, cardinality),
            _ => panic!("set values should merge into a gauge"),
        }
    }

    #[test]
    fn merge_histogram_accumulates_sum_and_count() {
        let mut value = get_prom_value_for_prom_context(&prom_context(MetricType::Histogram));
        let (_, values, _) = Metric::histogram("h", [1.0, 2.0, 3.0]).into_parts();
        merge_metric_values_with_prom_value(values, &mut value);
        match value {
            PrometheusValue::Histogram(histogram) => {
                assert_eq!(3, histogram.count);
                assert_eq!(6.0, histogram.sum);
            }
            _ => panic!("histogram values should stay a histogram"),
        }
    }

    #[test]
    fn merge_distribution_folds_into_ddsketch_summary() {
        let mut value = get_prom_value_for_prom_context(&prom_context(MetricType::Summary));
        let (_, values, _) = Metric::distribution("d", [1.0, 2.0, 3.0, 4.0, 5.0]).into_parts();
        merge_metric_values_with_prom_value(values, &mut value);
        match value {
            PrometheusValue::Summary(sketch) => {
                assert_eq!(5, sketch.count());
                let median = sketch.quantile(0.5).expect("median should be computable");
                assert!((median - 3.0).abs() <= 0.5, "median should be ~= 3.0, got {median}");
            }
            _ => panic!("distribution values should merge into a summary"),
        }
    }

    #[test]
    #[should_panic(expected = "Mismatched metric types")]
    fn merge_mismatched_value_and_accumulator_types_panics() {
        // Feeding gauge values into a counter accumulator is an invariant violation and panics.
        let mut value = get_prom_value_for_prom_context(&prom_context(MetricType::Counter));
        let (_, values, _) = Metric::gauge("g", 1.0).into_parts();
        merge_metric_values_with_prom_value(values, &mut value);
    }

    #[test]
    fn collect_tags_skips_bare_tags_and_keeps_key_value_pairs() {
        let mut tags_deduplicator = ReusableDeduplicator::new();
        let context = Context::from_static_parts("m", &["env:prod", "bare", "team:core"]);

        let labels = collect_tags(&context, &mut tags_deduplicator).expect("tags should collect");
        let labels = labels.into_iter().collect::<BTreeSet<_>>();

        // Key/value tags become labels; the bare `bare` tag (no value) is dropped.
        assert_eq!(BTreeSet::from([("env", "prod"), ("team", "core")]), labels);
    }

    #[test]
    fn collect_tags_returns_none_when_buffer_limit_exceeded() {
        let mut tags_deduplicator = ReusableDeduplicator::new();
        // A single tag larger than the 2048-byte buffer trips the size-limit bail-out.
        let oversized_tag = format!("big:{}", "x".repeat(TAGS_BUFFER_SIZE_LIMIT_BYTES + 1));
        let tags = std::iter::once(Tag::from(oversized_tag)).collect::<TagSet>();
        let context = Context::from_parts("m", tags);

        assert_eq!(None, collect_tags(&context, &mut tags_deduplicator));
    }
}
