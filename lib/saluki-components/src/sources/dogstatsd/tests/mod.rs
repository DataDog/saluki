//! Unit tests for the DogStatsD source.
//!
//! Fixtures shared by more than one test module live here. Each submodule covers one concern of the source.

mod config;
mod decoder;
mod origin_capture;
mod readers;
mod supervision;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex as StdMutex,
    },
};

use bytes::{Buf as _, BufMut as _};
use metrics::{Key, Label};
use saluki_core::{
    components::{
        sources::{SourceBuilder as _, SourceContext},
        test_util::{ContextParts, Outputs, TestComponentDriver},
        ComponentContext,
    },
    pooling::helpers::get_pooled_object_via_builder,
    topology::EventsBuffer,
};
use saluki_env::workload::{CaptureEntityResolver, EntityId};
use saluki_io::{
    buf::{BytesBuffer, FixedSizeVec},
    deser::codec::dogstatsd::{DogStatsDCodec, DogStatsDCodecConfiguration, MetricPacket, ParsedPacket},
    net::{ConnectionAddress, ListenAddress, ProcessCredentials, ProcessIdentity},
};
use stringtheory::MetaString;

use super::{
    filters::EnablePayloadsConfiguration, metrics::build_metrics, metrics::Metrics,
    replay::test_support::wait_until_inactive, BufferDecodeContext, BufferDecodeMode, ContextResolvers, DecodeOutcome,
    DecoderContext, DogStatsDConfiguration, DogStatsDDecoder, PacketForwarder, ReceivedBuffer, TrafficCapture,
    TrafficCaptureReader,
};

const TEST_WINDOWS_PIPE_SECURITY_DESCRIPTOR: &str = "D:AI(A;;GA;;;WD)";

const TEST_BUFFER_SIZE: usize = 8192;

/// PID that [`pid_container_resolver`] maps to [`pid_container_entity`].
const TEST_PID: i32 = 42;

fn test_component_context() -> ComponentContext {
    ComponentContext::test_source("dogstatsd_test")
}

#[derive(Default)]
struct CaptureTestEntityResolver {
    pid_map: StdMutex<HashMap<u32, EntityId>>,
    resolution_count: AtomicUsize,
}

impl CaptureTestEntityResolver {
    fn with_pid_mapping(process_id: u32, entity_id: EntityId) -> Self {
        let mut pid_map = HashMap::new();
        pid_map.insert(process_id, entity_id);
        Self {
            pid_map: StdMutex::new(pid_map),
            resolution_count: AtomicUsize::new(0),
        }
    }

    fn resolution_count(&self) -> usize {
        self.resolution_count.load(Ordering::Relaxed)
    }

    #[cfg(target_os = "linux")]
    fn set_pid_mapping(&self, process_id: u32, entity_id: EntityId) {
        self.pid_map
            .lock()
            .expect("PID map lock should not be poisoned")
            .insert(process_id, entity_id);
    }
}

impl CaptureEntityResolver for CaptureTestEntityResolver {
    fn resolve_container_entity_for_live_pid(&self, process_id: u32) -> Option<EntityId> {
        self.resolution_count.fetch_add(1, Ordering::Relaxed);
        self.pid_map
            .lock()
            .expect("PID map lock should not be poisoned")
            .get(&process_id)
            .cloned()
    }
}

fn pid_container_entity() -> EntityId {
    EntityId::from_local_data("ci-pid-container").expect("container entity")
}

/// A resolver that maps [`TEST_PID`] to [`pid_container_entity`].
fn pid_container_resolver() -> CaptureTestEntityResolver {
    CaptureTestEntityResolver::with_pid_mapping(TEST_PID as u32, pid_container_entity())
}

/// The address of a Unix socket peer whose credentials carry `pid`.
fn creds_peer(pid: i32) -> ConnectionAddress {
    ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials { pid, uid: 0, gid: 0 }))
}

/// Builds the key of a counter that the test source registers, from its name and the labels after the component's own.
fn source_counter(name: &'static str, labels: &[(&'static str, &str)]) -> Key {
    let component_labels = [
        Label::from_static_parts("component_id", "dogstatsd_test"),
        Label::from_static_parts("component_type", "source"),
    ];
    let labels = labels.iter().map(|(key, value)| Label::new(*key, value.to_string()));
    Key::from_parts(name, component_labels.into_iter().chain(labels).collect::<Vec<_>>())
}

fn processed_metric_key(listener_type: &str, origin: Option<&str>) -> Key {
    let mut labels = vec![("listener_type", listener_type), ("message_type", "metrics")];
    labels.extend(origin.map(|origin| ("origin", origin)));
    source_counter("component_events_received_total", &labels)
}

fn parse_metric(input: &[u8]) -> MetricPacket<'_> {
    let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
    match codec.decode_packet(input) {
        Ok(ParsedPacket::Metric(packet)) => packet,
        _ => panic!("{} should parse as a metric", String::from_utf8_lossy(input)),
    }
}

/// Builds the context of the source, along with its outputs.
///
/// Only useful for tests that drive pieces of the source directly. Hold the returned parts for as long as the test
/// runs, since they keep the outputs open. A test that runs the source itself goes through the driver, as the tests
/// in the `supervision` module do.
fn test_source_context() -> ContextParts<SourceContext, (), Outputs<EventsBuffer>> {
    TestComponentDriver::options()
        .with_component_id("dogstatsd_test")
        .source_context(DogStatsDConfiguration::for_test().outputs())
        .expect("the source's outputs should be valid")
}

fn test_decoder_context(origin_detection_enabled: bool) -> DecoderContext {
    DecoderContext {
        codec: DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default()),
        context_resolvers: ContextResolvers::for_tests(true, None),
        default_hostname: MetaString::from_static("default-hostname"),
        enable_payloads: EnablePayloadsConfiguration::for_test(),
        origin_detection_enabled,
        stream_log_too_big: false,
        disable_verbose_logs: false,
        additional_tags: Vec::<String>::new().into(),
        traffic_capture: TrafficCapture::new(PathBuf::new(), 1),
    }
}

fn test_io_buffer(payload: &[u8], capacity: usize) -> BytesBuffer {
    let mut buffer: BytesBuffer = get_pooled_object_via_builder(|| FixedSizeVec::with_capacity(capacity));
    buffer.put_slice(payload);
    buffer
}

fn named_pipe_listen_address() -> ListenAddress {
    ListenAddress::named_pipe_with_input_buffer_size(
        "datadog-dogstatsd",
        TEST_WINDOWS_PIPE_SECURITY_DESCRIPTOR,
        TEST_BUFFER_SIZE as u32,
    )
}

/// A listen address with its telemetry, for building the per-listener context a decoder decodes with.
struct TestListener {
    listen_addr: ListenAddress,
    metrics: Metrics,
}

impl TestListener {
    fn new(listen_addr: ListenAddress) -> Self {
        let metrics = build_metrics(&listen_addr, &test_component_context(), false);
        Self { listen_addr, metrics }
    }

    fn decode_context<'a>(
        &'a self, eol_required: bool, packet_forwarder: Option<&'a PacketForwarder>, mode: BufferDecodeMode,
    ) -> BufferDecodeContext<'a> {
        BufferDecodeContext::new(&self.listen_addr, eol_required, &self.metrics, packet_forwarder, mode)
    }
}

/// A decoder wired to a test source context, along with the outputs it dispatches to.
struct DecoderFixture {
    decoder: DogStatsDDecoder,
    outputs: Outputs<EventsBuffer>,
}

impl DecoderFixture {
    fn new(decoder_context: DecoderContext) -> Self {
        let source = test_source_context();
        Self {
            decoder: DogStatsDDecoder::new(source.context, decoder_context),
            outputs: source.outputs,
        }
    }

    /// Hands `io_buffer`, holding `bytes_read` freshly read bytes, to the decoder, and returns the outcome along with
    /// the I/O buffer the decoder gives back.
    async fn decode_once(
        &mut self, context: &mut BufferDecodeContext<'_>, io_buffer: BytesBuffer, bytes_read: usize,
        peer_addr: &ConnectionAddress,
    ) -> (DecodeOutcome, BytesBuffer) {
        let (received, returned_buffer) = ReceivedBuffer::with_return(io_buffer, bytes_read, peer_addr.clone(), None);
        let outcome = self.decoder.decode_buffer(context, received).await;
        let io_buffer = returned_buffer.await.expect("decoder should return the I/O buffer");
        (outcome, io_buffer)
    }
}

/// Stops `capture` and waits for its writer to finish.
fn stop_capture(capture: &TrafficCapture) {
    capture.stop_capture();
    wait_until_inactive(|| capture.is_ongoing());
}

/// Reads every record of the capture file at `path` as its payload and PID.
fn read_capture_records(path: &Path) -> Vec<(Vec<u8>, i32)> {
    let mut reader = TrafficCaptureReader::from_path(path).expect("capture should be readable");
    let mut records = Vec::new();
    while let Some(record) = reader.read_next().expect("capture record should decode") {
        records.push((record.payload, record.pid));
    }
    records
}
