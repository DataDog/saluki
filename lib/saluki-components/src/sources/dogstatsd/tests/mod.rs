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
    path::PathBuf,
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
    data_model::event::metric::context::{ContextResolverBuilder, TagsResolverBuilder},
    pooling::helpers::get_pooled_object_via_builder,
    topology::EventsBuffer,
};
use saluki_env::workload::{CaptureEntityResolver, EntityId};
use saluki_io::{
    buf::{BytesBuffer, FixedSizeVec},
    deser::codec::dogstatsd::{DogStatsDCodec, DogStatsDCodecConfiguration},
    net::ListenAddress,
};
use stringtheory::MetaString;

use super::{filters::EnablePayloadsFilter, ContextResolvers, DecoderContext, DogStatsDConfiguration, TrafficCapture};

const TEST_WINDOWS_PIPE_SECURITY_DESCRIPTOR: &str = "D:AI(A;;GA;;;WD)";

const TEST_BUFFER_SIZE: usize = 8192;

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

fn processed_metric_key(listener_type: &'static str, origin: Option<&str>) -> Key {
    let mut labels = vec![
        Label::from_static_parts("component_id", "dogstatsd_test"),
        Label::from_static_parts("component_type", "source"),
        Label::from_static_parts("listener_type", listener_type),
        Label::from_static_parts("message_type", "metrics"),
    ];
    if let Some(origin) = origin {
        labels.push(Label::new("origin", origin.to_string()));
    }

    Key::from_parts("component_events_received_total", labels)
}

fn test_context_resolvers() -> ContextResolvers {
    let tags_resolver = TagsResolverBuilder::for_tests().build();
    let context_resolver = ContextResolverBuilder::for_tests()
        .with_tags_resolver(Some(tags_resolver.clone()))
        .build();
    ContextResolvers::manual(context_resolver.clone(), context_resolver, tags_resolver)
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
        context_resolvers: test_context_resolvers(),
        default_hostname: MetaString::from_static("default-hostname"),
        enabled_filter: EnablePayloadsFilter::default(),
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
