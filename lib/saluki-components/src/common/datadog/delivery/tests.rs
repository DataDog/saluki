//! Tests of the delivery host's routing loop, driven through its control mailbox and its input, with test doubles of
//! the recipients' I/O loops.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use agent_data_plane_config::{Live, SalukiConfiguration};
use bytes::Bytes;
use futures::FutureExt as _;
use http::Request;
use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_core::{
    components::{forwarders::ForwarderBuilder, test_util::TestComponentSupervisor, ComponentContext},
    data_model::payload::{HttpPayload, Payload, PayloadMetadata},
    execution::{
        control::{control_mailbox, ControlError, ControlSender},
        GenerationId,
    },
    health::{Health, HealthRegistry},
    topology::interconnect::Consumer,
};
use saluki_metrics::test::TestRecorder;
use stringtheory::MetaString;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::timeout,
};

use super::{
    host::{Body, BootPublication, DeliveryHostStatus, HostKind, HostShared, RecipientState},
    router::{LoopSpawner, Router},
    CloseRequest, CloseRequests, Creator, CredentialSource, DeliveryControl, DeliveryEvent, DeliverySettings,
    DetachedLoop, DrainReport, IntakeAuthorities, PayloadClasses, PayloadRecipients, PreparedRecipient, RecipientExit,
    RecipientHandle, RecipientOrigin, RecipientPieces, RecipientProtocol, RecipientRef, RecipientSettings,
    RecipientSlot, RecipientSpec, ReopenReply, V3Acceptance, MAX_DRAINING_RECIPIENTS,
};
use crate::common::datadog::{
    config::ForwarderConfiguration,
    endpoints::{MetricsRecipient, MetricsRecipientRoute, ResolvedEndpoint},
    io::PublishedSettings,
    protocol::{MetricsEndpointRouting, MetricsPayloadInfo},
    test_util::{shared_configuration, LiveConfiguration},
    transaction::{Transaction, TransactionBody},
};

/// How long a test waits for something the host does before it fails.
const WAIT: Duration = Duration::from_secs(5);

const PRIMARY: &str = "http://primary.example.test";
const OVERRIDE: &str = "http://override.example.test";
const ADDITIONAL: &str = "http://additional.example.test";
const TEAM: &str = "http://team.example.test";

const TRANSACTION: Creator = Creator::Transaction(10);
const GENERATION: Creator = Creator::Generation(GenerationId::new(1));

// Credentials.

/// Returns a value with the `'static` lifetime equal to `value`, so that a configuration projection can return one.
///
/// Each distinct value is leaked once: tests use a handful.
fn interned(value: Option<&String>) -> &'static Option<MetaString> {
    static VALUES: LazyLock<Mutex<HashMap<Option<String>, &'static Option<MetaString>>>> =
        LazyLock::new(Mutex::default);
    let mut values = VALUES.lock().unwrap();
    values
        .entry(value.cloned())
        .or_insert_with(|| Box::leak(Box::new(value.map(|value| MetaString::from(value.as_str())))))
}

/// A credential source whose values a test sets, and which reports each change to the views it gave out.
///
/// The values live in a configuration map the views project, as an application's credentials live in its
/// configuration.
struct TestCredentials {
    live: LiveConfiguration,
    config: Mutex<SalukiConfiguration>,
}

impl TestCredentials {
    fn new(values: &[(&str, &str)]) -> Arc<Self> {
        let mut config = SalukiConfiguration::default();
        for (reference, value) in values {
            config
                .shared
                .endpoints
                .additional_endpoints
                .insert(reference.to_string(), vec![value.to_string()]);
        }
        Arc::new(Self {
            live: LiveConfiguration::new(config.clone()),
            config: Mutex::new(config),
        })
    }

    /// Sets the value of `reference`, or removes it.
    fn set(&self, reference: &str, value: Option<&str>) {
        let mut config = self.config.lock().unwrap();
        let credentials = &mut config.shared.endpoints.additional_endpoints;
        match value {
            Some(value) => credentials.insert(reference.to_string(), vec![value.to_string()]),
            None => credentials.remove(reference),
        };
        self.live.store(config.clone());
    }
}

impl CredentialSource for TestCredentials {
    fn resolve(&self, reference: &MetaString) -> Option<Live<Option<MetaString>>> {
        let reference = reference.to_string();
        if !reference.starts_with("native:") && !reference.starts_with("credentials/") {
            return None;
        }
        Some(self.live.live(move |config| {
            interned(
                config
                    .shared
                    .endpoints
                    .additional_endpoints
                    .get(&reference)
                    .and_then(|values| values.first()),
            )
        }))
    }
}

// Recipient loops.

/// One loop the host spawned: the ends a real loop would hold.
struct FakeLoop {
    spec: RecipientSpec,
    transactions: mpsc::Receiver<Transaction<Body>>,
    close: CloseRequests,
    exit: Option<oneshot::Sender<RecipientExit>>,
    pieces: RecipientPieces<Body>,

    /// The settings the host published to the loop.
    settings: PublishedSettings,
}

impl FakeLoop {
    async fn receive(&mut self) -> Transaction<Body> {
        timeout(WAIT, self.transactions.recv())
            .await
            .unwrap_or_else(|_| panic!("recipient '{}' received nothing", self.spec.id))
            .expect("the host still sends to the loop")
    }

    fn received_nothing(&mut self) -> bool {
        self.transactions.try_recv().is_err()
    }

    fn close_request(&self) -> CloseRequest {
        self.close.current()
    }

    async fn wait_for_close(&mut self, wanted: impl Fn(CloseRequest) -> bool) {
        timeout(WAIT, self.close.wait_for(wanted))
            .await
            .expect("the close request should change");
    }

    fn exit(&mut self, outcome: RecipientExit) {
        let _ = self.exit.take().expect("a loop exits once").send(outcome);
    }

    /// Returns the request the loop would send for `request`, mapped by its request mapper as at dispatch.
    fn map(&self, request: Request<TransactionBody<Body>>) -> Request<TransactionBody<Body>> {
        let mut mapper = (self.pieces.request_mapper)(self.pieces.endpoint.clone());
        mapper(request)
    }
}

/// Spawns test doubles of recipients' loops, each with a transaction channel of `capacity`.
struct FakeLoops {
    capacity: usize,
    spawned: Mutex<Vec<FakeLoop>>,
}

impl FakeLoops {
    /// Takes the loop spawned last for the recipient `id`.
    fn take(&self, id: &str) -> FakeLoop {
        let mut spawned = self.spawned.lock().unwrap();
        let index = spawned
            .iter()
            .rposition(|spawned| spawned.spec.id == id)
            .unwrap_or_else(|| panic!("no loop was spawned for recipient '{id}'"));
        spawned.remove(index)
    }

    fn count(&self) -> usize {
        self.spawned.lock().unwrap().len()
    }
}

impl LoopSpawner for FakeLoops {
    fn spawn(&self, spec: &RecipientSpec, pieces: RecipientPieces<Body>) -> RecipientHandle<Body> {
        let (handle, detached, settings) = RecipientHandle::detached_with_settings(spec, &pieces, self.capacity);
        let DetachedLoop {
            transactions,
            close,
            exit,
        } = detached;
        self.spawned.lock().unwrap().push(FakeLoop {
            spec: spec.clone(),
            transactions,
            close,
            exit: Some(exit),
            pieces,
            settings,
        });
        handle
    }
}

// Specifications.

fn intake(v3: V3Acceptance) -> RecipientProtocol {
    RecipientProtocol::DatadogIntake {
        authorities: IntakeAuthorities::default(),
        allow_arbitrary_tags: false,
        v3,
    }
}

/// Returns a native recipient of `dd_out`, sending with `native:<id>`.
fn native(id: &str, route: MetricsRecipientRoute, endpoint: &str) -> RecipientSpec {
    let classes = match route {
        MetricsRecipientRoute::MetricsOverride => PayloadClasses::MetricsOnly,
        MetricsRecipientRoute::Primary | MetricsRecipientRoute::Additional => PayloadClasses::All,
    };
    let protocol = intake(V3Acceptance::default());
    RecipientSpec {
        id: format!("dd_out/{}/{}", route.as_str(), id).into(),
        endpoint: endpoint.to_string(),
        credential: format!("native:{id}").into(),
        classes,
        protocol: protocol.clone(),
        policy: Default::default(),
        origin: RecipientOrigin::Native(MetricsRecipient {
            route,
            configured_endpoint: endpoint.to_string(),
            api_key_index: (route == MetricsRecipientRoute::Additional).then_some(0),
            classes,
            protocol,
        }),
    }
}

/// Returns a recipient a resources artifact declares, sending with `credentials/<credential>`.
fn declared(name: &str, endpoint: &str, credential: &str) -> RecipientSpec {
    RecipientSpec {
        id: format!("dd_out/declared/{credential}/{endpoint}/{name}").into(),
        endpoint: endpoint.to_string(),
        credential: format!("credentials/{credential}").into(),
        classes: PayloadClasses::MetricsOnly,
        protocol: intake(V3Acceptance::default()),
        policy: Default::default(),
        origin: RecipientOrigin::Declared {
            name: name.into(),
            credential: credential.into(),
        },
    }
}

fn recipient(slot: u16, spec: &RecipientSpec, incarnation: u64) -> RecipientRef {
    RecipientRef {
        slot: RecipientSlot::new(slot),
        id: spec.id.clone(),
        incarnation,
    }
}

fn settings_of(recipient: &RecipientRef, spec: &RecipientSpec) -> RecipientSettings {
    RecipientSettings {
        recipient: recipient.clone(),
        spec: spec.clone(),
    }
}

// Payloads.

const SERIES: &str = "/api/v2/series";
const LOGS: &str = "/api/v2/logs";

fn payload(path: &str, slots: Option<&[u16]>, info: Option<MetricsPayloadInfo>, body: &'static str) -> Payload {
    let mut metadata = PayloadMetadata::from_event_and_data_point_count(1, 1);
    if let Some(slots) = slots {
        metadata = metadata.with(PayloadRecipients::new(
            slots.iter().copied().map(RecipientSlot::new).collect(),
        ));
    }
    if let Some(info) = info {
        metadata = metadata.with(info);
    }
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .body(FrozenChunkedBytesBuffer::from(Bytes::from_static(body.as_bytes())))
        .expect("request should build");
    Payload::Http(HttpPayload::new(metadata, request))
}

/// Returns a payload that names its recipients.
fn named(slots: &[u16], body: &'static str) -> Payload {
    payload(SERIES, Some(slots), Some(MetricsPayloadInfo::v2_series()), body)
}

fn body_of(transaction: Transaction<Body>) -> String {
    use bytes::Buf as _;

    let (_, request) = transaction.into_parts();
    let mut body = request.into_body();
    String::from_utf8(body.copy_to_bytes(body.remaining()).to_vec()).expect("bodies are text")
}

// The host.

/// A delivery host's routing loop, running with test doubles of its recipients' loops.
struct Harness {
    supervisor: TestComponentSupervisor,
    shared: Arc<HostShared>,
    settings: Arc<DeliverySettings>,
    loops: Arc<FakeLoops>,
    credentials: Arc<TestCredentials>,
    control: ControlSender<DeliveryControl<Body>>,
    events: mpsc::Receiver<DeliveryEvent>,
    input: Option<mpsc::Sender<Payload>>,
    status: watch::Receiver<DeliveryHostStatus>,
    task: Option<JoinHandle<Router>>,
}

struct Options {
    loop_capacity: usize,
    event_capacity: usize,
    credentials: Vec<(&'static str, &'static str)>,
    boot: Vec<(RecipientRef, RecipientSpec)>,
    native_set: Vec<RecipientRef>,
    recorder: Option<Arc<TestRecorder>>,
    health: Option<Health>,
    settings: Option<DeliverySettings>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            loop_capacity: 8,
            event_capacity: 64,
            credentials: Vec::new(),
            boot: Vec::new(),
            native_set: Vec::new(),
            recorder: None,
            health: None,
            settings: None,
        }
    }
}

fn base_configuration() -> ForwarderConfiguration {
    let mut shared = shared_configuration();
    // No root certificate store is needed for the plain HTTP endpoints of these tests.
    shared.endpoints.tls.skip_ssl_validation = true;
    ForwarderConfiguration::from_configuration(&shared)
}

impl Harness {
    async fn start(options: Options) -> Self {
        let _ = saluki_tls::initialize_default_crypto_provider();
        let supervisor = TestComponentSupervisor::start("dd_out").await;
        let mut credentials = vec![
            ("native:primary", "primary-key"),
            ("native:override", "primary-key"),
            ("native:additional", "additional-key"),
        ];
        credentials.extend(options.credentials.iter().copied());
        let credentials = TestCredentials::new(&credentials);
        let base = base_configuration();
        let settings = Arc::new(options.settings.clone().unwrap_or_else(|| base.delivery_settings()));
        let loops = Arc::new(FakeLoops {
            capacity: options.loop_capacity,
            spawned: Mutex::default(),
        });
        let source: Arc<dyn CredentialSource> = credentials.clone();
        let build = || {
            supervisor.dataspace().with_current(|| {
                HostShared::build(
                    ComponentContext::test_forwarder("dd_out"),
                    base,
                    HostKind {
                        endpoint_name: Arc::new(|_| None),
                        secrets: None,
                    },
                    &source,
                    &settings,
                )
            })
        };
        // The forwarder's telemetry is registered when the host's shared state is built.
        let (shared, _) = match &options.recorder {
            Some(recorder) => metrics::with_local_recorder(&**recorder, build),
            None => build(),
        }
        .expect("the host should build");
        let shared = Arc::new(shared.with_loops(loops.clone() as Arc<dyn LoopSpawner>));

        let boot = options
            .boot
            .iter()
            .map(|(recipient, spec)| {
                let pieces = shared.build_pieces(spec, &settings).expect("boot recipients build");
                (settings_of(recipient, spec), pieces)
            })
            .collect::<Vec<_>>();
        let publication = BootPublication {
            revision: 1,
            settings: Arc::clone(&settings),
            recipients: boot.iter().map(|(settings, _)| settings.clone()).collect(),
            native_set: options.native_set,
            withdrawn: BTreeSet::new(),
        };
        let (control, mailbox) = control_mailbox();
        let (events_tx, events) = mpsc::channel(options.event_capacity);
        let (input_tx, input_rx) = mpsc::channel(64);
        let (status_tx, status) = watch::channel(DeliveryHostStatus::default());
        let new_router = || {
            Router::new(
                Arc::clone(&shared),
                Some(events_tx),
                Duration::from_secs(30),
                status_tx,
                Arc::clone(&settings),
            )
        };
        // The host's telemetry is registered when its routing loop is created.
        let mut router = match &options.recorder {
            Some(recorder) => metrics::with_local_recorder(&**recorder, new_router),
            None => new_router(),
        };
        let context = ComponentContext::test_forwarder("dd_out");
        let mut health = options.health;
        let task = tokio::spawn(supervisor.scope(async move {
            let mut input = Consumer::new(context, input_rx);
            router.start(boot, publication);
            router
                .run(&mut input, mailbox.into_receiver_for("delivery host"), health.as_mut())
                .await;
            router
        }));
        assert!(timeout(WAIT, control.running()).await.expect("the host should start"));

        Self {
            supervisor,
            shared,
            settings,
            loops,
            credentials,
            control,
            events,
            input: Some(input_tx),
            status,
            task: Some(task),
        }
    }

    async fn send_control(&self, message: DeliveryControl<Body>) -> Result<(), ControlError> {
        timeout(WAIT, self.control.send(message))
            .await
            .expect("the host should acknowledge")
    }

    async fn prepare(
        &self, creator: Creator, recipients: &[(RecipientRef, RecipientSpec)], requires: &[RecipientRef],
    ) -> Result<(), ControlError> {
        let recipients = recipients
            .iter()
            .map(|(recipient, spec)| PreparedRecipient {
                recipient: recipient.clone(),
                spec: spec.clone(),
                pieces: self
                    .shared
                    .build_pieces(spec, &self.settings)
                    .expect("the recipient's pieces build"),
            })
            .collect();
        self.send_control(DeliveryControl::Prepare {
            creator,
            recipients,
            requires: requires.to_vec(),
        })
        .await
    }

    async fn publish(
        &self, revision: u64, native_set: &[RecipientRef], recipients: &[(RecipientRef, RecipientSpec)],
        withdrawn: &[&RecipientSpec],
    ) -> Result<(), ControlError> {
        self.publish_with(revision, native_set, recipients, withdrawn, Arc::clone(&self.settings))
            .await
    }

    async fn publish_with(
        &self, revision: u64, native_set: &[RecipientRef], recipients: &[(RecipientRef, RecipientSpec)],
        withdrawn: &[&RecipientSpec], settings: Arc<DeliverySettings>,
    ) -> Result<(), ControlError> {
        self.send_control(DeliveryControl::Publish {
            revision,
            native_set: native_set.to_vec(),
            settings,
            recipients: recipients
                .iter()
                .map(|(recipient, spec)| settings_of(recipient, spec))
                .collect(),
            withdrawn: withdrawn.iter().map(|spec| spec.id.clone()).collect(),
        })
        .await
    }

    async fn release(&self, recipients: &[RecipientRef]) {
        self.send_control(DeliveryControl::Release {
            recipients: recipients.to_vec(),
        })
        .await
        .expect("a release is always accepted");
    }

    async fn reopen(
        &self, creator: Creator, recipients: &[(RecipientRef, RecipientSpec)],
    ) -> Vec<(RecipientRef, ReopenReply)> {
        let (reply, answers) = oneshot::channel();
        self.send_control(DeliveryControl::Reopen {
            creator,
            recipients: recipients
                .iter()
                .map(|(recipient, spec)| settings_of(recipient, spec))
                .collect(),
            reply,
        })
        .await
        .expect("a reopen is always accepted");
        answers.await.expect("the host answers each reopen")
    }

    async fn abort(&self, creator: Creator) {
        let message = match creator {
            Creator::Generation(generation) => DeliveryControl::Abort { generation },
            Creator::Transaction(revision) => DeliveryControl::AbortResources { revision },
        };
        self.send_control(message).await.expect("an abort is always accepted");
    }

    async fn send(&self, payload: Payload) {
        self.input
            .as_ref()
            .expect("the input is open")
            .send(payload)
            .await
            .expect("the host reads its input");
    }

    async fn event(&mut self) -> DeliveryEvent {
        timeout(WAIT, self.events.recv())
            .await
            .expect("the host should report an event")
            .expect("the host holds the event channel")
    }

    fn no_event(&mut self) -> bool {
        self.events.try_recv().is_err()
    }

    async fn wait_for_status(&mut self, wanted: impl Fn(&DeliveryHostStatus) -> bool) -> DeliveryHostStatus {
        timeout(WAIT, self.status.wait_for(|status| wanted(status)))
            .await
            .expect("the host's status should change")
            .expect("the host holds its status")
            .clone()
    }

    fn state(&self, recipient: &RecipientRef) -> Option<RecipientState> {
        self.status
            .borrow()
            .recipients
            .iter()
            .find(|status| status.recipient == *recipient)
            .map(|status| status.state)
    }

    /// Closes the host's input and returns its routing loop once it stopped.
    async fn stop(mut self) -> Router {
        self.input = None;
        let router = timeout(WAIT, self.task.take().expect("the host runs"))
            .await
            .expect("the host should stop")
            .expect("the host should not panic");
        drop(self.supervisor);
        router
    }
}

fn retired() -> RecipientExit {
    RecipientExit::Retired(DrainReport::default())
}

fn exited(recipient: &RecipientRef, outcome: RecipientExit) -> DeliveryEvent {
    DeliveryEvent::RecipientExited {
        recipient: recipient.clone(),
        outcome,
    }
}

fn rejection(result: Result<(), ControlError>) -> String {
    match result {
        Err(ControlError::Rejected { error, .. }) => format!("{error:#}"),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

// Tests.

#[tokio::test]
async fn a_payload_that_names_recipients_goes_to_each_in_order_and_a_released_recipient_retires() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let (a, b) = (recipient(0, &primary, 1), recipient(3, &additional, 2));
    let mut host = Harness::start(Options::default()).await;
    host.prepare(
        TRANSACTION,
        &[(a.clone(), primary.clone()), (b.clone(), additional.clone())],
        &[],
    )
    .await
    .expect("the recipients prepare");
    let mut loop_a = host.loops.take(&primary.id);
    let mut loop_b = host.loops.take(&additional.id);

    host.send(named(&[3, 0], "first")).await;
    assert_eq!(body_of(loop_b.receive().await), "first");
    assert_eq!(body_of(loop_a.receive().await), "first");

    // A release is acknowledged at once; the recipient drains, then retires when its loop exits.
    host.release(std::slice::from_ref(&a)).await;
    loop_a
        .wait_for_close(|request| matches!(request, CloseRequest::Retire { .. }))
        .await;
    assert_eq!(host.state(&a), Some(RecipientState::Draining));
    assert!(host.no_event(), "a drain reports nothing until the loop exits");
    loop_a.exit(retired());
    assert_eq!(host.event().await, exited(&a, retired()));
    host.wait_for_status(|status| status.recipients.len() == 1).await;
    assert_eq!(host.state(&b), Some(RecipientState::Live));

    // The other recipient still receives, and the freed slot can hold another recipient.
    host.send(named(&[3], "second")).await;
    assert_eq!(body_of(loop_b.receive().await), "second");
    let team = declared("team_d", TEAM, "team");
    let c = recipient(0, &team, 3);
    host.credentials.set("credentials/team", Some("team-key"));
    host.prepare(GENERATION, &[(c.clone(), team.clone())], &[])
        .await
        .expect("a freed slot takes another recipient");
    host.stop().await;
}

#[tokio::test]
async fn messages_with_a_stale_reference_are_ignored_and_a_repeated_prepare_is_done() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let current = recipient(0, &primary, 2);
    let stale = recipient(0, &primary, 1);
    let mut host = Harness::start(Options::default()).await;
    host.prepare(TRANSACTION, &[(current.clone(), primary.clone())], &[])
        .await
        .expect("the recipient prepares");

    // The same reference again, by the same or another preparation, spawns nothing.
    host.prepare(TRANSACTION, &[(current.clone(), primary.clone())], &[])
        .await
        .expect("a repeated prepare is done");
    host.prepare(GENERATION, &[(current.clone(), primary.clone())], &[])
        .await
        .expect("another preparation of the held reference is done");
    assert_eq!(host.loops.count(), 1);

    // Another recipient, or an older incarnation, cannot take the slot.
    let error = rejection(
        host.prepare(TRANSACTION, &[(recipient(0, &additional, 3), additional.clone())], &[])
            .await,
    );
    assert!(error.contains("Slot 0 holds recipient"), "{error}");
    let error = rejection(
        host.prepare(TRANSACTION, &[(stale.clone(), primary.clone())], &[])
            .await,
    );
    assert!(error.contains("Slot 0 holds recipient"), "{error}");

    // A release and a reopen of the older incarnation change nothing.
    host.release(std::slice::from_ref(&stale)).await;
    assert_eq!(
        host.reopen(GENERATION, &[(stale.clone(), primary.clone())]).await,
        vec![(stale.clone(), ReopenReply::Exited)]
    );
    let mut primary_loop = host.loops.take(&primary.id);
    assert_eq!(primary_loop.close_request(), CloseRequest::None);
    assert_eq!(host.state(&current), Some(RecipientState::Live));
    host.send(named(&[0], "still live")).await;
    assert_eq!(body_of(primary_loop.receive().await), "still live");

    // An abort of one of two preparations that created it keeps it.
    host.abort(GENERATION).await;
    host.send(named(&[0], "kept")).await;
    assert_eq!(body_of(primary_loop.receive().await), "kept");
    assert!(host.no_event());
    host.stop().await;
}

#[tokio::test]
async fn a_preparation_fails_when_a_recipient_it_requires_is_not_live() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let (a, b) = (recipient(0, &primary, 1), recipient(1, &additional, 2));
    let host = Harness::start(Options::default()).await;

    let error = rejection(
        host.prepare(GENERATION, &[(b.clone(), additional.clone())], std::slice::from_ref(&a))
            .await,
    );
    assert!(error.contains("is required by generation 1"), "{error}");
    assert!(error.contains("is not in the host's table"), "{error}");
    assert_eq!(host.loops.count(), 0, "a failed preparation adds nothing");

    host.prepare(TRANSACTION, &[(a.clone(), primary.clone())], &[])
        .await
        .expect("the required recipient prepares");
    host.prepare(GENERATION, &[(b.clone(), additional.clone())], std::slice::from_ref(&a))
        .await
        .expect("a preparation over a live recipient succeeds");

    host.release(std::slice::from_ref(&a)).await;
    let error = rejection(host.prepare(GENERATION, &[], std::slice::from_ref(&a)).await);
    assert!(
        error.contains("is draining") || error.contains("is released"),
        "{error}"
    );
    host.stop().await;
}

#[tokio::test]
async fn payloads_without_recipients_follow_the_native_set_by_class_route_routing_and_variant() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let mut additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    // The additional endpoint takes V3 series, so no V2 series.
    additional.protocol = intake(V3Acceptance {
        series: true,
        sketches: false,
    });
    let team = declared("team_d", TEAM, "team");
    let (p, x, t) = (
        recipient(0, &primary, 1),
        recipient(1, &additional, 1),
        recipient(2, &team, 1),
    );
    let options = Options {
        credentials: vec![("credentials/team", "team-key")],
        boot: vec![
            (p.clone(), primary.clone()),
            (x.clone(), additional.clone()),
            (t.clone(), team.clone()),
        ],
        native_set: vec![p.clone(), x.clone(), t.clone()],
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let mut primary_loop = host.loops.take(&primary.id);
    let mut additional_loop = host.loops.take(&additional.id);
    let mut team_loop = host.loops.take(&team.id);

    // Logs go to every recipient of every class; a declared recipient never gets a payload that does not name it.
    host.send(payload(LOGS, None, None, "logs")).await;
    assert_eq!(body_of(primary_loop.receive().await), "logs");
    assert_eq!(body_of(additional_loop.receive().await), "logs");

    // V2 series skip the recipient that takes V3 series; V3 series reach only it.
    host.send(payload(SERIES, None, Some(MetricsPayloadInfo::v2_series()), "v2"))
        .await;
    assert_eq!(body_of(primary_loop.receive().await), "v2");
    host.send(payload(SERIES, None, Some(MetricsPayloadInfo::v3_series()), "v3"))
        .await;
    assert_eq!(body_of(additional_loop.receive().await), "v3");
    assert!(primary_loop.received_nothing());

    // A legacy endpoint routing selects by policy identity.
    let routed = PayloadMetadata::from_event_and_data_point_count(1, 1).with(Arc::new(
        MetricsEndpointRouting::AllExcept([PRIMARY.to_string()].into()),
    ));
    let request = Request::builder()
        .method("POST")
        .uri(LOGS)
        .body(FrozenChunkedBytesBuffer::from(Bytes::from_static(b"routed")))
        .unwrap();
    host.send(Payload::Http(HttpPayload::new(routed, request))).await;
    assert_eq!(body_of(additional_loop.receive().await), "routed");

    // The routing loop sends one payload at a time, so the sentinel orders every check above it.
    host.send(named(&[2], "sentinel")).await;
    assert_eq!(body_of(team_loop.receive().await), "sentinel");
    assert!(primary_loop.received_nothing());
    assert!(additional_loop.received_nothing());
    host.stop().await;
}

#[tokio::test]
async fn metrics_go_to_the_alternate_intake_in_place_of_the_primary_and_other_payloads_never_do() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let alternate = native("override", MetricsRecipientRoute::MetricsOverride, OVERRIDE);
    let (p, o) = (recipient(0, &primary, 1), recipient(1, &alternate, 1));
    let options = Options {
        boot: vec![(p.clone(), primary.clone()), (o.clone(), alternate.clone())],
        native_set: vec![p.clone(), o.clone()],
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let mut primary_loop = host.loops.take(&primary.id);
    let mut alternate_loop = host.loops.take(&alternate.id);

    host.send(payload(SERIES, None, Some(MetricsPayloadInfo::v2_series()), "metrics"))
        .await;
    assert_eq!(body_of(alternate_loop.receive().await), "metrics");
    host.send(payload(LOGS, None, None, "logs")).await;
    assert_eq!(body_of(primary_loop.receive().await), "logs");
    host.send(payload(LOGS, None, None, "more logs")).await;
    assert_eq!(body_of(primary_loop.receive().await), "more logs");
    assert!(alternate_loop.received_nothing());
    host.stop().await;
}

#[tokio::test]
async fn a_payload_that_names_its_recipients_is_not_filtered_by_their_protocol() {
    let mut additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    additional.protocol = intake(V3Acceptance {
        series: true,
        sketches: true,
    });
    let x = recipient(4, &additional, 1);
    let host = Harness::start(Options::default()).await;
    host.prepare(GENERATION, &[(x.clone(), additional.clone())], &[])
        .await
        .expect("the recipient prepares");
    let mut additional_loop = host.loops.take(&additional.id);

    // A lease resolved the recipient for this V2 payload when it still took V2 series.
    host.send(named(&[4], "resolved")).await;
    assert_eq!(body_of(additional_loop.receive().await), "resolved");
    host.stop().await;
}

#[tokio::test]
async fn a_withdrawn_recipient_gets_nothing_whatever_names_it_until_a_publication_restores_it() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let (p, x) = (recipient(0, &primary, 1), recipient(1, &additional, 1));
    let recorder = Arc::new(TestRecorder::default());
    let options = Options {
        boot: vec![(p.clone(), primary.clone()), (x.clone(), additional.clone())],
        native_set: vec![p.clone(), x.clone()],
        recorder: Some(Arc::clone(&recorder)),
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let mut primary_loop = host.loops.take(&primary.id);
    let mut additional_loop = host.loops.take(&additional.id);

    host.publish(2, std::slice::from_ref(&p), &[], &[&additional])
        .await
        .expect("the publication applies");
    // Neither a stamped payload nor one routed over the native set reaches it.
    host.send(named(&[1, 0], "stamped")).await;
    host.send(payload(LOGS, None, None, "logs")).await;
    assert_eq!(body_of(primary_loop.receive().await), "stamped");
    assert_eq!(body_of(primary_loop.receive().await), "logs");
    assert!(additional_loop.received_nothing());
    let withdrawn = recorder
        .counters()
        .into_iter()
        .find(|(key, _)| key.name() == "plan_recipient_withdrawn_payloads_total")
        .map(|(_, value)| value);
    assert_eq!(withdrawn, Some(1));
    assert_eq!(host.status.borrow().withdrawn, vec![additional.id.clone()]);

    // A later publication that no longer withdraws it restores it.
    host.publish(3, &[p.clone(), x.clone()], &[], &[])
        .await
        .expect("the publication applies");
    host.send(named(&[1], "restored")).await;
    assert_eq!(body_of(additional_loop.receive().await), "restored");

    // An older publication changes nothing.
    host.publish(2, std::slice::from_ref(&p), &[], &[&additional])
        .await
        .expect("an older publication is ignored");
    host.send(named(&[1], "still restored")).await;
    assert_eq!(body_of(additional_loop.receive().await), "still restored");
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_input_barrier_routes_every_payload_received_before_a_release_even_with_a_send_pending() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let (p, x) = (recipient(0, &primary, 1), recipient(1, &additional, 1));
    let options = Options {
        loop_capacity: 1,
        boot: vec![(p.clone(), primary.clone()), (x.clone(), additional.clone())],
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let mut primary_loop = host.loops.take(&primary.id);
    let mut additional_loop = host.loops.take(&additional.id);

    // The primary's loop takes one transaction and then has no room: the second payload's send to it waits, and the
    // third and fourth wait in the host's input. Each send of the last one waits too, after it was received.
    for body in ["one", "two", "three", "four"] {
        host.send(named(&[0], body)).await;
    }
    while host.input.as_ref().unwrap().capacity() < 62 {
        tokio::task::yield_now().await;
    }

    // The lease's drain was acknowledged: the release orders after every payload sent before it.
    host.release(std::slice::from_ref(&p)).await;
    assert_eq!(primary_loop.close_request(), CloseRequest::None);
    assert_eq!(host.state(&p), Some(RecipientState::Releasing));
    host.send(named(&[1], "after")).await;

    for body in ["one", "two", "three", "four"] {
        assert_eq!(body_of(primary_loop.receive().await), body);
    }
    primary_loop
        .wait_for_close(|request| matches!(request, CloseRequest::Retire { .. }))
        .await;
    assert_eq!(body_of(additional_loop.receive().await), "after");
    assert!(primary_loop.received_nothing());
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_is_served_while_a_send_waits_for_a_full_recipient_and_withdrawal_ends_the_wait() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let (p, x) = (recipient(0, &primary, 1), recipient(1, &additional, 1));
    let options = Options {
        loop_capacity: 1,
        boot: vec![(p.clone(), primary.clone())],
        native_set: vec![p.clone()],
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let mut primary_loop = host.loops.take(&primary.id);

    // The primary's loop is stuck: the second payload waits for room, and nothing reads it.
    host.send(named(&[0], "one")).await;
    host.send(payload(LOGS, None, None, "two")).await;
    host.send(named(&[0], "three")).await;
    while host.input.as_ref().unwrap().capacity() < 63 {
        tokio::task::yield_now().await;
    }

    // Prepares and publications are answered meanwhile.
    host.prepare(TRANSACTION, &[(x.clone(), additional.clone())], &[])
        .await
        .expect("a prepare is served while a send waits");
    let mut additional_loop = host.loops.take(&additional.id);
    host.publish(2, &[p.clone(), x.clone()], &[], &[])
        .await
        .expect("a publication is served while a send waits");

    // Withdrawing the stuck recipient ends the wait: the waiting payload and the next one are dropped for it, and
    // routing continues.
    host.publish(3, std::slice::from_ref(&x), &[], &[&primary])
        .await
        .expect("the withdrawal applies");
    host.send(named(&[1], "four")).await;
    host.send(payload(LOGS, None, None, "logs")).await;
    assert_eq!(body_of(additional_loop.receive().await), "four");
    assert_eq!(body_of(additional_loop.receive().await), "logs");
    assert_eq!(body_of(primary_loop.receive().await), "one");
    assert!(primary_loop.received_nothing());
    host.stop().await;
}

#[tokio::test]
async fn a_reopen_cancels_a_release_at_the_barrier_or_a_drain_and_a_loop_that_exited_is_answered_exited() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let p = recipient(0, &primary, 1);
    let options = Options {
        loop_capacity: 1,
        boot: vec![(p.clone(), primary.clone())],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let mut primary_loop = host.loops.take(&primary.id);

    // A send waits for room, so the release waits at the barrier, where the reopen cancels it.
    host.send(named(&[0], "one")).await;
    host.send(named(&[0], "two")).await;
    host.release(std::slice::from_ref(&p)).await;
    assert_eq!(host.state(&p), Some(RecipientState::Releasing));
    assert_eq!(
        host.reopen(GENERATION, &[(p.clone(), primary.clone())]).await,
        vec![(p.clone(), ReopenReply::Reopened)]
    );
    assert_eq!(body_of(primary_loop.receive().await), "one");
    assert_eq!(body_of(primary_loop.receive().await), "two");
    host.send(named(&[0], "three")).await;
    assert_eq!(body_of(primary_loop.receive().await), "three");
    assert_eq!(primary_loop.close_request(), CloseRequest::None);

    // A reopen during the drain clears the close request; the loop continues with the same queue.
    host.release(std::slice::from_ref(&p)).await;
    primary_loop
        .wait_for_close(|request| matches!(request, CloseRequest::Retire { .. }))
        .await;
    assert_eq!(
        host.reopen(TRANSACTION, &[(p.clone(), primary.clone())]).await,
        vec![(p.clone(), ReopenReply::Reopened)]
    );
    assert_eq!(primary_loop.close_request(), CloseRequest::None);
    host.send(named(&[0], "four")).await;
    assert_eq!(body_of(primary_loop.receive().await), "four");

    // Once its loop exited, a reopen is answered `Exited`, and the exit was reported.
    host.release(std::slice::from_ref(&p)).await;
    primary_loop
        .wait_for_close(|request| matches!(request, CloseRequest::Retire { .. }))
        .await;
    primary_loop.exit(retired());
    assert_eq!(host.event().await, exited(&p, retired()));
    assert_eq!(
        host.reopen(GENERATION, &[(p.clone(), primary.clone())]).await,
        vec![(p.clone(), ReopenReply::Exited)]
    );
    host.stop().await;
}

#[tokio::test]
async fn an_abort_drops_only_what_it_alone_created_reporting_its_exit_and_returns_a_reopen_to_its_drain() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let team = declared("team_d", TEAM, "team");
    let (p, x, t) = (
        recipient(0, &primary, 1),
        recipient(1, &additional, 1),
        recipient(2, &team, 1),
    );
    let options = Options {
        credentials: vec![("credentials/team", "team-key")],
        boot: vec![(p.clone(), primary.clone())],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let mut primary_loop = host.loops.take(&primary.id);

    // A transaction and a generation prepare the additional endpoint; only the generation prepares the declared one.
    host.prepare(Creator::Transaction(5), &[(x.clone(), additional.clone())], &[])
        .await
        .unwrap();
    host.prepare(
        GENERATION,
        &[(x.clone(), additional.clone()), (t.clone(), team.clone())],
        &[],
    )
    .await
    .unwrap();
    let mut team_loop = host.loops.take(&team.id);

    // The released boot recipient drains; the generation reopens it.
    host.release(std::slice::from_ref(&p)).await;
    primary_loop
        .wait_for_close(|request| matches!(request, CloseRequest::Retire { .. }))
        .await;
    let CloseRequest::Retire { deadline } = primary_loop.close_request() else {
        unreachable!("the recipient drains");
    };
    host.reopen(GENERATION, &[(p.clone(), primary.clone())]).await;
    assert_eq!(primary_loop.close_request(), CloseRequest::None);

    // A later drain would end later.
    tokio::time::sleep(Duration::from_millis(20)).await;
    host.abort(GENERATION).await;

    // The declared recipient is dropped: its loop closes as at shutdown, and its exit is reported like any other.
    team_loop
        .wait_for_close(|request| request == CloseRequest::Shutdown)
        .await;
    assert!(
        team_loop.transactions.recv().await.is_none(),
        "the host dropped its sender"
    );
    team_loop.exit(retired());
    assert_eq!(host.event().await, exited(&t, retired()));
    // The reopened recipient drains again, with the deadline of its first drain.
    assert_eq!(primary_loop.close_request(), CloseRequest::Retire { deadline });
    assert_eq!(host.state(&p), Some(RecipientState::Draining));
    // The transaction still holds the additional endpoint.
    assert_eq!(host.state(&x), Some(RecipientState::Live));

    // Once its revision is published, the transaction can no longer be aborted.
    host.publish(5, std::slice::from_ref(&x), &[], &[]).await.unwrap();
    host.abort(Creator::Transaction(5)).await;
    assert_eq!(host.state(&x), Some(RecipientState::Live));
    host.stop().await;
}

#[tokio::test]
async fn an_aborted_resource_transaction_drops_what_no_newer_one_prepared_again() {
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let (x, p) = (recipient(1, &additional, 1), recipient(0, &primary, 1));
    let mut host = Harness::start(Options::default()).await;
    host.prepare(
        Creator::Transaction(7),
        &[(x.clone(), additional.clone()), (p.clone(), primary.clone())],
        &[],
    )
    .await
    .unwrap();
    // The newer transaction needs the additional endpoint only.
    host.prepare(Creator::Transaction(8), &[(x.clone(), additional.clone())], &[])
        .await
        .unwrap();
    let mut primary_loop = host.loops.take(&primary.id);

    host.abort(Creator::Transaction(7)).await;
    primary_loop
        .wait_for_close(|request| request == CloseRequest::Shutdown)
        .await;
    primary_loop.exit(retired());
    assert_eq!(host.event().await, exited(&p, retired()));
    assert_eq!(host.state(&x), Some(RecipientState::Live));
    assert_eq!(host.state(&p), None);
    host.stop().await;
}

#[tokio::test]
async fn a_superseded_transaction_aborted_after_the_newer_one_published_drops_what_it_alone_created() {
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let (x, p) = (recipient(1, &additional, 1), recipient(0, &primary, 1));
    let mut host = Harness::start(Options::default()).await;
    host.prepare(
        Creator::Transaction(7),
        &[(x.clone(), additional.clone()), (p.clone(), primary.clone())],
        &[],
    )
    .await
    .unwrap();
    host.prepare(Creator::Transaction(8), &[(x.clone(), additional.clone())], &[])
        .await
        .unwrap();
    let mut primary_loop = host.loops.take(&primary.id);

    // The newer transaction publishes before the abort of the one it superseded arrives.
    host.publish(8, std::slice::from_ref(&x), &[], &[]).await.unwrap();
    host.abort(Creator::Transaction(7)).await;
    primary_loop
        .wait_for_close(|request| request == CloseRequest::Shutdown)
        .await;
    primary_loop.exit(retired());
    assert_eq!(host.event().await, exited(&p, retired()));
    assert_eq!(host.state(&p), None);
    assert_eq!(host.state(&x), Some(RecipientState::Live));

    // Its queue directory is free for the identity's next incarnation.
    host.prepare(
        Creator::Transaction(9),
        &[(recipient(2, &primary, 3), primary.clone())],
        &[],
    )
    .await
    .expect("the aborted recipient's directory is free once its loop exited");
    host.stop().await;
}

#[tokio::test]
async fn a_publication_commits_its_own_transactions_reopens_and_what_its_native_set_names() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let (p, x) = (recipient(0, &primary, 1), recipient(1, &additional, 1));
    let options = Options {
        boot: vec![(p.clone(), primary.clone()), (x.clone(), additional.clone())],
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let mut primary_loop = host.loops.take(&primary.id);
    let mut additional_loop = host.loops.take(&additional.id);
    for (recipient, fake) in [(&p, &mut primary_loop), (&x, &mut additional_loop)] {
        host.release(std::slice::from_ref(recipient)).await;
        fake.wait_for_close(|request| matches!(request, CloseRequest::Retire { .. }))
            .await;
    }

    // A transaction reopens the primary and publishes; a generation reopens the additional endpoint, which a
    // publication then names in its native set.
    host.reopen(Creator::Transaction(5), &[(p.clone(), primary.clone())])
        .await;
    host.publish(5, &[], &[], &[]).await.unwrap();
    host.reopen(GENERATION, &[(x.clone(), additional.clone())]).await;
    host.publish(6, std::slice::from_ref(&x), &[], &[]).await.unwrap();

    // Neither abort returns its recipient to the drain the reopen cancelled.
    host.abort(Creator::Transaction(5)).await;
    host.abort(GENERATION).await;
    assert_eq!(host.state(&p), Some(RecipientState::Live));
    assert_eq!(host.state(&x), Some(RecipientState::Live));
    assert_eq!(primary_loop.close_request(), CloseRequest::None);
    assert_eq!(additional_loop.close_request(), CloseRequest::None);

    // The native set holds what it names: an abort of the generation that created a recipient keeps it.
    let team = native("team", MetricsRecipientRoute::Additional, TEAM);
    let t = recipient(2, &team, 1);
    let generation = Creator::Generation(GenerationId::new(2));
    host.credentials.set("native:team", Some("team-key"));
    host.prepare(generation, &[(t.clone(), team.clone())], &[])
        .await
        .unwrap();
    host.publish(7, &[x.clone(), t.clone()], &[], &[]).await.unwrap();
    host.abort(generation).await;
    assert_eq!(host.state(&t), Some(RecipientState::Live));
    host.stop().await;
}

#[tokio::test]
async fn a_recipient_is_prepared_on_a_queue_directory_only_once_the_loop_that_owned_it_exited() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let first = recipient(0, &primary, 1);
    // The same identity in another slot: the same queue directory.
    let next = recipient(5, &primary, 2);
    let mut host = Harness::start(Options::default()).await;
    host.prepare(TRANSACTION, &[(first.clone(), primary.clone())], &[])
        .await
        .unwrap();
    let mut first_loop = host.loops.take(&primary.id);
    host.release(std::slice::from_ref(&first)).await;
    first_loop
        .wait_for_close(|request| matches!(request, CloseRequest::Retire { .. }))
        .await;

    let error = rejection(host.prepare(TRANSACTION, &[(next.clone(), primary.clone())], &[]).await);
    assert!(error.contains("is still owned by recipient"), "{error}");

    first_loop.exit(retired());
    assert_eq!(host.event().await, exited(&first, retired()));
    host.prepare(TRANSACTION, &[(next.clone(), primary.clone())], &[])
        .await
        .expect("the directory is free once the loop exited");
    host.stop().await;
}

#[tokio::test]
async fn a_failed_loop_reports_its_exit_once_and_its_slot_drops_payloads_until_its_next_incarnation() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let (failed, x) = (recipient(0, &primary, 1), recipient(1, &additional, 1));
    let options = Options {
        boot: vec![(failed.clone(), primary.clone()), (x.clone(), additional.clone())],
        native_set: vec![failed.clone(), x.clone()],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let mut failed_loop = host.loops.take(&primary.id);
    let mut additional_loop = host.loops.take(&additional.id);

    let failure = RecipientExit::Failed("the endpoint stopped answering".to_string());
    failed_loop.exit(failure.clone());
    assert_eq!(host.event().await, exited(&failed, failure));
    host.wait_for_status(|status| {
        status
            .recipients
            .iter()
            .any(|status| status.state == RecipientState::Failed)
    })
    .await;

    // Payloads naming it are dropped; the native set still routes to the others; a publication may still name it.
    host.send(named(&[0, 1], "dropped")).await;
    assert_eq!(body_of(additional_loop.receive().await), "dropped");
    host.publish(2, &[failed.clone(), x.clone()], &[], &[])
        .await
        .expect("a failed recipient may stay in the native set");

    // Its next incarnation takes its slot, and receives.
    let next = recipient(0, &primary, 2);
    host.prepare(TRANSACTION, &[(next.clone(), primary.clone())], &[])
        .await
        .expect("the next incarnation takes the failed slot");
    let mut next_loop = host.loops.take(&primary.id);
    host.send(named(&[0], "delivered")).await;
    assert_eq!(body_of(next_loop.receive().await), "delivered");
    assert!(host.no_event(), "each incarnation's exit is reported once");
    host.stop().await;
}

#[tokio::test]
async fn payloads_over_the_native_set_reach_a_failed_recipients_next_incarnation_before_a_publication_names_it() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let failed = recipient(0, &primary, 1);
    let options = Options {
        boot: vec![(failed.clone(), primary.clone())],
        native_set: vec![failed.clone()],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let mut failed_loop = host.loops.take(&primary.id);
    failed_loop.exit(RecipientExit::Failed("broken".to_string()));
    host.event().await;

    // The transaction that prepares the next incarnation publishes a native set that names it only later.
    let next = recipient(0, &primary, 2);
    host.prepare(TRANSACTION, &[(next.clone(), primary.clone())], &[])
        .await
        .unwrap();
    let mut next_loop = host.loops.take(&primary.id);
    host.send(payload(LOGS, None, None, "logs")).await;
    host.send(named(&[0], "sentinel")).await;
    assert_eq!(body_of(next_loop.receive().await), "logs");
    assert_eq!(body_of(next_loop.receive().await), "sentinel");

    // A publication then names it by its own reference.
    host.publish(2, std::slice::from_ref(&next), &[], &[]).await.unwrap();
    host.send(payload(LOGS, None, None, "published")).await;
    assert_eq!(body_of(next_loop.receive().await), "published");
    host.stop().await;
}

#[tokio::test]
async fn a_released_failed_recipient_frees_its_slot_without_another_exit() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let failed = recipient(0, &primary, 1);
    let options = Options {
        boot: vec![(failed.clone(), primary.clone())],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let mut failed_loop = host.loops.take(&primary.id);
    failed_loop.exit(RecipientExit::Failed("broken".to_string()));
    host.event().await;
    host.release(std::slice::from_ref(&failed)).await;
    host.wait_for_status(|status| status.recipients.is_empty()).await;
    assert!(host.no_event());
    host.stop().await;
}

/// Makes the routing loop wait for room in the loop of slot 1 (a loop of capacity 1 that nothing reads), and leaves a
/// payload for slot 0, `body`, waiting in the host's input behind it.
async fn wait_on_slot_1_with_a_payload_for_slot_0_queued(host: &Harness, body: &'static str) {
    host.send(named(&[1], "one")).await;
    host.send(named(&[1], "two")).await;
    host.send(named(&[0], body)).await;
    while host.input.as_ref().unwrap().capacity() < 63 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_recipients_slot_serves_the_next_recipient_only_payloads_received_after_its_release() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let team = declared("team_d", TEAM, "team");
    let (failed, x, t) = (
        recipient(0, &primary, 1),
        recipient(1, &additional, 1),
        recipient(0, &team, 2),
    );
    let options = Options {
        loop_capacity: 1,
        credentials: vec![("credentials/team", "team-key")],
        boot: vec![(failed.clone(), primary.clone()), (x.clone(), additional.clone())],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let mut failed_loop = host.loops.take(&primary.id);
    let mut additional_loop = host.loops.take(&additional.id);
    failed_loop.exit(RecipientExit::Failed("broken".to_string()));
    host.event().await;

    // The last payload of a drained lease names the failed recipient, and waits in the input when the core releases
    // the recipient and prepares another into the slot.
    wait_on_slot_1_with_a_payload_for_slot_0_queued(&host, "for the failed recipient").await;
    host.release(std::slice::from_ref(&failed)).await;
    host.prepare(GENERATION, &[(t.clone(), team.clone())], &[])
        .await
        .expect("the released slot takes another recipient at once");
    let mut team_loop = host.loops.take(&team.id);
    assert_eq!(body_of(additional_loop.receive().await), "one");
    assert_eq!(body_of(additional_loop.receive().await), "two");

    // The payload stamped for the failed recipient is dropped; the slot's new recipient gets only its own.
    host.send(named(&[0], "for the team")).await;
    assert_eq!(body_of(team_loop.receive().await), "for the team");
    assert!(team_loop.received_nothing());
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payloads_that_name_a_released_failed_recipient_are_dropped_for_it_once_its_slot_is_free() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let (failed, x) = (recipient(0, &primary, 1), recipient(1, &additional, 1));
    let options = Options {
        loop_capacity: 1,
        boot: vec![(failed.clone(), primary.clone()), (x.clone(), additional.clone())],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let mut failed_loop = host.loops.take(&primary.id);
    let mut additional_loop = host.loops.take(&additional.id);
    failed_loop.exit(RecipientExit::Failed("broken".to_string()));
    host.event().await;

    wait_on_slot_1_with_a_payload_for_slot_0_queued(&host, "for the failed recipient").await;
    host.release(std::slice::from_ref(&failed)).await;
    host.wait_for_status(|status| status.recipients.len() == 1).await;
    assert_eq!(body_of(additional_loop.receive().await), "one");
    assert_eq!(body_of(additional_loop.receive().await), "two");

    // Routing goes on: the payload for the freed slot was counted, not taken for one sent to an empty slot.
    host.send(named(&[1], "after")).await;
    assert_eq!(body_of(additional_loop.receive().await), "after");
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_loop_that_ends_while_its_release_waits_at_the_barrier_keeps_its_payloads_from_the_slots_next_recipient() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let team = declared("team_d", TEAM, "team");
    let (p, x, t) = (
        recipient(0, &primary, 1),
        recipient(1, &additional, 1),
        recipient(0, &team, 2),
    );
    let options = Options {
        loop_capacity: 1,
        credentials: vec![("credentials/team", "team-key")],
        boot: vec![(p.clone(), primary.clone()), (x.clone(), additional.clone())],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let mut primary_loop = host.loops.take(&primary.id);
    let mut additional_loop = host.loops.take(&additional.id);

    // The release waits at the barrier for a payload that names the primary, and its loop fails meanwhile.
    wait_on_slot_1_with_a_payload_for_slot_0_queued(&host, "for the primary").await;
    host.release(std::slice::from_ref(&p)).await;
    assert_eq!(host.state(&p), Some(RecipientState::Releasing));
    let failure = RecipientExit::Failed("broken".to_string());
    primary_loop.exit(failure.clone());
    assert_eq!(host.event().await, exited(&p, failure));

    // Its exit is reported at once, so the core may reuse the slot before the payload that names the primary is
    // routed: that payload is dropped, not sent to the slot's next recipient.
    host.prepare(GENERATION, &[(t.clone(), team.clone())], &[])
        .await
        .expect("the slot of the exited recipient takes another recipient");
    let mut team_loop = host.loops.take(&team.id);
    assert_eq!(body_of(additional_loop.receive().await), "one");
    assert_eq!(body_of(additional_loop.receive().await), "two");
    host.send(named(&[0], "for the team")).await;
    assert_eq!(body_of(team_loop.receive().await), "for the team");
    assert!(primary_loop.received_nothing());
    host.stop().await;
}

#[tokio::test]
async fn exits_wait_for_room_in_the_event_channel_without_holding_up_routing() {
    let specs = (0..3)
        .map(|index| declared(&format!("team_{index}"), TEAM, &format!("team{index}")))
        .collect::<Vec<_>>();
    let options = Options {
        event_capacity: 1,
        credentials: vec![
            ("credentials/team0", "key-0"),
            ("credentials/team1", "key-1"),
            ("credentials/team2", "key-2"),
        ],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let recipients = specs
        .iter()
        .enumerate()
        .map(|(slot, spec)| (recipient(slot as u16, spec, 1), spec.clone()))
        .collect::<Vec<_>>();
    host.prepare(GENERATION, &recipients, &[]).await.unwrap();
    let mut loops = specs.iter().map(|spec| host.loops.take(&spec.id)).collect::<Vec<_>>();

    for fake in &mut loops[..2] {
        fake.exit(RecipientExit::Failed("broken".to_string()));
    }
    // The channel holds one exit; routing continues while the other waits.
    host.send(named(&[2], "routed")).await;
    assert_eq!(body_of(loops[2].receive().await), "routed");
    let mut reported = vec![host.event().await, host.event().await];
    reported.sort_by_key(|event| match event {
        DeliveryEvent::RecipientExited { recipient, .. } => recipient.slot,
    });
    assert_eq!(
        reported,
        vec![
            exited(&recipients[0].0, RecipientExit::Failed("broken".to_string())),
            exited(&recipients[1].0, RecipientExit::Failed("broken".to_string())),
        ]
    );
    host.stop().await;
}

#[tokio::test]
async fn releasing_more_recipients_than_drain_at_once_closes_the_oldest_early() {
    let count = MAX_DRAINING_RECIPIENTS + 1;
    let names = (0..count).map(|index| format!("team{index}")).collect::<Vec<_>>();
    let credentials = names
        .iter()
        .map(|name| (&*format!("credentials/{name}").leak(), &*format!("key-{name}").leak()))
        .collect::<Vec<(&'static str, &'static str)>>();
    let options = Options {
        credentials,
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let recipients = names
        .iter()
        .enumerate()
        .map(|(slot, name)| {
            let spec = declared(name, TEAM, name);
            (recipient(slot as u16, &spec, 1), spec)
        })
        .collect::<Vec<_>>();
    host.prepare(GENERATION, &recipients, &[]).await.unwrap();
    let mut loops = recipients
        .iter()
        .map(|(_, spec)| host.loops.take(&spec.id))
        .collect::<Vec<_>>();

    for (index, (recipient, _)) in recipients.iter().enumerate() {
        host.release(std::slice::from_ref(recipient)).await;
        loops[index]
            .wait_for_close(|request| matches!(request, CloseRequest::Retire { .. }))
            .await;
    }
    let deadline = |fake: &FakeLoop| match fake.close_request() {
        CloseRequest::Retire { deadline } => deadline,
        other => panic!("expected a drain, got {other:?}"),
    };
    let now = tokio::time::Instant::now();
    assert!(deadline(&loops[0]) <= now, "the oldest drain is closed early");
    for fake in &loops[1..] {
        assert!(deadline(fake) > now, "the other drains keep their deadlines");
    }
    host.stop().await;
}

#[tokio::test]
async fn a_publication_changes_a_recipients_protocol_at_its_next_request() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let p = recipient(0, &primary, 1);
    let options = Options {
        boot: vec![(p.clone(), primary.clone())],
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let primary_loop = host.loops.take(&primary.id);
    let request = |path: &str| {
        Request::builder()
            .method("POST")
            .uri(path)
            .body(TransactionBody::<Body>::Rehydrated(None))
            .unwrap()
    };

    let before = primary_loop.map(request(LOGS));
    assert!(before.headers().get("allow-arbitrary-tag-value").is_none());
    assert_eq!(before.uri().host(), Some("primary.example.test"));
    assert!(
        !primary_loop.pieces.config.allow_arbitrary_tags(),
        "the loop never adds the header itself"
    );

    let mut changed = primary.clone();
    changed.protocol = RecipientProtocol::DatadogIntake {
        authorities: IntakeAuthorities {
            logs: Some("logs.example.test".into()),
            traces: None,
        },
        allow_arbitrary_tags: true,
        v3: V3Acceptance::default(),
    };
    host.publish(2, std::slice::from_ref(&p), &[(p.clone(), changed)], &[])
        .await
        .unwrap();

    // The request a retry sends again picks the new protocol up too.
    let after = primary_loop.map(before);
    assert_eq!(after.headers().get("allow-arbitrary-tag-value").unwrap(), "true");
    assert_eq!(after.uri().host(), Some("logs.example.test"));
    assert_eq!(after.headers().get("dd-api-key").unwrap(), "primary-key");

    // Turned off again, a retried request loses the header.
    host.publish(3, std::slice::from_ref(&p), &[(p.clone(), primary.clone())], &[])
        .await
        .unwrap();
    let off = primary_loop.map(after);
    assert!(off.headers().get("allow-arbitrary-tag-value").is_none());
    host.stop().await;
}

#[tokio::test]
async fn a_rotated_credential_reaches_the_next_request_and_a_removed_one_keeps_the_last_key() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let p = recipient(0, &primary, 1);
    let options = Options {
        boot: vec![(p.clone(), primary.clone())],
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let primary_loop = host.loops.take(&primary.id);
    let key = |fake: &FakeLoop| {
        fake.map(
            Request::builder()
                .uri(SERIES)
                .body(TransactionBody::<Body>::Rehydrated(None))
                .unwrap(),
        )
        .headers()
        .get("dd-api-key")
        .map(|value| value.to_str().unwrap().to_string())
    };
    assert_eq!(key(&primary_loop).as_deref(), Some("primary-key"));

    host.credentials.set("native:primary", Some("  rotated-key  "));
    timeout(WAIT, async {
        while key(&primary_loop).as_deref() != Some("rotated-key") {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the rotated key should reach the next request");

    // A removed or blank value keeps the last usable key.
    host.credentials.set("native:primary", Some(" "));
    host.credentials.set("native:primary", None);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(key(&primary_loop).as_deref(), Some("rotated-key"));
    host.stop().await;
}

#[tokio::test]
async fn a_declared_duplicate_of_a_live_recipient_fails_and_native_ones_are_reported_as_conflicts() {
    // A declared recipient whose endpoint and key equal the primary's.
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let copy = declared("copy", PRIMARY, "copy");
    let other = declared("other", PRIMARY, "other");
    let p = recipient(0, &primary, 1);
    let options = Options {
        credentials: vec![("credentials/copy", "primary-key"), ("credentials/other", "other-key")],
        boot: vec![(p.clone(), primary.clone())],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let mut primary_loop = host.loops.take(&primary.id);

    let error = rejection(
        host.prepare(GENERATION, &[(recipient(1, &copy, 2), copy.clone())], &[])
            .await,
    );
    assert!(error.contains("duplicates recipient"), "{error}");

    // A different key is no duplicate; a rotation that makes the keys equal is a conflict, which stops neither.
    let o = recipient(2, &other, 3);
    host.prepare(GENERATION, &[(o.clone(), other.clone())], &[])
        .await
        .unwrap();
    host.credentials.set("credentials/other", Some("primary-key"));
    let status = host.wait_for_status(|status| !status.conflicts.is_empty()).await;
    assert_eq!(status.conflicts.len(), 1);
    assert_eq!(status.conflicts[0].declared, o);
    assert_eq!(status.conflicts[0].duplicates, p);

    // A draining recipient never counts.
    host.release(std::slice::from_ref(&p)).await;
    primary_loop
        .wait_for_close(|request| matches!(request, CloseRequest::Retire { .. }))
        .await;
    host.wait_for_status(|status| status.conflicts.is_empty()).await;
    host.prepare(GENERATION, &[(recipient(1, &copy, 4), copy.clone())], &[])
        .await
        .expect_err("the declared duplicate of another live declared recipient still fails");
    host.stop().await;
}

#[tokio::test]
async fn a_renamed_credential_prepares_beside_the_withdrawn_recipient_it_replaces() {
    // The same key under a new credential name: the generation that declares it prepares it while the old recipient,
    // withdrawn by the resource transaction, is still live until the old generation retires.
    let old = declared("team_d", TEAM, "team_a");
    let renamed = declared("team_d", TEAM, "team_b");
    let options = Options {
        credentials: vec![("credentials/team_a", "same-key"), ("credentials/team_b", "same-key")],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let (a, b) = (recipient(0, &old, 1), recipient(1, &renamed, 2));
    host.prepare(GENERATION, &[(a.clone(), old.clone())], &[])
        .await
        .unwrap();
    host.publish(2, &[], &[], &[&old]).await.unwrap();
    host.prepare(
        Creator::Generation(GenerationId::new(2)),
        &[(b.clone(), renamed.clone())],
        &[],
    )
    .await
    .expect("a withdrawn recipient is no duplicate");
    assert!(host.status.borrow().conflicts.is_empty());

    // Restored, the old recipient sends with the same key again: a conflict, which stops neither.
    host.publish(3, &[], &[], &[]).await.unwrap();
    let status = host.wait_for_status(|status| !status.conflicts.is_empty()).await;
    assert_eq!(status.conflicts[0].declared, a);
    assert_eq!(status.conflicts[0].duplicates, b);
    host.stop().await;
}

#[tokio::test]
async fn a_cluster_agent_recipient_sends_its_current_token_at_dispatch() {
    let mut agent = native(
        "primary",
        MetricsRecipientRoute::Primary,
        "https://cluster-agent.example.test:5005",
    );
    agent.protocol = RecipientProtocol::ClusterAgent;
    let a = recipient(0, &agent, 1);
    let options = Options {
        credentials: vec![("native:primary", "token-1")],
        boot: vec![(a.clone(), agent.clone())],
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let agent_loop = host.loops.take(&agent.id);
    let map = || {
        agent_loop.map(
            Request::builder()
                .uri(SERIES)
                .header("dd-api-key", "leftover")
                .body(TransactionBody::<Body>::Rehydrated(None))
                .unwrap(),
        )
    };
    // The credential the host was built with is replaced by the source's value when the recipient registers.
    let request = map();
    assert_eq!(
        request.uri().to_string(),
        "https://cluster-agent.example.test:5005/series"
    );
    assert!(request.headers().get("dd-api-key").is_none());
    assert_eq!(
        request.headers().get(http::header::AUTHORIZATION).unwrap(),
        "Bearer token-1"
    );

    host.credentials.set("native:primary", Some("token-2"));
    timeout(WAIT, async {
        while map().headers().get(http::header::AUTHORIZATION).unwrap() != "Bearer token-2" {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the rotated token should reach the next request");
    host.stop().await;
}

/// Starts an intake that answers API key validation with `status`, and returns its URL and the keys it was asked to
/// validate.
async fn start_validation_intake(status: http::StatusCode) -> (String, Arc<Mutex<Vec<String>>>) {
    use axum::{extract::RawQuery, routing::get, Router as Routes};

    let asked = Arc::new(Mutex::new(Vec::new()));
    let routes = Routes::new().route(
        "/api/v1/validate",
        get({
            let asked = Arc::clone(&asked);
            move |RawQuery(query): RawQuery| {
                asked.lock().unwrap().push(query.unwrap_or_default());
                async move { status }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, routes).await });
    (url, asked)
}

#[tokio::test]
async fn a_host_without_native_intake_recipients_is_ready_and_validates_nothing() {
    let (intake, asked) = start_validation_intake(http::StatusCode::FORBIDDEN).await;
    let team = declared("team_d", &intake, "team");
    let registry = HealthRegistry::new();
    let id = saluki_core::support::SubsystemIdentifier::from_segments(["delivery_host_test"]);
    let options = Options {
        credentials: vec![("credentials/team", "rejected-key")],
        boot: vec![(recipient(0, &team, 1), team.clone())],
        health: registry.register_component(&id),
        ..Options::default()
    };
    let host = Harness::start(options).await;
    timeout(WAIT, registry.all_ready())
        .await
        .expect("a host whose recipients are all declared is ready");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        asked.lock().unwrap().is_empty(),
        "a declared credential is never validated"
    );
    let router = host.stop().await;
    assert!(router.validated().is_empty());
}

#[tokio::test]
async fn native_intake_recipients_are_validated_and_a_rejected_key_makes_the_host_not_ready() {
    let (intake, asked) = start_validation_intake(http::StatusCode::FORBIDDEN).await;
    let primary = native("primary", MetricsRecipientRoute::Primary, &intake);
    let p = recipient(0, &primary, 1);
    let registry = HealthRegistry::new();
    let id = saluki_core::support::SubsystemIdentifier::from_segments(["delivery_host_test"]);
    let options = Options {
        credentials: vec![("native:primary", "rejected-key")],
        health: registry.register_component(&id),
        ..Options::default()
    };
    let host = Harness::start(options).await;
    timeout(WAIT, registry.all_ready())
        .await
        .expect("an idle host is ready");

    // Adding a native recipient live validates its key; a rejected one makes the host not ready.
    host.prepare(TRANSACTION, &[(p.clone(), primary.clone())], &[])
        .await
        .unwrap();
    timeout(WAIT, async {
        while asked.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the native key should be validated");
    assert_eq!(asked.lock().unwrap()[0], "api_key=rejected-key");
    timeout(WAIT, async {
        while registry.all_ready().now_or_never().is_some() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("a host whose only key is rejected is not ready");
    let router = host.stop().await;
    assert_eq!(router.validated().to_vec(), vec![p]);
}

#[tokio::test]
async fn native_intake_recipients_are_validated_once_in_the_order_a_forwarder_validates_its_endpoints() {
    // The validator stops at the first key the intake accepts, as a native forwarder's does, so which keys are asked
    // about depends on the order: a forwarder's, the primary endpoint first, then the additional endpoints, whatever
    // slots the recipients hold and whatever order the native set names them in (a lowering's, by identity).
    let (intake, asked) = start_validation_intake(http::StatusCode::OK).await;
    let primary = native("primary", MetricsRecipientRoute::Primary, &intake);
    let additional = native("additional", MetricsRecipientRoute::Additional, &intake);
    let (a, p) = (recipient(0, &additional, 1), recipient(1, &primary, 2));
    let options = Options {
        credentials: vec![
            ("native:primary", "primary-key"),
            ("native:additional", "additional-key"),
        ],
        boot: vec![(a.clone(), additional.clone()), (p.clone(), primary.clone())],
        native_set: vec![a.clone(), p.clone()],
        ..Options::default()
    };
    let host = Harness::start(options).await;
    timeout(WAIT, async {
        while asked.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the native keys should be validated");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(*asked.lock().unwrap(), ["api_key=primary-key"]);
    let router = host.stop().await;
    assert_eq!(router.validated().to_vec(), vec![p, a]);
}

#[tokio::test]
async fn a_retired_or_aborted_recipient_stops_following_its_credential() {
    let team = declared("team_d", TEAM, "team");
    let other = declared("other", TEAM, "other");
    let options = Options {
        credentials: vec![("credentials/team", "team-key"), ("credentials/other", "other-key")],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let (t, o) = (recipient(0, &team, 1), recipient(1, &other, 1));
    host.prepare(GENERATION, &[(t.clone(), team.clone())], &[])
        .await
        .unwrap();
    host.prepare(
        Creator::Generation(GenerationId::new(2)),
        &[(o.clone(), other.clone())],
        &[],
    )
    .await
    .unwrap();
    let mut team_loop = host.loops.take(&team.id);
    let mut other_loop = host.loops.take(&other.id);

    host.release(std::slice::from_ref(&t)).await;
    team_loop.exit(retired());
    host.event().await;
    host.abort(Creator::Generation(GenerationId::new(2))).await;
    other_loop.exit(retired());
    host.event().await;
    let router = host.stop().await;
    assert_eq!(router.credentials_followed(), 0);
}

#[tokio::test]
async fn shutdown_closes_every_recipient_including_one_that_drains() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let additional = native("additional", MetricsRecipientRoute::Additional, ADDITIONAL);
    let (p, x) = (recipient(0, &primary, 1), recipient(1, &additional, 1));
    let options = Options {
        boot: vec![(p.clone(), primary.clone()), (x.clone(), additional.clone())],
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let mut primary_loop = host.loops.take(&primary.id);
    let mut additional_loop = host.loops.take(&additional.id);
    host.release(std::slice::from_ref(&p)).await;
    primary_loop
        .wait_for_close(|request| matches!(request, CloseRequest::Retire { .. }))
        .await;

    host.stop().await;
    assert_eq!(primary_loop.close_request(), CloseRequest::Shutdown);
    assert_eq!(additional_loop.close_request(), CloseRequest::Shutdown);
    assert!(
        additional_loop.transactions.recv().await.is_none(),
        "the loop's input closes"
    );
}

#[tokio::test]
async fn a_recipient_cannot_be_built_without_its_credential() {
    let team = declared("team_d", TEAM, "missing");
    let host = Harness::start(Options::default()).await;
    let error = host
        .shared
        .build_pieces(&team, &host.settings)
        .err()
        .expect("an unconfigured credential fails the build");
    assert!(format!("{error:#}").contains("is not configured"), "{error:#}");
    let mut unknown = team.clone();
    unknown.credential = "vault:team".into();
    let error = host.shared.build_pieces(&unknown, &host.settings).err().unwrap();
    assert!(format!("{error:#}").contains("cannot resolve"), "{error:#}");
    host.stop().await;
}

#[test]
fn declared_recipients_name_their_queue_directory_after_their_endpoint_and_credential() {
    use crate::common::datadog::io::recipient_queue_id;

    let context = ComponentContext::test_forwarder("dd_out");
    let queue = |spec: &RecipientSpec, key: &str| {
        recipient_queue_id(
            &context,
            spec,
            &ResolvedEndpoint::for_recipient(spec, key).expect("the endpoint resolves"),
        )
    };
    let team = declared("team_d", TEAM, "team");
    let primary = native("primary", MetricsRecipientRoute::Primary, TEAM);
    let renamed = declared("team_e", TEAM, "team");
    let other_credential = declared("team_d", TEAM, "other");

    // A rotated key keeps the queue; the declared name does not name it; another credential is another queue, and so
    // is a native recipient of the same endpoint.
    assert_eq!(queue(&team, "key-1"), queue(&team, "key-2"));
    assert_eq!(queue(&team, "key-1"), queue(&renamed, "key-1"));
    assert_ne!(queue(&team, "key-1"), queue(&other_credential, "key-1"));
    assert_ne!(queue(&team, "key-1"), queue(&primary, "key-1"));
    assert!(queue(&team, "key-1").starts_with("dd_out/team.example.test/"));

    // A native additional endpoint is named as a static forwarder names it: by raw URL and key position.
    let additional = native(
        "additional",
        MetricsRecipientRoute::Additional,
        "additional.example.test",
    );
    let endpoint = ResolvedEndpoint::for_recipient(&additional, "key").unwrap();
    assert_eq!(
        endpoint.additional_endpoint_queue_key(),
        Some(("additional.example.test", 0))
    );
}

// End to end: a controlled forwarder with its real recipient loops, and the host's client.

/// What an intake or a proxy received.
#[derive(Debug)]
struct Received {
    uri: String,
    headers: http::HeaderMap,
    body: Bytes,
}

/// Starts an HTTP server that answers every request with `200 OK` and reports each one, and returns its URL.
async fn start_recording_intake() -> (String, mpsc::UnboundedReceiver<Received>) {
    use axum::{extract::Request as AxumRequest, Router as Routes};

    let (received, requests) = mpsc::unbounded_channel();
    let routes = Routes::new().fallback(move |request: AxumRequest| {
        let received = received.clone();
        async move {
            let (parts, body) = request.into_parts();
            let body = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
            let _ = received.send(Received {
                uri: parts.uri.to_string(),
                headers: parts.headers,
                body,
            });
            http::StatusCode::OK
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, routes).await });
    (url, requests)
}

async fn next_request(
    requests: &mut mpsc::UnboundedReceiver<Received>, wanted: impl Fn(&Received) -> bool,
) -> Received {
    timeout(Duration::from_secs(30), async {
        loop {
            let received = requests.recv().await.expect("the server runs");
            if wanted(&received) {
                return received;
            }
        }
    })
    .await
    .expect("the server should receive the request")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_controlled_forwarder_delivers_through_real_recipient_loops_from_its_first_payload() {
    use saluki_core::{
        accounting::{ComponentRegistry, MemoryLimiter},
        components::{
            forwarders::{ForwarderBuilder as _, ForwarderContext},
            BuildContext,
        },
        runtime::state::{DataspaceRegistry, ResourceRegistry},
        topology::TopologyContext,
    };

    use super::{BootPublication as Boot, DeliveryHostConfiguration};
    use crate::forwarders::DatadogForwarderConfiguration;

    let _ = saluki_tls::initialize_default_crypto_provider();
    let (intake_url, mut requests) = start_recording_intake().await;
    let primary = native("primary", MetricsRecipientRoute::Primary, &intake_url);
    let mut team = declared("team_d", &format!("{intake_url}/team"), "team");
    // It takes V3 series now; the V2 payloads its lease resolved for it earlier still reach it, unfiltered.
    team.protocol = intake(V3Acceptance {
        series: true,
        sketches: false,
    });
    let (p, t) = (recipient(0, &primary, 1), recipient(1, &team, 2));
    let credentials = TestCredentials::new(&[("native:primary", "boot-key"), ("credentials/team", "team-key")]);
    let mut shared = shared_configuration();
    shared.endpoints.tls.skip_ssl_validation = true;
    let settings = Arc::new(ForwarderConfiguration::from_configuration(&shared).delivery_settings());

    let (control, mailbox) = control_mailbox();
    let (events_tx, _events) = mpsc::channel(8);
    let host = DeliveryHostConfiguration::new(
        mailbox,
        credentials.clone(),
        Boot {
            revision: 1,
            settings: Arc::clone(&settings),
            recipients: vec![settings_of(&p, &primary)],
            native_set: vec![p.clone()],
            withdrawn: BTreeSet::new(),
        },
    )
    .with_events(events_tx);
    let factory = host.factory();
    let error = factory
        .build(&team, &settings)
        .err()
        .expect("nothing builds before the host");
    assert!(
        format!("{error:#}").contains("before its delivery host is built"),
        "{error:#}"
    );
    let mut status = host.status();
    let config = DatadogForwarderConfiguration::controlled(&shared, Live::new_fixed(Default::default()), host);

    let supervisor = TestComponentSupervisor::start("dd_out").await;
    let component = ComponentContext::test_forwarder("dd_out");
    let forwarder = supervisor
        .dataspace()
        .with_current(|| {
            futures::executor::block_on(config.build(BuildContext::new(component.clone(), ResourceRegistry::new())))
        })
        .expect("the controlled forwarder builds");
    let error = supervisor
        .dataspace()
        .with_current(|| {
            futures::executor::block_on(config.build(BuildContext::new(component.clone(), ResourceRegistry::new())))
        })
        .err()
        .expect("a delivery host builds once");
    assert!(format!("{error:#}").contains("already built"), "{error:#}");

    let (input, input_rx) = mpsc::channel(8);
    let registry = HealthRegistry::new();
    let health = registry
        .register_component(&saluki_core::support::SubsystemIdentifier::from_dotted("dd_out"))
        .unwrap();
    let topology = TopologyContext::new(
        Arc::from("test"),
        MemoryLimiter::noop(),
        HealthRegistry::new(),
        tokio::runtime::Handle::current(),
        DataspaceRegistry::new(),
    );
    let context = ForwarderContext::new(
        &topology,
        &component,
        ComponentRegistry::default(),
        health,
        Consumer::new(component.clone(), input_rx),
    );
    let run = tokio::spawn(supervisor.scope(async move { forwarder.run(context).await }));
    assert!(
        timeout(WAIT, control.running()).await.unwrap(),
        "the host reads its mailbox once it runs"
    );

    // The boot recipient's key is validated, and it receives payloads routed over the boot native set.
    let validation = next_request(&mut requests, |received| received.uri.contains("/api/v1/validate")).await;
    assert!(validation.uri.contains("api_key=boot-key"), "{validation:?}");
    timeout(WAIT, registry.all_ready())
        .await
        .expect("a valid key makes the host ready");
    input.send(payload(LOGS, None, None, "logs")).await.unwrap();
    let logs = next_request(&mut requests, |received| received.uri.ends_with(LOGS)).await;
    assert_eq!(logs.headers.get("dd-api-key").unwrap(), "boot-key");

    // A recipient built by the factory and prepared live receives what names it, with its own credential.
    let prepared = PreparedRecipient {
        recipient: t.clone(),
        spec: team.clone(),
        pieces: factory
            .build(&team, &settings)
            .expect("the factory builds once the host is built"),
    };
    timeout(
        WAIT,
        control.send(DeliveryControl::Prepare {
            creator: GENERATION,
            recipients: vec![prepared],
            requires: vec![p.clone()],
        }),
    )
    .await
    .unwrap()
    .expect("the declared recipient prepares");
    timeout(WAIT, status.wait_for(|status| status.recipients.len() == 2))
        .await
        .unwrap()
        .unwrap();
    input.send(named(&[1], "team")).await.unwrap();
    let to_team = next_request(&mut requests, |received| received.uri.ends_with(SERIES)).await;
    assert_eq!(to_team.headers.get("dd-api-key").unwrap(), "team-key");

    // A rotated credential reaches the next request the real loop sends.
    credentials.set("native:primary", Some("rotated-key"));
    timeout(Duration::from_secs(30), async {
        loop {
            input.send(payload(LOGS, None, None, "logs")).await.unwrap();
            let logs = next_request(&mut requests, |received| received.uri.ends_with(LOGS)).await;
            if logs.headers.get("dd-api-key").unwrap() == "rotated-key" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the rotated key should reach the intake");

    drop(input);
    timeout(WAIT, run)
        .await
        .expect("the host stops when its input closes")
        .unwrap()
        .expect("the host stops cleanly");
}

/// A forwarder built through its builder and run as a topology runs it, with real recipient loops.
struct RunningForwarder {
    input: mpsc::Sender<Payload>,
    run: JoinHandle<Result<(), saluki_error::GenericError>>,
    _supervisor: TestComponentSupervisor,

    /// Held so that the component's health handle waits for liveness probes rather than finding its registry gone.
    _registry: HealthRegistry,
}

impl RunningForwarder {
    async fn start<F: ForwarderBuilder>(builder: &F) -> Self {
        use saluki_core::{
            accounting::{ComponentRegistry, MemoryLimiter},
            components::{forwarders::ForwarderContext, BuildContext},
            runtime::state::{DataspaceRegistry, ResourceRegistry},
            topology::TopologyContext,
        };

        let _ = saluki_tls::initialize_default_crypto_provider();
        let supervisor = TestComponentSupervisor::start("dd_out").await;
        let component = ComponentContext::test_forwarder("dd_out");
        let forwarder = supervisor
            .dataspace()
            .with_current(|| {
                futures::executor::block_on(
                    builder.build(BuildContext::new(component.clone(), ResourceRegistry::new())),
                )
            })
            .expect("the forwarder builds");
        let (input, input_rx) = mpsc::channel(8);
        let registry = HealthRegistry::new();
        let health = registry
            .register_component(&saluki_core::support::SubsystemIdentifier::from_dotted("dd_out"))
            .unwrap();
        let topology = TopologyContext::new(
            Arc::from("test"),
            MemoryLimiter::noop(),
            HealthRegistry::new(),
            tokio::runtime::Handle::current(),
            DataspaceRegistry::new(),
        );
        let context = ForwarderContext::new(
            &topology,
            &component,
            ComponentRegistry::default(),
            health,
            Consumer::new(component.clone(), input_rx),
        );
        let run = tokio::spawn(supervisor.scope(async move { forwarder.run(context).await }));
        Self {
            input,
            run,
            _supervisor: supervisor,
            _registry: registry,
        }
    }

    async fn send(&self, payload: Payload) {
        self.input.send(payload).await.expect("the forwarder reads its input");
    }

    async fn stop(self) {
        drop(self.input);
        timeout(WAIT, self.run)
            .await
            .expect("the forwarder stops when its input closes")
            .unwrap()
            .expect("the forwarder stops cleanly");
    }
}

/// Returns a payload with `metadata`, sent to `path`.
fn payload_with(path: &str, metadata: PayloadMetadata, body: &'static str) -> Payload {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .body(FrozenChunkedBytesBuffer::from(Bytes::from_static(body.as_bytes())))
        .expect("request should build");
    Payload::Http(HttpPayload::new(metadata, request))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_metrics_payload_over_the_native_set_reaches_a_real_loop_after_a_publication_changes_its_variants() {
    use super::DeliveryHostConfiguration;
    use crate::forwarders::DatadogForwarderConfiguration;

    let (intake_url, mut requests) = start_recording_intake().await;
    let mut primary = native("primary", MetricsRecipientRoute::Primary, &intake_url);
    primary.protocol = intake(V3Acceptance::default());
    let p = recipient(0, &primary, 1);
    let credentials = TestCredentials::new(&[("native:primary", "primary-key")]);
    let mut shared = shared_configuration();
    shared.endpoints.tls.skip_ssl_validation = true;
    let settings = Arc::new(ForwarderConfiguration::from_configuration(&shared).delivery_settings());
    let (control, mailbox) = control_mailbox();
    let host = DeliveryHostConfiguration::new(
        mailbox,
        credentials,
        BootPublication {
            revision: 1,
            settings: Arc::clone(&settings),
            recipients: vec![settings_of(&p, &primary)],
            native_set: vec![p.clone()],
            withdrawn: BTreeSet::new(),
        },
    );
    let forwarder = RunningForwarder::start(&DatadogForwarderConfiguration::controlled(
        &shared,
        Live::new_fixed(Default::default()),
        host,
    ))
    .await;
    assert!(timeout(WAIT, control.running()).await.unwrap());

    // A resource transaction makes the recipient take V3 series, which its loop was not spawned with.
    let mut v3 = primary.clone();
    v3.protocol = intake(V3Acceptance {
        series: true,
        sketches: false,
    });
    timeout(
        WAIT,
        control.send(DeliveryControl::Publish {
            revision: 2,
            native_set: vec![p.clone()],
            settings,
            recipients: vec![settings_of(&p, &v3)],
            withdrawn: BTreeSet::new(),
        }),
    )
    .await
    .unwrap()
    .expect("the publication applies");

    // A legacy encoder's V3 series, routed over the native set by the recipient's current variants, reaches the
    // intake, which the logs sent after it reach too.
    let metadata = PayloadMetadata::from_event_and_data_point_count(1, 1).with(MetricsPayloadInfo::v3_series());
    forwarder
        .send(payload_with("/api/intake/metrics/v3/series", metadata, "v3 series"))
        .await;
    forwarder.send(payload(LOGS, None, None, "logs")).await;
    // The loop sends requests concurrently, so they may arrive in either order.
    let mut received = Vec::new();
    for _ in 0..2 {
        let request = timeout(
            WAIT,
            next_request(&mut requests, |received| !received.uri.contains("/api/v1/validate")),
        )
        .await
        .expect("both payloads should reach the intake");
        received.push(String::from_utf8(request.body.to_vec()).unwrap());
    }
    received.sort();
    assert_eq!(received, ["logs", "v3 series"]);
    forwarder.stop().await;
}

/// The native recipients a delivery host runs for `config`, as a controller lowers them from the same configuration,
/// and the value of each one's credential.
fn native_recipients_of(config: &ForwarderConfiguration) -> (Vec<RecipientSettings>, Vec<(String, String)>) {
    use crate::common::datadog::endpoints::EndpointRoute;

    let mut recipients = Vec::new();
    let mut credentials = Vec::new();
    for (slot, routable) in config
        .build_routable_endpoints()
        .expect("the endpoints resolve")
        .iter()
        .enumerate()
    {
        let endpoint = routable.endpoint();
        let configured = endpoint.configured_endpoint().to_string();
        let (route, classes) = match routable.route() {
            EndpointRoute::Primary => (MetricsRecipientRoute::Primary, PayloadClasses::All),
            EndpointRoute::MetricsPrimary => (MetricsRecipientRoute::MetricsOverride, PayloadClasses::MetricsOnly),
            EndpointRoute::Additional => (MetricsRecipientRoute::Additional, PayloadClasses::All),
        };
        let index = endpoint.api_key_index();
        let credential = format!("native:{}/{}/{}", route.as_str(), configured, index.unwrap_or(0));
        credentials.push((credential.clone(), endpoint.api_key().to_string()));
        let protocol = config.datadog_intake_protocol(routable.route(), endpoint);
        let spec = RecipientSpec {
            id: format!("dd_out/{}/{}/{}", route.as_str(), index.unwrap_or(0), configured).into(),
            endpoint: configured.clone(),
            credential: credential.into(),
            classes,
            protocol: protocol.clone(),
            policy: Default::default(),
            origin: RecipientOrigin::Native(MetricsRecipient {
                route,
                configured_endpoint: configured,
                api_key_index: index,
                classes,
                protocol,
            }),
        };
        recipients.push(settings_of(&recipient(slot as u16, &spec, 1), &spec));
    }
    (recipients, credentials)
}

/// Collects what `intakes` receive, other than API key validations: at least `expected` requests in all, unless they
/// take too long, and then whatever else arrives until they receive nothing for a while.
async fn received_until_quiet(
    intakes: &mut [mpsc::UnboundedReceiver<Received>], expected: usize,
) -> Vec<Vec<(String, String, Bytes)>> {
    let mut received = vec![Vec::new(); intakes.len()];
    let quiet = Duration::from_millis(300);
    let started = tokio::time::Instant::now();
    let mut last = started;
    loop {
        let count = received.iter().map(Vec::len).sum::<usize>();
        let waiting = count < expected && started.elapsed() < Duration::from_secs(10);
        if !waiting && last.elapsed() >= quiet {
            break;
        }
        for (index, intake) in intakes.iter_mut().enumerate() {
            while let Ok(request) = intake.try_recv() {
                last = tokio::time::Instant::now();
                if request.uri.contains("/api/v1/validate") {
                    continue;
                }
                let key = request
                    .headers
                    .get("dd-api-key")
                    .map(|key| key.to_str().unwrap().to_string())
                    .unwrap_or_default();
                received[index].push((request.uri, key, request.body));
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for requests in &mut received {
        requests.sort();
    }
    received
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_booted_from_a_configuration_delivers_what_the_static_forwarder_delivers_for_it() {
    use std::collections::BTreeSet as Set;

    use super::DeliveryHostConfiguration;
    use crate::forwarders::DatadogForwarderConfiguration;

    // A primary, a metrics override, and an additional endpoint with two keys, each on an intake of its own.
    let (primary_url, primary_requests) = start_recording_intake().await;
    let (override_url, override_requests) = start_recording_intake().await;
    let (additional_url, additional_requests) = start_recording_intake().await;
    let mut intakes = [primary_requests, override_requests, additional_requests];
    let mut shared = shared_configuration();
    shared.endpoints.tls.skip_ssl_validation = true;
    shared.endpoints.api_key = "primary-key".to_string();
    shared.endpoints.dd_url = agent_data_plane_config::ConfigValue::explicit(primary_url.clone());
    shared.endpoints.opw_intake.enabled = true;
    shared.endpoints.opw_intake.url = override_url.clone();
    shared.endpoints.additional_endpoints = [(
        additional_url.clone(),
        vec!["first-key".to_string(), "second-key".to_string()],
    )]
    .into();

    let routing = |routing: MetricsEndpointRouting| Arc::new(routing);
    let payloads = || {
        let metrics = |info| PayloadMetadata::from_event_and_data_point_count(1, 1).with(info);
        vec![
            payload(LOGS, None, None, "logs"),
            payload_with(SERIES, metrics(MetricsPayloadInfo::v2_series()), "series"),
            payload_with(
                "/api/beta/sketches",
                metrics(MetricsPayloadInfo::v2_sketches()),
                "sketches",
            ),
            // A legacy endpoint routing selects the metrics override by the primary endpoint's identity.
            payload_with(
                SERIES,
                metrics(MetricsPayloadInfo::v2_series())
                    .with(routing(MetricsEndpointRouting::Only(Set::from([primary_url.clone()])))),
                "series for the primary's policy",
            ),
            payload_with(
                SERIES,
                metrics(MetricsPayloadInfo::v2_series()).with(routing(MetricsEndpointRouting::AllExcept(Set::from([
                    additional_url.clone(),
                ])))),
                "series without the additional endpoint's policy",
            ),
            payload_with(
                LOGS,
                PayloadMetadata::from_event_and_data_point_count(1, 1).with(routing(
                    MetricsEndpointRouting::AllExcept(Set::from([primary_url.clone()])),
                )),
                "logs without the primary's policy",
            ),
        ]
    };

    // Today's static forwarder.
    let configuration = DatadogForwarderConfiguration::from_configuration(
        &shared,
        Live::new_fixed(shared.endpoints.api_key.clone()),
        Live::new_fixed(shared.endpoints.additional_endpoints.clone()),
        Live::new_fixed(Default::default()),
    );
    let forwarder = RunningForwarder::start(&configuration).await;
    for payload in payloads() {
        forwarder.send(payload).await;
    }
    // One payload to the primary, four to the override, and four to the additional endpoint under each of its keys.
    let native = received_until_quiet(&mut intakes, 13).await;
    forwarder.stop().await;

    // A delivery host booted with the recipients the same configuration lowers to, its native set in their order.
    let (recipients, credentials) = native_recipients_of(&ForwarderConfiguration::from_configuration(&shared));
    let credentials = credentials
        .iter()
        .map(|(reference, value)| (reference.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    let (control, mailbox) = control_mailbox();
    let host = DeliveryHostConfiguration::new(
        mailbox,
        TestCredentials::new(&credentials),
        BootPublication {
            revision: 1,
            settings: Arc::new(ForwarderConfiguration::from_configuration(&shared).delivery_settings()),
            native_set: recipients.iter().map(|settings| settings.recipient.clone()).collect(),
            recipients,
            withdrawn: BTreeSet::new(),
        },
    );
    let forwarder = RunningForwarder::start(&DatadogForwarderConfiguration::controlled(
        &shared,
        Live::new_fixed(Default::default()),
        host,
    ))
    .await;
    assert!(timeout(WAIT, control.running()).await.unwrap());
    for payload in payloads() {
        forwarder.send(payload).await;
    }
    let hosted = received_until_quiet(&mut intakes, 13).await;
    forwarder.stop().await;

    // Hand-checked: the primary gets what is not metrics, the override every metrics payload the routing allows,
    // and the additional endpoint, under each key, all but what the routing excludes it from.
    let bodies = |requests: &[(String, String, Bytes)]| {
        requests
            .iter()
            .map(|(_, key, body)| (key.clone(), String::from_utf8(body.to_vec()).unwrap()))
            .collect::<Set<_>>()
    };
    let expected = |entries: &[(&str, &str)]| {
        entries
            .iter()
            .map(|(key, body)| (key.to_string(), body.to_string()))
            .collect::<Set<_>>()
    };
    assert_eq!(bodies(&native[0]), expected(&[("primary-key", "logs")]));
    assert_eq!(
        bodies(&native[1]),
        expected(&[
            ("primary-key", "series"),
            ("primary-key", "sketches"),
            ("primary-key", "series for the primary's policy"),
            ("primary-key", "series without the additional endpoint's policy"),
        ])
    );
    let mut additional = Vec::new();
    for key in ["first-key", "second-key"] {
        for body in ["logs", "series", "sketches", "logs without the primary's policy"] {
            additional.push((key, body));
        }
    }
    assert_eq!(bodies(&native[2]), expected(&additional));
    for (index, (native, hosted)) in native.iter().zip(&hosted).enumerate() {
        assert_eq!(native, hosted, "intake {index} received something else from the host");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proxy_credential_change_or_new_settings_switch_the_host_to_a_new_client_and_a_failed_build_keeps_the_old() {
    use headers::{Authorization, HeaderMapExt as _};

    use super::ProxyEndpoint;

    let _ = saluki_tls::initialize_default_crypto_provider();
    // The proxy records the `CONNECT` of each request to an `https` target, which carries the proxy's authorization;
    // what follows fails, since the proxy does not tunnel.
    let (proxy, mut proxied) = start_recording_intake().await;
    let mut settings = base_configuration().delivery_settings();
    settings.proxy.https = Some(ProxyEndpoint {
        url: proxy.clone(),
        credential: "native:proxy.https".into(),
    });
    let options = Options {
        credentials: vec![("native:proxy.https", "user:first")],
        settings: Some(settings.clone()),
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let authorization = |password: &str| {
        let mut headers = http::HeaderMap::new();
        headers.typed_insert(Authorization::basic("user", password));
        headers.remove(http::header::AUTHORIZATION).unwrap()
    };
    let client = host.shared.client();
    let send = || {
        let request = Request::get("https://target.example.test/")
            .body(http_body_util::Empty::<Bytes>::new())
            .unwrap();
        let mut client = client.clone();
        async move {
            let _ = timeout(WAIT, client.send(request)).await;
        }
    };
    let proxy_authorization = |received: &Received| received.headers.get("proxy-authorization").cloned();

    // The recipients' client sends through the proxy with the credential's user information.
    send().await;
    let first = next_request(&mut proxied, |_| true).await;
    assert_eq!(proxy_authorization(&first), Some(authorization("first")));

    // A rotated proxy password applies without a change of settings: the host builds a new client and switches to it.
    host.credentials.set("native:proxy.https", Some("user:second"));
    timeout(Duration::from_secs(30), async {
        loop {
            send().await;
            let received = next_request(&mut proxied, |_| true).await;
            if proxy_authorization(&received) == Some(authorization("second")) {
                return;
            }
        }
    })
    .await
    .expect("the client should switch to the rotated password");

    // A publication whose proxy cannot be built keeps the client the host has.
    let mut broken = settings.clone();
    broken.proxy.https = Some(ProxyEndpoint {
        url: "http://[::1".to_string(),
        credential: "native:proxy.https".into(),
    });
    host.publish_with(2, &[], &[], &[], Arc::new(broken)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    send().await;
    let kept = next_request(&mut proxied, |_| true).await;
    assert_eq!(proxy_authorization(&kept), Some(authorization("second")));

    // New settings without the proxy switch the client again: requests no longer reach the proxy.
    let mut direct = settings.clone();
    direct.proxy.https = None;
    host.publish_with(3, &[], &[], &[], Arc::new(direct)).await.unwrap();
    timeout(Duration::from_secs(30), async {
        loop {
            while proxied.try_recv().is_ok() {}
            send().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            if proxied.try_recv().is_err() {
                return;
            }
        }
    })
    .await
    .expect("the client should stop sending through the proxy");
    let router = host.stop().await;
    assert_eq!(
        router.credentials_followed(),
        0,
        "the host stops following the proxy's credential"
    );
}

/// Returns the host's settings with an `https` proxy whose URL cannot be parsed, which no client can be built with.
fn with_a_broken_proxy(settings: &DeliverySettings) -> Arc<DeliverySettings> {
    let mut broken = settings.clone();
    broken.proxy.https = Some(super::ProxyEndpoint {
        url: "http://[::1".to_string(),
        credential: "native:proxy.https".into(),
    });
    Arc::new(broken)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_fails_to_build_is_reported_and_built_again_when_its_settings_are_published_again() {
    let mut host = Harness::start(Options::default()).await;
    let broken = with_a_broken_proxy(&host.settings);
    host.publish_with(2, &[], &[], &[], Arc::clone(&broken)).await.unwrap();
    let status = host.wait_for_status(|status| status.client_failure.is_some()).await;
    assert!(
        status
            .client_failure
            .as_deref()
            .is_some_and(|reason| !reason.is_empty()),
        "{status:?}"
    );

    // Republishing the same settings retries the build: a failure need not be permanent.
    host.publish_with(3, &[], &[], &[], broken).await.unwrap();
    let router = host.stop().await;
    assert_eq!(router.client_builds(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_the_current_client_fits_clear_a_failed_build_without_building_again() {
    let mut host = Harness::start(Options::default()).await;
    host.publish_with(2, &[], &[], &[], with_a_broken_proxy(&host.settings))
        .await
        .unwrap();
    host.wait_for_status(|status| status.client_failure.is_some()).await;

    // The settings the host's client was built for: nothing to build, and nothing failed.
    host.publish(3, &[], &[], &[]).await.unwrap();
    host.wait_for_status(|status| status.client_failure.is_none()).await;
    let router = host.stop().await;
    assert_eq!(router.client_builds(), 1);
}

#[tokio::test]
async fn an_aborted_reopen_made_at_the_barrier_returns_the_recipient_to_its_release() {
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let p = recipient(0, &primary, 1);
    let options = Options {
        loop_capacity: 1,
        boot: vec![(p.clone(), primary.clone())],
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let mut primary_loop = host.loops.take(&primary.id);

    // A send waits for room, so the release waits at the barrier; a generation reopens it there, then aborts.
    host.send(named(&[0], "one")).await;
    host.send(named(&[0], "two")).await;
    host.release(std::slice::from_ref(&p)).await;
    host.reopen(GENERATION, &[(p.clone(), primary.clone())]).await;
    assert_eq!(host.state(&p), Some(RecipientState::Live));
    host.abort(GENERATION).await;
    assert_eq!(host.state(&p), Some(RecipientState::Releasing));

    // Its release then completes as if the reopen had not happened: it drains once the payloads before it are routed.
    assert_eq!(body_of(primary_loop.receive().await), "one");
    assert_eq!(body_of(primary_loop.receive().await), "two");
    primary_loop
        .wait_for_close(|request| matches!(request, CloseRequest::Retire { .. }))
        .await;
    host.stop().await;
}

#[tokio::test]
async fn a_publication_reaches_every_running_loop_with_its_settings_and_the_recipients_policy() {
    // Plan 4.10: only a resource transaction's publication changes a live recipient's settings. The host's settings
    // reach every loop, a declared recipient's policy narrows them for its own, and a publication that changes nothing
    // does not wake a loop.
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let team = declared("team_d", TEAM, "team");
    let (p, t) = (recipient(0, &primary, 1), recipient(1, &team, 1));
    let options = Options {
        boot: vec![(p.clone(), primary.clone())],
        native_set: vec![p.clone()],
        credentials: vec![("credentials/team", "team-key")],
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let mut primary_loop = host.loops.take(&primary.id);
    host.prepare(TRANSACTION, &[(t.clone(), team.clone())], &[])
        .await
        .expect("the declared recipient prepares");
    let mut team_loop = host.loops.take(&team.id);
    let (concurrency, buffer, queue) = primary_loop.settings.limits();
    let _ = (primary_loop.settings.changed(), team_loop.settings.changed());

    // New host settings reach both loops; the declared recipient's own queue bound caps its retry queue.
    let mut settings = DeliverySettings::clone(&host.settings);
    settings.endpoint_concurrency = concurrency + 3;
    settings.endpoint_buffer_size = buffer + 5;
    let mut bounded = team.clone();
    bounded.policy.queue_bytes = Some(4096);
    host.publish_with(
        2,
        std::slice::from_ref(&p),
        &[(t.clone(), bounded.clone())],
        &[],
        Arc::new(settings.clone()),
    )
    .await
    .expect("the publication applies");
    assert!(primary_loop.settings.changed() && team_loop.settings.changed());
    let (primary_concurrency, primary_buffer, primary_queue) = primary_loop.settings.limits();
    assert!(
        primary_concurrency > concurrency,
        "the primary runs with the new concurrency"
    );
    assert_eq!(primary_buffer, buffer + 5);
    assert_eq!(primary_queue, queue, "the primary keeps the host's own queue bound");
    let (team_concurrency, team_buffer, team_queue) = team_loop.settings.limits();
    assert_eq!((team_concurrency, team_buffer), (primary_concurrency, primary_buffer));
    assert_eq!(team_queue, 4096, "the declared recipient's policy bounds its queue");

    // The same settings again wake neither loop.
    host.publish_with(
        3,
        std::slice::from_ref(&p),
        &[(t.clone(), bounded)],
        &[],
        Arc::new(settings),
    )
    .await
    .expect("the publication applies");
    assert!(!primary_loop.settings.changed() && !team_loop.settings.changed());
    host.stop().await;
}

#[tokio::test]
async fn an_incarnation_the_host_dropped_is_never_created_again() {
    // Plan 4.7, Prepare row (P7.4 design note R4): a co-creation sent again after an abort dropped the incarnation
    // must not start a second loop under one reference; the next incarnation prepares.
    let team = declared("team_d", TEAM, "team");
    let options = Options {
        credentials: vec![("credentials/team", "team-key")],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let first = recipient(1, &team, 4);
    host.prepare(TRANSACTION, &[(first.clone(), team.clone())], &[])
        .await
        .expect("the recipient prepares");
    let mut first_loop = host.loops.take(&team.id);
    host.abort(TRANSACTION).await;
    first_loop
        .wait_for_close(|request| request == CloseRequest::Shutdown)
        .await;
    first_loop.exit(RecipientExit::Retired(DrainReport::default()));
    assert_eq!(
        host.event().await,
        exited(&first, RecipientExit::Retired(DrainReport::default()))
    );

    let error = rejection(host.prepare(GENERATION, &[(first.clone(), team.clone())], &[]).await);
    assert!(error.contains("creates an incarnation once"), "{error}");
    let older = recipient(1, &team, 3);
    let error = rejection(host.prepare(GENERATION, &[(older, team.clone())], &[]).await);
    assert!(error.contains("creates an incarnation once"), "{error}");
    assert_eq!(host.loops.count(), 0, "no loop was spawned for a refused create");

    let next = recipient(1, &team, 5);
    host.prepare(GENERATION, &[(next.clone(), team.clone())], &[])
        .await
        .expect("the next incarnation prepares");
    assert_eq!(host.state(&next), Some(RecipientState::Live));
    host.stop().await;
}

#[tokio::test]
async fn an_aborted_next_incarnation_of_a_failed_recipient_gives_the_slot_back_to_the_failed_one() {
    // Plan 4.7, Fail row (P7.4 design note R4): leases keep naming a failed recipient's slot, so an abort that drops
    // its next incarnation puts the failed marker back, and payloads naming the slot are dropped and counted, rather
    // than finding an empty slot.
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let failed = recipient(0, &primary, 1);
    let options = Options {
        boot: vec![(failed.clone(), primary.clone())],
        native_set: vec![failed.clone()],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let mut failed_loop = host.loops.take(&primary.id);
    failed_loop.exit(RecipientExit::Failed("broken".to_string()));
    host.event().await;

    let next = recipient(0, &primary, 2);
    host.prepare(TRANSACTION, &[(next.clone(), primary.clone())], &[])
        .await
        .expect("the next incarnation takes the failed slot");
    let mut next_loop = host.loops.take(&primary.id);
    host.abort(TRANSACTION).await;
    next_loop
        .wait_for_close(|request| request == CloseRequest::Shutdown)
        .await;
    assert_eq!(host.state(&failed), Some(RecipientState::Failed));
    assert_eq!(host.state(&next), None);

    // A payload naming the slot is dropped for the failed recipient (in a debug build, an empty slot would fail the
    // routing loop's assertion). Once the dropped loop's exit is reported, a later incarnation takes the slot.
    host.send(named(&[0], "dropped")).await;
    next_loop.exit(RecipientExit::Retired(DrainReport::default()));
    assert_eq!(
        host.event().await,
        exited(&next, RecipientExit::Retired(DrainReport::default()))
    );
    assert!(next_loop.received_nothing());
    let later = recipient(0, &primary, 3);
    host.prepare(TRANSACTION, &[(later.clone(), primary.clone())], &[])
        .await
        .expect("a later incarnation takes the failed slot");
    let mut later_loop = host.loops.take(&primary.id);
    host.send(named(&[0], "delivered")).await;
    assert_eq!(body_of(later_loop.receive().await), "delivered");
    host.stop().await;
}

/// Sends a preparation of `recipients` for `creator` without waiting for its acknowledgment.
fn send_prepare(
    host: &Harness, creator: Creator, recipients: &[(RecipientRef, RecipientSpec)],
) -> JoinHandle<Result<(), ControlError>> {
    let recipients = recipients
        .iter()
        .map(|(recipient, spec)| PreparedRecipient {
            recipient: recipient.clone(),
            spec: spec.clone(),
            pieces: host
                .shared
                .build_pieces(spec, &host.settings)
                .expect("the recipient's pieces build"),
        })
        .collect();
    tokio::spawn(host.control.send(DeliveryControl::Prepare {
        creator,
        recipients,
        requires: Vec::new(),
    }))
}

#[tokio::test]
async fn a_preparation_on_a_directory_a_dropped_loop_owns_waits_for_its_exit_and_an_abort_ends_the_wait() {
    // A newer candidate cancels the preparation that created a declared recipient first, and its own preparation
    // creates the recipient's next incarnation at once: the dropped loop still owns the queue directory, so the
    // preparation waits for its exit rather than failing, which would reject the resources revision.
    let team = declared("team_d", TEAM, "team");
    let options = Options {
        credentials: vec![("credentials/team", "team-key")],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let (cancelled, next, aborted) = (
        Creator::Generation(GenerationId::new(5)),
        Creator::Generation(GenerationId::new(6)),
        Creator::Generation(GenerationId::new(7)),
    );
    let first = recipient(1, &team, 4);
    host.prepare(cancelled, &[(first.clone(), team.clone())], &[])
        .await
        .expect("the first incarnation prepares");
    let mut first_loop = host.loops.take(&team.id);
    host.abort(cancelled).await;

    let second = recipient(2, &team, 5);
    let waiting = send_prepare(&host, next, &[(second.clone(), team.clone())]);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !waiting.is_finished(),
        "the preparation waits for the dropped loop's exit"
    );
    assert_eq!(host.state(&second), None);
    assert_eq!(
        host.loops.count(),
        0,
        "no loop runs on the directory beside the dropped one"
    );

    first_loop
        .wait_for_close(|request| request == CloseRequest::Shutdown)
        .await;
    first_loop.exit(retired());
    assert_eq!(host.event().await, exited(&first, retired()));
    timeout(WAIT, waiting)
        .await
        .expect("the preparation proceeds once the dropped loop exited")
        .unwrap()
        .expect("the next incarnation prepares on the directory");
    assert_eq!(host.state(&second), Some(RecipientState::Live));
    let mut second_loop = host.loops.take(&team.id);

    // A preparation that waits so is rejected by an abort of its own preparation, and adds nothing.
    host.abort(next).await;
    let third = recipient(3, &team, 6);
    let waiting = send_prepare(&host, aborted, &[(third.clone(), team.clone())]);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !waiting.is_finished(),
        "the preparation waits for the dropped loop's exit"
    );
    host.abort(aborted).await;
    let error = rejection(timeout(WAIT, waiting).await.expect("the abort answers it").unwrap());
    assert!(error.contains("was aborted while it waited"), "{error}");
    second_loop.exit(retired());
    assert_eq!(host.event().await, exited(&second, retired()));
    assert_eq!(host.state(&third), None);
    assert_eq!(host.loops.count(), 0, "the aborted preparation spawned no loop");
    host.stop().await;
}

#[tokio::test]
async fn a_failed_incarnation_its_committed_successor_replaced_is_let_go() {
    // A failed incarnation is kept inside its successor's entry only to be put back if an abort drops the
    // successor: once a publication commits the successor, nothing can, and the failed one's pieces are dropped
    // (its closer, its settings watch, its endpoint), rather than nesting one level deeper at every failure.
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let first = recipient(0, &primary, 1);
    let options = Options {
        boot: vec![(first.clone(), primary.clone())],
        native_set: vec![first.clone()],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let mut first_loop = host.loops.take(&primary.id);
    first_loop.exit(RecipientExit::Failed("broken 1".to_string()));
    host.event().await;

    let second = recipient(0, &primary, 2);
    host.prepare(Creator::Transaction(10), &[(second.clone(), primary.clone())], &[])
        .await
        .unwrap();
    assert_eq!(
        first_loop.close_request(),
        CloseRequest::None,
        "kept, to be put back if the successor is aborted"
    );
    host.publish(10, std::slice::from_ref(&second), &[], &[]).await.unwrap();
    assert_eq!(
        first_loop.close_request(),
        CloseRequest::Shutdown,
        "let go once the successor is committed"
    );

    let mut second_loop = host.loops.take(&primary.id);
    second_loop.exit(RecipientExit::Failed("broken 2".to_string()));
    host.event().await;
    let third = recipient(0, &primary, 3);
    host.prepare(Creator::Transaction(11), &[(third.clone(), primary.clone())], &[])
        .await
        .unwrap();
    host.publish(11, std::slice::from_ref(&third), &[], &[]).await.unwrap();
    assert_eq!(second_loop.close_request(), CloseRequest::Shutdown);
    host.stop().await;
}

#[tokio::test]
async fn a_host_remembers_what_it_created_only_of_the_identities_it_runs_and_the_latest_it_ran() {
    // Distinct identities that come and go leave the host's memory of what it created bounded, while a creation a
    // controller repeats after an abort dropped its incarnation still fails.
    let options = Options {
        credentials: vec![("credentials/team", "team-key")],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let cycles = super::router::REMEMBERED_IDENTITIES + 50;
    for cycle in 0..cycles {
        let spec = declared(&format!("team_{cycle}"), &format!("{TEAM}/{cycle}"), "team");
        let created = recipient(1, &spec, 2 * cycle as u64 + 1);
        host.prepare(TRANSACTION, &[(created.clone(), spec.clone())], &[])
            .await
            .unwrap();
        host.publish(10 + cycle as u64, &[], &[], &[]).await.unwrap();
        host.release(std::slice::from_ref(&created)).await;
        let mut created_loop = host.loops.take(&spec.id);
        created_loop
            .wait_for_close(|request| matches!(request, CloseRequest::Retire { .. }))
            .await;
        created_loop.exit(retired());
        assert_eq!(host.event().await, exited(&created, retired()));
    }

    // The last identity's dropped incarnation is still remembered.
    let last = declared("team_last", &format!("{TEAM}/last"), "team");
    let dropped = recipient(1, &last, 5_000);
    host.prepare(Creator::Transaction(1_000_000), &[(dropped.clone(), last.clone())], &[])
        .await
        .unwrap();
    let mut dropped_loop = host.loops.take(&last.id);
    host.abort(Creator::Transaction(1_000_000)).await;
    dropped_loop.exit(retired());
    host.event().await;
    let error = rejection(host.prepare(GENERATION, &[(dropped.clone(), last.clone())], &[]).await);
    assert!(error.contains("creates an incarnation once"), "{error}");

    let router = host.stop().await;
    let (created, forgotten) = router.remembered_identities();
    assert_eq!(created, 0, "no identity runs any more");
    assert_eq!(forgotten, super::router::REMEMBERED_IDENTITIES);
}

/// Returns the value of every counter named `name` that `recorder` holds, by the value of its `tag` tag.
fn counters_by(recorder: &TestRecorder, name: &str, tag: &str) -> BTreeMap<String, u64> {
    recorder
        .counters()
        .into_iter()
        .filter(|(key, _)| key.name() == name)
        .filter_map(|(key, value)| {
            key.labels()
                .find(|label| label.key() == tag)
                .map(|label| (label.value().to_string(), value))
        })
        .collect()
}

#[tokio::test]
async fn a_retirement_is_counted_by_what_became_of_the_work_the_recipient_held() {
    // A `persist` closure writes the queue to disk at once, delivering nothing: it is no drain.
    let recorder = Arc::new(TestRecorder::default());
    let specs = ["drained", "persisted", "discarded", "deadline"].map(|name| {
        native(
            name,
            MetricsRecipientRoute::Additional,
            &format!("http://{name}.example.test"),
        )
    });
    let recipients = specs
        .iter()
        .enumerate()
        .map(|(slot, spec)| recipient(slot as u16, spec, 1))
        .collect::<Vec<_>>();
    let options = Options {
        credentials: vec![
            ("native:drained", "key"),
            ("native:persisted", "key"),
            ("native:discarded", "key"),
            ("native:deadline", "key"),
        ],
        boot: recipients.iter().cloned().zip(specs.iter().cloned()).collect(),
        recorder: Some(Arc::clone(&recorder)),
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let reports = [
        DrainReport {
            delivered: 2,
            ..DrainReport::default()
        },
        DrainReport {
            persisted: 3,
            ..DrainReport::default()
        },
        DrainReport {
            persisted: 2,
            dropped: 1,
            ..DrainReport::default()
        },
        DrainReport {
            persisted: 1,
            deadline_reached: true,
            ..DrainReport::default()
        },
    ];
    host.release(&recipients).await;
    for ((spec, released), report) in specs.iter().zip(&recipients).zip(reports) {
        let mut released_loop = host.loops.take(&spec.id);
        released_loop
            .wait_for_close(|request| matches!(request, CloseRequest::Retire { .. }))
            .await;
        released_loop.exit(RecipientExit::Retired(report));
        assert_eq!(host.event().await, exited(released, RecipientExit::Retired(report)));
    }
    let expected = ["deadline", "discarded", "drained", "persisted"]
        .map(|outcome| (outcome.to_string(), 1))
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        counters_by(&recorder, "plan_recipient_retirements_total", "outcome"),
        expected
    );
    host.stop().await;
}

#[tokio::test]
async fn a_payload_no_recipient_of_the_native_set_takes_is_counted_as_dropped() {
    // Without a primary endpoint, nothing takes logs, events, service checks, or traces: they are dropped and counted,
    // as a native forwarder counts what it cannot send, rather than vanishing.
    let recorder = Arc::new(TestRecorder::default());
    let metrics_override = native("override", MetricsRecipientRoute::MetricsOverride, OVERRIDE);
    let o = recipient(0, &metrics_override, 1);
    let options = Options {
        boot: vec![(o.clone(), metrics_override.clone())],
        native_set: vec![o.clone()],
        recorder: Some(Arc::clone(&recorder)),
        ..Options::default()
    };
    let host = Harness::start(options).await;
    let mut override_loop = host.loops.take(&metrics_override.id);
    host.send(payload(LOGS, None, None, "logs")).await;
    host.send(payload(LOGS, None, None, "more logs")).await;
    host.send(payload(SERIES, None, Some(MetricsPayloadInfo::v2_series()), "series"))
        .await;
    assert_eq!(body_of(override_loop.receive().await), "series");
    assert!(override_loop.received_nothing());
    assert_eq!(
        counters_by(&recorder, "component_errors_total", "error_type").get("http_send"),
        Some(&2)
    );
    host.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_real_loops_an_identitys_next_incarnation_prepared_right_after_an_abort_waits_for_the_dropped_loop() {
    // As the installer sends them when a newer candidate cancels a preparation: the abort and the preparation of the
    // recipient's next incarnation, back to back, without awaiting the abort. A real loop the abort drops closes as at
    // shutdown and still owns its queue directory for a while; the preparation waits for its exit, every time.
    use super::{BootPublication as Boot, DeliveryHostConfiguration};
    use crate::forwarders::DatadogForwarderConfiguration;

    let (intake_url, mut requests) = start_recording_intake().await;
    let primary = native("primary", MetricsRecipientRoute::Primary, &intake_url);
    let team = declared("team_d", &format!("{intake_url}/team"), "team");
    let p = recipient(0, &primary, 1);
    let credentials = TestCredentials::new(&[("native:primary", "boot-key"), ("credentials/team", "team-key")]);
    let mut shared = shared_configuration();
    shared.endpoints.tls.skip_ssl_validation = true;
    let settings = Arc::new(ForwarderConfiguration::from_configuration(&shared).delivery_settings());
    let (control, mailbox) = control_mailbox();
    let (events_tx, mut events) = mpsc::channel(64);
    let host = DeliveryHostConfiguration::new(
        mailbox,
        credentials.clone(),
        Boot {
            revision: 1,
            settings: Arc::clone(&settings),
            recipients: vec![settings_of(&p, &primary)],
            native_set: vec![p.clone()],
            withdrawn: BTreeSet::new(),
        },
    )
    .with_events(events_tx);
    let factory = host.factory();
    let mut status = host.status();
    let config = DatadogForwarderConfiguration::controlled(&shared, Live::new_fixed(Default::default()), host);
    let forwarder = RunningForwarder::start(&config).await;
    assert!(timeout(WAIT, control.running()).await.unwrap());
    let prepare = |creator: Creator, recipient: &RecipientRef| {
        control.send(DeliveryControl::Prepare {
            creator,
            recipients: vec![PreparedRecipient {
                recipient: recipient.clone(),
                spec: team.clone(),
                pieces: factory.build(&team, &settings).expect("the recipient's pieces build"),
            }],
            requires: Vec::new(),
        })
    };

    let mut next = None;
    for round in 0..5u64 {
        let (cancelled, newer) = (GenerationId::new(10 + 2 * round), GenerationId::new(11 + 2 * round));
        let first = recipient(1, &team, 2 + 2 * round);
        let second = recipient(2, &team, 3 + 2 * round);
        timeout(WAIT, prepare(Creator::Generation(cancelled), &first))
            .await
            .unwrap()
            .expect("the first incarnation prepares");
        let abort = control.send(DeliveryControl::Abort { generation: cancelled });
        let prepared = prepare(Creator::Generation(newer), &second);
        timeout(WAIT, abort).await.unwrap().unwrap();
        timeout(WAIT, prepared)
            .await
            .unwrap()
            .unwrap_or_else(|error| panic!("round {round}: the next incarnation failed to prepare: {error:#}"));
        let event = timeout(WAIT, events.recv()).await.unwrap().unwrap();
        assert!(
            matches!(&event, DeliveryEvent::RecipientExited { recipient, .. } if *recipient == first),
            "the dropped incarnation exited before its successor prepared: {event:?}"
        );
        next = Some(second.clone());
        // Each round but the last drops the successor too, so that the next round starts from a directory its loop
        // owned.
        if round < 4 {
            timeout(WAIT, control.send(DeliveryControl::Abort { generation: newer }))
                .await
                .unwrap()
                .unwrap();
            let event = timeout(WAIT, events.recv()).await.unwrap().unwrap();
            assert!(matches!(&event, DeliveryEvent::RecipientExited { recipient, .. } if *recipient == second));
        }
    }

    // The last successor runs on the directory, and delivers what names it.
    let last = next.expect("a successor prepared");
    timeout(
        WAIT,
        status.wait_for(|status| status.recipients.iter().any(|held| held.recipient == last)),
    )
    .await
    .unwrap()
    .unwrap();
    forwarder.send(named(&[2], "to the successor")).await;
    let received = next_request(&mut requests, |received| {
        received.uri.ends_with(SERIES) && received.headers.get("dd-api-key").is_some_and(|key| key == "team-key")
    })
    .await;
    assert_eq!(received.body, Bytes::from_static(b"to the successor"));
    forwarder.stop().await;
}

#[tokio::test]
async fn a_failed_recipient_takes_its_slot_back_after_its_next_incarnation_failed_too_and_both_successors_are_aborted()
{
    // The failed marker the leases name returns even when the incarnation that replaced it failed before it was
    // committed, and a third replaced that one: an abort of both successors puts back the newest one still held.
    let primary = native("primary", MetricsRecipientRoute::Primary, PRIMARY);
    let first = recipient(0, &primary, 1);
    let options = Options {
        boot: vec![(first.clone(), primary.clone())],
        native_set: vec![first.clone()],
        ..Options::default()
    };
    let mut host = Harness::start(options).await;
    let mut first_loop = host.loops.take(&primary.id);
    first_loop.exit(RecipientExit::Failed("broken 1".to_string()));
    host.event().await;

    let second = recipient(0, &primary, 2);
    host.prepare(Creator::Transaction(10), &[(second.clone(), primary.clone())], &[])
        .await
        .unwrap();
    let mut second_loop = host.loops.take(&primary.id);
    second_loop.exit(RecipientExit::Failed("broken 2".to_string()));
    host.event().await;
    let third = recipient(0, &primary, 3);
    host.prepare(Creator::Transaction(11), &[(third.clone(), primary.clone())], &[])
        .await
        .unwrap();
    let mut third_loop = host.loops.take(&primary.id);

    host.abort(Creator::Transaction(10)).await;
    host.abort(Creator::Transaction(11)).await;
    third_loop
        .wait_for_close(|request| request == CloseRequest::Shutdown)
        .await;
    assert_eq!(
        host.state(&first),
        Some(RecipientState::Failed),
        "the committed marker is back"
    );
    assert_eq!(host.state(&second), None);
    assert_eq!(host.state(&third), None);
    // A payload naming the slot is dropped for the failed marker, not for an empty slot.
    host.send(named(&[0], "dropped")).await;
    third_loop.exit(retired());
    assert_eq!(host.event().await, exited(&third, retired()));
    let fourth = recipient(0, &primary, 4);
    host.prepare(Creator::Transaction(12), &[(fourth.clone(), primary.clone())], &[])
        .await
        .expect("the next incarnation takes the failed slot");
    host.stop().await;
}
