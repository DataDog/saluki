//! Sample sketch batches without changing authoritative delivery.

use std::sync::{Arc, LazyLock};

use rand::RngExt as _;
use regex::Regex;
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

use super::*;
use crate::common::datadog::sketch_shadow::{SketchShadowDestination, SketchShadowMetadata};

static SITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|\.)([a-z]{2,}\d{1,2}\.)?(datad(?:oghq|0g)\.(?:com|eu)|ddog-gov\.com)$").unwrap()
});

pub(super) fn site_allowed(endpoint: &Url, sites: &[String]) -> bool {
    let Some(host) = endpoint.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let Some(captures) = SITE.captures(&host) else {
        return false;
    };
    let start = captures.get(1).or_else(|| captures.get(2)).unwrap().start();
    sites.iter().any(|site| site == &host[start..])
}

#[derive(Default)]
pub(super) struct SketchShadowConfig {
    pub eligible_destinations: Vec<Arc<str>>,
    pub sample_rate: f64,
    pub beta_route: String,
}

pub(super) struct SketchShadowState<'a> {
    config: &'a SketchShadowConfig,
    sampling_decided: bool,
    sampled_destinations: Option<Arc<[SketchShadowDestination]>>,
    metrics: Vec<Metric>,
    ratio: V3CompressionRatio,
}

impl<'a> SketchShadowState<'a> {
    pub fn new(config: &'a SketchShadowConfig) -> Self {
        Self {
            config,
            sampling_decided: false,
            sampled_destinations: None,
            metrics: Vec::new(),
            ratio: V3CompressionRatio::default(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        !self.config.eligible_destinations.is_empty()
    }

    fn sample_batch(&mut self) {
        self.sample_batch_with_rng(|| rand::rng().random::<f64>());
    }

    // Each V2 builder batch is a complete validation group with its own destination IDs.
    // The validator waits for V2 and V3 independently, so their request counts can differ.
    fn sample_batch_with_rng(&mut self, mut random: impl FnMut() -> f64) {
        if self.sampling_decided {
            return;
        }
        self.sampling_decided = true;
        let mut sampled_destinations = Vec::new();
        for endpoint in &self.config.eligible_destinations {
            if random() < self.config.sample_rate {
                let id = Uuid::now_v7();
                let mut buffer = Uuid::encode_buffer();
                sampled_destinations.push(SketchShadowDestination {
                    configured_endpoint: Arc::clone(endpoint),
                    batch_id: HeaderValue::from_str(id.hyphenated().encode_lower(&mut buffer)).unwrap(),
                });
            }
        }
        if !sampled_destinations.is_empty() {
            self.sampled_destinations = Some(sampled_destinations.into());
        }
    }

    pub async fn encode(
        &mut self, builder: &mut RequestBuilder<MetricsEndpointEncoder>, metric: Metric, context: V3FlushContext<'_>,
        tx: &mut Sender<Payload>,
    ) -> Result<EncodeResult, GenericError> {
        self.sample_batch();
        let mut copy = self.sampled_destinations.as_ref().map(|_| metric.clone());
        let mut flushed = false;
        let result = match builder.encode(metric).await {
            Ok(Some(metric)) => {
                // The rejected metric starts the next V2 batch; it must not enter the previous shadow.
                self.flush(builder, context, tx).await?;
                flushed = true;
                self.sample_batch();
                copy = self.sampled_destinations.as_ref().map(|_| metric.clone());
                builder.encode(metric).await
            }
            result => result,
        };
        match result {
            Ok(None) => {
                if let Some(metric) = copy {
                    self.metrics.push(metric);
                }
                Ok(EncodeResult::new(true, flushed))
            }
            result => {
                error!(?result, "Failed to encode sketch metric.");
                context.telemetry.events_dropped_encoder().increment(1);
                Ok(EncodeResult::new(false, flushed))
            }
        }
    }

    pub async fn flush(
        &mut self, builder: &mut RequestBuilder<MetricsEndpointEncoder>, context: V3FlushContext<'_>,
        tx: &mut Sender<Payload>,
    ) -> Result<(), GenericError> {
        let mut metrics = std::mem::take(&mut self.metrics);
        let result = self.flush_batch(builder, context, tx, &metrics).await;
        metrics.clear();
        self.metrics = metrics;
        result
    }

    async fn flush_batch(
        &mut self, builder: &mut RequestBuilder<MetricsEndpointEncoder>, context: V3FlushContext<'_>,
        tx: &mut Sender<Payload>, metrics: &[Metric],
    ) -> Result<(), GenericError> {
        let Some(sampled_destinations) = self.sampled_destinations.take() else {
            // Allow a fresh sampling decision for the next batch.
            self.sampling_decided = false;
            return flush_v2_metrics(builder, tx, Some(MetricsPayloadInfo::v2_sketches()))
                .await
                .map(|_| ());
        };
        // Allow a fresh sampling decision for the next batch.
        self.sampling_decided = false;
        let mut requests = Vec::new();
        let mut baseline_failed = false;
        let mut fatal_error = None;
        for request in builder.flush().await {
            match request {
                Ok(request) => requests.push(request),
                Err(error) => {
                    baseline_failed = true;
                    warn!(%error, "Failed to flush sketch baseline; skipping its shadow.");
                    if !error.is_recoverable() {
                        fatal_error = Some(error);
                    }
                }
            }
        }
        let request_count = requests.len();
        for (request_index, (events, points, request)) in requests.into_iter().enumerate() {
            enqueue_sketch_request(
                request,
                events,
                points,
                tx,
                SketchShadowMetadata {
                    sampled_destinations: Arc::clone(&sampled_destinations),
                    request_index,
                    request_count,
                    is_shadow: false,
                },
            )
            .await?;
        }
        if let Some(error) = fatal_error {
            return Err(GenericError::from(error).context("Failed to flush request."));
        }
        if !baseline_failed && request_count != 0 {
            // The baseline is already dispatched. An invalid shadow route or failed shadow encode cannot suppress it.
            encode_v3_payload_requests(
                &self.config.beta_route,
                metrics,
                V3FlushContext {
                    is_shadow: true,
                    ..context
                },
                "sketches",
                &mut self.ratio,
                tx,
                Some(MetricsPayloadInfo::v3_sketches()),
                Some(sampled_destinations),
            )
            .await?;
        }
        Ok(())
    }
}

pub(super) async fn enqueue_sketch_request(
    request: Request<FrozenChunkedBytesBuffer>, events: usize, points: usize, tx: &mut Sender<Payload>,
    shadow: SketchShadowMetadata,
) -> Result<(), GenericError> {
    let info = if shadow.is_shadow {
        MetricsPayloadInfo::v3_sketches()
    } else {
        MetricsPayloadInfo::v2_sketches()
    };
    let metadata = PayloadMetadata::from_event_and_data_point_count(events, points)
        .with(info)
        .with(shadow);
    tx.send(Payload::Http(HttpPayload::new(metadata, request)))
        .await
        .error_context("Failed to send sketch payload.")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use saluki_core::{components::test_util::TestComponentDriver, data_model::event::Event};

    use super::*;
    use crate::common::datadog::{test_util::datadog_metrics_configuration, METRICS_SKETCHES_PATH};

    fn configuration() -> DatadogMetricsConfiguration {
        let mut config = datadog_metrics_configuration();
        config.use_v3_api.series.enabled = V3SeriesMode::Enabled;
        config.v3_api.sketches.shadow_sample_rate = 1.0;
        config.v3_api.sketches.shadow_sites = vec!["datadoghq.com".into()];
        config.v3_api.sketches.beta_route = "/custom/sketch-shadow".into();
        config.flush_timeout = Duration::from_millis(10);
        config
    }

    #[test]
    fn shadow_eligibility_matches_resolved_destinations_and_routing() {
        // Core #54581: site-gated V2 destinations, with one decision across a resolver's API keys.
        for (url, expected) in [
            ("https://app.datadoghq.com", true),
            ("https://APP.DATADOGHQ.COM./", true),
            ("https://app.us3.datadoghq.com", false),
            ("https://notdatadoghq.com", false),
            ("https://app.datadoghq.com.example.org", false),
        ] {
            assert_eq!(
                site_allowed(&Url::parse(url).unwrap(), &["datadoghq.com".into()]),
                expected,
                "{url}"
            );
        }
        let mut config = configuration();
        config
            .additional_endpoints
            .insert("https://other.datadoghq.com".into(), vec!["a".into(), "b".into()]);
        config
            .additional_endpoints
            .insert("https://app.us3.datadoghq.com".into(), vec!["c".into()]);
        let eligible = |config: &DatadogMetricsConfiguration, disabled_by_compressor| {
            config
                .build_sketch_shadow_config(disabled_by_compressor)
                .unwrap()
                .eligible_destinations
        };
        assert_eq!(eligible(&config, false).len(), 2);
        assert!(eligible(&config, true).is_empty());
        config.v3_api.sketches.endpoints.push(config.primary_endpoint.clone());
        assert_eq!(
            eligible(&config, false).as_slice(),
            [Arc::<str>::from("https://other.datadoghq.com")]
        );
        let catalog =
            Arc::new(RoutingTargetCatalog::new(&config.primary_endpoint, &config.additional_endpoints).unwrap());
        let primary = [catalog.primary()].into_iter().collect();
        assert!(eligible(&config.with_routing(catalog, primary), false).is_empty());
        assert!(eligible(
            &configuration().with_metrics_endpoint_override("https://app.datadoghq.com".into()),
            false
        )
        .is_empty());
        assert!(eligible(&configuration().with_v2_series_only(), false).is_empty());
    }

    #[test]
    fn sampling_is_per_destination_and_stable_until_flush() {
        let config = SketchShadowConfig {
            eligible_destinations: vec![Arc::from("first"), Arc::from("second")],
            sample_rate: 0.5,
            ..Default::default()
        };
        let mut state = SketchShadowState::new(&config);
        let mut samples = [0.4, 0.5].into_iter();
        state.sample_batch_with_rng(|| samples.next().unwrap());
        assert_eq!(samples.next(), None);
        let selected = state.sampled_destinations.as_ref().unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].configured_endpoint.as_ref(), "first");
        let id = selected[0].batch_id.clone();
        state.sample_batch_with_rng(|| panic!("must not resample an active batch"));
        assert_eq!(state.sampled_destinations.as_ref().unwrap()[0].batch_id, id);
    }

    #[tokio::test]
    async fn sketch_shadow_preserves_batches_across_v2_overflow_and_v3_splits() {
        let mut config = configuration();
        config.max_metrics_per_payload = 2;
        config.v3_api.sketches.max_points_per_payload = 1;
        let (control, mut output) = TestComponentDriver::encoder(config).await.unwrap();
        let mut events = ["shadow.one", "shadow.two", "shadow.three"]
            .into_iter()
            .map(|name| Event::Metric(Metric::distribution(name, [(123, 1.0)])))
            .collect::<Vec<_>>();
        events.push(Event::Metric(Metric::counter("series.stays.v3", 1.0)));
        control.send_events(events).await;
        let (result, payloads) = tokio::join!(control.shutdown(), output.collect());
        result.unwrap();
        let mut batches = BTreeMap::<String, (usize, usize, Vec<usize>, usize)>::new();
        let mut series = 0;
        for payload in payloads {
            let (metadata, request) = payload.try_into_http_payload().unwrap().into_parts();
            if request.uri() == METRICS_SERIES_V3_PATH {
                series += metadata.event_count();
                assert!(metadata.get::<SketchShadowMetadata>().is_none());
                continue;
            }
            let shadow = metadata
                .get::<SketchShadowMetadata>()
                .expect("both sketch versions are correlated");
            assert_eq!(shadow.sampled_destinations.len(), 1);
            let id = shadow.sampled_destinations[0].batch_id.to_str().unwrap().to_owned();
            let batch = batches.entry(id).or_default();
            if shadow.is_shadow {
                assert_eq!(request.uri(), "/custom/sketch-shadow");
                batch.1 += metadata.event_count();
                batch.2.push(shadow.request_index);
                batch.3 = shadow.request_count;
            } else {
                assert_eq!(request.uri(), METRICS_SKETCHES_PATH);
                assert_eq!((shadow.request_index, shadow.request_count), (0, 1));
                batch.0 += metadata.event_count();
            }
        }
        assert_eq!(series, 1);
        assert_eq!(batches.len(), 2, "overflow starts a fresh correlated batch");
        assert_eq!(batches.values().map(|batch| batch.0).sum::<usize>(), 3);
        for (baseline, shadow, sequences, request_count) in batches.values() {
            assert_eq!(baseline, shadow, "the retried metric belongs only to the next batch");
            assert_eq!(sequences, &(0..*request_count).collect::<Vec<_>>());
            assert_eq!(
                shadow, request_count,
                "the V3 point limit splits each sketch separately"
            );
        }
    }

    #[tokio::test]
    async fn disabled_or_failed_shadow_preserves_baseline() {
        for (rate, route) in [(0.0, "/custom/sketch-shadow"), (1.0, "invalid route")] {
            let mut config = configuration();
            config.v3_api.sketches.shadow_sample_rate = rate;
            config.v3_api.sketches.beta_route = route.into();
            let (control, mut output) = TestComponentDriver::encoder(config).await.unwrap();
            control
                .send_events([Event::Metric(Metric::distribution("baseline.survives", [(123, 1.0)]))])
                .await;
            let (result, mut payloads) = tokio::join!(control.shutdown(), output.collect());
            result.unwrap();
            assert_eq!(payloads.len(), 1);
            let (metadata, request) = payloads.pop().unwrap().try_into_http_payload().unwrap().into_parts();
            assert_eq!(request.uri(), METRICS_SKETCHES_PATH);
            assert_eq!(metadata.event_count(), 1);
            if rate == 0.0 {
                assert!(metadata.get::<SketchShadowMetadata>().is_none());
            }
        }
    }
}
