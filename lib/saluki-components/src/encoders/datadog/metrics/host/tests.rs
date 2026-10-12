use std::{collections::HashMap, io::Cursor, num::NonZeroU8, sync::Arc, time::Duration};

use agent_data_plane_config::shared::{AltMetricsIntake, SharedConfiguration, V3SeriesMode};
use bytes::Bytes;
use http::Request;
use saluki_common::{buf::ChunkedBytesBuffer, resource_tracking::ResourceGroupRegistry};
use saluki_core::{
    accounting::{ComponentBounds, ComponentRegistry, MemoryLimiter},
    components::{
        encoders::{Encoder, EncoderBuilder as _, EncoderContext},
        BuildContext, ComponentContext,
    },
    data_model::{
        event::{
            metric::{context::Context, Metric},
            Event,
        },
        payload::{Payload, PayloadMetadata},
        tags::{SharedTagSet, Tag, TagSet},
    },
    execution::{
        control::{control_mailbox, ControlError, ControlSender},
        EntryId,
    },
    health::HealthRegistry,
    runtime::{
        state::{DataspaceRegistry, ResourceRegistry},
        Supervisor,
    },
    support::SubsystemIdentifier,
    topology::{
        interconnect::{Consumer, Dispatcher},
        ComponentId, EventsBuffer, OutputName, TopologyContext,
    },
};
use saluki_error::GenericError;
use saluki_metrics::test::TestRecorder;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};

use super::*;
use crate::{
    common::datadog::{
        delivery::{EncoderSettings, PayloadRecipients, ProtocolVariants, RecipientSlot},
        io::RB_BUFFER_CHUNK_SIZE,
        protocol::MetricsEndpointRouting,
        test_util::shared_configuration,
        METRICS_SERIES_V2_PATH, METRICS_SERIES_V3_PATH, METRICS_SKETCHES_PATH,
    },
    encoders::DatadogMetricsConfiguration,
};

/// The component identifier the hosts of these tests run under.
const HOST_ID: &str = "dd_metrics_encode";

/// The buffers the input of a host under test holds.
const INPUT_BUFFERS: usize = 4;

/// How long a test waits for something that must happen. The asynchronous tests run on a paused clock, which advances
/// only once every task waits, so this is also how a test waits until everything that can happen has happened.
const SETTLE: Duration = Duration::from_secs(5);

/// A builder shard a test plays: it holds the shard's buffers, sends payloads and the closing marker as a builder
/// does, and fails the host by dropping `exit` without closing it.
pub(super) struct ShardProbe {
    pub(super) identity: ShardIdentity,
    pub(super) events: mpsc::Receiver<EventsBuffer>,
    pub(super) sink: LeaseSink,
    pub(super) exit: ShardExit,
}

impl ShardProbe {
    /// Sends a payload of `events` events, of the variant `variant`, as the shard's builder does.
    async fn send_payload(&mut self, events: usize, variant: MetricsPayloadInfo) {
        let body = ChunkedBytesBuffer::new(RB_BUFFER_CHUNK_SIZE).freeze();
        let request = Request::builder().uri(METRICS_SERIES_V2_PATH).body(body).unwrap();
        self.sink
            .send_payload(PayloadMetadata::from_event_count(events), request, variant)
            .await
            .expect("the host takes the payload");
    }

    /// Closes the shard as its builder does once its channel closed: records its exit as closed, and sends its
    /// marker after its last payload.
    async fn close(mut self) {
        self.exit.closed();
        let marker = HostPayload::LeaseClosed {
            lease: self.identity.lease,
            shard: self.identity.shard,
        };
        self.sink
            .payloads
            .send(marker)
            .await
            .expect("the host takes the marker");
    }
}

/// Returns a lease to prepare whose builder shards the test plays, with the receivers of each shard's probe.
fn probe_lease(spec: EncodeLeaseSpec) -> (LeasePrepare, Vec<oneshot::Receiver<ShardProbe>>) {
    let (shards, probes) = (0..spec.builders.get())
        .map(|_| {
            let (probe, receiver) = oneshot::channel();
            (BuiltShard::Probe(probe), receiver)
        })
        .unzip();
    (
        LeasePrepare {
            spec,
            built: BuiltLease { shards },
        },
        probes,
    )
}

fn slots(slots: &[u16]) -> PayloadRecipients {
    PayloadRecipients::new(slots.iter().copied().map(RecipientSlot::new).collect())
}

/// Returns resolved recipients that give the same slots to every protocol variant.
fn recipients(every_variant: &[u16]) -> ResolvedRecipients {
    ResolvedRecipients {
        v2_series: slots(every_variant),
        v2_sketches: slots(every_variant),
        v3_series: slots(every_variant),
        v3_sketches: slots(every_variant),
    }
}

/// Returns the native settings of the default configuration with V2 series and sketches only, and a flush timeout
/// long enough that only a full payload, a drain, or the end of the input flushes one.
fn settings() -> EncoderSettings {
    EncoderSettings {
        variants: ProtocolVariants {
            v2_series: true,
            v2_sketches: true,
            v3_series: false,
            v3_sketches: false,
        },
        flush_timeout: Duration::from_secs(3_600),
        ..DatadogMetricsConfiguration::from_configuration(&shared_configuration())
            .encoder_settings()
            .expect("the default endpoints resolve")
    }
}

fn key(output: &str, binding: u8) -> LeaseKeyDigest {
    LeaseKeyDigest {
        output: output.into(),
        binding: [binding; 32],
    }
}

fn spec(lease: u16, output: &str, settings: EncoderSettings, recipients: ResolvedRecipients) -> EncodeLeaseSpec {
    EncodeLeaseSpec {
        lease: EntryId::new(lease),
        key: key(output, lease as u8),
        telemetry_id: output.into(),
        settings,
        recipients: Arc::new(recipients),
        builders: NonZeroU8::MIN,
    }
}

/// Returns a buffer of `metrics` addressed to the lease `entry`.
fn buffer(entry: Option<u16>, metrics: impl IntoIterator<Item = Metric>) -> EventsBuffer {
    let mut buffer = EventsBuffer::default();
    for metric in metrics {
        assert!(buffer.try_push(Event::Metric(metric)).is_none(), "the buffer has room");
    }
    buffer.header_mut().entry = entry.map(EntryId::new);
    buffer
}

fn counters(prefix: &str, count: usize) -> Vec<Metric> {
    (0..count)
        .map(|index| {
            Metric::counter(
                Context::from_parts(format!("{}.counter.{}", prefix, index), TagSet::default()),
                1.0,
            )
        })
        .collect()
}

/// Returns the encoder context of a component under test, with the sender of its input and the receiver of its
/// output.
fn encoder_context(output_payloads: usize) -> (EncoderContext, mpsc::Sender<EventsBuffer>, mpsc::Receiver<Payload>) {
    let topology = SubsystemIdentifier::from_segments(["test"]);
    let component = ComponentContext::encoder(&topology, ComponentId::try_from(HOST_ID).unwrap());
    let health_registry = HealthRegistry::new();
    let health = health_registry
        .register_component(&component.identity())
        .expect("the encoder registers once");
    let topology_context = TopologyContext::new(
        Arc::from("test"),
        MemoryLimiter::noop(),
        health_registry,
        tokio::runtime::Handle::current(),
        DataspaceRegistry::new(),
    );
    let (output_tx, output) = mpsc::channel(output_payloads);
    let mut dispatcher = Dispatcher::new(component.clone());
    dispatcher.add_output(OutputName::Default).unwrap();
    dispatcher
        .attach_sender_to_output(&OutputName::Default, output_tx)
        .unwrap();
    let (input, input_rx) = mpsc::channel(INPUT_BUFFERS);
    let context = EncoderContext::new(
        &topology_context,
        &component,
        ComponentRegistry::default(),
        health,
        dispatcher,
        Consumer::new(component.clone(), input_rx),
    );
    (context, input, output)
}

fn build_context() -> BuildContext {
    let topology = SubsystemIdentifier::from_segments(["test"]);
    BuildContext::new(
        ComponentContext::encoder(&topology, ComponentId::try_from(HOST_ID).unwrap()),
        ResourceRegistry::new(),
    )
}

/// An encode host running in a task of its own, with its input, its output, and its controller.
struct Harness {
    input: Option<mpsc::Sender<EventsBuffer>>,
    output: mpsc::Receiver<Payload>,
    control: ControlSender<EncodeControl>,
    factory: Arc<dyn EncodeLeaseFactory>,
    host: JoinHandle<Result<(), GenericError>>,
}

impl Harness {
    /// Builds and starts a host, whose output holds `output_payloads` payloads.
    async fn start(configuration: MetricsEncodeHostConfiguration, output_payloads: usize) -> Self {
        let (control, mailbox) = control_mailbox();
        let configuration = configuration.with_control_mailbox(mailbox);
        let factory = configuration.lease_factory();
        let encoder = configuration
            .build(build_context())
            .await
            .expect("the encode host builds");
        let (context, input, output) = encoder_context(output_payloads);
        let host = tokio::spawn(encoder.run(context));
        assert!(
            control.running().await,
            "the encode host reads its mailbox once it runs"
        );
        Self {
            input: Some(input),
            output,
            control,
            factory,
            host,
        }
    }

    async fn send(&self, buffer: EventsBuffer) {
        self.input
            .as_ref()
            .expect("the input is open")
            .send(buffer)
            .await
            .expect("the host runs");
    }

    /// Prepares a lease the host's factory builds, and waits for the acknowledgment.
    async fn prepare(&self, spec: EncodeLeaseSpec) -> Result<(), ControlError> {
        let built = self.factory.build(&spec).expect("the lease builds");
        self.control
            .send(EncodeControl::Prepare(LeasePrepare { spec, built }))
            .await
    }

    /// Ends the host's input, and returns every payload the host dispatches until it stops, and how it stopped.
    async fn finish(self) -> (Vec<Payload>, Result<(), GenericError>) {
        self.finish_closing(Vec::new()).await
    }

    /// Ends the host's input, closes each probe once the host closed its shard, and returns every payload the host
    /// dispatches until it stops, and how it stopped.
    async fn finish_closing(mut self, probes: Vec<ShardProbe>) -> (Vec<Payload>, Result<(), GenericError>) {
        drop(self.input.take());
        for mut probe in probes {
            while probe.events.recv().await.is_some() {}
            probe.close().await;
        }
        let mut payloads = Vec::new();
        while let Some(payload) = self.output.recv().await {
            payloads.push(payload);
        }
        (payloads, self.host.await.expect("the host does not panic"))
    }
}

/// What a test checks of one dispatched payload.
#[derive(Debug, PartialEq)]
struct Delivered {
    uri: String,
    headers: Vec<(String, String)>,
    body: Bytes,
    events: usize,
    points: usize,
    info: Option<MetricsPayloadInfo>,
}

/// Returns what a test checks of a payload, with the recipients it names and whether it carries a native routing
/// policy.
fn delivered(payload: Payload) -> (Delivered, Option<PayloadRecipients>, bool) {
    let (metadata, request) = payload
        .try_into_http_payload()
        .expect("metrics payloads are HTTP payloads")
        .into_parts();
    let mut headers = request
        .headers()
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_str().unwrap().to_string()))
        .collect::<Vec<_>>();
    headers.sort();
    let delivered = Delivered {
        uri: request.uri().to_string(),
        headers,
        body: request.into_body().into_bytes(),
        events: metadata.event_count(),
        points: metadata.data_point_count(),
        info: metadata.get::<MetricsPayloadInfo>().copied(),
    };
    let recipients = metadata.get::<PayloadRecipients>().cloned();
    let routed = metadata.get::<Arc<MetricsEndpointRouting>>().is_some();
    (delivered, recipients, routed)
}

/// Returns the decompressed body of a zstd-compressed payload.
fn decompressed(delivered: &Delivered) -> Vec<u8> {
    zstd::stream::decode_all(Cursor::new(delivered.body.clone())).expect("the body is zstd-compressed")
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle.as_bytes())
}

/// Returns the value of the counter named `name` with exactly the labels `labels`, in any order.
fn counter(recorder: &TestRecorder, name: &str, labels: &[(&str, &str)]) -> Option<u64> {
    let mut expected = labels
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect::<Vec<_>>();
    expected.sort();
    recorder.counters().into_iter().find_map(|(key, value)| {
        let mut labels = key
            .labels()
            .map(|label| (label.key().to_string(), label.value().to_string()))
            .collect::<Vec<_>>();
        labels.sort();
        (key.name() == name && labels == expected).then_some(value)
    })
}

/// Returns the bounds of the component at `path` below `bounds`, a dotted path of subcomponent names.
fn bounds_at<'a>(bounds: &'a ComponentBounds, path: &str) -> &'a ComponentBounds {
    path.split('.').fold(bounds, |node, segment| {
        node.subcomponents()
            .into_iter()
            .find_map(|(name, child)| (name.as_str() == segment).then_some(child))
            .unwrap_or_else(|| panic!("no component '{}' on the path '{}'", segment, path))
    })
}

/// Returns the names of every resource group registered in the process.
fn resource_groups() -> Vec<String> {
    let mut groups = Vec::new();
    ResourceGroupRegistry::global().visit_resource_groups(|name, _| groups.push(name.to_string()));
    groups
}

#[tokio::test(start_paused = true)]
async fn buffers_reach_the_lease_their_entry_names_and_its_payloads_carry_its_recipients() {
    let main = spec(1, "dd_metrics_encode", settings(), recipients(&[0, 2]));
    let group = spec(4, "metrics_routing_encode_0", settings(), recipients(&[3]));
    let harness = Harness::start(
        MetricsEncodeHostConfiguration::new().with_lease(main).with_lease(group),
        64,
    )
    .await;

    harness.send(buffer(Some(1), counters("main", 3))).await;
    harness.send(buffer(Some(4), counters("group", 2))).await;
    harness
        .send(buffer(Some(1), [Metric::distribution("main.distribution", 1.0)]))
        .await;
    let (payloads, result) = harness.finish().await;
    result.expect("the host stops cleanly");

    let mut by_recipients = payloads
        .into_iter()
        .map(|payload| {
            let (delivered, recipients, _) = delivered(payload);
            let recipients = recipients.expect("every payload of a lease names its recipients");
            (recipients.slots().to_vec(), delivered)
        })
        .collect::<Vec<_>>();
    by_recipients.sort_by(|(a, x), (b, y)| a.cmp(b).then(x.uri.cmp(&y.uri)));

    let main = [RecipientSlot::new(0), RecipientSlot::new(2)];
    let group = [RecipientSlot::new(3)];
    let summary = by_recipients
        .iter()
        .map(|(slots, delivered)| (slots.as_slice(), delivered.uri.as_str(), delivered.events))
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        [
            (&main[..], METRICS_SKETCHES_PATH, 1),
            (&main[..], METRICS_SERIES_V2_PATH, 3),
            (&group[..], METRICS_SERIES_V2_PATH, 2),
        ]
    );
    let main_series = decompressed(&by_recipients[1].1);
    assert!(contains(&main_series, "main.counter.2") && !contains(&main_series, "group.counter"));
    let group_series = decompressed(&by_recipients[2].1);
    assert!(contains(&group_series, "group.counter.1") && !contains(&group_series, "main.counter"));
}

#[tokio::test(start_paused = true)]
async fn each_payload_names_the_recipients_its_lease_resolved_for_its_protocol_variant() {
    let settings = EncoderSettings {
        variants: ProtocolVariants {
            v2_series: true,
            v2_sketches: true,
            v3_series: true,
            v3_sketches: false,
        },
        ..settings()
    };
    let resolved = ResolvedRecipients {
        v2_series: slots(&[0]),
        v2_sketches: slots(&[0, 3]),
        v3_series: slots(&[3]),
        v3_sketches: slots(&[]),
    };
    let harness = Harness::start(
        MetricsEncodeHostConfiguration::new().with_lease(spec(2, HOST_ID, settings, resolved)),
        64,
    )
    .await;

    harness
        .send(buffer(
            Some(2),
            [
                Metric::gauge("variants.gauge", 1.0),
                Metric::distribution("variants.distribution", 2.0),
            ],
        ))
        .await;
    let (payloads, result) = harness.finish().await;
    result.expect("the host stops cleanly");

    let mut seen = payloads
        .into_iter()
        .map(|payload| {
            let (delivered, recipients, _) = delivered(payload);
            (
                delivered.uri,
                delivered.info,
                recipients.expect("every payload names its recipients"),
            )
        })
        .collect::<Vec<_>>();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    // Series are tagged with their variant, since the lease produces them in both protocols; sketches are not, since
    // it produces them in one.
    assert_eq!(
        seen,
        [
            (METRICS_SKETCHES_PATH.to_string(), None, slots(&[0, 3])),
            (
                METRICS_SERIES_V3_PATH.to_string(),
                Some(MetricsPayloadInfo::v3_series()),
                slots(&[3])
            ),
            (
                METRICS_SERIES_V2_PATH.to_string(),
                Some(MetricsPayloadInfo::v2_series()),
                slots(&[0])
            ),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_drain_flushes_the_leases_open_payloads_with_its_own_recipients_before_it_is_acknowledged() {
    // A rebinding: the output's old lease drains while its new lease, prepared live, keeps its payloads open.
    let old = spec(1, HOST_ID, settings(), recipients(&[0, 3]));
    let new = spec(2, HOST_ID, settings(), recipients(&[0]));
    let mut harness = Harness::start(MetricsEncodeHostConfiguration::new().with_lease(old.clone()), 64).await;
    harness.prepare(new).await.expect("the new lease prepares");

    harness.send(buffer(Some(1), counters("old", 2))).await;
    harness.send(buffer(Some(2), counters("new", 3))).await;
    // A controller drains a lease only once its generations retired, which waits for their last buffers to be
    // consumed: let the builders take them first.
    tokio::time::sleep(SETTLE).await;
    timeout(
        SETTLE,
        harness.control.send(EncodeControl::Drain {
            lease: old.lease,
            key: old.key.clone(),
        }),
    )
    .await
    .expect("the drain is acknowledged")
    .expect("the drain succeeds");

    // The acknowledgment comes once the old lease's payload is in the delivery host's input.
    let (drained, recipients, _) = delivered(
        harness
            .output
            .try_recv()
            .expect("the old lease's payload was dispatched"),
    );
    assert_eq!((drained.uri.as_str(), drained.events), (METRICS_SERIES_V2_PATH, 2));
    assert_eq!(recipients, Some(slots(&[0, 3])));
    assert!(
        harness.output.try_recv().is_err(),
        "the new lease keeps its payload open"
    );

    // The drained lease is gone: a buffer still naming it is lost.
    harness.send(buffer(Some(1), counters("late", 1))).await;
    let (payloads, result) = harness.finish().await;
    result.expect("the host stops cleanly");
    let payloads = payloads.into_iter().map(delivered).collect::<Vec<_>>();
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].0.events, 3);
    assert_eq!(payloads[0].1, Some(slots(&[0])));
}

#[tokio::test(start_paused = true)]
async fn a_drain_is_acknowledged_only_after_the_payloads_still_waiting_for_the_front_task_are_dispatched() {
    let lease = spec(5, HOST_ID, settings(), recipients(&[1]));
    // The delivery host takes one payload and is not read, so the front task blocks on the second one.
    let mut harness = Harness::start(MetricsEncodeHostConfiguration::new(), 1).await;
    let (prepare, probes) = probe_lease(lease.clone());
    harness
        .control
        .send(EncodeControl::Prepare(prepare))
        .await
        .expect("the lease prepares");
    let mut probe = probes.into_iter().next().unwrap().await.unwrap();

    for events in [1, 2, 3] {
        probe.send_payload(events, MetricsPayloadInfo::v2_series()).await;
    }
    // The first payload fills the delivery host's input, the front task waits to dispatch the second, and the third
    // waits in the channel the builders share.
    tokio::time::sleep(SETTLE).await;
    let mut drained = harness.control.send(EncodeControl::Drain {
        lease: lease.lease,
        key: lease.key.clone(),
    });
    // The drain closes the shard while the front task waits, and the builder answers with its marker.
    assert!(
        timeout(SETTLE, probe.events.recv()).await.unwrap().is_none(),
        "the drain closes the shard while the front task waits for the delivery host"
    );
    probe.close().await;
    assert!(
        timeout(SETTLE, &mut drained).await.is_err(),
        "the drain waits for the payloads queued before the marker"
    );

    let mut events = Vec::new();
    for _ in 0..2 {
        events.push(delivered(harness.output.recv().await.unwrap()).0.events);
    }
    timeout(SETTLE, &mut drained)
        .await
        .expect("the drain is acknowledged after the marker")
        .expect("the drain succeeds");
    // The acknowledgment came after the front task dispatched the last payload queued before the marker.
    events.push(
        delivered(harness.output.try_recv().expect("the last payload was dispatched"))
            .0
            .events,
    );
    assert_eq!(events, [1, 2, 3]);

    let (payloads, result) = harness.finish().await;
    assert!(payloads.is_empty());
    result.expect("the host stops cleanly");
}

#[tokio::test(start_paused = true)]
async fn prepares_and_drains_are_served_while_the_front_task_waits_for_room_in_a_full_shard() {
    let full = spec(1, HOST_ID, settings(), recipients(&[0]));
    let harness = Harness::start(MetricsEncodeHostConfiguration::new(), 64).await;
    let (prepare, probes) = probe_lease(full.clone());
    harness
        .control
        .send(EncodeControl::Prepare(prepare))
        .await
        .expect("the lease prepares");
    let mut probe = probes.into_iter().next().unwrap().await.unwrap();

    // The shard holds `LEASE_QUEUE_BUFFERS` buffers; the next one waits in the front task, and the one after it in the
    // host's input.
    for index in 0..=LEASE_QUEUE_BUFFERS + 1 {
        harness
            .send(buffer(Some(1), counters(&format!("full.{}", index), 1)))
            .await;
    }

    // A prepare, a drain of a lease that retired already, and a drain of a lease that has to close are all served.
    let other = spec(2, "metrics_routing_encode_0", settings(), recipients(&[4]));
    let (prepare, other_probes) = probe_lease(other.clone());
    timeout(SETTLE, harness.control.send(EncodeControl::Prepare(prepare)))
        .await
        .expect("the prepare is answered while the front task waits")
        .expect("the prepare succeeds");
    timeout(
        SETTLE,
        harness.control.send(EncodeControl::Drain {
            lease: EntryId::new(9),
            key: key("gone", 9),
        }),
    )
    .await
    .expect("the drain of a lease the host does not hold is answered at once")
    .expect("the drain succeeds");
    let drained = harness.control.send(EncodeControl::Drain {
        lease: other.lease,
        key: other.key,
    });
    let other_probe = other_probes.into_iter().next().unwrap().await.unwrap();
    other_probe.close().await;
    timeout(SETTLE, drained)
        .await
        .expect("the drain is acknowledged while the front task waits")
        .expect("the drain succeeds");

    // Room in the shard lets the waiting buffer in, and then the one from the input.
    let mut names = Vec::new();
    for _ in 0..LEASE_QUEUE_BUFFERS + 2 {
        let buffer = timeout(SETTLE, probe.events.recv()).await.unwrap().unwrap();
        let metric = buffer.into_iter().next().unwrap().try_into_metric().unwrap();
        names.push(metric.context().name().to_string());
    }
    let expected = (0..=LEASE_QUEUE_BUFFERS + 1)
        .map(|index| format!("full.{}.counter.0", index))
        .collect::<Vec<_>>();
    assert_eq!(names, expected, "buffers reach the shard in order");

    let (_, result) = harness.finish_closing(vec![probe]).await;
    result.expect("the host stops cleanly");
}

#[tokio::test(start_paused = true)]
async fn a_full_shard_holds_back_the_host_input_until_its_builder_takes_a_buffer() {
    let lease = spec(1, HOST_ID, settings(), recipients(&[0]));
    let harness = Harness::start(MetricsEncodeHostConfiguration::new(), 64).await;
    let (prepare, probes) = probe_lease(lease);
    harness
        .control
        .send(EncodeControl::Prepare(prepare))
        .await
        .expect("the lease prepares");
    let mut probe = probes.into_iter().next().unwrap().await.unwrap();

    let input = harness.input.as_ref().unwrap();
    for _ in 0..LEASE_QUEUE_BUFFERS + 1 {
        harness.send(buffer(Some(1), counters("backpressure", 1))).await;
    }
    harness.send(buffer(Some(1), counters("backpressure", 1))).await;
    tokio::time::sleep(SETTLE).await;
    // The shard holds its buffers, the front task one more, and the last one waits in the input.
    assert_eq!(input.capacity(), INPUT_BUFFERS - 1);

    probe.events.recv().await.unwrap();
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        input.capacity(),
        INPUT_BUFFERS,
        "the front task takes the next buffer once the shard has room"
    );

    let (_, result) = harness.finish_closing(vec![probe]).await;
    result.expect("the host stops cleanly");
}

#[tokio::test(start_paused = true)]
async fn buffers_that_name_no_live_lease_are_counted_as_invalid_loss() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);

    let live = spec(1, HOST_ID, settings(), recipients(&[0]));
    let drained = spec(2, "metrics_routing_encode_0", settings(), recipients(&[3]));
    let harness = Harness::start(
        MetricsEncodeHostConfiguration::new()
            .with_lease(live)
            .with_lease(drained.clone()),
        64,
    )
    .await;
    harness
        .control
        .send(EncodeControl::Drain {
            lease: drained.lease,
            key: drained.key,
        })
        .await
        .expect("the lease drains");

    // No entry, an entry never prepared, and the entry of a lease that drained: one, two, and four points.
    harness.send(buffer(None, counters("none", 1))).await;
    harness
        .send(buffer(Some(77), [Metric::counter("never", [(1, 1.0), (2, 1.0)])]))
        .await;
    harness.send(buffer(Some(2), counters("drained", 4))).await;
    harness.send(buffer(Some(1), counters("live", 2))).await;
    let (payloads, result) = harness.finish().await;
    result.expect("the host stops cleanly");

    assert_eq!(
        counter(
            &recorder,
            PLAN_LOSS_TOTAL,
            &[("component_id", HOST_ID), ("class", "invalid")]
        ),
        Some(7)
    );
    let events = payloads
        .into_iter()
        .map(|payload| delivered(payload).0.events)
        .collect::<Vec<_>>();
    assert_eq!(events, [2], "the live lease's buffers still flow");
}

#[tokio::test(start_paused = true)]
async fn a_buffer_naming_a_lease_while_it_drains_is_counted_as_invalid_loss_and_the_host_keeps_running() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);

    let live = spec(1, HOST_ID, settings(), recipients(&[0]));
    let draining = spec(6, "metrics_routing_encode_0", settings(), recipients(&[2]));
    let harness = Harness::start(MetricsEncodeHostConfiguration::new().with_lease(live), 64).await;
    let (prepare, probes) = probe_lease(draining.clone());
    harness
        .control
        .send(EncodeControl::Prepare(prepare))
        .await
        .expect("the lease prepares");
    let mut probe = probes.into_iter().next().unwrap().await.unwrap();

    // A drain sent too early, as a wiring fault would send it: the host holds the lease until its builder closes,
    // but the lease takes no buffer any more.
    let drained = harness.control.send(EncodeControl::Drain {
        lease: draining.lease,
        key: draining.key.clone(),
    });
    assert!(
        timeout(SETTLE, probe.events.recv()).await.unwrap().is_none(),
        "the drain closes the shard"
    );
    harness.send(buffer(Some(6), counters("early", 3))).await;
    harness.send(buffer(Some(1), counters("live", 2))).await;
    tokio::time::sleep(SETTLE).await;
    assert!(!harness.host.is_finished(), "the host keeps running");
    assert_eq!(
        counter(
            &recorder,
            PLAN_LOSS_TOTAL,
            &[("component_id", HOST_ID), ("class", "invalid")]
        ),
        Some(3)
    );

    probe.close().await;
    timeout(SETTLE, drained)
        .await
        .expect("the drain is acknowledged once the builder closed")
        .expect("the drain succeeds");
    let (payloads, result) = harness.finish().await;
    result.expect("the host stops cleanly");
    let events = payloads
        .into_iter()
        .map(|payload| delivered(payload).0.events)
        .collect::<Vec<_>>();
    assert_eq!(events, [2], "the live lease's buffers still flow");
}

#[tokio::test(start_paused = true)]
async fn a_repeated_prepare_is_acknowledged_and_one_into_an_entry_held_by_another_or_a_draining_lease_is_rejected() {
    let lease = spec(3, HOST_ID, settings(), recipients(&[0]));
    let harness = Harness::start(MetricsEncodeHostConfiguration::new(), 64).await;
    harness.prepare(lease.clone()).await.expect("the lease prepares");

    // A prepare of the lease the host holds, repeated after a controller restart, changes nothing: its builders are
    // never spawned.
    let (repeated, probes) = probe_lease(lease.clone());
    harness
        .control
        .send(EncodeControl::Prepare(repeated))
        .await
        .expect("a repeated prepare is acknowledged as done");
    assert!(probes.into_iter().next().unwrap().await.is_err());

    // Another lease in the same entry is refused, and naming it in a drain leaves the held lease running.
    let other = EncodeLeaseSpec {
        key: key("metrics_routing_encode_1", 9),
        telemetry_id: "metrics_routing_encode_1".into(),
        ..lease.clone()
    };
    let error = harness.prepare(other.clone()).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "The encode host could not apply the control message: Lease 3 cannot be prepared for output \
         'metrics_routing_encode_1': this encode host still holds it for output 'dd_metrics_encode'. A lease \
         identifier is reused only after its lease retired."
    );
    harness
        .control
        .send(EncodeControl::Drain {
            lease: other.lease,
            key: other.key,
        })
        .await
        .expect("a drain naming a lease the host does not hold is acknowledged");

    // A draining lease's entry refuses a prepare too, under the lease's own key as under another: acknowledging it
    // would commit generations over a lease that takes no buffer.
    let draining = spec(7, "metrics_routing_encode_2", settings(), recipients(&[1]));
    let (prepare, probes) = probe_lease(draining.clone());
    harness
        .control
        .send(EncodeControl::Prepare(prepare))
        .await
        .expect("the lease prepares");
    let mut probe = probes.into_iter().next().unwrap().await.unwrap();
    let drained = harness.control.send(EncodeControl::Drain {
        lease: draining.lease,
        key: draining.key.clone(),
    });
    assert!(timeout(SETTLE, probe.events.recv()).await.unwrap().is_none());
    let replacement = EncodeLeaseSpec {
        key: key("metrics_routing_encode_3", 9),
        telemetry_id: "metrics_routing_encode_3".into(),
        ..draining.clone()
    };
    for prepared in [&draining, &replacement] {
        let (repeated, probes) = probe_lease(prepared.clone());
        let error = harness
            .control
            .send(EncodeControl::Prepare(repeated))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "The encode host could not apply the control message: Lease 7 cannot be prepared for output '{}': \
                 this encode host still holds it for output 'metrics_routing_encode_2', and is draining it. A lease \
                 identifier is reused only after its lease retired.",
                prepared.key.output
            )
        );
        assert!(
            probes.into_iter().next().unwrap().await.is_err(),
            "a refused lease's builders are never spawned"
        );
    }
    // Once the lease retired, its entry takes a new lease.
    probe.close().await;
    timeout(SETTLE, drained)
        .await
        .expect("the drain is acknowledged once the builder closed")
        .expect("the drain succeeds");
    harness
        .prepare(replacement)
        .await
        .expect("a retired lease's entry takes a new lease");

    harness.send(buffer(Some(3), counters("held", 2))).await;
    harness.send(buffer(Some(7), counters("replacement", 1))).await;
    let (payloads, result) = harness.finish().await;
    result.expect("the host stops cleanly");
    let mut events = payloads
        .into_iter()
        .map(|payload| delivered(payload).0.events)
        .collect::<Vec<_>>();
    events.sort();
    assert_eq!(events, [1, 2], "the held lease and the replacement run");
}

#[tokio::test(start_paused = true)]
async fn a_lease_with_several_builder_shards_gives_each_buffer_to_the_shard_with_the_most_room() {
    let lease = EncodeLeaseSpec {
        builders: NonZeroU8::new(2).unwrap(),
        ..spec(1, HOST_ID, settings(), recipients(&[0]))
    };
    let harness = Harness::start(MetricsEncodeHostConfiguration::new(), 64).await;
    let (prepare, probes) = probe_lease(lease.clone());
    harness
        .control
        .send(EncodeControl::Prepare(prepare))
        .await
        .expect("the lease prepares");
    let mut shards = Vec::new();
    for probe in probes {
        shards.push(probe.await.unwrap());
    }

    // Both shards empty: the rotation decides, and then the shard with more room.
    for index in 0..3 {
        harness
            .send(buffer(Some(1), counters(&format!("first.{}", index), 1)))
            .await;
    }
    tokio::time::sleep(SETTLE).await;
    assert_eq!((shards[0].events.len(), shards[1].events.len()), (2, 1));

    // Shard 0 holds more, so shard 1 has the most room whatever the rotation says.
    shards[1].events.recv().await.unwrap();
    for index in 0..2 {
        harness
            .send(buffer(Some(1), counters(&format!("second.{}", index), 1)))
            .await;
    }
    tokio::time::sleep(SETTLE).await;
    assert_eq!((shards[0].events.len(), shards[1].events.len()), (2, 2));

    // The drain closes every shard, and is acknowledged once each sent its marker.
    let mut drained = harness.control.send(EncodeControl::Drain {
        lease: lease.lease,
        key: lease.key,
    });
    let mut closed = Vec::new();
    for mut shard in shards {
        while timeout(SETTLE, shard.events.recv()).await.unwrap().is_some() {}
        closed.push(shard);
    }
    let last = closed.pop().unwrap();
    closed.pop().unwrap().close().await;
    assert!(timeout(SETTLE, &mut drained).await.is_err(), "one shard is still open");
    last.close().await;
    timeout(SETTLE, drained)
        .await
        .expect("the drain is acknowledged once every shard closed")
        .expect("the drain succeeds");

    let (_, result) = harness.finish().await;
    result.expect("the host stops cleanly");
}

#[tokio::test(start_paused = true)]
async fn at_shutdown_the_host_flushes_every_lease_and_answers_what_it_cannot_finish_as_stopped() {
    let flushing = spec(1, HOST_ID, settings(), recipients(&[0]));
    let draining = spec(2, "metrics_routing_encode_0", settings(), recipients(&[3]));
    let mut harness = Harness::start(MetricsEncodeHostConfiguration::new().with_lease(flushing), 64).await;
    let (prepare, probes) = probe_lease(draining.clone());
    harness
        .control
        .send(EncodeControl::Prepare(prepare))
        .await
        .expect("the lease prepares");
    let probe = probes.into_iter().next().unwrap().await.unwrap();

    harness.send(buffer(Some(1), counters("open", 4))).await;
    // The drain waits for the probe's marker, which the probe sends only after the host's input closes.
    let drained = harness.control.send(EncodeControl::Drain {
        lease: draining.lease,
        key: draining.key,
    });
    tokio::time::sleep(SETTLE).await;

    drop(harness.input.take());
    assert!(matches!(
        timeout(SETTLE, drained).await.expect("the drain is answered"),
        Err(ControlError::Stopped {
            component: "encode host"
        })
    ));
    let late = harness
        .factory
        .build(&spec(6, HOST_ID, settings(), recipients(&[0])))
        .unwrap();
    assert!(matches!(
        timeout(
            SETTLE,
            harness.control.send(EncodeControl::Prepare(LeasePrepare {
                spec: spec(6, HOST_ID, settings(), recipients(&[0])),
                built: late,
            }))
        )
        .await
        .expect("a prepare after the input ended is answered"),
        Err(ControlError::Stopped { .. })
    ));

    // The host keeps dispatching until every builder exited, its own included.
    probe.close().await;
    let (payloads, result) = harness.finish().await;
    result.expect("the host stops cleanly");
    let payloads = payloads.into_iter().map(delivered).collect::<Vec<_>>();
    assert_eq!(payloads.len(), 1);
    assert_eq!((payloads[0].0.events, payloads[0].1.clone()), (4, Some(slots(&[0]))));
}

#[tokio::test(start_paused = true)]
async fn a_builder_shard_that_stops_without_closing_fails_the_host() {
    let lease = spec(8, "resources_team_d", settings(), recipients(&[5]));
    let harness = Harness::start(MetricsEncodeHostConfiguration::new(), 64).await;
    let (prepare, probes) = probe_lease(lease);
    harness
        .control
        .send(EncodeControl::Prepare(prepare))
        .await
        .expect("the lease prepares");

    // As when its task panics.
    drop(probes.into_iter().next().unwrap().await.unwrap());
    let error = timeout(SETTLE, harness.host)
        .await
        .expect("the host stops")
        .expect("the host does not panic")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Builder shard 0 of lease 8 for output 'resources_team_d' failed: its task stopped before the shard closed: \
         it panicked, or was cancelled"
    );
}

#[tokio::test(start_paused = true)]
async fn a_lease_counts_the_metrics_of_payloads_too_large_on_their_own_under_its_output() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);

    // A compressed series payload of one metric is larger than 20 bytes.
    let settings = EncoderSettings {
        max_series_payload_size: 20,
        ..settings()
    };
    let harness = Harness::start(
        MetricsEncodeHostConfiguration::new().with_lease(spec(
            1,
            "metrics_routing_encode_2",
            settings,
            recipients(&[0]),
        )),
        64,
    )
    .await;
    // Three metrics: the payload splits in halves of one and two metrics, both still too large.
    harness.send(buffer(Some(1), counters("oversized", 3))).await;
    let (payloads, result) = harness.finish().await;
    result.expect("the host stops cleanly");

    assert!(payloads.is_empty());
    assert_eq!(
        counter(
            &recorder,
            "component_events_dropped_total",
            &[
                ("component_id", "metrics_routing_encode_2"),
                ("component_type", "encoder"),
                ("intentional", "false"),
                ("drop_reason", "payload_too_large"),
            ]
        ),
        Some(3)
    );
}

#[tokio::test(start_paused = true)]
async fn a_lease_counts_the_metrics_its_recipients_accept_no_protocol_variant_of_as_dropped_under_its_output() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);

    // A lease whose recipients take only V2 series, as the Cluster Agent's do, and one whose recipients take only
    // sketches.
    let variants = |series: bool, sketches: bool| EncoderSettings {
        variants: ProtocolVariants {
            v2_series: series,
            v2_sketches: sketches,
            v3_series: false,
            v3_sketches: false,
        },
        ..settings()
    };
    let harness = Harness::start(
        MetricsEncodeHostConfiguration::new()
            .with_lease(spec(1, "af_metrics_encode", variants(true, false), recipients(&[0])))
            .with_lease(spec(
                2,
                "metrics_routing_encode_1",
                variants(false, true),
                recipients(&[1]),
            )),
        64,
    )
    .await;
    let distributions = |prefix: &str, count: usize| {
        (0..count)
            .map(|index| {
                let name = format!("{}.distribution.{}", prefix, index);
                Metric::distribution(Context::from_parts(name, TagSet::default()), 1.0)
            })
            .collect::<Vec<_>>()
    };
    let series_only = counters("series_only", 2)
        .into_iter()
        .chain(distributions("series_only", 3));
    harness.send(buffer(Some(1), series_only)).await;
    let sketches_only = counters("sketches_only", 4)
        .into_iter()
        .chain(distributions("sketches_only", 1));
    harness.send(buffer(Some(2), sketches_only)).await;
    let (payloads, result) = harness.finish().await;
    result.expect("the host stops cleanly");

    let mut summary = payloads
        .into_iter()
        .map(|payload| {
            let delivered = delivered(payload).0;
            (delivered.uri, delivered.events)
        })
        .collect::<Vec<_>>();
    summary.sort();
    assert_eq!(
        summary,
        [
            (METRICS_SKETCHES_PATH.to_string(), 1),
            (METRICS_SERIES_V2_PATH.to_string(), 2)
        ]
    );
    for (output, dropped) in [("af_metrics_encode", 3), ("metrics_routing_encode_1", 4)] {
        assert_eq!(
            counter(
                &recorder,
                "component_events_dropped_total",
                &[
                    ("component_id", output),
                    ("component_type", "encoder"),
                    ("intentional", "false"),
                    ("drop_reason", "encoder_failure"),
                ]
            ),
            Some(dropped),
            "the metrics dropped by {}",
            output
        );
    }
}

#[test]
fn the_lease_factory_refuses_settings_its_streams_cannot_produce() {
    let factory = MetricsEncodeHostConfiguration::new().lease_factory();
    let v3 = ProtocolVariants {
        v2_series: true,
        v2_sketches: true,
        v3_series: true,
        v3_sketches: false,
    };
    let cases = [
        (
            EncoderSettings {
                variants: v3,
                compressor_kind: "zlib".into(),
                ..settings()
            },
            "V3 metrics payloads cannot be compressed with zlib. Set `serializer_compressor_kind` to `zstd`, or send \
             V2 payloads to every endpoint.",
        ),
        (
            EncoderSettings {
                variants: v3,
                series: crate::common::datadog::delivery::SeriesVariant::V1,
                ..settings()
            },
            "V3 series payloads need the V2 series API. Set `use_v2_api.series` to `true`, or send V2 series to every \
             endpoint.",
        ),
        (
            EncoderSettings {
                max_series_uncompressed_payload_size: 0,
                ..settings()
            },
            "uncompressed size limit is lower (0 bytes) than minimum payload size",
        ),
    ];
    for (settings, cause) in cases {
        let error = factory
            .build(&spec(4, "metrics_routing_encode_0", settings, recipients(&[0])))
            .unwrap_err();
        let message = format!("{:#}", error);
        assert!(
            message.starts_with(
                "Failed to build the request builders of lease 4 for output 'metrics_routing_encode_0'.: "
            ) && message.contains(cause),
            "{}",
            message
        );
    }
}

#[test]
fn a_leases_bound_counts_the_reencode_buffers_and_full_queued_buffers_of_each_shard() {
    let lease = EncodeLeaseSpec {
        builders: NonZeroU8::new(3).unwrap(),
        settings: EncoderSettings {
            max_metrics_per_payload: 250,
            ..settings()
        },
        ..spec(1, HOST_ID, settings(), recipients(&[0]))
    };
    let full_buffer = size_of::<EventsBuffer>() + 1_024 * size_of::<Event>();
    let per_shard = 2 * 250 * size_of::<Metric>() + 8 * full_buffer;
    assert_eq!(lease_bound_bytes(&lease), 3 * per_shard as u64);
}

#[test]
fn the_hosts_boot_bounds_count_the_shard_channels_and_reencode_buffers_of_each_boot_lease() {
    let lease = |entry, builders, max_metrics_per_payload| EncodeLeaseSpec {
        builders: NonZeroU8::new(builders).unwrap(),
        settings: EncoderSettings {
            max_metrics_per_payload,
            ..settings()
        },
        ..spec(entry, HOST_ID, settings(), recipients(&[0]))
    };
    let declared = EncodeLeaseSpec {
        key: key("resources_team_a", 3),
        telemetry_id: "resources_team_a".into(),
        ..lease(3, 1, 50)
    };
    let configuration = MetricsEncodeHostConfiguration::new()
        .with_lease(lease(1, 1, 100))
        .with_lease(lease(2, 2, 10))
        .with_lease(declared);
    let registry = ComponentRegistry::default();
    configuration.specify_bounds(&mut registry.bounds_builder(&SubsystemIdentifier::from_dotted("test")));
    let bounds = registry.as_bounds();

    let minimum = size_of::<MetricsEncodeHost>() + 8 * size_of::<HostPayload>() + 4 * 8 * size_of::<EventsBuffer>();
    let native_reencode = 2 * (100 + 2 * 10) * size_of::<Metric>();
    let declared_reencode = 2 * 50 * size_of::<Metric>();
    assert_eq!(bounds.total_minimum_required_bytes(), minimum);
    assert_eq!(
        bounds.total_firm_limit_bytes(),
        minimum + native_reencode + declared_reencode
    );
    // The re-encode buffers count in the groups the leases' builders run in, below the host's.
    assert_eq!(
        bounds_at(&bounds, "test.leases.dd_metrics_encode").total_firm_limit_bytes(),
        native_reencode
    );
    assert_eq!(
        bounds_at(&bounds, "test.resources").total_firm_limit_bytes(),
        declared_reencode
    );
}

#[tokio::test(start_paused = true)]
async fn a_leases_builders_are_accounted_in_a_resource_group_below_the_hosts_own() {
    let native = spec(1, "metrics_routing_encode_4", settings(), recipients(&[0]));
    let declared = spec(2, "resources_team_f", settings(), recipients(&[1]));
    let encoder = MetricsEncodeHostConfiguration::new()
        .with_lease(native)
        .with_lease(declared)
        .build(build_context())
        .await
        .expect("the encode host builds");
    // Under a supervisor, as a topology runs it, so that the builders run as the host's children.
    let payloads = run_to_end(
        encoder,
        vec![
            buffer(Some(1), counters("native", 1)),
            buffer(Some(2), counters("declared", 1)),
        ],
    )
    .await;
    assert_eq!(payloads.len(), 2);

    // The host's identity is `test.encoders.dd_metrics_encode`, its component group.
    let groups = resource_groups();
    for group in [
        "test.encoders.dd_metrics_encode.leases.metrics_routing_encode_4",
        "test.encoders.dd_metrics_encode.resources",
    ] {
        assert!(groups.iter().any(|name| name == group), "{} in {:?}", group, groups);
    }
    assert!(
        !groups
            .iter()
            .any(|name| name.starts_with("metrics_routing_encode_4") || name.starts_with("dd_metrics_encode")),
        "no lease group at the root: {:?}",
        groups
    );
}

/// Runs an encoder under a supervisor, as a topology does, over `buffers`, and returns every payload it dispatches
/// before it stops.
async fn run_to_end(encoder: Box<dyn Encoder + Send>, buffers: Vec<EventsBuffer>) -> Vec<Payload> {
    let (context, input, mut output) = encoder_context(1_024);
    let mut supervisor = Supervisor::new("encode-host-differential").expect("the supervisor is created");
    // A one-shot child, as a topology runs a component: the encoder stops when its input ends.
    supervisor.add_worker(runtime::worker(HOST_ID, encoder.run(context)).build());
    let (shutdown, shutdown_rx) = oneshot::channel();
    let supervisor = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

    let feed = tokio::spawn(async move {
        for buffer in buffers {
            input.send(buffer).await.expect("the encoder runs");
        }
    });
    let mut payloads = Vec::new();
    while let Some(payload) = output.recv().await {
        payloads.push(payload);
    }
    feed.await.unwrap();
    let _ = shutdown.send(());
    supervisor.await.unwrap().expect("the supervisor stops cleanly");
    payloads
}

/// Returns metrics of every type, some of them sharing names, a few with values the encoders drop, and enough of
/// them that the payload limits of the differential cut several payloads of each kind.
fn differential_metrics() -> Vec<Metric> {
    let tags = |index: usize| -> TagSet {
        [
            format!("host:host-{}", index % 7),
            "env:prod".to_string(),
            format!("shard:{}", index % 13),
        ]
        .iter()
        .map(|tag| Tag::from(tag.as_str()))
        .collect()
    };
    (0..600)
        .map(|index| {
            let name = format!("differential.metric.{}", index % 40);
            let context = Context::from_parts(name, tags(index));
            match index % 8 {
                0 => Metric::counter(context, index as f64),
                1 => Metric::gauge(context, [(1_700_000_000 + index as u64, 1.5), (1_700_000_010, 2.5)]),
                2 => Metric::rate(context, index as f64, Duration::from_secs(10)),
                3 => Metric::set(context, format!("member-{}", index % 5).as_str()),
                4 => Metric::distribution(context, [1.0, 2.0, index as f64].as_slice()),
                5 => Metric::histogram(context, [3.0, index as f64].as_slice()),
                6 => Metric::gauge(context, f64::NAN),
                _ => Metric::counter(context, [(1_700_000_000, f64::INFINITY), (1_700_000_001, 4.0)]),
            }
        })
        .collect()
}

/// Returns `metrics` in buffers of at most `per_buffer` events, addressed to `entry`.
fn buffers(metrics: &[Metric], per_buffer: usize, entry: Option<u16>) -> Vec<EventsBuffer> {
    metrics
        .chunks(per_buffer)
        .map(|chunk| buffer(entry, chunk.iter().cloned()))
        .collect()
}

/// A configuration of the differential, and whether its native encoder carries a routing policy.
struct DifferentialCase {
    name: &'static str,
    native: DatadogMetricsConfiguration,
    routed: bool,
}

fn differential_cases() -> Vec<DifferentialCase> {
    let configure = |edit: &dyn Fn(&mut SharedConfiguration)| {
        let mut shared = shared_configuration();
        shared.metrics_encoding.max_metrics_per_payload = 37;
        shared.metrics_encoding.max_series_points_per_payload = 45;
        // A flush timeout long enough that only the payload limits and the end of the input cut payloads.
        shared.metrics_encoding.flush_timeout = Duration::from_secs(3_600);
        edit(&mut shared);
        DatadogMetricsConfiguration::from_configuration(&shared)
    };
    let additional = HashMap::from([(
        "https://custom.example.com".to_string(),
        vec!["additional-api-key".to_string()],
    )]);

    vec![
        DifferentialCase {
            name: "V2 series and sketches",
            native: configure(&|shared| shared.metrics_encoding.v3_series_mode = V3SeriesMode::Disabled),
            routed: false,
        },
        DifferentialCase {
            name: "the default V3 series for a Datadog endpoint",
            native: configure(&|_| {}),
            routed: false,
        },
        DifferentialCase {
            name: "V1 series",
            native: configure(&|shared| shared.metrics_encoding.use_v2_series_api = false),
            routed: false,
        },
        DifferentialCase {
            name: "V3 series beside V2 series, and V3 sketches",
            native: configure(&|shared| {
                shared.metrics_encoding.v3_series_mode = V3SeriesMode::DatadogOnly;
                shared.metrics_encoding.v3_api.sketches.endpoints = vec!["https://app.datadoghq.com".to_string()];
                shared.metrics_encoding.v3_api.compression_level = 5;
                shared.endpoints.additional_endpoints = additional.clone();
            }),
            routed: false,
        },
        DifferentialCase {
            name: "V3 requested but turned off by a zlib compressor",
            native: configure(&|shared| {
                shared.metrics_encoding.v3_series_mode = V3SeriesMode::Enabled;
                shared.endpoints.compression.compressor_kind = "zlib".to_string();
            }),
            routed: false,
        },
        DifferentialCase {
            name: "the alternate metrics intake with V3 series",
            native: configure(&|shared| {
                shared.endpoints.opw_intake = AltMetricsIntake {
                    enabled: true,
                    url: "https://opw.example.com".to_string(),
                    use_v3_series: true,
                };
            }),
            routed: false,
        },
        DifferentialCase {
            name: "a routing group, with additional tags",
            native: configure(&|shared| shared.endpoints.additional_endpoints = additional.clone())
                .with_endpoint_routing(MetricsEndpointRouting::Only(
                    ["https://custom.example.com".to_string()].into(),
                ))
                .with_additional_tags(SharedTagSet::from(TagSet::from_iter([Tag::from("team:platform")]))),
            routed: true,
        },
        DifferentialCase {
            name: "the Cluster Agent's V2 series only",
            native: configure(&|shared| shared.metrics_encoding.v3_series_mode = V3SeriesMode::Enabled)
                .with_v2_series_only(),
            routed: false,
        },
    ]
}

#[tokio::test(start_paused = true)]
async fn one_lease_with_native_settings_builds_the_payloads_of_the_native_encoder() {
    let metrics = differential_metrics();
    let resolved = ResolvedRecipients {
        v2_series: slots(&[0, 2]),
        v2_sketches: slots(&[0, 2, 5]),
        v3_series: slots(&[1]),
        v3_sketches: slots(&[2]),
    };

    for case in differential_cases() {
        let native_encoder = case
            .native
            .build(build_context())
            .await
            .expect("the native encoder builds");
        let native = run_to_end(native_encoder, buffers(&metrics, 50, None)).await;

        let settings = case.native.encoder_settings().expect("the endpoints resolve");
        let mut host = MetricsEncodeHostConfiguration::new().with_lease(spec(7, HOST_ID, settings, resolved.clone()));
        if let Some(additional_tags) = case.native.additional_tags.clone() {
            host = host.with_additional_tags(additional_tags);
        }
        let host_encoder = host.build(build_context()).await.expect("the encode host builds");
        let hosted = run_to_end(host_encoder, buffers(&metrics, 50, Some(7))).await;

        assert!(native.len() > 4, "{}: the limits cut several payloads", case.name);
        assert_eq!(native.len(), hosted.len(), "{}: payload count", case.name);
        for (index, (native, hosted)) in native.into_iter().zip(hosted).enumerate() {
            let (native, native_recipients, native_routed) = delivered(native);
            let (hosted, hosted_recipients, hosted_routed) = delivered(hosted);
            assert_eq!(native, hosted, "{}: payload {}", case.name, index);

            // The routing value is the one difference: the native encoder names endpoints by its routing policy,
            // and a lease names the recipients it resolved for the payload's variant.
            assert_eq!((native_recipients, native_routed), (None, case.routed), "{}", case.name);
            let variant = match (hosted.uri.as_str(), hosted.info) {
                (_, Some(info)) => info,
                (METRICS_SKETCHES_PATH, None) => MetricsPayloadInfo::v2_sketches(),
                (_, None) => MetricsPayloadInfo::v2_series(),
            };
            assert_eq!(
                hosted_recipients.as_ref(),
                Some(resolved.for_payload(variant)),
                "{}",
                case.name
            );
            assert!(!hosted_routed, "{}", case.name);
        }
    }
}

#[test]
fn native_settings_name_the_variants_the_native_encoder_produces() {
    let mut shared = shared_configuration();
    let datadog_only = DatadogMetricsConfiguration::from_configuration(&shared)
        .encoder_settings()
        .unwrap();
    assert_eq!(
        datadog_only.variants,
        ProtocolVariants {
            v2_series: false,
            v2_sketches: true,
            v3_series: true,
            v3_sketches: false,
        },
        "the default sends V3 series to a Datadog endpoint"
    );

    shared.metrics_encoding.v3_series_mode = V3SeriesMode::Disabled;
    let v2_only = DatadogMetricsConfiguration::from_configuration(&shared)
        .encoder_settings()
        .unwrap();
    assert_eq!(
        v2_only.variants,
        ProtocolVariants {
            v2_series: true,
            v2_sketches: true,
            v3_series: false,
            v3_sketches: false,
        }
    );
    assert_eq!(&*v2_only.compressor_kind, "zstd");
    assert_eq!(v2_only.series, crate::common::datadog::delivery::SeriesVariant::V2);

    shared.metrics_encoding.v3_series_mode = V3SeriesMode::Enabled;
    shared.metrics_encoding.v3_api.sketches.endpoints = vec!["https://app.datadoghq.com".to_string()];
    let v3 = DatadogMetricsConfiguration::from_configuration(&shared)
        .encoder_settings()
        .unwrap();
    assert_eq!(
        v3.variants,
        ProtocolVariants {
            v2_series: false,
            v2_sketches: true,
            v3_series: true,
            v3_sketches: true,
        }
    );

    shared.endpoints.compression.compressor_kind = "zlib".to_string();
    let zlib = DatadogMetricsConfiguration::from_configuration(&shared)
        .encoder_settings()
        .unwrap();
    assert_eq!(zlib.variants, v2_only.variants, "zlib turns V3 off");
}

#[test]
fn a_built_lease_reports_its_shards_and_nothing_else() {
    let lease = EncodeLeaseSpec {
        builders: NonZeroU8::new(2).unwrap(),
        ..spec(1, HOST_ID, settings(), recipients(&[0]))
    };
    let built = MetricsEncodeHostConfiguration::new()
        .lease_factory()
        .build(&lease)
        .unwrap();
    assert_eq!(format!("{:?}", built), "BuiltLease { shards: 2, .. }");
    assert!(matches!(
        &built.shards[..],
        [BuiltShard::Builders(_), BuiltShard::Builders(_)]
    ));
}

#[test]
fn a_stream_builds_a_v2_builder_only_for_the_variants_its_settings_name() {
    // A lease whose recipients take no sketches builds no sketch builder, where the native encoder always builds one:
    // its sketches are dropped and counted.
    let settings = EncoderSettings {
        variants: ProtocolVariants {
            v2_series: true,
            v2_sketches: false,
            v3_series: false,
            v3_sketches: false,
        },
        ..settings()
    };
    let builders = StreamBuilders::build(&settings, None, &MetricsBuilder::default()).unwrap();
    assert!(builders.v2_series_builder.is_some());
    assert!(builders.v2_sketch_builder.is_none());
    assert!(!builders.sketches_mode.needs_v3());
}
