use std::{mem::replace, pin::Pin, sync::Arc};

use agent_data_plane_config::{shared::SharedConfiguration, ConfigValue, SalukiConfiguration};
use arc_swap::ArcSwap;
use foldspace_core::{
    proto::stateful::{
        metric_datum,
        stateful_intake_server::{StatefulIntake, StatefulIntakeServer},
        BatchStatus, MetricDatumSequence, StatefulBatch, StatelessRequest, StatelessResponse,
    },
    LogicalMetricSeries, MetricOrigin as FoldspaceOrigin, MetricPoint, MetricResource, MetricSeriesType, MetricTagSet,
};
use futures::Stream;
use prost::Message as _;
use saluki_context::{tags::TagSet, Context};
use saluki_core::{
    accounting::{ComponentRegistry, MemoryLimiter},
    components::{
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
use tempfile::TempDir;
use tokio::{
    net::TcpListener,
    runtime::Handle,
    sync::{mpsc, watch, Mutex},
    task::JoinHandle,
    time::{advance, timeout},
};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{transport::Server, Request, Response, Status, Streaming};

use super::*;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

fn shared() -> SharedConfiguration {
    let mut shared = SharedConfiguration::default();
    shared.endpoints.forwarder.storage_max_size_in_bytes = 0;
    shared.endpoints.forwarder.high_prio_buffer_size = 32;
    shared.endpoints.forwarder.retry_queue_payloads_max_size = ConfigValue::explicit(1024 * 1024);
    shared.endpoints.forwarder.outdated_file_in_days = 7;
    shared
}

async fn queue() -> PendingTransactions<RetryBatch> {
    build_queue(
        &DeliveryQueueConfiguration::from_configuration(&shared()),
        "http://127.0.0.1:8080",
        0,
        &MetricsBuilder::default(),
    )
    .await
    .unwrap()
}

async fn worker() -> StatefulMetricsWorker {
    StatefulMetricsWorker::new(
        Endpoint::from_static("http://127.0.0.1:8080"),
        parse_api_key("test-key").unwrap(),
        3,
        Duration::from_secs(2),
        512,
        queue().await,
        MetricsBuilder::default(),
    )
}

async fn open_worker() -> (StatefulMetricsWorker, mpsc::Receiver<StatefulBatch>, StreamId) {
    let mut worker = worker().await;
    let effects = worker.core.start().unwrap();
    let MetricClientEffect::OpenStream { stream_id } = effects[0] else {
        panic!("missing open")
    };
    worker.core.handle_stream_opened(stream_id).unwrap();
    let (sender, receiver) = mpsc::channel(MAX_INFLIGHT_BATCHES + 1);
    worker.transport = Some(Transport {
        stream_id,
        sender,
        state: TransportState::Connecting(pending().boxed()),
        pending: VecDeque::new(),
    });
    (worker, receiver, stream_id)
}

async fn buffer(worker: &mut StatefulMetricsWorker, name: &'static str) {
    worker.accept([Event::Metric(Metric::gauge(name, (123, 2.0)))]).await;
    worker.pump().await.unwrap();
}

async fn submit(worker: &mut StatefulMetricsWorker, name: &'static str) {
    buffer(worker, name).await;
    worker.flush().await.unwrap();
}

async fn queued_names(worker: &mut StatefulMetricsWorker) -> Vec<String> {
    let mut names = Vec::new();
    while let Some(attempt) = worker.queue.pop().await {
        let batch = match attempt {
            PendingTransaction::HighPriority(batch) | PendingTransaction::LowPriority(batch) => batch,
        };
        names.extend(batch.0.series().iter().map(|s| s.name().to_owned()));
    }
    names
}

fn ack(stream_id: StreamId, batch_id: u32) -> TransportEvent {
    TransportEvent {
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
fn conversion_preserves_rate_metadata_and_v3_resources() {
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
            MetricResource::new("device", "disk"),
            MetricResource::new("container", "abc"),
        ]
    );
    assert_eq!(series.origin(), Some(FoldspaceOrigin::new(10, 10, 0)));
    assert_eq!(series.unit(), Some("request"));
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
            endpoint,
            parse_api_key("test-key").unwrap(),
            3,
            Duration::from_secs(2),
            512,
            queue().await,
            MetricsBuilder::default(),
        );
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
        let event = timeout(TEST_TIMEOUT, next_transport_event(&mut self.worker.transport))
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
    assert!(harness.worker.is_empty());
    submit(&mut harness.worker, "requests").await;
    let second = harness.receive().await;
    assert_eq!(second.batch_id, 2);
    assert!(!has_name(&sequence(&second)));
    harness.ack(second.batch_id).await;
}

#[tokio::test]
async fn grpc_disconnect_reconnects_with_snapshot_and_reencodes_unacknowledged_batch() {
    let mut harness = Harness::new().await;
    submit(&mut harness.worker, "confirmed").await;
    let first = harness.receive().await;
    harness.ack(first.batch_id).await;
    submit(&mut harness.worker, "speculative").await;
    let second = harness.receive().await;
    assert_eq!(second.batch_id, 2);
    harness
        .replies
        .send(Err(Status::unavailable("disconnect")))
        .await
        .unwrap();
    harness.progress().await;
    assert!(!harness.worker.queue.is_empty());
    let (id, kind) = timeout(TEST_TIMEOUT, harness.worker.timers.next())
        .await
        .unwrap()
        .unwrap();
    let effects = harness.worker.core.handle_timer(id, kind);
    harness.worker.apply(effects).await.unwrap();
    harness.progress().await;
    let snapshot = harness.receive().await;
    assert_eq!(snapshot.batch_id, 0);
    assert!(has_name(&sequence(&snapshot)));
    harness.ack(0).await;
    harness.worker.pump().await.unwrap();
    harness.worker.flush().await.unwrap();
    let replay = harness.receive().await;
    assert_eq!(replay.batch_id, 1);
    assert!(has_name(&sequence(&replay)));
    harness.ack(1).await;
    assert!(harness.worker.is_empty());
}

#[tokio::test]
async fn failure_requeues_inflight_and_partial_behind_fresh_work() {
    let (mut worker, _wire, _) = open_worker().await;
    submit(&mut worker, "inflight").await;
    buffer(&mut worker, "partial").await;
    worker.accept([Event::Metric(Metric::gauge("fresh", (123, 1.0)))]).await;
    worker
        .fail(MetricStreamFailureKind::Unavailable, "disconnect")
        .await
        .unwrap();
    assert_eq!(worker.core.inflight_len(), 0);
    assert_eq!(worker.core.buffered_series_len(), 0);
    assert_eq!(queued_names(&mut worker).await, ["fresh", "inflight", "partial"]);
    assert_eq!(worker.timers.len(), 1);
}

#[tokio::test]
async fn failure_policy_never_switches_to_http() {
    for (kind, suspended, retry) in [
        (MetricStreamFailureKind::Unavailable, false, true),
        (MetricStreamFailureKind::DeadlineExceeded, false, true),
        (MetricStreamFailureKind::ResourceExhausted, false, true),
        (MetricStreamFailureKind::Unauthenticated, true, true),
        (MetricStreamFailureKind::InvalidArgument, true, false),
        (MetricStreamFailureKind::FailedPrecondition, true, true),
    ] {
        let (mut worker, _wire, _) = open_worker().await;
        submit(&mut worker, "sent").await;
        buffer(&mut worker, "partial").await;
        worker.fail(kind, "injected failure").await.unwrap();
        assert_eq!(worker.suspended, suspended);
        assert!(worker.transport.is_none());
        assert_eq!(worker.timers.len(), usize::from(!suspended));
        let names = queued_names(&mut worker).await;
        assert_eq!(
            names,
            if retry {
                vec!["sent", "partial"]
            } else {
                vec!["partial"]
            }
        );
    }
}

#[tokio::test]
async fn invalid_ack_recovers_all_work_and_suspends() {
    let (mut worker, _wire, stream_id) = open_worker().await;
    submit(&mut worker, "sent").await;
    buffer(&mut worker, "partial").await;
    worker.on_transport(ack(stream_id, 99)).await.unwrap();
    assert!(worker.suspended);
    assert!(worker.timers.is_empty());
    assert_eq!(queued_names(&mut worker).await, ["sent", "partial"]);
}

#[tokio::test]
async fn acknowledged_batches_are_not_retried() {
    let (mut worker, _wire, stream_id) = open_worker().await;
    submit(&mut worker, "acked").await;
    worker.on_transport(ack(stream_id, 1)).await.unwrap();
    submit(&mut worker, "unacked").await;
    worker
        .fail(MetricStreamFailureKind::Unavailable, "disconnect")
        .await
        .unwrap();
    assert_eq!(queued_names(&mut worker).await, ["unacked"]);
}

#[tokio::test]
async fn closed_transport_recovers_logical_work() {
    let (mut worker, wire, _) = open_worker().await;
    drop(wire);
    submit(&mut worker, "sent").await;
    assert!(worker.transport.is_none());
    assert_eq!(worker.core.inflight_len(), 0);
    assert_eq!(queued_names(&mut worker).await, ["sent"]);
}

#[tokio::test]
async fn full_transport_preserves_payload_without_failing_stream() {
    let (mut worker, _wire, stream_id) = open_worker().await;
    let (sender, mut receiver) = mpsc::channel(1);
    worker.transport.as_mut().unwrap().sender = sender;
    submit(&mut worker, "first").await;
    submit(&mut worker, "second").await;
    assert_eq!(worker.core.current_stream_id(), Some(stream_id));
    assert_eq!(worker.core.inflight_len(), 2);
    assert!(worker.queue.is_empty());
    assert_eq!(receiver.try_recv().unwrap().batch_id, 1);
    assert_eq!(worker.transport.as_ref().unwrap().pending.front().unwrap().batch_id, 2);
    assert!(worker.timers.is_empty());
}

#[tokio::test]
async fn worker_cores_and_flush_timers_are_independent() {
    let (mut first, mut first_wire, _) = open_worker().await;
    let (mut second, mut second_wire, _) = open_worker().await;
    submit(&mut first, "same").await;
    submit(&mut second, "same").await;
    assert!(has_name(&sequence(&first_wire.try_recv().unwrap())));
    assert!(has_name(&sequence(&second_wire.try_recv().unwrap())));
    first
        .fail(MetricStreamFailureKind::Unauthenticated, "rejected")
        .await
        .unwrap();
    assert!(first.suspended);
    assert!(!second.suspended);
    assert_eq!(second.core.inflight_len(), 1);
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
async fn rotation_and_credentials_return_logical_batches() {
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
        assert_eq!(worker.core.inflight_len(), 0);
        assert_eq!(queued_names(&mut worker).await, ["sent", "partial"]);
        assert_ne!(worker.core.current_stream_id(), Some(id));
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
    let ack_deadline = worker.ack_deadline;
    let buffered_deadline = worker.buffered_deadline;

    for key in ["", " \t\r\n", "bad\nkey", "bad\u{7f}key", " \ttest-key\r\n"] {
        worker.update_credentials(key).await.unwrap();
        assert_eq!(worker.api_key, "test-key");
        assert_eq!(worker.core.current_stream_id(), Some(id));
        assert_eq!(worker.core.inflight_len(), 1);
        assert_eq!(worker.core.buffered_series_len(), 1);
        assert_eq!(worker.ack_deadline, ack_deadline);
        assert_eq!(worker.buffered_deadline, buffered_deadline);
        assert!(!worker.suspended);
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
        .fail(MetricStreamFailureKind::Unauthenticated, "rejected key")
        .await
        .unwrap();
    assert!(worker.suspended);
    worker.update_credentials("bad\nkey").await.unwrap();
    assert!(worker.suspended);
    assert_eq!(worker.api_key, "test-key");
    worker.update_credentials(" \tnew-key\r\n").await.unwrap();
    assert!(!worker.suspended);
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

async fn persisted_queue(settings: &SharedConfiguration) -> PendingTransactions<RetryBatch> {
    build_queue(
        &DeliveryQueueConfiguration::from_configuration(settings),
        "http://127.0.0.1:8080",
        0,
        &MetricsBuilder::default(),
    )
    .await
    .unwrap()
}

fn logical(name: &'static str) -> RetryBatch {
    RetryBatch(LogicalMetricBatch::new(vec![conversion::convert(&Metric::gauge(
        name,
        (123, 2.0),
    ))
    .unwrap()]))
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
    let entry = RetryBatch(expected.clone());
    assert_eq!(entry.event_count(), 1);
    assert_eq!(entry.data_point_count(), 1);
    let settings = persisted_settings(&dir, entry.size_bytes() + logical("new").size_bytes() - 1);
    let mut queue = persisted_queue(&settings).await;
    assert!(!queue.push_low_priority(entry).await.unwrap().had_drops());
    assert!(!queue.push_low_priority(logical("new")).await.unwrap().had_drops());
    // Memory is read before disk. Remove the newer entry, then reopen storage.
    let Some(PendingTransaction::LowPriority(new)) = queue.pop().await else {
        panic!("missing new")
    };
    assert_eq!(new.0.series()[0].name(), "new");
    drop(queue);
    let mut queue = persisted_queue(&settings).await;
    let Some(PendingTransaction::LowPriority(restored)) = queue.pop().await else {
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
    let mut settings = persisted_settings(&dir, logical("old").size_bytes());
    settings.endpoints.forwarder.storage_max_size_in_bytes = 0;
    settings.endpoints.forwarder.high_prio_buffer_size = 1;
    let mut queue = persisted_queue(&settings).await;
    assert!(!queue.push_low_priority(logical("old")).await.unwrap().had_drops());
    assert!(!queue.push_high_priority(logical("new")).await.unwrap().had_drops());
    let evicted = queue.push_high_priority(logical("overflow")).await;
    // An oversized entry is reported explicitly by the queue.
    assert!(evicted.is_err());
    let evicted = queue.push_low_priority(logical("two")).await.unwrap();
    assert_eq!(evicted.items_dropped, 1);
    assert_eq!(evicted.data_points_dropped, 1);
    let Some(PendingTransaction::HighPriority(first)) = queue.pop().await else {
        panic!("missing fresh")
    };
    assert_eq!(first.0.series()[0].name(), "new");
    let Some(PendingTransaction::LowPriority(second)) = queue.pop().await else {
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
    assert!(harness.worker.suspended);
    assert!(harness.worker.transport.is_none());
    assert_eq!(queued_names(&mut harness.worker).await, ["requests"]);
}

async fn start_destination(
    endpoint: MetaString, flush_timeout: Duration, settings: &SharedConfiguration,
) -> (mpsc::Sender<EventsBuffer>, JoinHandle<Result<(), GenericError>>) {
    let configuration = StatefulMetricsConfiguration {
        endpoint,
        workers: NonZeroUsize::new(1).unwrap(),
        api_key: Live::new_fixed("test-key".to_string()),
        compression_level: 3,
        flush_timeout,
        batch_capacity: 512,
        queue: DeliveryQueueConfiguration::from_configuration(settings),
        stop_timeout: TEST_TIMEOUT,
    };
    start_configured_destination(configuration).await
}

async fn start_configured_destination(
    configuration: StatefulMetricsConfiguration,
) -> (mpsc::Sender<EventsBuffer>, JoinHandle<Result<(), GenericError>>) {
    let component = ComponentContext::test_destination("stateful_metrics");
    let destination = configuration
        .build(BuildContext::new(component.clone(), ResourceRegistry::new()))
        .await
        .unwrap();
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
    let context = DestinationContext::new(
        &topology,
        &component,
        ComponentRegistry::default(),
        health,
        Consumer::new(component.clone(), in_rx),
    );
    (in_tx, tokio::spawn(destination.run(context)))
}

#[tokio::test]
async fn destination_flushes_sparse_input_and_waits_for_ack_on_shutdown() {
    let mut harness = Harness::new().await;
    harness.worker.transport = None;
    let (in_tx, task) = start_destination(
        harness.worker.endpoint.uri().to_string().into(),
        Duration::from_millis(20),
        &shared(),
    )
    .await;
    let mut events = EventsBuffer::default();
    assert!(events
        .try_push(Event::Metric(Metric::gauge("sparse", (123, 1.0))))
        .is_none());
    in_tx.send(events).await.unwrap();
    let batch = harness.receive().await;
    assert!(has_name(&sequence(&batch)));
    drop(in_tx);
    assert!(!task.is_finished());
    harness
        .replies
        .send(Ok(BatchStatus {
            batch_id: batch.batch_id,
            status: 1,
        }))
        .await
        .unwrap();
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn router_sends_supported_series_only_to_stateful_destination() {
    let component = ComponentContext::test_transform("stateful_metrics_route");
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
    let _request_sender = replace(&mut harness.worker.transport.as_mut().unwrap().sender, sender);
    submit(&mut harness.worker, "first").await;
    submit(&mut harness.worker, "second").await;
    assert_eq!(receiver.try_recv().unwrap().batch_id, 1);
    // Cancel the event wait after it moves the pending payload into the available slot.
    assert!(timeout(
        Duration::from_millis(20),
        next_transport_event(&mut harness.worker.transport)
    )
    .await
    .is_err());
    assert_eq!(receiver.try_recv().unwrap().batch_id, 2);
    assert!(harness.worker.transport.as_ref().unwrap().pending.is_empty());
    assert_eq!(harness.worker.core.inflight_len(), 2);
}

#[tokio::test]
async fn threshold_flush_and_rejected_partial_remain_recoverable() {
    let mut worker = StatefulMetricsWorker::new(
        Endpoint::from_static("http://127.0.0.1:8080"),
        parse_api_key("test-key").unwrap(),
        3,
        Duration::from_secs(2),
        2,
        queue().await,
        MetricsBuilder::default(),
    );
    let effects = worker.core.start().unwrap();
    let MetricClientEffect::OpenStream { stream_id } = effects[0] else {
        panic!("missing stream")
    };
    worker.core.handle_stream_opened(stream_id).unwrap();
    let (sender, mut wire) = mpsc::channel(9);
    worker.transport = Some(Transport {
        stream_id,
        sender,
        state: TransportState::Connecting(pending().boxed()),
        pending: VecDeque::new(),
    });
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
    let settings = persisted_settings(&dir, logical("tiny").size_bytes());
    let (mut worker, _wire, _) = open_worker().await;
    worker.queue = persisted_queue(&settings).await;
    submit(&mut worker, "too-large-to-retry").await;
    submit(&mut worker, "tiny").await;
    worker
        .fail(MetricStreamFailureKind::Unavailable, "disconnect")
        .await
        .unwrap();
    assert_eq!(queued_names(&mut worker).await, ["tiny"]);
    assert_eq!(worker.timers.len(), 1);
}

#[tokio::test]
async fn destination_shutdown_timeout_persists_missing_ack() {
    let dir = TempDir::new().unwrap();
    let settings = persisted_settings(&dir, 1024 * 1024);
    let mut harness = Harness::new().await;
    harness.worker.transport = None;
    let endpoint: MetaString = harness.worker.endpoint.uri().to_string().into();
    let (in_tx, task) = start_destination(endpoint.clone(), Duration::from_millis(10), &settings).await;
    let mut events = EventsBuffer::default();
    assert!(events
        .try_push(Event::Metric(Metric::gauge("unacknowledged", (123, 1.0))))
        .is_none());
    in_tx.send(events).await.unwrap();
    harness.receive().await;
    drop(in_tx);
    // The intake deliberately never acknowledges this payload.
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap().unwrap();
    let mut queue = build_queue(
        &DeliveryQueueConfiguration::from_configuration(&settings),
        &endpoint,
        0,
        &MetricsBuilder::default(),
    )
    .await
    .unwrap();
    let Some(PendingTransaction::LowPriority(batch)) = queue.pop().await else {
        panic!("shutdown lost unacknowledged data")
    };
    assert_eq!(batch.0.series()[0].name(), "unacknowledged");
    assert!(queue.is_empty());
}

impl StatefulMetricsWorker {
    async fn accept(&mut self, events: impl IntoIterator<Item = Event>) {
        self.enqueue(sharding::partition(events, 1).pop().unwrap()).await;
    }
}

#[test]
fn sharding_routes_series_independently_of_points_and_tag_order() {
    let first = Metric::from_parts(
        Context::from_parts(
            "requests",
            ["z:1", "a:2"].into_iter().map(Into::into).collect::<TagSet>(),
        ),
        MetricValues::gauge([(123, 2.0)]),
        MetricMetadata::default(),
    );
    let second = Metric::from_parts(
        Context::from_parts(
            "requests",
            ["a:2", "z:1", "a:2"].into_iter().map(Into::into).collect::<TagSet>(),
        ),
        MetricValues::gauge([(456, 3.0)]),
        MetricMetadata::default(),
    );
    let first_hash = sharding::series_hash(&conversion::convert(&first).unwrap());
    assert_eq!(
        first_hash,
        sharding::series_hash(&conversion::convert(&second).unwrap())
    );
    for count in [1, 2, 3, 4, 8, 17] {
        let shards = sharding::partition([Event::Metric(first.clone()), Event::Metric(second.clone())], count);
        let shard = &shards[(first_hash % count as u64) as usize];
        assert_eq!(shard.series().len(), 2);
        assert_eq!(shard.series()[0].points()[0].timestamp, 123);
        assert_eq!(shard.series()[1].points()[0].timestamp, 456);
        assert_eq!(shards.iter().map(LogicalMetricBatch::point_count).sum::<usize>(), 2);
    }
}

#[test]
fn sharding_identity_includes_metadata_and_canonical_resources() {
    let base = LogicalMetricSeries::new("requests", MetricSeriesType::Gauge, vec![MetricPoint::new(1, 2.0)]);
    let hash = sharding::series_hash(&base);
    for series in [
        LogicalMetricSeries::new("other", MetricSeriesType::Gauge, vec![MetricPoint::new(1, 2.0)]),
        LogicalMetricSeries::new("requests", MetricSeriesType::Count, vec![MetricPoint::new(1, 2.0)]),
        base.clone().with_interval(10),
        base.clone().with_unit("request"),
        base.clone().with_source_type_name(Some("integration".to_owned())),
        base.clone().with_origin(FoldspaceOrigin::new(1, 2, 3)),
        base.clone().with_no_index(true),
        base.clone()
            .with_tags(MetricTagSet::standalone(vec!["env:test".to_owned()])),
        base.clone().with_resources(vec![MetricResource::new("host", "a")]),
    ] {
        assert_ne!(hash, sharding::series_hash(&series));
    }
    let one = base.clone().with_resources(vec![
        MetricResource::new("host", "a"),
        MetricResource::new("device", "b"),
    ]);
    let two = base.with_resources(vec![
        MetricResource::new("device", "b"),
        MetricResource::new("host", "a"),
    ]);
    assert_eq!(sharding::series_hash(&one), sharding::series_hash(&two));
}

#[tokio::test]
async fn sharding_storage_rejects_count_changes_without_consuming_retries() {
    let dir = TempDir::new().unwrap();
    let settings = persisted_settings(&dir, 1024 * 1024);
    let config = DeliveryQueueConfiguration::from_configuration(&settings);
    let endpoint = "http://127.0.0.1:8080";
    let one = NonZeroUsize::new(1).unwrap();
    let three = SalukiConfiguration::default().domains.stateful_metrics.workers;
    assert_eq!(three.get(), 3);
    // A legacy queue has no count manifest and belongs to worker zero.
    let mut legacy = build_queue(&config, endpoint, 0, &MetricsBuilder::default())
        .await
        .unwrap();
    assert!(!legacy.push_low_priority(logical("legacy")).await.unwrap().had_drops());
    assert!(!legacy.flush().await.unwrap().had_drops());
    assert!(prepare_storage(&config, endpoint, three).await.is_err());
    prepare_storage(&config, endpoint, one).await.unwrap();
    let mut legacy = build_queue(&config, endpoint, 0, &MetricsBuilder::default())
        .await
        .unwrap();
    assert!(legacy.pop().await.is_some());
    assert!(legacy.is_empty());
    drop(legacy);
    prepare_storage(&config, endpoint, three).await.unwrap();
    for id in 0..3 {
        let (mut worker, _wire, _) = open_worker().await;
        worker.queue = build_queue(&config, endpoint, id, &MetricsBuilder::default())
            .await
            .unwrap();
        submit(&mut worker, "inflight").await;
        buffer(&mut worker, "partial").await;
        worker
            .accept([Event::Metric(Metric::gauge("queued", (123, 1.0)))])
            .await;
        worker.shutdown().await.unwrap();
    }
    for count in [1, 2, 4] {
        let error = prepare_storage(&config, endpoint, NonZeroUsize::new(count).unwrap())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Restart with 3 workers"));
    }
    prepare_storage(&config, endpoint, three).await.unwrap();
    for id in 0..3 {
        let mut worker = worker().await;
        worker.queue = build_queue(&config, endpoint, id, &MetricsBuilder::default())
            .await
            .unwrap();
        let mut names = queued_names(&mut worker).await;
        names.sort();
        assert_eq!(names, ["inflight", "partial", "queued"]);
    }
    prepare_storage(&config, endpoint, one).await.unwrap();
    // Other destinations have independent layouts.
    prepare_storage(&config, "http://127.0.0.1:8081", three).await.unwrap();
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
struct ShardedIntake {
    sessions: mpsc::Sender<TestSession>,
}

#[tonic::async_trait]
impl StatefulIntake for ShardedIntake {
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

struct ShardedHarness {
    endpoint: MetaString,
    sessions: mpsc::Receiver<TestSession>,
    server: JoinHandle<()>,
}

impl ShardedHarness {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap()).into();
        let (sessions, sessions_rx) = mpsc::channel(32);
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(StatefulIntakeServer::new(ShardedIntake { sessions }))
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

    async fn session(&mut self) -> TestSession {
        timeout(TEST_TIMEOUT, self.sessions.recv()).await.unwrap().unwrap()
    }

    async fn start(
        &self, settings: &SharedConfiguration, key: Live<String>,
    ) -> (mpsc::Sender<EventsBuffer>, JoinHandle<Result<(), GenericError>>) {
        self.start_with_workers(settings, key, 2).await
    }

    async fn start_with_workers(
        &self, settings: &SharedConfiguration, key: Live<String>, workers: usize,
    ) -> (mpsc::Sender<EventsBuffer>, JoinHandle<Result<(), GenericError>>) {
        start_configured_destination(StatefulMetricsConfiguration {
            endpoint: self.endpoint.clone(),
            workers: NonZeroUsize::new(workers).unwrap(),
            api_key: key,
            compression_level: 3,
            flush_timeout: Duration::from_millis(10),
            batch_capacity: 512,
            queue: DeliveryQueueConfiguration::from_configuration(settings),
            stop_timeout: Duration::from_millis(200),
        })
        .await
    }
}

impl Drop for ShardedHarness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn send_both_shards(input: &mpsc::Sender<EventsBuffer>, timestamp: u64) {
    send_all_shards(input, 2, timestamp).await;
}

async fn send_all_shards(input: &mpsc::Sender<EventsBuffer>, workers: usize, timestamp: u64) {
    let mut events = EventsBuffer::default();
    for id in 0..workers {
        let metric = (0..1000)
            .map(|n| {
                Metric::gauge(
                    Context::from_parts(format!("sharded.{n}"), TagSet::default()),
                    (timestamp, 1.0),
                )
            })
            .find(|m| sharding::series_hash(&conversion::convert(m).unwrap()) % workers as u64 == id as u64)
            .unwrap();
        assert!(events.try_push(Event::Metric(metric)).is_none());
    }
    input.send(events).await.unwrap();
}

#[tokio::test]
async fn sharding_tasks_isolate_stream_failure_and_refresh_all_credentials() {
    let mut harness = ShardedHarness::new().await;
    let mut config = SalukiConfiguration::default();
    config.shared.endpoints.api_key = "test-key".to_owned();
    let cell = Arc::new(ArcSwap::from_pointee(config));
    let (tick, rx) = watch::channel(());
    let key = Live::new_dynamic(cell.clone(), rx, |c| &c.shared.endpoints.api_key);
    let (input, task) = harness.start(&shared(), key).await;
    let mut failed = harness.session().await;
    let mut healthy = harness.session().await;
    assert_eq!(failed.key, "test-key");
    assert_eq!(healthy.key, "test-key");
    send_both_shards(&input, 123).await;
    let bad = failed.receive().await;
    let good = healthy.receive().await;
    assert_eq!(bad.batch_id, 1);
    assert_eq!(good.batch_id, 1);
    assert!(has_name(&sequence(&bad)) && has_name(&sequence(&good)));
    failed
        .replies
        .send(Err(Status::unauthenticated("rotate key")))
        .await
        .unwrap();
    healthy.acknowledge(&good).await;
    send_both_shards(&input, 124).await;
    let next = healthy.receive().await;
    assert_eq!(next.batch_id, 2);
    assert!(!has_name(&sequence(&next)));
    healthy.acknowledge(&next).await;

    let mut updated = (**cell.load()).clone();
    updated.shared.endpoints.api_key = " ".to_owned();
    cell.store(Arc::new(updated));
    tick.send(()).unwrap();
    send_both_shards(&input, 125).await;
    let next = healthy.receive().await;
    assert_eq!(next.batch_id, 3);
    healthy.acknowledge(&next).await;

    let mut updated = (**cell.load()).clone();
    updated.shared.endpoints.api_key = "new-key".to_owned();
    cell.store(Arc::new(updated));
    tick.send(()).unwrap();
    let mut first = harness.session().await;
    let mut second = harness.session().await;
    assert_eq!(first.key, "new-key");
    assert_eq!(second.key, "new-key");
    send_both_shards(&input, 126).await;
    let one = first.receive().await;
    let two = second.receive().await;
    assert_eq!(one.batch_id, 1);
    assert_eq!(two.batch_id, 1);
    assert!(has_name(&sequence(&one)) && has_name(&sequence(&two)));
    first.acknowledge(&one).await;
    second.acknowledge(&two).await;
    drop(input);
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn sharding_destination_shutdown_and_restart_preserve_each_streams_retries() {
    for workers in [1, 2, 3, 4, 8] {
        let mut harness = ShardedHarness::new().await;
        let dir = TempDir::new().unwrap();
        let settings = persisted_settings(&dir, 1024 * 1024);
        let (input, task) = harness
            .start_with_workers(&settings, Live::new_fixed("test-key".to_owned()), workers)
            .await;
        let mut sessions = Vec::new();
        for _ in 0..workers {
            sessions.push(harness.session().await);
        }
        send_all_shards(&input, workers, 123).await;
        let mut original = Vec::new();
        for session in &mut sessions {
            original.push(sequence(&session.receive().await));
        }
        drop(input);
        // No stream acknowledges; every worker must exhaust its delivery budget and persist.
        timeout(Duration::from_secs(1), task).await.unwrap().unwrap().unwrap();
        let (input, task) = harness
            .start_with_workers(&settings, Live::new_fixed("test-key".to_owned()), workers)
            .await;
        for _ in 0..workers {
            let mut session = harness.session().await;
            let batch = session.receive().await;
            let replay = sequence(&batch);
            let matched = original
                .iter()
                .position(|expected| *expected == replay)
                .expect("restart must replay each worker's original logical data exactly once");
            original.swap_remove(matched);
            session.acknowledge(&batch).await;
            // Keep the reply channel alive until the destination closes the stream.
            sessions.push(session);
        }
        assert!(original.is_empty());
        drop(input);
        timeout(TEST_TIMEOUT, task).await.unwrap().unwrap().unwrap();
        prepare_storage(
            &DeliveryQueueConfiguration::from_configuration(&settings),
            &harness.endpoint,
            NonZeroUsize::new(1).unwrap(),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn sharding_stalled_acknowledgements_do_not_fill_healthy_workers_window() {
    let mut harness = ShardedHarness::new().await;
    let (input, task) = harness.start(&shared(), Live::new_fixed("test-key".to_owned())).await;
    let mut stalled = harness.session().await;
    let mut healthy = harness.session().await;
    send_both_shards(&input, 123).await;
    let _unacknowledged = stalled.receive().await;
    let first = healthy.receive().await;
    healthy.acknowledge(&first).await;
    // Fill the stalled core's entire inflight window and keep accepting input on both shards.
    for offset in 1..=MAX_INFLIGHT_BATCHES + WORKER_INPUT_CAPACITY + 1 {
        send_both_shards(&input, 123 + offset as u64).await;
        let batch = healthy.receive().await;
        assert_eq!(batch.batch_id, 1 + offset as u32);
        healthy.acknowledge(&batch).await;
    }
    drop(input);
    timeout(Duration::from_secs(1), task).await.unwrap().unwrap().unwrap();
}
