use std::{pin::Pin, sync::Arc};

use foldspace_core::{
    proto::stateful::{
        metric_datum,
        stateful_intake_server::{StatefulIntake, StatefulIntakeServer},
        MetricDatumSequence, StatelessRequest, StatelessResponse,
    },
    MetricOrigin as FoldspaceOrigin, MetricPoint, MetricResource, MetricSeriesType,
};
use futures::Stream;
use prost::Message as _;
use saluki_context::{tags::TagSet, Context};
use saluki_core::data_model::event::metric::{MetricMetadata, MetricOrigin, MetricValues};
use saluki_core::{
    accounting::{ComponentRegistry, MemoryLimiter},
    components::ComponentContext,
    health::HealthRegistry,
    runtime::state::{DataspaceRegistry, ResourceRegistry},
    support::SubsystemIdentifier,
    topology::{
        interconnect::{Consumer, Dispatcher},
        EventsBuffer, OutputName, TopologyContext,
    },
};
use tokio::{net::TcpListener, runtime::Handle, sync::Mutex, task::JoinHandle, time::advance};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{transport::Server, Response};

use super::*;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

fn worker() -> StatefulMetricsWorker {
    StatefulMetricsWorker::new(
        Endpoint::from_static("http://127.0.0.1:8080"),
        parse_api_key("test-key").unwrap(),
        3,
        Duration::from_secs(2),
        512,
    )
}

fn open_worker() -> (StatefulMetricsWorker, mpsc::Receiver<StatefulBatch>, StreamId) {
    let mut worker = worker();
    let effects = worker.core.start().unwrap();
    let MetricClientEffect::OpenStream { stream_id } = effects[0] else {
        panic!("missing open")
    };
    worker.core.handle_stream_opened(stream_id).unwrap();
    let (sender, receiver) = mpsc::channel(MAX_INFLIGHT_BATCHES + 1);
    worker.transport = Some(Transport {
        sender,
        state: TransportState::Connecting(pending().boxed()),
    });
    (worker, receiver, stream_id)
}

fn buffer(worker: &mut StatefulMetricsWorker, name: &'static str) {
    assert!(worker
        .accept([Event::Metric(Metric::gauge(name, (123, 2.0)))])
        .is_empty());
    worker.pump().unwrap();
}

fn submit(worker: &mut StatefulMetricsWorker, name: &'static str) {
    buffer(worker, name);
    worker.flush().unwrap();
}

fn pending_names(worker: &StatefulMetricsWorker) -> Vec<&str> {
    worker
        .pending
        .iter()
        .flat_map(|batch| batch.logical.series().iter().map(|series| series.name()))
        .collect()
}

fn original_names(worker: &StatefulMetricsWorker) -> Vec<&str> {
    worker
        .pending
        .iter()
        .flat_map(|batch| {
            batch
                .originals
                .metrics
                .iter()
                .map(|metric| metric.context().name().as_ref())
        })
        .collect()
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

#[test]
fn only_supported_series_leave_the_http_path() {
    let mut worker = worker();
    let passthrough = worker.accept([
        Event::Metric(Metric::gauge("gauge", 1.0)),
        Event::Metric(Metric::counter("counter", 1.0)),
        Event::Metric(Metric::histogram("histogram", 1.0)),
        Event::Metric(Metric::distribution("distribution", 1.0)),
        Event::Metric(Metric::set("set", "value")),
        Event::Metric(Metric::gauge("invalid", f64::INFINITY)),
    ]);
    assert_eq!(worker.pending.front().unwrap().logical.series().len(), 2);
    let names: Vec<_> = passthrough
        .iter()
        .map(|event| match event {
            Event::Metric(metric) => metric.context().name().as_ref(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(names, ["histogram", "distribution", "set"]);
}

#[tokio::test]
async fn retry_returns_logical_batches_before_unsent_work() {
    let (mut worker, mut wire, _) = open_worker();
    submit(&mut worker, "first");
    submit(&mut worker, "second");
    worker.accept([Event::Metric(Metric::gauge("third", 3.0))]);
    assert_eq!(wire.recv().await.unwrap().batch_id, 1);
    worker
        .fail(MetricStreamFailureKind::Unavailable, "test disconnect")
        .unwrap();
    let names: Vec<_> = worker
        .pending
        .iter()
        .map(|batch| batch.logical.series()[0].name())
        .collect();
    assert_eq!(names, ["first", "second", "third"]);
    assert!(worker.inflight.is_empty());
    assert!(worker.transport.is_none());
    assert_eq!(worker.timers.len(), 1);
}

#[tokio::test]
async fn failure_policy_controls_retry_and_http_fallback() {
    for (kind, expected_mode) in [
        (MetricStreamFailureKind::Unavailable, DeliveryMode::Stateful),
        (MetricStreamFailureKind::DeadlineExceeded, DeliveryMode::Stateful),
        (MetricStreamFailureKind::ResourceExhausted, DeliveryMode::Stateful),
        (MetricStreamFailureKind::Unauthenticated, DeliveryMode::Suspended),
        (MetricStreamFailureKind::InvalidArgument, DeliveryMode::Suspended),
        (MetricStreamFailureKind::FailedPrecondition, DeliveryMode::Http),
    ] {
        let (mut worker, _wire, _) = open_worker();
        submit(&mut worker, "first");
        worker.fail(kind, "injected failure").unwrap();
        assert!(worker.mode == expected_mode);
        assert!(worker.transport.is_none());
        if kind == MetricStreamFailureKind::FailedPrecondition {
            assert_eq!(worker.fallback.len(), 1);
            assert_eq!(worker.fallback[0].context().name(), "first");
            assert_eq!(worker.accept([Event::Metric(Metric::gauge("next", 1.0))]).len(), 1);
        } else if kind == MetricStreamFailureKind::InvalidArgument {
            assert_eq!(worker.buffered_batches(), 0);
        } else {
            assert_eq!(worker.pending.len(), 1);
        }
        assert_eq!(
            worker.timers.len(),
            usize::from(expected_mode == DeliveryMode::Stateful)
        );
    }
}

#[tokio::test]
async fn invalid_ack_retains_speculative_work_and_stops_reconnecting() {
    let (mut worker, _wire, _) = open_worker();
    submit(&mut worker, "first");
    worker
        .on_transport(TransportEvent::Ack(BatchStatus {
            batch_id: 99,
            status: 1,
        }))
        .unwrap();
    assert!(worker.mode == DeliveryMode::Suspended);
    assert_eq!(worker.pending.len(), 1);
    assert!(worker.timers.is_empty());
}

#[tokio::test]
async fn each_worker_owns_its_dictionary_stream_and_failure_state() {
    let (mut first, mut first_wire, _) = open_worker();
    let (mut second, mut second_wire, _) = open_worker();
    submit(&mut first, "shared.name");
    submit(&mut second, "shared.name");
    assert!(has_name(&sequence(&first_wire.recv().await.unwrap())));
    assert!(has_name(&sequence(&second_wire.recv().await.unwrap())));
    second
        .on_transport(TransportEvent::Ack(BatchStatus { batch_id: 1, status: 1 }))
        .unwrap();
    first
        .fail(MetricStreamFailureKind::Unauthenticated, "first worker only")
        .unwrap();
    assert!(first.mode == DeliveryMode::Suspended);
    assert!(second.core.has_send_capacity());
    submit(&mut second, "shared.name");
    let batch = second_wire.recv().await.unwrap();
    assert_eq!(batch.batch_id, 2);
    assert!(!has_name(&sequence(&batch)));
    assert_eq!(first.pending.len(), 1);
    assert_eq!(second.core.inflight_len(), 1);
}

#[tokio::test]
async fn inflight_limit_keeps_work_logical_until_capacity_returns() {
    let (mut worker, _wire, _) = open_worker();
    for _ in 0..MAX_INFLIGHT_BATCHES + 1 {
        submit(&mut worker, "metric");
    }
    assert_eq!(worker.core.inflight_len(), MAX_INFLIGHT_BATCHES);
    assert_eq!(worker.pending.len(), 1);
    assert_eq!(worker.core.encoding_count(), MAX_INFLIGHT_BATCHES as u64);
    worker
        .on_transport(TransportEvent::Ack(BatchStatus { batch_id: 1, status: 1 }))
        .unwrap();
    worker.pump().unwrap();
    worker.flush().unwrap();
    assert!(worker.pending.is_empty());
    assert_eq!(worker.core.inflight_len(), MAX_INFLIGHT_BATCHES);
}

#[tokio::test]
async fn credential_change_restarts_a_suspended_worker_without_losing_queued_batches() {
    let (mut worker, _wire, _) = open_worker();
    submit(&mut worker, "confirmed");
    worker
        .on_transport(TransportEvent::Ack(BatchStatus { batch_id: 1, status: 1 }))
        .unwrap();
    submit(&mut worker, "metric");
    worker
        .fail(MetricStreamFailureKind::Unauthenticated, "expired key")
        .unwrap();
    worker.update_credentials("replacement-key").unwrap();
    assert!(worker.mode == DeliveryMode::Stateful);
    assert_eq!(worker.pending.len(), 1);
    assert!(worker.transport.is_some());
    assert_eq!(worker.api_key, "replacement-key");
    let id = worker.core.current_stream_id().unwrap();
    // An identity change must not send a snapshot from the previous API key.
    assert!(worker.core.handle_stream_opened(id).unwrap().is_empty());
}

#[tokio::test]
async fn rotation_drain_timeout_returns_all_unacknowledged_batches() {
    let (mut worker, _wire, id) = open_worker();
    submit(&mut worker, "first");
    submit(&mut worker, "second");
    let effects = worker.core.handle_timer(id, TimerKind::RotateStream);
    worker.apply(effects).unwrap();
    assert!(!worker.core.has_send_capacity());
    assert_eq!(worker.core.inflight_len(), 2);
    let effects = worker.core.handle_timer(id, TimerKind::DrainExpired);
    worker.apply(effects).unwrap();
    assert_eq!(worker.pending.len(), 2);
    assert_eq!(worker.pending[0].logical.series()[0].name(), "first");
    assert_eq!(worker.pending[1].logical.series()[0].name(), "second");
    assert_eq!(worker.core.inflight_len(), 0);
    assert_ne!(worker.core.current_stream_id(), Some(id));
}

#[tokio::test]
async fn failed_send_returns_ownership_to_the_worker() {
    let (mut worker, wire, _) = open_worker();
    drop(wire);
    submit(&mut worker, "metric");
    assert_eq!(worker.core.inflight_len(), 0);
    assert_eq!(worker.inflight.len(), 0);
    assert_eq!(worker.pending.len(), 1);
    assert_eq!(worker.pending[0].originals.metrics[0].context().name(), "metric");
    assert_eq!(worker.timers.len(), 1);
}

#[test]
fn grpc_statuses_map_to_foldspace_failure_categories() {
    for (status, expected) in [
        (Code::Unavailable, MetricStreamFailureKind::Unavailable),
        (Code::DeadlineExceeded, MetricStreamFailureKind::DeadlineExceeded),
        (Code::ResourceExhausted, MetricStreamFailureKind::ResourceExhausted),
        (Code::InvalidArgument, MetricStreamFailureKind::InvalidArgument),
        (Code::Unauthenticated, MetricStreamFailureKind::Unauthenticated),
        (Code::FailedPrecondition, MetricStreamFailureKind::FailedPrecondition),
        (Code::Unimplemented, MetricStreamFailureKind::FailedPrecondition),
        (Code::PermissionDenied, MetricStreamFailureKind::Unauthenticated),
    ] {
        assert_eq!(classify(status), expected);
    }
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
        assert_eq!(
            request.metadata().get("dd-state-request-bytes").unwrap(),
            REQUESTED_STATE_BYTES
        );
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
        Err(Status::unimplemented(
            "HTTP fallback is handled by the existing encoder",
        ))
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
        );
        let effects = worker.core.start();
        worker.apply(effects).unwrap();
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
        self.worker.on_transport(event).unwrap();
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
    submit(&mut harness.worker, "requests");
    let first = harness.receive().await;
    assert_eq!(first.batch_id, 1);
    assert!(has_name(&sequence(&first)));
    harness.ack(first.batch_id).await;
    assert_eq!(harness.worker.buffered_batches(), 0);
    submit(&mut harness.worker, "requests");
    let second = harness.receive().await;
    assert_eq!(second.batch_id, 2);
    assert!(!has_name(&sequence(&second)));
    harness.ack(second.batch_id).await;
}

#[tokio::test]
async fn grpc_disconnect_reconnects_with_snapshot_and_reencodes_unacknowledged_batch() {
    let mut harness = Harness::new().await;
    submit(&mut harness.worker, "confirmed");
    let first = harness.receive().await;
    harness.ack(first.batch_id).await;
    submit(&mut harness.worker, "speculative");
    let second = harness.receive().await;
    assert_eq!(second.batch_id, 2);
    harness
        .replies
        .send(Err(Status::unavailable("disconnect")))
        .await
        .unwrap();
    harness.progress().await;
    assert_eq!(harness.worker.pending.len(), 1);
    let (id, kind) = timeout(TEST_TIMEOUT, harness.worker.timers.next())
        .await
        .unwrap()
        .unwrap();
    let effects = harness.worker.core.handle_timer(id, kind);
    harness.worker.apply(effects).unwrap();
    harness.progress().await;
    let snapshot = harness.receive().await;
    assert_eq!(snapshot.batch_id, 0);
    assert!(has_name(&sequence(&snapshot)));
    harness.ack(0).await;
    harness.worker.pump().unwrap();
    harness.worker.flush().unwrap();
    let replay = harness.receive().await;
    assert_eq!(replay.batch_id, 1);
    assert!(has_name(&sequence(&replay)));
    harness.ack(1).await;
    assert_eq!(harness.worker.buffered_batches(), 0);
}

#[tokio::test]
async fn grpc_capability_failure_returns_original_metrics_for_http_encoding() {
    let mut harness = Harness::new().await;
    submit(&mut harness.worker, "requests");
    harness.receive().await;
    harness
        .replies
        .send(Err(Status::failed_precondition("stateful disabled")))
        .await
        .unwrap();
    harness.progress().await;
    assert!(harness.worker.mode == DeliveryMode::Http);
    assert_eq!(harness.worker.fallback[0].context().name(), "requests");
    assert!(harness.worker.timers.is_empty());
}

async fn start_component(
    endpoint: MetaString, flush_timeout: Duration,
) -> (
    mpsc::Sender<EventsBuffer>,
    mpsc::Receiver<EventsBuffer>,
    JoinHandle<Result<(), GenericError>>,
) {
    let configuration = StatefulMetricsConfiguration {
        endpoint,
        api_key: Live::new_fixed("test-key".to_string()),
        compression_level: 3,
        flush_timeout,
        batch_capacity: 512,
    };
    let component = ComponentContext::test_transform("stateful_metrics");
    let transform = configuration
        .build(BuildContext::new(component.clone(), ResourceRegistry::new()))
        .await
        .unwrap();
    let mut dispatcher = Dispatcher::new(component.clone());
    let (out_tx, out_rx) = mpsc::channel(4);
    dispatcher.add_output(OutputName::Default).unwrap();
    dispatcher
        .attach_sender_to_output(&OutputName::Default, out_tx)
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
    let context = TransformContext::new(
        &topology,
        &component,
        ComponentRegistry::default(),
        health,
        dispatcher,
        Consumer::new(component.clone(), in_rx),
    );
    let task = tokio::spawn(transform.run(context));
    (in_tx, out_rx, task)
}

#[tokio::test]
async fn component_run_routes_sketches_and_waits_for_series_ack_before_shutdown() {
    let mut harness = Harness::new().await;
    harness.worker.transport = None;
    let (in_tx, mut out_rx, task) = start_component(
        harness.worker.endpoint.uri().to_string().into(),
        Duration::from_secs(60),
    )
    .await;
    let mut events = EventsBuffer::default();
    assert!(events
        .try_push(Event::Metric(Metric::gauge("series", (123, 1.0))))
        .is_none());
    assert!(events
        .try_push(Event::Metric(Metric::distribution("sketch", 2.0)))
        .is_none());
    in_tx.send(events).await.unwrap();
    drop(in_tx);
    let sketches = timeout(TEST_TIMEOUT, out_rx.recv()).await.unwrap().unwrap();
    assert_eq!(sketches.len(), 1);
    assert_eq!(
        sketches
            .into_iter()
            .next()
            .unwrap()
            .try_into_metric()
            .unwrap()
            .context()
            .name(),
        "sketch"
    );
    let batch = harness.receive().await;
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

#[tokio::test(start_paused = true)]
async fn first_pending_metric_sets_deadline_and_flush_coalesces_originals() {
    let (mut worker, mut wire, _) = open_worker();
    buffer(&mut worker, "first");
    let deadline = worker.flush_deadline().unwrap();
    assert!(wire.try_recv().is_err());
    assert!(worker.ack_deadline.is_none());
    advance(Duration::from_secs(1)).await;
    buffer(&mut worker, "second");
    assert_eq!(worker.flush_deadline(), Some(deadline));
    assert_eq!(worker.buffered_batches(), 2);
    assert_eq!(worker.core.inflight_len(), 0);
    wait_deadline(worker.flush_deadline()).await;
    worker.flush().unwrap();
    let batch = wire.try_recv().unwrap();
    let names: Vec<_> = sequence(&batch)
        .data
        .into_iter()
        .filter_map(|datum| match datum.data {
            Some(metric_datum::Data::MetricNameDefine(definition)) => Some(definition.value),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["first", "second"]);
    assert_eq!(
        worker.inflight[0]
            .metrics
            .iter()
            .map(|metric| metric.context().name().as_ref())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(worker.core.inflight_len(), 1);
    assert_eq!(worker.inflight.front().unwrap().input_batches, 2);
    assert_eq!(worker.buffered_batches(), 2);
    assert!(worker.flush_deadline().is_none());
    worker.flush().unwrap();
    assert!(wire.try_recv().is_err());
    worker
        .on_transport(TransportEvent::Ack(BatchStatus {
            batch_id: batch.batch_id,
            status: 1,
        }))
        .unwrap();
    assert_eq!(worker.buffered_batches(), 0);
}

#[tokio::test]
async fn threshold_flush_tracks_one_payload_and_preserves_input_buffer_bound() {
    let (mut worker, mut wire, _) = open_worker();
    worker.core = StatefulMetricsClient::new(
        CoreConfig {
            batch_capacity: 2,
            ..CoreConfig::default()
        },
        ZstdBatchCompressor::new(3),
    );
    let effects = worker.core.start().unwrap();
    let MetricClientEffect::OpenStream { stream_id } = effects[0] else {
        unreachable!()
    };
    worker.core.handle_stream_opened(stream_id).unwrap();
    buffer(&mut worker, "first");
    buffer(&mut worker, "second");
    assert_eq!(wire.try_recv().unwrap().batch_id, 1);
    assert_eq!(worker.inflight.len(), 1);
    assert_eq!(worker.inflight[0].metrics.len(), 2);
    assert!(worker.buffered.is_none());
    assert!(worker.flush_deadline().is_none());

    let (mut worker, _wire, _) = open_worker();
    for _ in 0..MAX_BUFFERED_BATCHES {
        buffer(&mut worker, "metric");
    }
    assert_eq!(worker.buffered_batches(), MAX_BUFFERED_BATCHES);
    worker.flush().unwrap();
    assert_eq!(worker.core.inflight_len(), 1);
    assert_eq!(worker.buffered_batches(), MAX_BUFFERED_BATCHES);
}

#[tokio::test]
async fn recovery_preserves_coalesced_inflight_then_partial_then_queued_input() {
    for kind in [
        MetricStreamFailureKind::Unavailable,
        MetricStreamFailureKind::DeadlineExceeded,
        MetricStreamFailureKind::ResourceExhausted,
        MetricStreamFailureKind::Unauthenticated,
        MetricStreamFailureKind::InvalidArgument,
        MetricStreamFailureKind::FailedPrecondition,
    ] {
        let (mut worker, _wire, _) = open_worker();
        buffer(&mut worker, "sent-one");
        submit(&mut worker, "sent-two");
        buffer(&mut worker, "partial");
        worker.accept([Event::Metric(Metric::gauge("queued", 3.0))]);
        worker.fail(kind, "injected failure").unwrap();
        assert!(worker.buffered.is_none());
        assert!(worker.inflight.is_empty());
        if kind == MetricStreamFailureKind::FailedPrecondition {
            assert_eq!(
                worker
                    .fallback
                    .iter()
                    .map(|metric| metric.context().name().as_ref())
                    .collect::<Vec<_>>(),
                ["sent-one", "sent-two", "partial", "queued"]
            );
        } else {
            let expected: &[&str] = if kind == MetricStreamFailureKind::InvalidArgument {
                &["partial", "queued"]
            } else {
                &["sent-one", "sent-two", "partial", "queued"]
            };
            assert_eq!(pending_names(&worker), expected);
            assert_eq!(original_names(&worker), expected);
            assert_eq!(worker.buffered_batches(), expected.len());
        }
    }
}

#[tokio::test]
async fn rejected_flush_during_rotation_preserves_unsent_data_after_inflight() {
    let (mut worker, _wire, id) = open_worker();
    submit(&mut worker, "sent");
    buffer(&mut worker, "partial");
    worker.accept([Event::Metric(Metric::gauge("queued", 3.0))]);
    let effects = worker.core.handle_timer(id, TimerKind::RotateStream);
    worker.apply(effects).unwrap();
    assert!(worker.flush_deadline().is_none());
    worker.flush().unwrap();
    assert!(worker.buffered.is_none());
    assert_eq!(pending_names(&worker), ["partial", "queued"]);
    let effects = worker.core.handle_timer(id, TimerKind::DrainExpired);
    worker.apply(effects).unwrap();
    assert_eq!(pending_names(&worker), ["sent", "partial", "queued"]);
    assert_eq!(original_names(&worker), ["sent", "partial", "queued"]);
}

#[tokio::test]
async fn last_ack_during_rotation_releases_sent_originals_and_returns_partial() {
    let (mut worker, _wire, id) = open_worker();
    submit(&mut worker, "sent");
    buffer(&mut worker, "partial");
    let effects = worker.core.handle_timer(id, TimerKind::RotateStream);
    worker.apply(effects).unwrap();
    worker
        .on_transport(TransportEvent::Ack(BatchStatus { batch_id: 1, status: 1 }))
        .unwrap();
    assert!(worker.inflight.is_empty());
    assert!(worker.buffered.is_none());
    assert_eq!(pending_names(&worker), ["partial"]);
    assert_eq!(worker.buffered_batches(), 1);
}

#[tokio::test]
async fn reset_and_invalid_ack_return_unsent_metrics_in_order() {
    for reset in [false, true] {
        let (mut worker, _wire, _) = open_worker();
        submit(&mut worker, "sent");
        buffer(&mut worker, "partial");
        worker.accept([Event::Metric(Metric::gauge("queued", 3.0))]);
        if reset {
            worker.update_credentials("replacement").unwrap();
        } else {
            worker
                .on_transport(TransportEvent::Ack(BatchStatus {
                    batch_id: 99,
                    status: 1,
                }))
                .unwrap();
        }
        assert_eq!(pending_names(&worker), ["sent", "partial", "queued"]);
        assert_eq!(original_names(&worker), ["sent", "partial", "queued"]);
        assert_eq!(worker.buffered_batches(), 3);
        assert!(worker.buffered.is_none());
    }
}

#[tokio::test(start_paused = true)]
async fn expired_queued_input_flushes_when_capacity_returns_without_new_input() {
    let (mut worker, mut wire, _) = open_worker();
    for _ in 0..MAX_INFLIGHT_BATCHES {
        submit(&mut worker, "sent");
        wire.try_recv().unwrap();
    }
    buffer(&mut worker, "queued");
    assert_eq!(worker.pending.len(), 1);
    assert!(worker.flush_deadline().is_none());
    advance(Duration::from_secs(3)).await;
    worker
        .on_transport(TransportEvent::Ack(BatchStatus { batch_id: 1, status: 1 }))
        .unwrap();
    worker.pump().unwrap();
    assert_eq!(wire.try_recv().unwrap().batch_id, (MAX_INFLIGHT_BATCHES + 1) as u32);
    assert!(worker.pending.is_empty());
    assert!(worker.buffered.is_none());
}

#[tokio::test(start_paused = true)]
async fn worker_flush_timers_are_independent_and_zero_timeout_uses_encoder_minimum() {
    let (mut first, mut first_wire, _) = open_worker();
    let (mut second, mut second_wire, _) = open_worker();
    buffer(&mut first, "first");
    advance(Duration::from_secs(1)).await;
    buffer(&mut second, "second");
    advance(Duration::from_secs(1)).await;
    first.pump().unwrap();
    second.pump().unwrap();
    assert!(first_wire.try_recv().is_ok());
    assert!(second_wire.try_recv().is_err());
    assert!(second.buffered.is_some());
    let worker = StatefulMetricsWorker::new(
        Endpoint::from_static("http://localhost:8080"),
        parse_api_key("test-key").unwrap(),
        3,
        Duration::ZERO,
        512,
    );
    assert_eq!(worker.flush_timeout, MIN_FLUSH_TIMEOUT);
}

#[tokio::test]
async fn component_flushes_sparse_metrics_without_new_input_or_shutdown() {
    let mut harness = Harness::new().await;
    harness.worker.transport = None;
    let (in_tx, _out_rx, task) = start_component(
        harness.worker.endpoint.uri().to_string().into(),
        Duration::from_millis(50),
    )
    .await;
    let mut events = EventsBuffer::default();
    let metric = Metric::rate("rate", (123, 20.0), Duration::from_secs(10));
    assert!(events.try_push(Event::Metric(metric)).is_none());
    in_tx.send(events).await.unwrap();
    let batch = harness.receive().await;
    assert!(sequence(&batch).data.iter().any(|datum| matches!(
        &datum.data, Some(metric_datum::Data::MetricNameDefine(definition)) if definition.value == "rate"
    )));
    assert!(!task.is_finished());
    harness
        .replies
        .send(Ok(BatchStatus {
            batch_id: batch.batch_id,
            status: 1,
        }))
        .await
        .unwrap();
    drop(in_tx);
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap().unwrap();
}
