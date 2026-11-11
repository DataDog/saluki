//! Tests covering the source's background work running as supervised children.
//!
//! The other test modules exercise the decode path in isolation. Here, the wiring is the important part. These tests
//! check that `run` puts the pool shrinker, datagram decoders, listeners, and per-connection handlers below the source
//! as its own children. They also check that the whole subtree stops cleanly and does not drop queued work.

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use bytes::BytesMut;
use saluki_core::{
    components::test_util::{ComponentControl, DriverOptions, NoInput, Outputs, PrebuiltSource, TestComponentDriver},
    data_model::event::metric::context::{ContextResolverBuilder, TagsResolverBuilder},
    runtime::{state::ResourceRegistry, NodeSnapshot, NodeState, SupervisorError},
    support::SubsystemIdentifier,
    topology::EventsBuffer,
};
use saluki_io::net::listener::Listener;
use saluki_io::net::{BoundListenAddress, ListenAddress};
use stringtheory::MetaString;
use tokio::net::UdpSocket;
use tokio::time::timeout;

use crate::sources::dogstatsd::*;

/// Bound on socket operations, so a hang fails rather than stalling the suite.
const RUN_TIMEOUT: Duration = Duration::from_secs(10);

/// ID of the source under test, which names its worker `sources.dogstatsd` in the supervision tree.
const COMPONENT_ID: &str = "dogstatsd";

/// Decoder workers the test source runs with.
///
/// More than one, so the drain covers the real case of several decoders sharing the queue.
const DECODER_WORKERS: usize = 2;

/// How long to let datagrams make their way through to the (single-slot) output before asserting on backpressure.
const BACKPRESSURE_SETTLE: Duration = Duration::from_millis(250);

/// How long a drain test holds a decoder mid-dispatch after shutdown.
///
/// This is longer than five seconds, the default timeout of a closure-based child. Thus, the drain survives the hold
/// only if the decoders have no timeout of their own.
const DRAIN_HOLD: Duration = Duration::from_secs(6);

/// Shutdown timeout of the component in a drain test. It must be longer than [`DRAIN_HOLD`].
const DRAIN_COMPONENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Children a running source with one UDP listener should have.
///
/// These children are:
///
/// - the pool shrinker
/// - one child for each decoder worker
/// - the listener
/// - one stream handler, below the listener
///
/// For a connectionless listener, `accept` returns its socket immediately. Thus, its handler exists from the start,
/// and not only after a peer appears.
const fn udp_child_count() -> usize {
    1 + DECODER_WORKERS + 1 + 1
}

/// Processes that run below the component's supervisor while a source with one UDP listener runs.
///
/// These processes are the source itself and its children.
const fn udp_descendant_count() -> usize {
    1 + udp_child_count()
}

/// Runs `source` through the driver with `options`, declaring the outputs that its configuration declares.
async fn run_source(
    options: DriverOptions, source: Box<DogStatsD>,
) -> (ComponentControl<NoInput>, Outputs<EventsBuffer>) {
    let outputs = DogStatsDConfiguration::for_test().outputs().to_vec();
    options
        .with_component_id(COMPONENT_ID)
        .source(PrebuiltSource::new(source, outputs))
        .await
        .expect("the pre-built source should build")
}

/// Counts the running processes below `node`.
fn running_beneath(node: &NodeSnapshot) -> usize {
    node.children
        .iter()
        .map(|child| usize::from(child.state == NodeState::Running) + running_beneath(child))
        .sum()
}

/// Finds the first node with the name `name` below `node`, in depth-first order.
fn find_node<'a>(node: &'a NodeSnapshot, name: &str) -> Option<&'a NodeSnapshot> {
    node.children.iter().find_map(|child| {
        if child.name == name {
            Some(child)
        } else {
            find_node(child, name)
        }
    })
}

/// Builds a source with a single UDP listener bound to an ephemeral port, and returns it with that port's address.
///
/// The source is built by hand, since its configuration can't ask for an ephemeral port: `build_addresses` treats
/// port 0 as disabled.
async fn build_source() -> (Box<DogStatsD>, SocketAddr) {
    let listener = leased_listener(ListenAddress::udp_loopback(0)).await;
    let listen_addr = match listener.bound_listen_address() {
        BoundListenAddress::Udp(addr) => addr,
        other => panic!("expected a bound UDP address, got {other:?}"),
    };

    let io_buffer_count = 16;
    let (io_buffer_pool, io_buffer_pool_shrinker) = build_io_buffer_pool(
        io_buffer_count,
        io_buffer_count,
        DogStatsDConfiguration::for_test().buffer_size,
    );

    let tags_resolver = TagsResolverBuilder::for_tests().build();
    let context_resolver = ContextResolverBuilder::for_tests()
        .with_tags_resolver(Some(tags_resolver.clone()))
        .build();

    let source = Box::new(DogStatsD {
        listeners: vec![listener],
        decoder_worker_count: NonZeroUsize::new(DECODER_WORKERS).expect("decoder count should be non-zero"),
        io_buffer_pool,
        io_buffer_queue_capacity: 16,
        io_buffer_pool_shrinker: Box::pin(io_buffer_pool_shrinker),
        codec: DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default()),
        context_resolvers: ContextResolvers::manual(context_resolver.clone(), context_resolver, tags_resolver),
        default_hostname: MetaString::from_static("test-host"),
        enabled_filter: EnablePayloadsFilter::default(),
        origin_detection_enabled: false,
        origin_telemetry_enabled: false,
        stream_log_too_big: false,
        disable_verbose_logs: false,
        eol_required: EolRequired::default(),
        additional_tags: Vec::<String>::new().into(),
        capture_entity_resolver: None,
        traffic_capture: TrafficCapture::new(PathBuf::new(), 1),
        packet_forwarder_target: None,
    });

    (source, listen_addr)
}

/// Acquires a listener the way a real build does, so the source holds a lease rather than a bare listener.
///
/// A registry per call is enough for a test that only needs one holder: a `ResourceLease` keeps its own clone of
/// the registry, so the registry outlives the lease without the caller holding it.
async fn leased_listener(address: ListenAddress) -> ResourceLease<Listener> {
    leased_listener_from(&ResourceRegistry::new(), address).await
}

/// Acquires a listener from a specific registry, for a test that cares about what the registry does afterwards.
async fn leased_listener_from(registry: &ResourceRegistry, address: ListenAddress) -> ResourceLease<Listener> {
    registry
        .acquire(
            &SubsystemIdentifier::from_segments(["test", COMPONENT_ID]),
            SocketSpecification::new(address),
        )
        .await
        .expect("listener should bind")
}

#[tokio::test]
async fn background_work_runs_as_the_sources_own_children() {
    // The pool shrinker, each datagram decoder, and each listener are children of the source. They are not detached
    // tasks or siblings. Thus, all of them appear below the source while it runs, and none of them remain after it
    // stops.
    let (source, _) = build_source().await;
    let (control, _outputs) = run_source(TestComponentDriver::options(), source).await;
    control
        .supervisor()
        .wait_for_running_descendants(udp_descendant_count())
        .await;

    let root = control.supervisor().tree_handle().snapshot().root;
    let source_node = find_node(&root, "sources.dogstatsd").expect("the source is running");
    assert_eq!(running_beneath(source_node), udp_child_count());

    // If the error is `ShutdownTimedOut`, a child ignored shutdown and was aborted. The pool shrinker is a deliberate
    // exception. It stops brutally, and a brutal stop does not count as forced.
    control
        .shutdown()
        .await
        .expect("every child should have stopped on its own");
}

#[tokio::test]
async fn source_does_not_finish_until_the_decoders_have_drained() {
    // The drain guarantee that matters: work already queued when shutdown is signalled must still be decoded and
    // dispatched. The decoders deliberately ignore shutdown for exactly this reason. They exit only after the
    // listeners drop their senders and the queue is empty. The source owns the decoders, and it does not finish
    // until they exit.
    //
    // Rather than race a datagram against shutdown, this pins a decoder open: the metrics output holds a single
    // buffer, so one dispatched buffer fills it and the next blocks a decoder mid-dispatch. The source must not
    // finish while that is true, even after the test signals shutdown. The test holds the decoder there for longer
    // than the default timeout of a closure-based child, but for less than the shutdown timeout of the component.
    let (source, listen_addr) = build_source().await;
    let options = TestComponentDriver::options()
        .with_shutdown_timeout(DRAIN_COMPONENT_TIMEOUT)
        .with_interconnect_capacity(NonZeroUsize::MIN);
    let (mut control, mut outputs) = run_source(options, source).await;
    let mut metrics = outputs.take_named_output("metrics");
    control
        .supervisor()
        .wait_for_running_descendants(udp_descendant_count())
        .await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client should bind");

    // Establish that the pipeline works, and leave the output empty again.
    client
        .send_to(b"first.metric:1|c", listen_addr)
        .await
        .expect("client should send");
    let buffer = metrics.next().await.expect("the metrics output should stay open");
    assert_eq!(buffer.len(), 1);

    // Now back the output up with a single datagram holding one more metric than an event buffer can hold. The
    // decoder that receives it dispatches a full buffer as soon as the buffer fills, which takes the single slot,
    // and then blocks dispatching the remaining metric, either on its next flush or when it drains at shutdown.
    // This doesn't depend on which decoder receives the datagram, or on when decoders flush.
    let backed_up_metrics = EventsBuffer::default().capacity() + 1;
    // Short metric lines keep the datagram within the source's 8 KiB receive buffer.
    let payload = vec!["m:1|c"; backed_up_metrics].join("\n");
    client
        .send_to(payload.as_bytes(), listen_addr)
        .await
        .expect("client should send");
    tokio::time::sleep(BACKPRESSURE_SETTLE).await;

    // From here on, the test does not wait on the socket. Thus, it can pause the clock, and the hold below then
    // takes no real time.
    tokio::time::pause();
    control.signal_shutdown();

    // The body of the source already returned. But the source still waits for its blocked decoder, and its
    // supervisor also waits. Without the drain, the source and the supervisor finish at this point. If the
    // decoders had the default timeout of a closure-based child, the scope of the source aborts them during the
    // hold.
    tokio::time::sleep(DRAIN_HOLD).await;
    assert!(
        !control.supervisor().is_finished(),
        "the source finished while a decoder was still mid-dispatch"
    );

    // The test drains the output, and this unblocks the decoder. Then the drain completes, and the source also
    // completes. The output closes after the source and all of its decoders stop.
    let drained = 1 + metrics.collect().await.iter().map(EventsBuffer::len).sum::<usize>();

    // Nothing accepted before shutdown should have been dropped on the way out.
    assert_eq!(
        drained,
        1 + backed_up_metrics,
        "every queued metric should have been dispatched"
    );

    control
        .wait()
        .await
        .expect("every child should have stopped on its own");
}

#[tokio::test]
async fn a_source_ignoring_shutdown_is_halted_with_its_children() {
    // In the orderly path, the body of the source returns when it receives the shutdown signal, and this stops its
    // children. This test covers the other path: a source that never sees the signal is halted along with
    // everything below it.
    //
    // This cannot occur in production, because there the shutdown signal of a source *is* the signal of its
    // worker, so the test withholds the signal from the source.
    let (source, _) = build_source().await;
    let options = TestComponentDriver::options()
        .with_shutdown_timeout(Duration::from_millis(500))
        .with_shutdown_withheld();
    let (control, _outputs) = run_source(options, source).await;
    control
        .supervisor()
        .wait_for_running_descendants(udp_descendant_count())
        .await;

    let error = control
        .shutdown()
        .await
        .expect_err("the source should be halted once its timeout runs out");
    match error.downcast_ref::<SupervisorError>() {
        // The body never returned, so every child is still running when the source is halted. All are counted
        // except the brutal pool shrinker.
        Some(SupervisorError::ShutdownTimedOut { aborted }) => assert_eq!(*aborted, udp_descendant_count() - 1),
        other => panic!("expected the source to be halted and counted, got {other:?}"),
    }
}

#[tokio::test]
async fn stream_handlers_are_the_listeners_own_children() {
    // Per-connection handlers are also supervised, with one fixed name for each listener type. Each handler belongs
    // to the listener that accepted its connection. When the listener accepts a connection, it adds a handler. When
    // the source stops, its handlers stop with everything else.

    // A TCP listener, so there are real connections to accept.
    let tcp_listener = leased_listener(ListenAddress::Tcp("127.0.0.1:0".parse().unwrap())).await;
    let listen_addr = match tcp_listener.bound_listen_address() {
        BoundListenAddress::Tcp(addr) => addr,
        other => panic!("expected a bound TCP address, got {other:?}"),
    };

    let (mut source, _) = build_source().await;
    source.listeners = vec![tcp_listener];
    let (control, _outputs) = run_source(TestComponentDriver::options(), source).await;

    // The baseline is the source, the pool shrinker, the decoders, and the listener.
    let baseline = 1 + 1 + DECODER_WORKERS + 1;
    control.supervisor().wait_for_running_descendants(baseline).await;

    let _client = tokio::net::TcpStream::connect(listen_addr)
        .await
        .expect("client should connect");

    // Each connection adds two processes. One is the handler, below the listener. The other is the stream reader
    // that the handler starts below itself.
    control.supervisor().wait_for_running_descendants(baseline + 2).await;
    let root = control.supervisor().tree_handle().snapshot().root;
    let listener_node = find_node(&root, "listener_tcp").expect("the listener is running");
    let handler_node = find_node(listener_node, "conn_tcp")
        .expect("the handler should hang beneath the listener that accepted its connection");
    assert!(
        find_node(handler_node, "stream_reader").is_some(),
        "the stream reader should hang beneath the handler that started it"
    );

    control
        .shutdown()
        .await
        .expect("every child should have stopped on its own");
}

#[tokio::test]
async fn the_registry_keeps_the_listener_bound_after_the_source_stops() {
    // The property the whole migration is for. The source only ever borrows its listener, so stopping it hands the
    // socket back to the registry still bound, rather than releasing the port to the operating system and leaving
    // a rebuilt source to race whatever else wants it.

    // A registry the test keeps, so it can ask for the same address once the source is gone.
    //
    // Port 0 is what makes the assertion below mean anything: the specification is keyed on the address as asked
    // for, so re-acquiring it can only come back on the same port if the registry really held the socket. A fixed
    // port could be re-bound from scratch and look identical.
    let registry = ResourceRegistry::new();
    let address = ListenAddress::udp_loopback(0);

    let (mut source, _) = build_source().await;
    source.listeners = vec![leased_listener_from(&registry, address.clone()).await];
    let bound = source.listeners[0].bound_listen_address();
    let listen_addr = match bound {
        BoundListenAddress::Udp(addr) => addr,
        other => panic!("expected a bound UDP address, got {other:?}"),
    };

    let (control, _outputs) = run_source(TestComponentDriver::options(), source).await;
    control
        .supervisor()
        .wait_for_running_descendants(udp_descendant_count())
        .await;
    control.shutdown().await.expect("the source should stop cleanly");

    // The same socket, not a lucky re-bind: the registry never let go of it.
    let mut listener = leased_listener_from(&registry, address).await;
    assert_eq!(listener.bound_listen_address(), bound);

    // And it is still usable. A listener whose socket had been moved out by the previous holder rather than
    // borrowed would come back exhausted and hang here instead of yielding anything.
    let mut stream = timeout(RUN_TIMEOUT, listener.accept())
        .await
        .expect("accept should not hang once the source has released its lease")
        .expect("listener should yield its socket");

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client should bind");
    client
        .send_to(b"after.teardown:1|c", listen_addr)
        .await
        .expect("client should send");

    let mut buf = BytesMut::with_capacity(64);
    let (read, _) = timeout(RUN_TIMEOUT, stream.receive(&mut buf))
        .await
        .expect("receive should not time out")
        .expect("receive should succeed");
    assert_eq!(&buf[..read], b"after.teardown:1|c");
}

/// Socket paths for a DogStatsD source, in a temporary directory unique to the test.
///
/// Socket paths rather than ports: the configuration has to name the address concretely for the build to ask
/// for the same one, and a path in a temporary directory is unique by construction, so there is no ephemeral
/// port to discover and race over.
#[cfg(unix)]
struct ListenerPaths {
    _temp_dir: tempfile::TempDir,
    datagram: PathBuf,
    stream: PathBuf,
}

#[cfg(unix)]
impl ListenerPaths {
    fn new() -> Self {
        let temp_dir = tempfile::tempdir().expect("temp directory should be created");
        Self {
            datagram: temp_dir.path().join("dogstatsd.socket"),
            stream: temp_dir.path().join("dogstatsd-stream.socket"),
            _temp_dir: temp_dir,
        }
    }

    /// Configures a source to listen on both socket paths.
    fn configure(&self, requirement: ListenerRequirement) -> DogStatsDConfiguration {
        let mut config = listenerless_config(requirement);
        config.socket_path = Some(self.datagram.to_string_lossy().into_owned());
        config.socket_stream_path = Some(self.stream.to_string_lossy().into_owned());
        config
    }
}

#[cfg(unix)]
const LISTENER_REQUIREMENTS: [ListenerRequirement; 3] = [
    ListenerRequirement::All,
    ListenerRequirement::AtLeastOne,
    ListenerRequirement::Optional,
];

#[cfg(unix)]
fn listenerless_config(requirement: ListenerRequirement) -> DogStatsDConfiguration {
    let mut config = DogStatsDConfiguration::for_test();
    // The fixture defaults to the real DogStatsD port, which these tests have no business binding.
    config.port = 0;
    config.listener_requirement = requirement;
    config
}

#[cfg(unix)]
type DriveResult = Result<(ComponentControl<NoInput>, Outputs<EventsBuffer>), GenericError>;

#[cfg(unix)]
async fn drive_source(registry: ResourceRegistry, config: DogStatsDConfiguration) -> DriveResult {
    TestComponentDriver::options()
        .with_component_id(COMPONENT_ID)
        .with_resource_registry(registry)
        .source(config)
        .await
}

#[cfg(unix)]
async fn assert_runs_idle(result: DriveResult, requirement: ListenerRequirement) {
    let (control, _outputs) =
        result.unwrap_or_else(|error| panic!("the source should build without listeners ({requirement:?}): {error:#}"));
    control.wait_until_ready().await;
    assert!(
        !control.is_finished(),
        "the source should keep running without listeners ({requirement:?})"
    );
    control.shutdown().await.expect("the source should stop cleanly");
}

#[cfg(unix)]
fn assert_fails_to_start(result: DriveResult, requirement: ListenerRequirement, expected: &str) {
    let Err(error) = result else {
        panic!("the component build should fail ({requirement:?})");
    };
    let error = format!("{error:#}");
    assert!(error.contains(expected), "unexpected error ({requirement:?}): {error}");
}

#[cfg(unix)]
const NO_LISTENERS: &str = "No listeners could be created";
#[cfg(unix)]
const ALREADY_LEASED: &str = "already leased";

#[cfg(unix)]
#[tokio::test]
async fn zero_io_buffer_capacity_fails_to_start_without_listeners() {
    // Without listeners, no listener reserves a buffer, so the reservation check alone would accept a capacity of
    // zero, and the datagram queue can't be created with no capacity.
    let mut config = listenerless_config(ListenerRequirement::Optional);
    config.buffer_count = 0;
    config.buffer_count_max = 0;

    let result = drive_source(ResourceRegistry::new(), config).await;
    assert_fails_to_start(
        result,
        ListenerRequirement::Optional,
        "maximum I/O buffer count must be greater than zero",
    );
}

#[cfg(unix)]
#[tokio::test]
async fn no_configured_listener_runs_idle_only_when_listeners_are_optional() {
    for requirement in LISTENER_REQUIREMENTS {
        let result = drive_source(ResourceRegistry::new(), listenerless_config(requirement)).await;
        match requirement {
            ListenerRequirement::All | ListenerRequirement::AtLeastOne => {
                assert_fails_to_start(result, requirement, NO_LISTENERS)
            }
            ListenerRequirement::Optional => assert_runs_idle(result, requirement).await,
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn no_created_listener_runs_idle_only_when_listeners_are_optional() {
    // Every configured address is already leased, so the build can create none of them.
    for requirement in LISTENER_REQUIREMENTS {
        let paths = ListenerPaths::new();
        let registry = ResourceRegistry::new();
        let _held_datagram = leased_listener_from(&registry, ListenAddress::Unixgram(paths.datagram.clone())).await;
        let _held_stream = leased_listener_from(&registry, ListenAddress::Unix(paths.stream.clone())).await;

        let result = drive_source(registry, paths.configure(requirement)).await;
        match requirement {
            ListenerRequirement::All => assert_fails_to_start(result, requirement, ALREADY_LEASED),
            ListenerRequirement::AtLeastOne => assert_fails_to_start(result, requirement, NO_LISTENERS),
            ListenerRequirement::Optional => assert_runs_idle(result, requirement).await,
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn listener_that_cannot_be_created_is_skipped_unless_all_are_required() {
    // The stream address is already leased, but the datagram listener can still be created.
    for requirement in LISTENER_REQUIREMENTS {
        let paths = ListenerPaths::new();
        let registry = ResourceRegistry::new();
        let _held_stream = leased_listener_from(&registry, ListenAddress::Unix(paths.stream.clone())).await;

        let result = drive_source(registry, paths.configure(requirement)).await;
        if requirement == ListenerRequirement::All {
            assert_fails_to_start(result, requirement, ALREADY_LEASED);
            continue;
        }

        let (control, mut outputs) = result.unwrap_or_else(|error| {
            panic!("the source should build with the listener it could create ({requirement:?}): {error:#}")
        });
        control.wait_until_ready().await;
        assert_datagram_listener_receives(&mut outputs, &paths.datagram).await;
        control.shutdown().await.expect("the source should stop cleanly");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unresolvable_bind_host_skips_only_udp_and_tcp_unless_all_are_required() {
    // An unresolvable `bind_host` prevents the UDP listener, but the datagram socket listener can still be created.
    for requirement in LISTENER_REQUIREMENTS {
        let paths = ListenerPaths::new();
        let mut config = listenerless_config(requirement);
        config.port = 8125;
        config.bind_host = Some("unresolvable.invalid".to_string());
        config.socket_path = Some(paths.datagram.to_string_lossy().into_owned());

        let result = drive_source(ResourceRegistry::new(), config).await;
        if requirement == ListenerRequirement::All {
            assert_fails_to_start(result, requirement, "Could not resolve bind_host");
            continue;
        }

        let (control, mut outputs) = result.unwrap_or_else(|error| {
            panic!("the source should build with the socket listener ({requirement:?}): {error:#}")
        });
        control.wait_until_ready().await;
        assert_datagram_listener_receives(&mut outputs, &paths.datagram).await;
        control.shutdown().await.expect("the source should stop cleanly");
    }
}

/// Sends a metric to the datagram socket at `path`, and asserts that the source emits it.
#[cfg(unix)]
async fn assert_datagram_listener_receives(outputs: &mut Outputs<EventsBuffer>, path: &std::path::Path) {
    let sender = std::os::unix::net::UnixDatagram::unbound().expect("sender should be created");
    sender
        .send_to(b"survivor:1|c", path)
        .expect("the datagram listener should accept a metric");
    let events = timeout(RUN_TIMEOUT, outputs.named_output("metrics").next())
        .await
        .expect("the metric should arrive")
        .expect("the metrics output should stay open");
    let metric = events
        .into_iter()
        .next()
        .and_then(|event| event.try_into_metric())
        .expect("the output should carry the metric");
    assert_eq!(metric.context().name(), "survivor");
}
