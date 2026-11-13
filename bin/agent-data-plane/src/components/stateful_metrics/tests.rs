use std::{io, mem::replace, pin::Pin, sync::Arc};

use agent_data_plane_config::{
    defaults::{
        DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_BYTES, DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_ENTRIES,
        DEFAULT_STATEFUL_METRICS_RETRY_MAX_AGE, DEFAULT_STATEFUL_METRICS_RETRY_MAX_ATTEMPTS,
        DEFAULT_STATEFUL_METRICS_RETRY_MAX_BYTES,
    },
    shared::SharedConfiguration,
    ConfigValue,
};
use foldspace_core::{
    proto::stateful::{
        metric_datum,
        stateful_intake_server::{StatefulIntake, StatefulIntakeServer},
        BatchStatus, MetricDatumSequence, StatefulBatch, StatelessRequest, StatelessResponse,
    },
    LogicalMetricSeries, MetricOrigin as FoldspaceOrigin, MetricPoint, MetricResource, MetricRetryStats,
    MetricSeriesType,
};
use futures::Stream;
use metrics::{Key, Label};
use prost::Message as _;
use rustls::{RootCertStore, ServerConfig};
use saluki_common::sync::live::{channel, Live};
use saluki_core::data_model::{event::metric::context::Context, tags::TagSet};
use saluki_core::{
    accounting::{ComponentRegistry, MemoryLimiter},
    components::{
        test_util::{ComponentControl, TestComponentDriver},
        transforms::{TransformBuilder, TransformContext},
        ComponentContext,
    },
    data_model::event::{
        metric::{Metric, MetricMetadata, MetricOrigin, MetricValues},
        Event,
    },
    health::HealthRegistry,
    runtime::state::{DataspaceRegistry, ResourceRegistry},
    support::SubsystemIdentifier,
    topology::{
        interconnect::{Consumer, Dispatcher},
        EventsBuffer, OutputName, TopologyContext,
    },
};
use saluki_io::net::util::retry::{EventContainer, Retryable};
use saluki_metrics::test::TestRecorder;
use saluki_tls::{initialize_default_crypto_provider, load_platform_root_certificates, test_util::SelfSignedCert};
use tempfile::TempDir;
use tokio::{
    net::TcpListener,
    runtime::Handle,
    sync::{mpsc, Mutex},
    task::JoinHandle,
    time::{advance, timeout},
};
use tokio_rustls::TlsAcceptor;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{transport::Server, Request, Response, Status, Streaming};

use super::{retry::RetryBatch, *};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const E0: MetricEndpointId = MetricEndpointId(0);
const E1: MetricEndpointId = MetricEndpointId(1);

fn shared() -> SharedConfiguration {
    let mut shared = SharedConfiguration::default();
    shared.endpoints.forwarder.storage_max_size_in_bytes = 0;
    shared.endpoints.forwarder.high_prio_buffer_size = 32;
    shared.endpoints.forwarder.retry_queue_payloads_max_size = ConfigValue::explicit(1024 * 1024);
    shared.endpoints.forwarder.outdated_file_in_days = 7;
    shared
}

fn queue_configuration(shared: &SharedConfiguration) -> DeliveryQueueConfiguration {
    let config = crate::component_inputs::forwarder_configuration(shared);
    DeliveryQueueConfiguration::new(config.endpoint_buffer_size, config.retry)
}

fn test_endpoints(count: usize) -> Vec<MetaString> {
    (0..count)
        .map(|port| format!("http://127.0.0.1:{}", 8080 + port).into())
        .collect()
}

async fn queue_for(settings: &SharedConfiguration, endpoints: &[MetaString]) -> LanedRetryQueue {
    LanedRetryQueue::build(&queue_configuration(settings), endpoints, &MetricsBuilder::default())
        .await
        .unwrap()
}

fn configuration(endpoints: usize, batch_capacity: usize) -> StatefulMetricsConfiguration {
    StatefulMetricsConfiguration {
        endpoints: test_endpoints(endpoints),

        api_key: Live::new_fixed("test-key".to_string()),
        compression_level: 3,
        flush_timeout: Duration::from_secs(2),
        batch_capacity,
        dictionary_max_entries: DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_ENTRIES,
        dictionary_max_bytes: DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_BYTES,
        retry_max_bytes: DEFAULT_STATEFUL_METRICS_RETRY_MAX_BYTES,
        retry_max_age: DEFAULT_STATEFUL_METRICS_RETRY_MAX_AGE,
        retry_max_attempts: DEFAULT_STATEFUL_METRICS_RETRY_MAX_ATTEMPTS,
        queue: queue_configuration(&shared()),
        stop_timeout: TEST_TIMEOUT,
    }
}

fn core_config(endpoints: usize, batch_capacity: usize) -> CoreConfig {
    configuration(endpoints, batch_capacity).core_config()
}

async fn worker_with(endpoints: usize, batch_capacity: usize) -> StatefulMetricsWorker {
    worker_for(configuration(endpoints, batch_capacity)).await
}

async fn worker_for(configuration: StatefulMetricsConfiguration) -> StatefulMetricsWorker {
    let addresses = configuration.endpoints.clone();
    StatefulMetricsWorker::new(
        addresses
            .iter()
            .map(|endpoint| Endpoint::from_shared(endpoint.to_string()).unwrap())
            .collect(),
        parse_api_key("test-key").unwrap(),
        configuration.core_config(),
        3,
        Duration::from_secs(2),
        queue_for(&shared(), &addresses).await,
        MetricsBuilder::default(),
    )
    .unwrap()
}

/// A worker whose core hands every payload waiting for an endpoint straight to the retry queue.
async fn worker_without_internal_retries(endpoints: usize) -> StatefulMetricsWorker {
    let mut configuration = configuration(endpoints, 512);
    configuration.retry_max_bytes = 0;
    worker_for(configuration).await
}

async fn worker() -> StatefulMetricsWorker {
    worker_with(1, 512).await
}

/// Replaces a stream's transport with an in-memory wire and marks the stream open, which sends it any replays.
async fn attach(
    worker: &mut StatefulMetricsWorker, endpoint: MetricEndpointId, stream_id: StreamId,
) -> mpsc::Receiver<StatefulBatch> {
    let (sender, receiver) = mpsc::channel(2 * MAX_INFLIGHT_BATCHES + 1);
    worker.transports[endpoint.get()] = Some(Transport {
        endpoint,
        stream_id,
        sender,
        state: TransportState::Connecting(pending().boxed()),
        pending: VecDeque::new(),
    });
    let effects = worker.core.handle_stream_opened(stream_id);
    worker.apply(effects).await.unwrap();
    receiver
}

async fn start_all(worker: &mut StatefulMetricsWorker) -> Vec<(mpsc::Receiver<StatefulBatch>, StreamId)> {
    let effects = worker.core.start();
    worker.apply(effects.clone()).await.unwrap();
    let mut opened = Vec::new();
    for effect in effects {
        let MetricClientEffect::OpenStream { endpoint, stream_id } = effect else {
            panic!("unexpected start effect")
        };
        assert_eq!(endpoint.get(), opened.len());
        opened.push((attach(worker, endpoint, stream_id).await, stream_id));
    }
    opened
}

async fn open_worker() -> (StatefulMetricsWorker, mpsc::Receiver<StatefulBatch>, StreamId) {
    let mut worker = worker().await;
    let (wire, stream_id) = start_all(&mut worker).await.pop().unwrap();
    (worker, wire, stream_id)
}

async fn buffer(worker: &mut StatefulMetricsWorker, name: &'static str) {
    worker.accept([Event::Metric(Metric::gauge(name, (123, 2.0)))]).await;
    worker.pump().await.unwrap();
}

async fn submit(worker: &mut StatefulMetricsWorker, name: &'static str) {
    buffer(worker, name).await;
    worker.flush().await.unwrap();
}

async fn shared_names(queue: &mut LanedRetryQueue) -> Vec<String> {
    let mut names = Vec::new();
    while let Some(attempt) = queue.pop_shared().await {
        let batch = match attempt {
            PendingTransaction::HighPriority(batch) | PendingTransaction::LowPriority(batch) => batch,
        };
        names.extend(batch.0.series().iter().map(|s| s.name().to_owned()));
    }
    names
}

async fn lane_names(queue: &mut LanedRetryQueue, endpoint: MetricEndpointId) -> Vec<String> {
    let mut names = Vec::new();
    while let Some(batch) = queue.pop_lane(endpoint).await {
        names.extend(batch.0.series().iter().map(|s| s.name().to_owned()));
    }
    names
}

async fn queued_names(worker: &mut StatefulMetricsWorker) -> Vec<String> {
    let mut names = shared_names(&mut worker.queue).await;
    for index in 0..worker.queue.lane_count() {
        names.extend(lane_names(&mut worker.queue, MetricEndpointId(index)).await);
    }
    names
}

fn ack(stream_id: StreamId, batch_id: u32) -> TransportEvent {
    ack_on(E0, stream_id, batch_id)
}

fn ack_on(endpoint: MetricEndpointId, stream_id: StreamId, batch_id: u32) -> TransportEvent {
    TransportEvent {
        endpoint,
        stream_id,
        kind: TransportEventKind::Ack(BatchStatus { batch_id, status: 1 }),
    }
}

fn sequence(batch: &StatefulBatch) -> MetricDatumSequence {
    let bytes = zstd::stream::decode_all(batch.data.as_slice()).unwrap();
    MetricDatumSequence::decode(bytes.as_slice()).unwrap()
}

fn has_name(sequence: &MetricDatumSequence) -> bool {
    sequence
        .data
        .iter()
        .any(|datum| matches!(datum.data, Some(metric_datum::Data::MetricNameDefine(_))))
}

#[test]
fn conversion_preserves_rate_metadata_and_resources() {
    let tags: TagSet = [
        "env:test",
        "device:disk",
        "dd.internal.resource:container:abc",
        "env:test",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let context = Context::from_parts("requests", tags).with_host(MetaString::from("host-a"));
    let metric = Metric::from_parts(
        context,
        MetricValues::rate([(123, 20.0), (124, f64::NAN)], Duration::from_secs(10)),
        MetricMetadata::default()
            .with_origin(MetricOrigin::dogstatsd())
            .with_unit("request"),
    );
    let series = conversion::convert(&metric).unwrap();
    assert_eq!(series.metric_type(), MetricSeriesType::Rate);
    assert_eq!(series.points(), [MetricPoint::new(123, 2.0)]);
    assert_eq!(series.interval(), 10);
    assert_eq!(series.tags().values, ["env:test"]);
    assert_eq!(
        series.resources(),
        [
            MetricResource::new("host", "host-a"),
            MetricResource::new("container", "abc"),
            MetricResource::new("device", "disk"),
        ]
    );
    assert_eq!(series.origin(), Some(FoldspaceOrigin::new(10, 10, 0)));
    assert_eq!(series.unit(), Some("request"));
}

#[test]
fn conversion_discards_series_without_finite_points() {
    for points in [
        &[][..],
        &[(123, f64::NAN), (124, f64::INFINITY), (125, f64::NEG_INFINITY)][..],
    ] {
        for values in [
            MetricValues::counter(points),
            MetricValues::gauge(points),
            MetricValues::rate(points, Duration::from_secs(10)),
        ] {
            let metric = Metric::from_parts(
                Context::from_static_parts("invalid", &[]),
                values,
                MetricMetadata::default(),
            );
            assert!(conversion::convert(&metric).is_none(), "{metric:?}");
        }
    }

    let overflowing_rate = Metric::rate("overflow", (123, f64::MAX), Duration::from_nanos(1));
    assert!(conversion::convert(&overflowing_rate).is_none());
}

#[test]
fn conversion_deduplicates_tags_and_resource_tags() {
    let tagset = |tags: &[&str]| tags.iter().copied().map(Into::into).collect::<TagSet>();
    let context = Context::from_parts(
        "requests",
        tagset(&[
            "env:prod",
            "device:sdb",
            "device:sda",
            "env:prod",
            "dd.internal.resource:container:abc",
            "dd.internal.resource:kind:",
            "device",
            "a:b:c",
            "pod:é",
        ]),
    )
    .with_origin_tags(tagset(&["device:sda", "dd.internal.resource:container:abc", "pod:é"]))
    .with_host(MetaString::from("host-a"));
    let series = conversion::convert(&Metric::from_parts(
        context,
        MetricValues::gauge([(123, 1.0)]),
        MetricMetadata::default(),
    ))
    .unwrap();
    // Origin tags are read first and a repeated device tag is skipped, so the last distinct one wins.
    assert_eq!(
        series.resources(),
        [
            MetricResource::new("host", "host-a"),
            MetricResource::new("container", "abc"),
            MetricResource::new("device", "sdb"),
        ]
    );
    assert_eq!(series.tags().values, ["a:b:c", "device", "env:prod", "pod:é"]);

    let cleared = conversion::convert(&Metric::from_parts(
        Context::from_parts("requests", tagset(&["device:sda", "device:"])),
        MetricValues::gauge([(123, 1.0)]),
        MetricMetadata::default(),
    ))
    .unwrap();
    assert!(cleared.resources().is_empty());
    assert!(cleared.tags().values.is_empty());
}

#[test]
fn zero_interval_and_source_type_survive_conversion() {
    let mut metric = Metric::rate("rate", (123, 20.0), Duration::ZERO);
    metric.metadata_mut().set_source_type(Arc::<str>::from("integration"));
    let series = conversion::convert(&metric).unwrap();
    assert_eq!(series.points(), [MetricPoint::new(123, 20.0)]);
    assert_eq!(series.interval(), 0);
    assert_eq!(series.source_type_name(), Some("integration"));
}

#[derive(Clone)]
struct TestIntake {
    received: mpsc::Sender<StatefulBatch>,
    replies: Arc<Mutex<mpsc::Receiver<Result<BatchStatus, Status>>>>,
}

#[tonic::async_trait]
impl StatefulIntake for TestIntake {
    type StatefulStreamStream = Pin<Box<dyn Stream<Item = Result<BatchStatus, Status>> + Send>>;

    async fn stateful_stream(
        &self, request: Request<Streaming<StatefulBatch>>,
    ) -> Result<Response<Self::StatefulStreamStream>, Status> {
        assert_eq!(request.metadata().get("dd-api-key").unwrap(), "test-key");
        assert_eq!(request.metadata().get("dd-content-encoding").unwrap(), "zstd");
        assert_eq!(request.metadata().get("dd-state-request-bytes").unwrap(), "5242880");
        let mut inbound = request.into_inner();
        let received = self.received.clone();
        let replies = self.replies.clone();
        let stream = async_stream::try_stream! {
            while let Some(batch) = inbound.message().await? {
                received.send(batch).await.map_err(|_| Status::cancelled("test finished"))?;
                let reply = replies.lock().await.recv().await.ok_or_else(|| Status::cancelled("test finished"))??;
                yield reply;
            }
        };
        Ok(Response::new(Box::pin(stream)))
    }

    async fn stateless(&self, _: Request<StatelessRequest>) -> Result<Response<StatelessResponse>, Status> {
        Err(Status::unimplemented("stateless delivery is not enabled"))
    }
}

struct Harness {
    worker: StatefulMetricsWorker,
    received: mpsc::Receiver<StatefulBatch>,
    replies: mpsc::Sender<Result<BatchStatus, Status>>,
    server: JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Harness {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Endpoint::from_shared(format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (received_tx, received) = mpsc::channel(16);
        let (replies, replies_rx) = mpsc::channel(16);
        let service = TestIntake {
            received: received_tx,
            replies: Arc::new(Mutex::new(replies_rx)),
        };
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(StatefulIntakeServer::new(service))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let mut worker = StatefulMetricsWorker::new(
            vec![endpoint],
            parse_api_key("test-key").unwrap(),
            core_config(1, 512),
            3,
            Duration::from_secs(2),
            queue_for(&shared(), &test_endpoints(1)).await,
            MetricsBuilder::default(),
        )
        .unwrap();
        let effects = worker.core.start();
        worker.apply(effects).await.unwrap();
        let mut harness = Self {
            worker,
            received,
            replies,
            server,
        };
        harness.progress().await;
        assert!(harness.worker.core.has_send_capacity());
        harness
    }

    async fn progress(&mut self) {
        let event = timeout(TEST_TIMEOUT, next_transport_event(&mut self.worker.transports))
            .await
            .unwrap();
        self.worker.on_transport(event).await.unwrap();
    }

    async fn receive(&mut self) -> StatefulBatch {
        timeout(TEST_TIMEOUT, self.received.recv()).await.unwrap().unwrap()
    }

    async fn ack(&mut self, id: u32) {
        self.replies
            .send(Ok(BatchStatus {
                batch_id: id,
                status: 1,
            }))
            .await
            .unwrap();
        self.progress().await;
    }
}

#[tokio::test]
async fn grpc_sends_compressed_metrics_and_reuses_acknowledged_dictionary() {
    let mut harness = Harness::new().await;
    submit(&mut harness.worker, "requests").await;
    let first = harness.receive().await;
    assert_eq!(first.batch_id, 1);
    assert!(has_name(&sequence(&first)));
    harness.ack(first.batch_id).await;
    assert!(harness.worker.is_drained());
    submit(&mut harness.worker, "requests").await;
    let second = harness.receive().await;
    assert_eq!(second.batch_id, 2);
    assert!(!has_name(&sequence(&second)));
    harness.ack(second.batch_id).await;
}

#[tokio::test]
async fn grpc_disconnect_replays_the_unacknowledged_payload_without_reencoding() {
    let mut harness = Harness::new().await;
    submit(&mut harness.worker, "confirmed").await;
    let first = harness.receive().await;
    harness.ack(first.batch_id).await;
    submit(&mut harness.worker, "unconfirmed").await;
    let second = harness.receive().await;
    assert_eq!(second.batch_id, 2);
    harness
        .replies
        .send(Err(Status::unavailable("disconnect")))
        .await
        .unwrap();
    harness.progress().await;
    assert!(harness.worker.queue.is_empty());
    assert_eq!(harness.worker.core.inflight_len(), 1);
    let (id, kind) = timeout(TEST_TIMEOUT, next_timer(&mut harness.worker.endpoints))
        .await
        .unwrap();
    let effects = harness.worker.core.handle_timer(id, kind);
    harness.worker.apply(effects).await.unwrap();
    harness.progress().await;
    // The replacement stream is sent the same compressed bytes; they define the name they introduced.
    let replay = harness.receive().await;
    assert_eq!(replay.batch_id, 1);
    assert_eq!(replay.data, second.data);
    assert!(has_name(&sequence(&replay)));
    assert_eq!(harness.worker.core.encoding_count(), 2);
    assert_eq!(harness.worker.core.retry_stats().replayed_payloads, 1);
    harness.ack(1).await;
    assert!(harness.worker.is_drained());
}

#[tokio::test]
async fn failure_keeps_inflight_in_the_core_and_fresh_work_queued() {
    let (mut worker, _wire, _) = open_worker().await;
    submit(&mut worker, "inflight").await;
    buffer(&mut worker, "partial").await;
    worker.accept([Event::Metric(Metric::gauge("fresh", (123, 1.0)))]).await;
    worker
        .fail(E0, MetricStreamFailureKind::Unavailable, "disconnect")
        .await
        .unwrap();
    // A retryable failure keeps the sent payload, compressed, and the unsent partial batch in the core.
    assert_eq!(worker.core.inflight_len(), 1);
    assert_eq!(worker.core.buffered_series_len(), 1);
    assert_eq!(queued_names(&mut worker).await, ["fresh"]);
    assert_eq!(worker.endpoints[0].timers.len(), 1);
}

#[tokio::test]
async fn failure_policy_never_switches_to_http() {
    // Retryable failures keep the sent payload in the core for replay. Suspending the only endpoint returns it, and
    // the unsent partial batch, to the retry queue.
    for (kind, suspended, queued) in [
        (MetricStreamFailureKind::Unavailable, false, &[][..]),
        (MetricStreamFailureKind::DeadlineExceeded, false, &[]),
        (MetricStreamFailureKind::ResourceExhausted, false, &[]),
        (MetricStreamFailureKind::Unauthenticated, true, &["sent", "partial"]),
        (MetricStreamFailureKind::InvalidArgument, true, &["partial"]),
        (MetricStreamFailureKind::FailedPrecondition, true, &["sent", "partial"]),
    ] {
        let (mut worker, _wire, _) = open_worker().await;
        submit(&mut worker, "sent").await;
        buffer(&mut worker, "partial").await;
        worker.fail(E0, kind, "injected failure").await.unwrap();
        assert_eq!(worker.endpoints[0].suspended, suspended);
        assert!(worker.transports[0].is_none());
        assert_eq!(worker.endpoints[0].timers.len(), usize::from(!suspended));
        assert_eq!(worker.core.inflight_len(), usize::from(!suspended));
        assert_eq!(queued_names(&mut worker).await, queued);
    }
}

#[tokio::test]
async fn invalid_ack_recovers_all_work_and_suspends() {
    let (mut worker, _wire, stream_id) = open_worker().await;
    submit(&mut worker, "sent").await;
    buffer(&mut worker, "partial").await;
    worker.on_transport(ack(stream_id, 99)).await.unwrap();
    assert!(worker.endpoints[0].suspended);
    assert!(worker.endpoints[0].timers.is_empty());
    assert_eq!(queued_names(&mut worker).await, ["sent", "partial"]);
}

#[tokio::test]
async fn acknowledged_batches_are_not_retried() {
    let (mut worker, _wire, stream_id) = open_worker().await;
    submit(&mut worker, "acked").await;
    worker.on_transport(ack(stream_id, 1)).await.unwrap();
    submit(&mut worker, "unacked").await;
    worker
        .fail(E0, MetricStreamFailureKind::Unavailable, "disconnect")
        .await
        .unwrap();
    assert_eq!(
        worker.core.inflight_len(),
        1,
        "only the unacknowledged payload is retained"
    );
    assert!(queued_names(&mut worker).await.is_empty());
    worker.shutdown_into_queue().await;
    assert_eq!(queued_names(&mut worker).await, ["unacked"]);
}

#[tokio::test]
async fn closed_transport_keeps_the_payload_for_replay() {
    let (mut worker, wire, _) = open_worker().await;
    drop(wire);
    submit(&mut worker, "sent").await;
    assert!(worker.transports[0].is_none());
    assert_eq!(worker.core.inflight_len(), 1);
    assert!(queued_names(&mut worker).await.is_empty());
    assert_eq!(worker.endpoints[0].timers.len(), 1);
}

#[tokio::test]
async fn full_transport_preserves_payload_without_failing_stream() {
    let (mut worker, _wire, stream_id) = open_worker().await;
    let (sender, mut receiver) = mpsc::channel(1);
    worker.transports[0].as_mut().unwrap().sender = sender;
    submit(&mut worker, "first").await;
    submit(&mut worker, "second").await;
    assert_eq!(worker.core.current_stream_id(E0), Some(stream_id));
    assert_eq!(worker.core.inflight_len(), 2);
    assert!(worker.queue.is_empty());
    assert_eq!(receiver.try_recv().unwrap().batch_id, 1);
    assert_eq!(
        worker.transports[0].as_ref().unwrap().pending.front().unwrap().batch_id,
        2
    );
    assert!(worker.endpoints[0].timers.is_empty());
}

#[tokio::test(start_paused = true)]
async fn flush_deadline_is_not_extended_by_continuous_input() {
    let (mut worker, mut wire, _) = open_worker().await;
    buffer(&mut worker, "first").await;
    let deadline = worker.flush_deadline().unwrap();
    advance(Duration::from_secs(1)).await;
    buffer(&mut worker, "second").await;
    assert_eq!(worker.flush_deadline(), Some(deadline));
    assert!(wire.try_recv().is_err());
    advance(Duration::from_secs(1)).await;
    worker.pump().await.unwrap();
    assert_eq!(worker.core.buffered_series_len(), 0);
    assert_eq!(worker.core.inflight_len(), 1);
    assert!(wire.try_recv().is_ok());
    worker.flush().await.unwrap();
    assert!(wire.try_recv().is_err());
}

#[tokio::test]
async fn inflight_window_keeps_remaining_work_in_queue() {
    let (mut worker, _wire, id) = open_worker().await;
    for _ in 0..MAX_INFLIGHT_BATCHES {
        submit(&mut worker, "inflight").await;
    }
    buffer(&mut worker, "waiting").await;
    assert!(!worker.queue.is_empty());
    assert_eq!(worker.core.inflight_len(), MAX_INFLIGHT_BATCHES);
    worker.on_transport(ack(id, 1)).await.unwrap();
    worker.pump().await.unwrap();
    worker.flush().await.unwrap();
    assert!(worker.queue.is_empty());
    assert_eq!(worker.core.inflight_len(), MAX_INFLIGHT_BATCHES);
}

#[tokio::test]
async fn rotation_and_credentials_keep_payloads_for_replay() {
    for reset in [false, true] {
        let (mut worker, _wire, id) = open_worker().await;
        submit(&mut worker, "sent").await;
        buffer(&mut worker, "partial").await;
        if reset {
            worker.update_credentials("new-key").await.unwrap();
        } else {
            let effects = worker.core.handle_timer(id, TimerKind::RotateStream);
            worker.apply(effects).await.unwrap();
            let effects = worker.core.handle_timer(id, TimerKind::DrainExpired);
            worker.apply(effects).await.unwrap();
        }
        assert_eq!(worker.core.inflight_len(), 1);
        assert_eq!(worker.core.buffered_series_len(), 1);
        assert!(queued_names(&mut worker).await.is_empty());
        let replacement = worker.core.current_stream_id(E0).unwrap();
        assert_ne!(replacement, id);
        let mut wire = attach(&mut worker, E0, replacement).await;
        let replay = wire.try_recv().unwrap();
        assert_eq!(replay.batch_id, 1);
        assert!(has_name(&sequence(&replay)));
    }
}

#[test]
fn api_keys_are_trimmed_and_invalid_values_are_rejected() {
    let key = parse_api_key(" \ttest-key\r\n").unwrap();
    assert_eq!(key, "test-key");
    assert!(key.is_sensitive());
    for invalid in ["", " \t\r\n", "bad\nkey", "bad\u{7f}key"] {
        assert!(parse_api_key(invalid).is_err());
    }
}

#[tokio::test]
async fn invalid_credential_updates_preserve_delivery_and_shutdown_persistence() {
    let dir = TempDir::new().unwrap();
    let settings = persisted_settings(&dir, 1024 * 1024);
    let (mut worker, _wire, id) = open_worker().await;
    worker.queue = persisted_queue(&settings).await;
    submit(&mut worker, "inflight").await;
    buffer(&mut worker, "partial").await;
    worker.accept([Event::Metric(Metric::gauge("fresh", (123, 1.0)))]).await;
    let ack_deadline = worker.endpoints[0].ack_deadline;
    let buffered_deadline = worker.buffered_deadline;

    for key in ["", " \t\r\n", "bad\nkey", "bad\u{7f}key", " \ttest-key\r\n"] {
        worker.update_credentials(key).await.unwrap();
        assert_eq!(worker.api_key, "test-key");
        assert_eq!(worker.core.current_stream_id(E0), Some(id));
        assert_eq!(worker.core.inflight_len(), 1);
        assert_eq!(worker.core.buffered_series_len(), 1);
        assert_eq!(worker.endpoints[0].ack_deadline, ack_deadline);
        assert_eq!(worker.buffered_deadline, buffered_deadline);
        assert!(!worker.endpoints[0].suspended);
    }

    worker.on_transport(ack(id, 1)).await.unwrap();
    assert_eq!(worker.core.inflight_len(), 0);
    worker.shutdown().await.unwrap();
    let (mut restarted, _restarted_wire, _) = open_worker().await;
    restarted.queue = persisted_queue(&settings).await;
    let mut names = queued_names(&mut restarted).await;
    names.sort();
    assert_eq!(names, ["fresh", "partial"]);
}

#[tokio::test]
async fn valid_credential_update_resumes_after_ignored_invalid_update() {
    let (mut worker, _wire, _) = open_worker().await;
    submit(&mut worker, "inflight").await;
    worker
        .fail(E0, MetricStreamFailureKind::Unauthenticated, "rejected key")
        .await
        .unwrap();
    assert!(worker.endpoints[0].suspended);
    worker.update_credentials("bad\nkey").await.unwrap();
    assert!(worker.endpoints[0].suspended);
    assert_eq!(worker.api_key, "test-key");
    worker.update_credentials(" \tnew-key\r\n").await.unwrap();
    assert!(!worker.endpoints[0].suspended);
    assert_eq!(worker.api_key, "new-key");
    assert_eq!(queued_names(&mut worker).await, ["inflight"]);
}

fn persisted_settings(dir: &TempDir, memory_bytes: u64) -> SharedConfiguration {
    let mut settings = shared();
    let retry = &mut settings.endpoints.forwarder;
    retry.storage_path = dir.path().to_path_buf();
    retry.storage_max_size_in_bytes = 1024 * 1024;
    retry.storage_max_disk_ratio = 1.0;
    retry.retry_queue_payloads_max_size = ConfigValue::explicit(memory_bytes);
    settings
}

async fn persisted_queue(settings: &SharedConfiguration) -> LanedRetryQueue {
    queue_for(settings, &test_endpoints(1)).await
}

fn logical(name: &'static str) -> LogicalMetricBatch {
    LogicalMetricBatch::new(vec![gauge(name)])
}

fn gauge(name: &str) -> LogicalMetricSeries {
    let context = Context::from_parts(name.to_owned(), TagSet::default());
    conversion::convert(&Metric::gauge(context, (123, 2.0))).unwrap()
}

/// Returns the retry memory `batch` occupies as one entry.
fn entry_bytes(batch: &LogicalMetricBatch) -> u64 {
    RetryBatch(batch.clone(), 0).size_bytes()
}

#[tokio::test]
async fn disk_spill_restores_complete_logical_batches() {
    let dir = TempDir::new().unwrap();
    let metric = Metric::from_parts(
        Context::from_parts("rate", ["env:test"].into_iter().map(Into::into).collect::<TagSet>())
            .with_host(MetaString::from("host-a")),
        MetricValues::rate([(123, 20.0)], Duration::from_secs(10)),
        MetricMetadata::default()
            .with_origin(MetricOrigin::dogstatsd())
            .with_unit("request"),
    );
    let expected = LogicalMetricBatch::new(vec![conversion::convert(&metric).unwrap()]);
    let entry = RetryBatch(expected.clone(), 0);
    assert_eq!(entry.event_count(), 1);
    assert_eq!(entry.data_point_count(), 1);
    let settings = persisted_settings(&dir, entry.size_bytes() + entry_bytes(&logical("new")) - 1);
    let mut queue = persisted_queue(&settings).await;
    assert!(!queue.push_retry(None, expected.clone()).await.had_drops());
    assert!(!queue.push_retry(None, logical("new")).await.had_drops());
    // Memory is read before disk. Remove the newer entry, then reopen storage.
    let Some(PendingTransaction::LowPriority(new)) = queue.pop_shared().await else {
        panic!("missing new")
    };
    assert_eq!(new.0.series()[0].name(), "new");
    drop(queue);
    let mut queue = persisted_queue(&settings).await;
    let Some(PendingTransaction::LowPriority(restored)) = queue.pop_shared().await else {
        panic!("missing persisted entry")
    };
    assert_eq!(restored.0, expected);
    assert!(queue.is_empty());
}

#[tokio::test]
async fn shutdown_persists_unacknowledged_partial_and_queued_work_for_reencoding() {
    let dir = TempDir::new().unwrap();
    let settings = persisted_settings(&dir, 1024 * 1024);
    let (mut worker, _wire, id) = open_worker().await;
    worker.queue = persisted_queue(&settings).await;
    submit(&mut worker, "acknowledged").await;
    worker.on_transport(ack(id, 1)).await.unwrap();
    submit(&mut worker, "inflight").await;
    buffer(&mut worker, "partial").await;
    worker.accept([Event::Metric(Metric::gauge("fresh", (123, 1.0)))]).await;
    worker.shutdown().await.unwrap();
    let (mut restarted, mut wire, _) = open_worker().await;
    restarted.queue = persisted_queue(&settings).await;
    restarted.pump().await.unwrap();
    restarted.flush().await.unwrap();
    let batch = wire.try_recv().unwrap();
    assert_eq!(batch.batch_id, 1);
    let decoded = sequence(&batch);
    let names: Vec<_> = decoded
        .data
        .iter()
        .filter_map(|datum| match &datum.data {
            Some(metric_datum::Data::MetricNameDefine(name)) => Some(name),
            _ => None,
        })
        .collect();
    assert_eq!(names.len(), 3);
    assert!(restarted.queue.is_empty());
}

#[tokio::test]
async fn queue_prioritizes_new_input_and_counts_evicted_retries() {
    let dir = TempDir::new().unwrap();
    let mut settings = persisted_settings(&dir, entry_bytes(&logical("old")));
    settings.endpoints.forwarder.storage_max_size_in_bytes = 0;
    settings.endpoints.forwarder.high_prio_buffer_size = 1;
    let mut queue = persisted_queue(&settings).await;
    assert!(!queue.push_retry(None, logical("old")).await.had_drops());
    assert!(!queue.push_fresh(logical("new")).await.had_drops());
    let evicted = queue.push_fresh(logical("overflow")).await;
    // An oversized entry is counted as dropped without evicting the retry that does fit.
    assert_eq!(evicted.items_dropped, 1);
    assert_eq!(evicted.data_points_dropped, 1);
    let evicted = queue.push_retry(None, logical("two")).await;
    assert_eq!(evicted.items_dropped, 1);
    assert_eq!(evicted.data_points_dropped, 1);
    let Some(PendingTransaction::HighPriority(first)) = queue.pop_shared().await else {
        panic!("missing fresh")
    };
    assert_eq!(first.0.series()[0].name(), "new");
    let Some(PendingTransaction::LowPriority(second)) = queue.pop_shared().await else {
        panic!("missing retry")
    };
    assert_eq!(second.0.series()[0].name(), "two");
}

#[tokio::test]
async fn grpc_capability_failure_retains_logical_work_without_http_fallback() {
    let mut harness = Harness::new().await;
    submit(&mut harness.worker, "requests").await;
    harness.receive().await;
    harness
        .replies
        .send(Err(Status::failed_precondition("stateful disabled")))
        .await
        .unwrap();
    harness.progress().await;
    assert!(harness.worker.endpoints[0].suspended);
    assert!(harness.worker.transports[0].is_none());
    assert_eq!(queued_names(&mut harness.worker).await, ["requests"]);
}

async fn start_destination(
    endpoint: MetaString, flush_timeout: Duration, settings: &SharedConfiguration,
) -> ComponentControl<EventsBuffer> {
    let configuration = StatefulMetricsConfiguration {
        endpoints: vec![endpoint],

        api_key: Live::new_fixed("test-key".to_string()),
        compression_level: 3,
        flush_timeout,
        batch_capacity: 512,
        dictionary_max_entries: DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_ENTRIES,
        dictionary_max_bytes: DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_BYTES,
        retry_max_bytes: DEFAULT_STATEFUL_METRICS_RETRY_MAX_BYTES,
        retry_max_age: DEFAULT_STATEFUL_METRICS_RETRY_MAX_AGE,
        retry_max_attempts: DEFAULT_STATEFUL_METRICS_RETRY_MAX_ATTEMPTS,
        queue: queue_configuration(settings),
        stop_timeout: TEST_TIMEOUT,
    };
    start_configured_destination(configuration).await
}

async fn start_configured_destination(configuration: StatefulMetricsConfiguration) -> ComponentControl<EventsBuffer> {
    TestComponentDriver::options()
        .with_component_id("dd_stateful_metrics_out")
        .with_interconnect_capacity(NonZeroUsize::new(4).unwrap())
        .with_shutdown_timeout(TEST_TIMEOUT)
        .destination(configuration)
        .await
        .unwrap()
}

#[tokio::test]
async fn destination_flushes_sparse_input_and_waits_for_ack_on_shutdown() {
    let mut harness = Harness::new().await;
    harness.worker.transports[0] = None;
    let mut control = start_destination(
        harness.worker.endpoints[0].address.uri().to_string().into(),
        Duration::from_millis(20),
        &shared(),
    )
    .await;
    let mut events = EventsBuffer::default();
    assert!(events
        .try_push(Event::Metric(Metric::gauge("sparse", (123, 1.0))))
        .is_none());
    control.send(events).await;
    let batch = harness.receive().await;
    assert!(has_name(&sequence(&batch)));
    control.supervisor().wait_for_running_descendants(2).await;
    let tree = control.supervisor().tree_handle().snapshot();
    assert_eq!(tree.root.children[0].children[0].name, "stateful-metrics-single-sender");
    control.signal_shutdown();
    control
        .wait_until(
            || control.is_finished(),
            || "destination to close the sender input".to_owned(),
        )
        .await;
    assert!(!control.supervisor().is_finished());
    harness
        .replies
        .send(Ok(BatchStatus {
            batch_id: batch.batch_id,
            status: 1,
        }))
        .await
        .unwrap();
    control.wait().await.unwrap();
}

#[tokio::test]
async fn router_sends_supported_series_only_to_stateful_destination() {
    let component = ComponentContext::test_transform("dd_stateful_metrics_router");
    let router = StatefulMetricsRouterConfiguration
        .build(BuildContext::new(component.clone(), ResourceRegistry::new()))
        .await
        .unwrap();
    let mut dispatcher = Dispatcher::new(component.clone());
    let (series_tx, mut series_rx) = mpsc::channel(4);
    let (http_tx, mut http_rx) = mpsc::channel(4);
    let series_output = OutputName::Given("stateful".into());
    let http_output = OutputName::Given("http".into());
    dispatcher.add_output(series_output.clone()).unwrap();
    dispatcher.add_output(http_output.clone()).unwrap();
    dispatcher.attach_sender_to_output(&series_output, series_tx).unwrap();
    dispatcher.attach_sender_to_output(&http_output, http_tx).unwrap();
    let (in_tx, in_rx) = mpsc::channel(4);
    let health_registry = HealthRegistry::new();
    let health = health_registry
        .register_component(&SubsystemIdentifier::from_dotted("test"))
        .unwrap();
    let topology = TopologyContext::new(
        Arc::from("test"),
        MemoryLimiter::noop(),
        health_registry,
        Handle::current(),
        DataspaceRegistry::new(),
    );
    let context = TransformContext::new(
        &topology,
        &component,
        ComponentRegistry::default(),
        health,
        dispatcher,
        Consumer::new(component.clone(), in_rx),
    );
    let task = tokio::spawn(router.run(context));
    let mut events = EventsBuffer::default();
    for metric in [
        Metric::gauge("gauge", 1.0),
        Metric::counter("count", 1.0),
        Metric::rate("rate", 1.0, Duration::from_secs(10)),
        Metric::distribution("sketch", 1.0),
        Metric::histogram("histogram", 1.0),
        Metric::set("set", "value"),
    ] {
        assert!(events.try_push(Event::Metric(metric)).is_none());
    }
    in_tx.send(events).await.unwrap();
    drop(in_tx);
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap().unwrap();
    let series = series_rx.recv().await.unwrap();
    assert_eq!(series.len(), 3);
    assert!(series.into_iter().all(|event| router::supports(&event)));
    let http = http_rx.recv().await.unwrap();
    assert_eq!(http.len(), 3);
    assert!(http.into_iter().all(|event| !router::supports(&event)));
}

#[tokio::test]
async fn transport_backpressure_drains_in_order_without_an_ack() {
    let mut harness = Harness::new().await;
    let (sender, mut receiver) = mpsc::channel(1);
    let _request_sender = replace(&mut harness.worker.transports[0].as_mut().unwrap().sender, sender);
    submit(&mut harness.worker, "first").await;
    submit(&mut harness.worker, "second").await;
    assert_eq!(receiver.try_recv().unwrap().batch_id, 1);
    // Cancel the event wait after it moves the pending payload into the available slot.
    assert!(timeout(
        Duration::from_millis(20),
        next_transport_event(&mut harness.worker.transports)
    )
    .await
    .is_err());
    assert_eq!(receiver.try_recv().unwrap().batch_id, 2);
    assert!(harness.worker.transports[0].as_ref().unwrap().pending.is_empty());
    assert_eq!(harness.worker.core.inflight_len(), 2);
}

#[tokio::test]
async fn threshold_flush_and_rejected_partial_remain_recoverable() {
    let mut worker = worker_with(1, 2).await;
    let (mut wire, stream_id) = start_all(&mut worker).await.pop().unwrap();
    buffer(&mut worker, "first").await;
    buffer(&mut worker, "second").await;
    assert!(wire.try_recv().is_ok());
    assert_eq!(worker.core.inflight_len(), 1);
    assert_eq!(worker.flush_deadline(), None);
    buffer(&mut worker, "partial").await;
    let effects = worker.core.handle_timer(stream_id, TimerKind::RotateStream);
    worker.apply(effects).await.unwrap();
    worker.flush().await.unwrap();
    assert_eq!(queued_names(&mut worker).await, ["partial"]);
    assert_eq!(worker.buffered_deadline, None);
}

#[tokio::test]
async fn oversized_retry_does_not_stop_remaining_recovery() {
    let dir = TempDir::new().unwrap();
    let settings = persisted_settings(&dir, entry_bytes(&logical("tiny")));
    let mut worker = worker_without_internal_retries(1).await;
    let (_wire, _) = start_all(&mut worker).await.pop().unwrap();
    worker.queue = persisted_queue(&settings).await;
    submit(&mut worker, "too-large-to-retry").await;
    submit(&mut worker, "tiny").await;
    worker
        .fail(E0, MetricStreamFailureKind::Unavailable, "disconnect")
        .await
        .unwrap();
    assert_eq!(queued_names(&mut worker).await, ["tiny"]);
    assert_eq!(worker.endpoints[0].timers.len(), 1);
}

#[tokio::test]
async fn destination_shutdown_timeout_persists_missing_ack() {
    let dir = TempDir::new().unwrap();
    let settings = persisted_settings(&dir, 1024 * 1024);
    let mut harness = Harness::new().await;
    harness.worker.transports[0] = None;
    let endpoint: MetaString = harness.worker.endpoints[0].address.uri().to_string().into();
    let mut control = start_destination(endpoint.clone(), Duration::from_millis(10), &settings).await;
    let mut events = EventsBuffer::default();
    assert!(events
        .try_push(Event::Metric(Metric::gauge("unacknowledged", (123, 1.0))))
        .is_none());
    control.send(events).await;
    harness.receive().await;
    control.signal_shutdown();
    // The intake deliberately never acknowledges this payload.
    control.wait().await.unwrap();
    let mut queue = queue_for(&settings, &[endpoint]).await;
    let Some(PendingTransaction::LowPriority(batch)) = queue.pop_shared().await else {
        panic!("shutdown lost unacknowledged data")
    };
    assert_eq!(batch.0.series()[0].name(), "unacknowledged");
    assert!(queue.is_empty());
}

impl StatefulMetricsWorker {
    async fn accept(&mut self, events: impl IntoIterator<Item = Event>) {
        self.enqueue(Self::convert_batch(events)).await;
    }

    /// Returns retained work to the retry queue while keeping the queue open for inspection.
    async fn shutdown_into_queue(&mut self) {
        let effects = self.core.shutdown();
        self.apply(effects).await.unwrap();
    }
}

struct TestSession {
    key: String,
    received: mpsc::Receiver<StatefulBatch>,
    replies: mpsc::Sender<Result<BatchStatus, Status>>,
}

impl TestSession {
    async fn receive(&mut self) -> StatefulBatch {
        timeout(TEST_TIMEOUT, self.received.recv()).await.unwrap().unwrap()
    }

    async fn acknowledge(&self, batch: &StatefulBatch) {
        self.replies
            .send(Ok(BatchStatus {
                batch_id: batch.batch_id,
                status: 1,
            }))
            .await
            .unwrap();
    }
}

#[derive(Clone)]
struct SessionIntake {
    sessions: mpsc::Sender<TestSession>,
}

#[tonic::async_trait]
impl StatefulIntake for SessionIntake {
    type StatefulStreamStream = Pin<Box<dyn Stream<Item = Result<BatchStatus, Status>> + Send>>;

    async fn stateful_stream(
        &self, request: Request<Streaming<StatefulBatch>>,
    ) -> Result<Response<Self::StatefulStreamStream>, Status> {
        let key = request
            .metadata()
            .get("dd-api-key")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let (received, received_rx) = mpsc::channel(32);
        let (replies_tx, mut replies) = mpsc::channel(32);
        self.sessions
            .send(TestSession {
                key,
                received: received_rx,
                replies: replies_tx,
            })
            .await
            .unwrap();
        let mut inbound = request.into_inner();
        let stream = async_stream::try_stream! {
            while let Some(batch) = inbound.message().await? {
                received.send(batch).await.map_err(|_| Status::cancelled("test finished"))?;
                yield replies.recv().await.ok_or_else(|| Status::cancelled("test finished"))??;
            }
        };
        Ok(Response::new(Box::pin(stream)))
    }

    async fn stateless(&self, _: Request<StatelessRequest>) -> Result<Response<StatelessResponse>, Status> {
        Err(Status::unimplemented("stateless"))
    }
}

struct EndpointHarness {
    endpoint: MetaString,
    sessions: mpsc::Receiver<TestSession>,
    server: JoinHandle<()>,
}

impl EndpointHarness {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap()).into();
        let (sessions, sessions_rx) = mpsc::channel(32);
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(StatefulIntakeServer::new(SessionIntake { sessions }))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        Self {
            endpoint,
            sessions: sessions_rx,
            server,
        }
    }

    async fn tls(cert: &SelfSignedCert) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("https://{}", listener.local_addr().unwrap()).into();
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert.cert_chain(), cert.private_key())
            .unwrap();
        // build_https_connector used to negotiate HTTP/1.1 while Tonic sent HTTP/2.
        // Advertising only h2 must override this server's HTTP/1.1 preference.
        config.alpn_protocols = vec![b"http/1.1".to_vec(), b"h2".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let (sessions, sessions_rx) = mpsc::channel(32);
        let server = tokio::spawn(async move {
            let incoming = async_stream::stream! {
                loop {
                    let (socket, _) = listener.accept().await.unwrap();
                    if let Ok(stream) = acceptor.accept(socket).await {
                        assert_eq!(stream.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
                        yield Ok::<_, io::Error>(stream);
                    }
                }
            };
            Server::builder()
                .add_service(StatefulIntakeServer::new(SessionIntake { sessions }))
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });
        Self {
            endpoint,
            sessions: sessions_rx,
            server,
        }
    }

    async fn session(&mut self) -> TestSession {
        timeout(TEST_TIMEOUT, self.sessions.recv()).await.unwrap().unwrap()
    }

    async fn start(&self, settings: &SharedConfiguration, key: Live<String>) -> ComponentControl<EventsBuffer> {
        start_configured_destination(StatefulMetricsConfiguration {
            endpoints: vec![self.endpoint.clone()],

            api_key: key,
            compression_level: 3,
            flush_timeout: Duration::from_millis(10),
            batch_capacity: 512,
            dictionary_max_entries: DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_ENTRIES,
            dictionary_max_bytes: DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_BYTES,
            retry_max_bytes: DEFAULT_STATEFUL_METRICS_RETRY_MAX_BYTES,
            retry_max_age: DEFAULT_STATEFUL_METRICS_RETRY_MAX_AGE,
            retry_max_attempts: DEFAULT_STATEFUL_METRICS_RETRY_MAX_ATTEMPTS,
            queue: queue_configuration(settings),
            stop_timeout: Duration::from_millis(200),
        })
        .await
    }
}

impl Drop for EndpointHarness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn down_endpoint_payloads_wait_in_the_core_without_blocking_others() {
    let mut worker = worker_with(2, 512).await;
    let mut opened = start_all(&mut worker).await;
    let (_down_wire, down_stream) = opened.pop().unwrap();
    let (mut healthy_wire, healthy_stream) = opened.pop().unwrap();
    submit(&mut worker, "first").await;
    worker
        .fail(E1, MetricStreamFailureKind::Unavailable, "disconnect")
        .await
        .unwrap();
    // The down endpoint's copy of later payloads waits in the core, compressed, not in its lane.
    submit(&mut worker, "second").await;
    assert_eq!(healthy_wire.try_recv().unwrap().batch_id, 1);
    assert_eq!(healthy_wire.try_recv().unwrap().batch_id, 2);
    assert!(worker.queue.is_empty());
    worker.on_transport(ack_on(E0, healthy_stream, 1)).await.unwrap();
    worker.on_transport(ack_on(E0, healthy_stream, 2)).await.unwrap();
    assert_eq!(worker.core.inflight_len(), 2);
    assert_eq!(worker.core.retry_stats().pending_copies, 2);
    assert!(!worker.can_pump());
    submit(&mut worker, "third").await;
    assert_eq!(healthy_wire.try_recv().unwrap().batch_id, 3);

    let effects = worker.core.handle_timer(down_stream, TimerKind::Reconnect);
    worker.apply(effects).await.unwrap();
    let reconnected = worker.core.current_stream_id(E1).unwrap();
    let mut replay_wire = attach(&mut worker, E1, reconnected).await;
    let mut replayed = Vec::new();
    while let Ok(batch) = replay_wire.try_recv() {
        assert!(has_name(&sequence(&batch)));
        replayed.push(batch.batch_id);
    }
    assert_eq!(replayed, [1, 2, 3]);
    assert!(healthy_wire.try_recv().is_err());
    assert!(worker.queue.is_empty());
    assert_eq!(worker.core.encoding_count(), 3);
}

/// Past the core's retry budget, a down endpoint's copies move to its lane, and are encoded again once it returns.
#[tokio::test]
async fn down_endpoint_payloads_over_the_budget_wait_in_its_lane() {
    let mut worker = worker_without_internal_retries(2).await;
    let mut opened = start_all(&mut worker).await;
    let (_down_wire, down_stream) = opened.pop().unwrap();
    let (mut healthy_wire, healthy_stream) = opened.pop().unwrap();
    submit(&mut worker, "first").await;
    worker
        .fail(E1, MetricStreamFailureKind::Unavailable, "disconnect")
        .await
        .unwrap();
    submit(&mut worker, "second").await;
    assert_eq!(healthy_wire.try_recv().unwrap().batch_id, 1);
    assert_eq!(healthy_wire.try_recv().unwrap().batch_id, 2);
    assert!(worker.queue.lane_is_empty(E0));
    assert!(!worker.queue.lane_is_empty(E1));
    worker.on_transport(ack_on(E0, healthy_stream, 1)).await.unwrap();
    worker.on_transport(ack_on(E0, healthy_stream, 2)).await.unwrap();
    assert_eq!(worker.core.inflight_len(), 0);
    assert_eq!(worker.core.retry_stats().returned_over_budget, 2);

    let effects = worker.core.handle_timer(down_stream, TimerKind::Reconnect);
    worker.apply(effects).await.unwrap();
    let reconnected = worker.core.current_stream_id(E1).unwrap();
    let mut replay_wire = attach(&mut worker, E1, reconnected).await;
    worker.pump().await.unwrap();
    let mut resent = Vec::new();
    while let Ok(batch) = replay_wire.try_recv() {
        assert!(has_name(&sequence(&batch)));
        resent.push(batch.batch_id);
    }
    assert_eq!(resent, [1, 2]);
    assert!(worker.queue.is_empty());
    assert_eq!(worker.core.encoding_count(), 4, "lane batches are encoded again");
}

#[tokio::test]
async fn acknowledgement_deadlines_are_tracked_per_endpoint() {
    let mut worker = worker_with(2, 512).await;
    let opened = start_all(&mut worker).await;
    submit(&mut worker, "sent").await;
    assert!(worker.endpoints.iter().all(|endpoint| endpoint.ack_deadline.is_some()));
    worker.on_transport(ack_on(E0, opened[0].1, 1)).await.unwrap();
    assert_eq!(worker.endpoints[0].ack_deadline, None);
    let (endpoint, deadline) = worker.next_ack_deadline();
    assert_eq!(endpoint, E1);
    assert_eq!(deadline, worker.endpoints[1].ack_deadline);
    assert!(deadline.is_some());
    // The batch stays held until the second endpoint acknowledges it too.
    assert_eq!(worker.core.inflight_len(), 1);
    worker.on_transport(ack_on(E1, opened[1].1, 1)).await.unwrap();
    assert_eq!(worker.next_ack_deadline().1, None);
    assert!(worker.is_drained());
}

#[tokio::test]
async fn suspended_endpoint_keeps_its_lane_while_others_drain() {
    let mut worker = worker_with(2, 512).await;
    let opened = start_all(&mut worker).await;
    submit(&mut worker, "sent").await;
    worker
        .fail(E1, MetricStreamFailureKind::Unauthenticated, "rejected key")
        .await
        .unwrap();
    assert!(worker.endpoints[1].suspended);
    assert!(!worker.all_suspended());
    worker.on_transport(ack_on(E0, opened[0].1, 1)).await.unwrap();
    assert!(worker.is_drained());
    assert!(!worker.queue.lane_is_empty(E1));
    worker.update_credentials("new-key").await.unwrap();
    assert!(!worker.endpoints[1].suspended);
    assert_eq!(lane_names(&mut worker.queue, E1).await, ["sent"]);
}

#[tokio::test]
async fn endpoint_lanes_persist_by_address_across_endpoint_reordering() {
    let dir = TempDir::new().unwrap();
    let settings = persisted_settings(&dir, 1024 * 1024);
    let config = queue_configuration(&settings);
    let [primary, removed, added]: [MetaString; 3] = test_endpoints(3).try_into().unwrap();
    let original = [primary.clone(), removed.clone()];
    prepare_storage(&config, &original).await.unwrap();
    let mut queue = queue_for(&settings, &original).await;
    assert!(!queue.push_retry(Some(E1), logical("lane")).await.had_drops());
    assert!(!queue.push_retry(None, logical("shared")).await.had_drops());
    assert!(!queue.flush().await.unwrap().had_drops());

    // Lanes are keyed by address, so adding or reordering endpoints keeps each endpoint's retries.
    let reordered = [primary.clone(), added, removed];
    prepare_storage(&config, &reordered).await.unwrap();
    let mut queue = queue_for(&settings, &reordered).await;
    assert_eq!(shared_names(&mut queue).await, ["shared"]);
    assert!(lane_names(&mut queue, E1).await.is_empty());
    assert_eq!(lane_names(&mut queue, MetricEndpointId(2)).await, ["lane"]);
    assert!(!queue.flush().await.unwrap().had_drops());
    prepare_storage(&config, &[primary]).await.unwrap();
}

#[tokio::test]
async fn endpoint_removal_discards_only_removed_endpoints_persisted_retries() {
    for keep_additional in [false, true] {
        let dir = TempDir::new().unwrap();
        let settings = persisted_settings(&dir, 1024 * 1024);
        let config = queue_configuration(&settings);
        let original = test_endpoints(3);
        prepare_storage(&config, &original).await.unwrap();
        {
            let mut queue = queue_for(&settings, &original).await;
            assert!(!queue.push_retry(None, logical("shared")).await.had_drops());
            assert!(!queue.push_retry(Some(E0), logical("primary")).await.had_drops());
            assert!(!queue.push_retry(Some(E1), logical("removed")).await.had_drops());
            assert!(!queue
                .push_retry(Some(MetricEndpointId(2)), logical("other"))
                .await
                .had_drops());
            assert!(!queue.flush().await.unwrap().had_drops());
        }

        let retained = if keep_additional {
            vec![original[0].clone(), original[2].clone()]
        } else {
            vec![original[0].clone()]
        };
        prepare_storage(&config, &retained).await.unwrap();
        // Repeating startup is safe, including after migration to a single endpoint.
        prepare_storage(&config, &retained).await.unwrap();
        {
            let mut queue = queue_for(&settings, &retained).await;
            if keep_additional {
                assert_eq!(shared_names(&mut queue).await, ["shared"]);
                assert_eq!(lane_names(&mut queue, E0).await, ["primary"]);
                assert_eq!(lane_names(&mut queue, E1).await, ["other"]);
            } else {
                let mut names = shared_names(&mut queue).await;
                names.sort();
                assert_eq!(names, ["primary", "shared"]);
            }
            assert!(!queue.flush().await.unwrap().had_drops());
        }

        // Re-adding an endpoint must not resurrect the retries discarded during its removal.
        prepare_storage(&config, &original).await.unwrap();
        {
            let queue = queue_for(&settings, &original).await;
            assert!(queue.is_empty());
            assert!(!queue.flush().await.unwrap().had_drops());
        }
    }
}

fn names(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("series-{index:03}")).collect()
}

fn batch_of(names: &[String]) -> LogicalMetricBatch {
    LogicalMetricBatch::new(names.iter().map(|name| gauge(name)).collect())
}

fn settings_with_budget(memory_bytes: u64) -> SharedConfiguration {
    let mut settings = shared();
    settings.endpoints.forwarder.retry_queue_payloads_max_size = ConfigValue::explicit(memory_bytes);
    settings
}

/// A series whose tags make it far larger than an untagged gauge.
fn tagged(name: &'static str, tags: usize) -> LogicalMetricSeries {
    let tags: TagSet = (0..tags).map(|index| format!("tag-{index:04}:value").into()).collect();
    conversion::convert(&Metric::gauge(Context::from_parts(name, tags), (123, 2.0))).unwrap()
}

#[test]
fn retry_entries_persist_as_plain_logical_batches() {
    let batch = logical("persisted");
    let stored = serde_json::to_vec(&RetryBatch(batch.clone(), 42)).unwrap();
    assert_eq!(stored, serde_json::to_vec(&batch).unwrap());
    let restored: RetryBatch = serde_json::from_slice(&stored).unwrap();
    assert_eq!(restored.0, batch);
}

#[tokio::test]
async fn returned_batch_larger_than_a_lane_share_is_queued_as_ordered_chunks() {
    let names = names(40);
    let bytes = entry_bytes(&batch_of(&names));
    // The former even split gave each of the three queues a third of this budget, less than the batch.
    let budget = 2 * bytes;
    let mut worker = worker_without_internal_retries(2).await;
    worker.queue = queue_for(&settings_with_budget(budget), &test_endpoints(2)).await;
    let mut opened = start_all(&mut worker).await;
    let (_down_wire, _) = opened.pop().unwrap();
    let (mut healthy_wire, _) = opened.pop().unwrap();
    worker
        .accept(names.iter().map(|name| {
            Event::Metric(Metric::gauge(
                Context::from_parts(name.clone(), TagSet::default()),
                (123, 2.0),
            ))
        }))
        .await;
    worker.pump().await.unwrap();
    worker.flush().await.unwrap();
    assert_eq!(healthy_wire.try_recv().unwrap().batch_id, 1);
    worker
        .fail(E1, MetricStreamFailureKind::Unavailable, "disconnect")
        .await
        .unwrap();

    let mut chunks = 0;
    let mut queued = Vec::new();
    while let Some(entry) = worker.queue.pop_lane(E1).await {
        assert!(entry.size_bytes() <= budget / 4);
        chunks += 1;
        queued.extend(entry.0.series().iter().map(|series| series.name().to_owned()));
    }
    assert!(chunks > 1);
    assert_eq!(queued, names);
    assert!(worker.queue.is_empty());
}

#[tokio::test]
async fn oversized_retry_splits_without_drops_and_preserves_series_order() {
    let names = names(40);
    let batch = batch_of(&names);
    let budget = 2 * entry_bytes(&batch);
    let mut queue = queue_for(&settings_with_budget(budget), &test_endpoints(2)).await;
    let result = queue.push_retry(Some(E1), batch).await;
    assert!(!result.had_drops());
    let mut chunks = Vec::new();
    while let Some(entry) = queue.pop_lane(E1).await {
        assert!(entry.size_bytes() <= budget / 4);
        chunks.push(entry.0);
    }
    assert!(chunks.len() > 1);
    assert_eq!(
        chunks.iter().map(LogicalMetricBatch::point_count).sum::<usize>(),
        names.len()
    );
    let queued: Vec<_> = chunks
        .iter()
        .flat_map(|chunk| chunk.series().iter().map(|series| series.name().to_owned()))
        .collect();
    assert_eq!(queued, names);
}

#[tokio::test]
async fn one_lane_can_use_the_whole_pooled_budget() {
    let names = names(6);
    let entry = entry_bytes(&logical("series-000"));
    // The former even split capped this lane at two entries.
    let mut queue = queue_for(&settings_with_budget(6 * entry), &test_endpoints(2)).await;
    for name in &names {
        assert!(!queue
            .push_retry(Some(E0), batch_of(std::slice::from_ref(name)))
            .await
            .had_drops());
    }
    assert_eq!(lane_names(&mut queue, E0).await, names);
    assert!(queue.is_empty());
}

#[tokio::test]
async fn pooled_budget_evicts_the_globally_oldest_retry() {
    let mut queue = queue_for(
        &settings_with_budget(3 * entry_bytes(&logical("s1"))),
        &test_endpoints(2),
    )
    .await;
    assert!(!queue.push_retry(None, logical("s1")).await.had_drops());
    assert!(!queue.push_retry(Some(E0), logical("x1")).await.had_drops());
    assert!(!queue.push_retry(Some(E1), logical("y1")).await.had_drops());
    // A lane push evicts the oldest retry, which waits in the shared queue.
    let evicted = queue.push_retry(Some(E1), logical("y2")).await;
    assert_eq!(evicted.items_dropped, 1);
    assert_eq!(evicted.events_dropped, 1);
    assert_eq!(evicted.data_points_dropped, 1);
    // A shared push evicts the oldest retry, which now waits in the other lane.
    let evicted = queue.push_retry(None, logical("s2")).await;
    assert_eq!(evicted.items_dropped, 1);
    assert_eq!(evicted.data_points_dropped, 1);
    assert_eq!(shared_names(&mut queue).await, ["s2"]);
    assert!(lane_names(&mut queue, E0).await.is_empty());
    assert_eq!(lane_names(&mut queue, E1).await, ["y1", "y2"]);
}

#[tokio::test]
async fn pooled_budget_spills_the_globally_oldest_retry_to_disk() {
    let dir = TempDir::new().unwrap();
    let mut settings = persisted_settings(&dir, 3 * entry_bytes(&logical("s1")));
    settings.endpoints.forwarder.flush_to_disk_mem_ratio = 0.0;
    let mut queue = queue_for(&settings, &test_endpoints(2)).await;
    assert!(!queue.push_retry(None, logical("s1")).await.had_drops());
    assert!(!queue.push_retry(Some(E0), logical("x1")).await.had_drops());
    assert!(!queue.push_retry(Some(E1), logical("y1")).await.had_drops());
    assert!(!queue.push_retry(Some(E1), logical("y2")).await.had_drops());
    // The shared retry moved to disk; memory now holds the lanes' retries.
    assert!(!queue.push_retry(Some(E0), logical("x2")).await.had_drops());
    assert_eq!(lane_names(&mut queue, E0).await, ["x2", "x1"]);
    assert_eq!(lane_names(&mut queue, E1).await, ["y1", "y2"]);
    assert_eq!(shared_names(&mut queue).await, ["s1"]);
    assert!(queue.is_empty());
}

#[tokio::test]
async fn single_endpoint_queue_keeps_the_whole_budget_without_lanes() {
    let names = names(4);
    let entry = entry_bytes(&logical("series-000"));
    let mut queue = queue_for(&settings_with_budget(3 * entry), &test_endpoints(1)).await;
    assert_eq!(queue.lane_count(), 0);
    for name in &names[..3] {
        assert!(!queue
            .push_retry(Some(E0), batch_of(std::slice::from_ref(name)))
            .await
            .had_drops());
    }
    let evicted = queue.push_retry(Some(E0), batch_of(&names[3..])).await;
    assert_eq!(evicted.items_dropped, 1);
    assert_eq!(evicted.data_points_dropped, 1);
    assert_eq!(shared_names(&mut queue).await, names[1..]);
}

#[tokio::test]
async fn series_larger_than_the_chunk_limit_is_queued_alone() {
    let large = tagged("large", 60);
    let large_bytes = entry_bytes(&LogicalMetricBatch::new(vec![large.clone()]));
    let budget = 2 * large_bytes;
    let huge = tagged("huge", 600);
    assert!(entry_bytes(&LogicalMetricBatch::new(vec![huge.clone()])) > budget);
    assert!(entry_bytes(&LogicalMetricBatch::new(vec![gauge("a"), gauge("b")])) <= budget / 4);
    let mut queue = queue_for(&settings_with_budget(budget), &test_endpoints(2)).await;

    let batch = LogicalMetricBatch::new(vec![gauge("a"), gauge("b"), large, gauge("c"), gauge("d")]);
    assert!(!queue.push_retry(Some(E0), batch).await.had_drops());
    let mut chunks = Vec::new();
    while let Some(entry) = queue.pop_lane(E0).await {
        chunks.push(entry.0.series().iter().map(|s| s.name().to_owned()).collect::<Vec<_>>());
    }
    assert_eq!(chunks, [vec!["a", "b"], vec!["large"], vec!["c", "d"]]);

    // A series larger than the whole budget is still rejected and counted, without blocking its neighbours.
    let batch = LogicalMetricBatch::new(vec![gauge("a"), huge, gauge("b")]);
    let result = queue.push_retry(Some(E0), batch).await;
    assert_eq!(result.items_dropped, 1);
    assert_eq!(result.events_dropped, 1);
    assert_eq!(result.data_points_dropped, 1);
    assert_eq!(lane_names(&mut queue, E0).await, ["a", "b"]);
}

#[tokio::test]
async fn destination_sends_every_payload_to_every_endpoint_and_replays_only_to_the_failed_one() {
    let mut healthy = EndpointHarness::new().await;
    let mut flaky = EndpointHarness::new().await;
    let mut control = start_configured_destination(StatefulMetricsConfiguration {
        endpoints: vec![healthy.endpoint.clone(), flaky.endpoint.clone()],

        api_key: Live::new_fixed("test-key".to_owned()),
        compression_level: 3,
        flush_timeout: Duration::from_millis(10),
        batch_capacity: 512,
        dictionary_max_entries: DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_ENTRIES,
        dictionary_max_bytes: DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_BYTES,
        retry_max_bytes: DEFAULT_STATEFUL_METRICS_RETRY_MAX_BYTES,
        retry_max_age: DEFAULT_STATEFUL_METRICS_RETRY_MAX_AGE,
        retry_max_attempts: DEFAULT_STATEFUL_METRICS_RETRY_MAX_ATTEMPTS,
        queue: queue_configuration(&shared()),
        stop_timeout: TEST_TIMEOUT,
    })
    .await;
    let mut healthy_session = healthy.session().await;
    let mut flaky_session = flaky.session().await;
    send_metrics(&control, 123).await;
    let first = healthy_session.receive().await;
    let copy = flaky_session.receive().await;
    assert_eq!(first.batch_id, 1);
    assert_eq!(sequence(&first), sequence(&copy));
    flaky_session
        .replies
        .send(Err(Status::unavailable("disconnect")))
        .await
        .unwrap();
    healthy_session.acknowledge(&first).await;
    let mut reconnected = flaky.session().await;
    let replay = reconnected.receive().await;
    assert_eq!(replay.batch_id, 1);
    assert_eq!(sequence(&replay), sequence(&copy));
    reconnected.acknowledge(&replay).await;
    control.signal_shutdown();
    control.wait().await.unwrap();
    assert!(healthy_session.received.try_recv().is_err());
}

#[test]
fn core_config_uses_default_dictionary_caps() {
    let config = core_config(2, 256);
    assert_eq!(config.batch_capacity, 256);
    assert_eq!(config.metrics_endpoints, 2);
    assert_eq!(config.sender.max_inflight_payloads, MAX_INFLIGHT_BATCHES);
    assert_eq!(
        config.metrics_dictionary_eviction,
        MetricDictionaryEvictionConfig {
            max_item_count: 60_000,
            max_memory_bytes: 256 * 1024 * 1024,
            stale_after: Duration::from_secs(30 * 60),
            ..MetricDictionaryEvictionConfig::default()
        }
    );
}

#[test]
fn core_config_applies_configured_dictionary_caps() {
    let mut configuration = configuration(1, 512);
    configuration.dictionary_max_entries = NonZeroUsize::new(100_000).unwrap();
    configuration.dictionary_max_bytes = NonZeroU64::new(128 * 1024 * 1024).unwrap();
    let eviction = configuration.core_config().metrics_dictionary_eviction;
    assert_eq!(eviction.max_item_count, 100_000);
    assert_eq!(eviction.max_memory_bytes, 128 * 1024 * 1024);
    assert_eq!(eviction.stale_after, DICTIONARY_STALE_AFTER);

    configuration.dictionary_max_bytes = NonZeroU64::MAX;
    let eviction = configuration.core_config().metrics_dictionary_eviction;
    assert_eq!(eviction.max_memory_bytes, i64::MAX);
}

#[tokio::test]
async fn dictionary_gauges_report_core_dictionary_stats() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);
    let (mut worker, _wire, _) = open_worker().await;
    submit(&mut worker, "requests").await;
    worker.telemetry.record_dictionary(worker.core.dictionary_stats());

    let stats = worker.core.dictionary_stats();
    assert!(stats.entries > 0);
    assert!(stats.estimated_bytes > 0);
    // Below the caps every definition is retained.
    assert_eq!(stats.transient_entries, 0);
    assert!(stats.frequency_sketch_bytes > 0);
    for (name, value) in [
        ("stateful_metrics_dictionary_entries", stats.entries),
        ("stateful_metrics_dictionary_estimated_bytes", stats.estimated_bytes),
        ("stateful_metrics_dictionary_transient_entries", stats.transient_entries),
        ("stateful_metrics_dictionary_transient_bytes", stats.transient_bytes),
        (
            "stateful_metrics_dictionary_frequency_sketch_bytes",
            stats.frequency_sketch_bytes,
        ),
    ] {
        assert_eq!(recorder.gauge(name), Some(value as f64), "{name}");
    }
}

mod memory_accounting {
    use std::num::NonZeroU64;

    use agent_data_plane_config::ConfigValue;
    use saluki_core::{
        accounting::{ComponentRegistry, MemoryBounds as _, MemoryGrant, VerifierError},
        support::SubsystemIdentifier,
    };

    use super::{configuration, queue_configuration, shared};

    #[test]
    fn startup_memory_verification_includes_shared_budgets() {
        // Struct-only accounting admitted grants smaller than the configured dictionary and logical retry budgets.
        for endpoints in [1, 3] {
            for (retry_bytes, core_retry_bytes, budget_bytes) in [
                (8192, 4096, 77_824),
                (8192, 0, 73_728),
                (0, 4096, 69_632),
                (0, 0, 65_536),
            ] {
                let mut config = configuration(endpoints, 512);
                config.dictionary_max_bytes = NonZeroU64::new(65_536).unwrap();
                config.retry_max_bytes = core_retry_bytes;
                let mut shared = shared();
                shared.endpoints.forwarder.retry_queue_payloads_max_size = ConfigValue::explicit(retry_bytes);
                config.queue = queue_configuration(&shared);

                let registry = ComponentRegistry::default();
                config.specify_bounds(&mut registry.bounds_builder(&SubsystemIdentifier::from_dotted("test")));
                let required_bytes = registry.as_bounds().total_minimum_required_bytes() + budget_bytes;

                // Endpoint lanes share one retry budget for the sender, including when retry memory is disabled.
                assert!(registry
                    .root()
                    .verify_bounds(MemoryGrant::effective(required_bytes).unwrap())
                    .is_ok());
                assert_eq!(
                    registry
                        .root()
                        .verify_bounds(MemoryGrant::effective(required_bytes - 1).unwrap())
                        .err(),
                    Some(VerifierError::FirmLimitExceedsAvailable {
                        available_bytes: required_bytes - 1,
                        firm_limit_bytes: required_bytes,
                    })
                );
            }
        }
    }
}

async fn send_metrics(input: &ComponentControl<EventsBuffer>, timestamp: u64) {
    let mut events = EventsBuffer::default();
    for name in ["first", "second"] {
        assert!(events
            .try_push(Event::Metric(Metric::gauge(name, (timestamp, 1.0))))
            .is_none());
    }
    input.send(events).await;
}

#[tokio::test]
async fn destination_shutdown_and_restart_preserves_unacknowledged_metrics() {
    let mut harness = EndpointHarness::new().await;
    let dir = TempDir::new().unwrap();
    let settings = persisted_settings(&dir, 1024 * 1024);
    let mut control = harness.start(&settings, Live::new_fixed("test-key".to_owned())).await;
    let mut session = harness.session().await;
    send_metrics(&control, 123).await;
    let original = sequence(&session.receive().await);
    let mut names: Vec<_> = original
        .data
        .iter()
        .filter_map(|datum| match &datum.data {
            Some(metric_datum::Data::MetricNameDefine(name)) => Some(name.value.as_str()),
            _ => None,
        })
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["first", "second"]);
    control.signal_shutdown();
    control.wait().await.unwrap();
    let mut control = harness.start(&settings, Live::new_fixed("test-key".to_owned())).await;
    let mut replay_session = harness.session().await;
    let batch = replay_session.receive().await;
    assert_eq!(sequence(&batch), original);
    replay_session.acknowledge(&batch).await;
    control.signal_shutdown();
    control.wait().await.unwrap();
}

#[tokio::test]
async fn destination_refreshes_credentials_and_resumes_suspended_delivery() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);
    let mut harness = EndpointHarness::new().await;
    let (mut publisher, source) = channel("test-key".to_owned());
    let mut control = harness.start(&shared(), source.live(|key| key)).await;
    let mut session = harness.session().await;
    assert_eq!(session.key, "test-key");
    send_metrics(&control, 123).await;
    session.receive().await;
    session
        .replies
        .send(Err(Status::unauthenticated("rejected key")))
        .await
        .unwrap();
    let failure = Key::from_parts(
        "stateful_metrics_stream_failures_total",
        vec![
            Label::new("component_id", "dd_stateful_metrics_out"),
            Label::new("component_type", "destination"),
            Label::new("kind", "Unauthenticated"),
        ],
    );
    timeout(TEST_TIMEOUT, async {
        while recorder.counter(failure.clone()) != Some(1) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    publisher.publish("new-key".to_owned());
    let mut updated = harness.session().await;
    assert_eq!(updated.key, "new-key");
    let mut events = EventsBuffer::default();
    assert!(events
        .try_push(Event::Metric(Metric::gauge("after-refresh", (124, 1.0))))
        .is_none());
    control.send(events).await;
    timeout(TEST_TIMEOUT, async {
        loop {
            let batch = updated.receive().await;
            let fresh = sequence(&batch).data.iter().any(|datum| {
                matches!(
                    &datum.data, Some(metric_datum::Data::MetricNameDefine(name)) if name.value == "after-refresh"
                )
            });
            updated.acknowledge(&batch).await;
            if fresh {
                break;
            }
        }
    })
    .await
    .unwrap();
    control.signal_shutdown();
    control.wait().await.unwrap();
}

#[tokio::test]
async fn sender_drains_a_full_input_channel_when_input_closes() {
    let dir = TempDir::new().unwrap();
    let settings = persisted_settings(&dir, 1024 * 1024);
    let mut client = worker().await;
    client.queue = persisted_queue(&settings).await;
    let (input, receiver) = mpsc::channel(SENDER_INPUT_CAPACITY);
    input.try_send(logical("queued-before-start")).unwrap();
    assert_eq!(input.capacity(), 0);
    let task = tokio::spawn(client.run(receiver, Live::new_fixed("test-key".to_owned()), Duration::ZERO));
    input.send(logical("waiting-for-capacity")).await.unwrap();
    drop(input);
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap().unwrap();
    let mut queue = persisted_queue(&settings).await;
    let mut names = shared_names(&mut queue).await;
    names.sort_unstable();
    assert_eq!(names, ["queued-before-start", "waiting-for-capacity"]);
}

#[tokio::test]
async fn destination_shutdown_preserves_all_buffered_input_without_acks() {
    let harness = EndpointHarness::new().await;
    let dir = TempDir::new().unwrap();
    let settings = persisted_settings(&dir, 1024 * 1024);
    let mut control = harness.start(&settings, Live::new_fixed("test-key".to_owned())).await;
    // Exceed both the destination's four-entry input channel and the one-entry sender channel.
    for timestamp in 123..135 {
        send_metrics(&control, timestamp).await;
    }
    control.signal_shutdown();
    control.wait().await.unwrap();
    let mut queue = queue_for(&settings, std::slice::from_ref(&harness.endpoint)).await;
    let mut recovered = Vec::new();
    while let Some(batch) = queue.pop_shared().await {
        let batch = match batch {
            PendingTransaction::HighPriority(batch) | PendingTransaction::LowPriority(batch) => batch,
        };
        for series in batch.0.series() {
            for point in series.points() {
                assert_eq!(point.value, 1.0);
                recovered.push((series.name().to_owned(), point.timestamp));
            }
        }
    }
    recovered.sort_unstable();
    let expected: Vec<_> = ["first", "second"]
        .into_iter()
        .flat_map(|name| (123..135).map(move |timestamp| (name.to_owned(), timestamp)))
        .collect();
    assert_eq!(recovered, expected);
}

#[test]
fn core_config_applies_internal_retry_limits() {
    let config = core_config(2, 512);
    assert_eq!(
        config.metrics_retry,
        MetricRetryConfig {
            max_retained_bytes: 8 * 1024 * 1024,
            max_retry_age: Duration::from_secs(120),
            max_send_attempts: 8,
        }
    );
    let mut configuration = configuration(1, 512);
    configuration.retry_max_bytes = 1024;
    configuration.retry_max_age = Duration::ZERO;
    configuration.retry_max_attempts = 2;
    assert_eq!(
        configuration.core_config().metrics_retry,
        MetricRetryConfig {
            max_retained_bytes: 1024,
            max_retry_age: Duration::ZERO,
            max_send_attempts: 2,
        }
    );
}

#[tokio::test]
async fn retry_telemetry_tags_each_return_reason() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);
    let (mut worker, _wire, _) = open_worker().await;
    worker.telemetry.record_retries(MetricRetryStats {
        returned_over_budget: 1,
        returned_over_age: 2,
        returned_over_attempts: 3,
        returned_unencodable: 4,
        returned_on_suspension: 5,
        returned_on_shutdown: 6,
        ..MetricRetryStats::default()
    });
    for (reason, count) in [
        ("budget", 1),
        ("age", 2),
        ("attempts", 3),
        ("encode", 4),
        ("suspension", 5),
        ("shutdown", 6),
    ] {
        let key = metrics::Key::from_parts(
            "stateful_metrics_internal_retry_returned_total",
            vec![metrics::Label::new("reason", reason)],
        );
        assert_eq!(recorder.counter(key), Some(count), "{reason}");
    }
}

#[tokio::test]
async fn retry_telemetry_reports_retention_replays_and_returns() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);
    let mut configuration = configuration(1, 512);
    configuration.retry_max_attempts = 2;
    let mut worker = worker_for(configuration).await;
    let (_wire, stream) = start_all(&mut worker).await.pop().unwrap();
    submit(&mut worker, "requests").await;
    worker.telemetry.record_retries(worker.core.retry_stats());
    let retained = worker.core.retry_stats().retained_bytes;
    assert!(retained > 0);
    assert_eq!(
        recorder.gauge("stateful_metrics_internal_retry_retained_bytes"),
        Some(retained as f64)
    );
    assert_eq!(
        recorder.gauge("stateful_metrics_internal_retry_retained_payloads"),
        Some(1.0)
    );

    worker
        .fail(E0, MetricStreamFailureKind::Unavailable, "disconnect")
        .await
        .unwrap();
    let effects = worker.core.handle_timer(stream, TimerKind::Reconnect);
    worker.apply(effects).await.unwrap();
    let replacement = worker.core.current_stream_id(E0).unwrap();
    let _replay_wire = attach(&mut worker, E0, replacement).await;
    worker
        .fail(E0, MetricStreamFailureKind::Unavailable, "disconnect")
        .await
        .unwrap();
    worker.telemetry.record_retries(worker.core.retry_stats());
    worker.telemetry.record_retries(worker.core.retry_stats());
    assert_eq!(
        recorder.counter("stateful_metrics_internal_retry_replays_total"),
        Some(1)
    );
    let attempts = metrics::Key::from_parts(
        "stateful_metrics_internal_retry_returned_total",
        vec![metrics::Label::new("reason", "attempts")],
    );
    assert_eq!(recorder.counter(attempts), Some(1));
    assert_eq!(
        recorder.gauge("stateful_metrics_internal_retry_retained_payloads"),
        Some(0.0)
    );
    assert_eq!(queued_names(&mut worker).await, ["requests"]);
}

async fn tls_worker(harness: &EndpointHarness, trusted: &SelfSignedCert) -> StatefulMetricsWorker {
    let mut roots = RootCertStore::empty();
    roots.add(trusted.cert_chain().pop().unwrap()).unwrap();
    let mut worker = worker().await;
    worker.endpoints[0].address = Endpoint::from_shared(harness.endpoint.to_string()).unwrap();
    worker.https_connector =
        Some(build_https_connector(ClientTLSConfigBuilder::new().with_root_cert_store(roots)).unwrap());
    let effects = worker.core.start();
    worker.apply(effects).await.unwrap();
    worker
}

#[tokio::test]
async fn https_negotiates_http2_and_reuses_acknowledged_dictionary() {
    initialize_default_crypto_provider().unwrap();
    let cert = SelfSignedCert::new(["127.0.0.1"]);
    let mut harness = EndpointHarness::tls(&cert).await;
    let mut worker = tls_worker(&harness, &cert).await;
    let event = timeout(TEST_TIMEOUT, next_transport_event(&mut worker.transports))
        .await
        .unwrap();
    assert!(matches!(event.kind, TransportEventKind::Opened(_)));
    worker.on_transport(event).await.unwrap();
    let mut session = harness.session().await;
    assert_eq!(session.key, "test-key");
    for expected_definition in [true, false] {
        submit(&mut worker, "encrypted.requests").await;
        let batch = session.receive().await;
        assert_eq!(has_name(&sequence(&batch)), expected_definition);
        session.acknowledge(&batch).await;
        let event = timeout(TEST_TIMEOUT, next_transport_event(&mut worker.transports))
            .await
            .unwrap();
        assert!(matches!(event.kind, TransportEventKind::Ack(_)));
        worker.on_transport(event).await.unwrap();
        assert!(worker.is_drained());
    }
}

#[tokio::test]
async fn https_rejects_untrusted_certificates() {
    initialize_default_crypto_provider().unwrap();
    let cert = SelfSignedCert::new(["127.0.0.1"]);
    load_platform_root_certificates().unwrap();
    let mut harness = EndpointHarness::tls(&cert).await;
    let mut config = configuration(1, 512);
    config.endpoints = vec![harness.endpoint.clone()];
    let mut worker = worker_for(config).await;
    let effects = worker.core.start();
    worker.apply(effects).await.unwrap();
    let event = timeout(TEST_TIMEOUT, next_transport_event(&mut worker.transports))
        .await
        .unwrap();
    let TransportEventKind::Failed(status) = event.kind else {
        panic!("untrusted server was accepted")
    };
    assert_eq!(status.code(), tonic::Code::Unavailable);
    assert!(status.message().contains("UnknownIssuer"), "{status}");
    assert!(harness.sessions.try_recv().is_err());
}

#[tokio::test]
async fn https_rejects_certificate_hostname_mismatches() {
    initialize_default_crypto_provider().unwrap();
    let cert = SelfSignedCert::localhost();
    let mut harness = EndpointHarness::tls(&cert).await;
    let mut worker = tls_worker(&harness, &cert).await;
    let event = timeout(TEST_TIMEOUT, next_transport_event(&mut worker.transports))
        .await
        .unwrap();
    let TransportEventKind::Failed(status) = event.kind else {
        panic!("wrong server identity was accepted")
    };
    assert_eq!(status.code(), tonic::Code::Unavailable);
    assert!(status.message().contains("NotValidForName"), "{status}");
    assert!(harness.sessions.try_recv().is_err());
}

#[tokio::test]
async fn https_and_http_endpoints_receive_the_same_payload() {
    initialize_default_crypto_provider().unwrap();
    let cert = SelfSignedCert::new(["127.0.0.1"]);
    let mut encrypted = EndpointHarness::tls(&cert).await;
    let mut plaintext = EndpointHarness::new().await;
    let mut roots = RootCertStore::empty();
    roots.add(cert.cert_chain().pop().unwrap()).unwrap();
    let mut worker = worker_with(2, 512).await;
    worker.endpoints[0].address = Endpoint::from_shared(encrypted.endpoint.to_string()).unwrap();
    worker.endpoints[1].address = Endpoint::from_shared(plaintext.endpoint.to_string()).unwrap();
    worker.https_connector =
        Some(build_https_connector(ClientTLSConfigBuilder::new().with_root_cert_store(roots)).unwrap());
    let effects = worker.core.start();
    worker.apply(effects).await.unwrap();
    for _ in 0..2 {
        let event = timeout(TEST_TIMEOUT, next_transport_event(&mut worker.transports))
            .await
            .unwrap();
        assert!(matches!(event.kind, TransportEventKind::Opened(_)));
        worker.on_transport(event).await.unwrap();
    }
    let mut encrypted_session = encrypted.session().await;
    let mut plaintext_session = plaintext.session().await;
    submit(&mut worker, "both.endpoints").await;
    let encrypted_batch = encrypted_session.receive().await;
    let plaintext_batch = plaintext_session.receive().await;
    assert_eq!(sequence(&encrypted_batch), sequence(&plaintext_batch));
    encrypted_session.acknowledge(&encrypted_batch).await;
    plaintext_session.acknowledge(&plaintext_batch).await;
    for _ in 0..2 {
        let event = timeout(TEST_TIMEOUT, next_transport_event(&mut worker.transports))
            .await
            .unwrap();
        assert!(matches!(event.kind, TransportEventKind::Ack(_)));
        worker.on_transport(event).await.unwrap();
    }
    assert!(worker.is_drained());
}
