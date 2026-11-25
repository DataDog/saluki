//! Tests of how the trace encoder waits for the tags of a container.
//!
//! The encoder runs as a component. The tests change the tags that the workload provider knows while a trace waits.
//! They use Tokio's paused time, so the encoder's retry timer and the 12-second wait pass without delay.

use std::sync::Arc;
use std::time::Duration;

use datadog_protos::traces::{idx, AgentPayload};
use http_body_util::BodyExt as _;
use protobuf::Message as _;
use saluki_core::accounting::{ComponentRegistry, MemoryBounds as _};
use saluki_core::components::test_util::{ComponentControl, Output, TestComponentDriver};
use saluki_core::data_model::event::{
    trace::{AttributeValue, Span, Trace, TraceIngestInfo},
    Event,
};
use saluki_core::data_model::payload::Payload;
use saluki_core::support::SubsystemIdentifier;
use saluki_core::topology::{EventsBuffer, PayloadsBuffer};
use stringtheory::MetaString;
use tokio::time::Instant;

use super::{trace_configuration, DatadogTraceConfiguration, CONTAINER_TAGS_META_KEY};
use crate::common::datadog::container_tags::test_util::ScriptedProvider;
use crate::config::{AgentEnv, ContainerTagsBuffer};

type EncoderControl = ComponentControl<EventsBuffer>;

/// The container that the tests send traces for.
const CONTAINER_ID: &str = "cid-1";

/// An encoder with a scripted workload provider, and a buffer if `buffered` is set.
async fn start_encoder(provider: &Arc<ScriptedProvider>, buffered: bool) -> (EncoderControl, Output<PayloadsBuffer>) {
    let config = DatadogTraceConfiguration {
        flush_timeout: Duration::from_millis(10),
        workload_provider: Some(provider.clone()),
        container_tags_buffer: buffered.then(|| ContainerTagsBuffer::new(1_000_000)),
        ..trace_configuration(AgentEnv::resolve(None, None, &[], &[]))
    };
    // The wait for a payload covers the whole 12-second wait of a trace.
    TestComponentDriver::options()
        .with_wait_timeout(Duration::from_secs(60))
        .encoder(config)
        .await
        .expect("trace encoder should build")
}

fn trace(trace_id: u64, container_id: &str) -> Event {
    let span = Span::new("svc", "op", "res", "web", 1, 0, 0, 1000, 0);
    let mut trace = Trace::new(vec![span], TraceIngestInfo::Datadog);
    trace.trace_id_low = trace_id;
    trace.priority = Some(1);
    trace.payload.container_id = MetaString::from(container_id);
    Event::Trace(trace)
}

/// Returns the container tags of each tracer payload in an HTTP payload, `None` for a payload without any.
async fn container_tags(payload: Payload) -> Vec<Option<String>> {
    let (_, request) = payload
        .try_into_http_payload()
        .expect("traces are sent over HTTP")
        .into_parts();
    let body = request
        .into_body()
        .collect()
        .await
        .expect("body should collect")
        .to_bytes();
    let body = zstd::stream::decode_all(body.as_ref()).expect("body should decompress");
    let agent_payload = AgentPayload::parse_from_bytes(&body).expect("body should be an agent payload");
    agent_payload
        .idxTracerPayloads
        .iter()
        .map(|tracer_payload| {
            let strings = tracer_payload.strings();
            tracer_payload.attributes().iter().find_map(|(key, value)| {
                let Some(idx::any_value::Value::StringValueRef(value)) = &value.value else {
                    return None;
                };
                (strings[*key as usize] == CONTAINER_TAGS_META_KEY).then(|| strings[*value as usize].clone())
            })
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn the_encoder_adds_the_tags_of_the_workload_provider_without_a_buffer() {
    let provider = Arc::new(ScriptedProvider::default());
    provider.set_tags(CONTAINER_ID, &["image_tag:1"]);
    let (control, mut payloads) = start_encoder(&provider, false).await;

    control.send_event(trace(1, CONTAINER_ID)).await;
    let payload = payloads.next().await.expect("the encoder should send a payload");

    assert_eq!(vec![Some("image_tag:1".to_string())], container_tags(payload).await);
    control.shutdown().await.expect("encoder should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn without_a_buffer_the_encoder_does_not_wait_for_an_unknown_container() {
    let provider = Arc::new(ScriptedProvider::default());
    let (control, mut payloads) = start_encoder(&provider, false).await;
    let start = Instant::now();

    control.send_event(trace(1, CONTAINER_ID)).await;
    let payload = payloads.next().await.expect("the encoder should send a payload");

    assert!(start.elapsed() < Duration::from_secs(1), "the encoder waited");
    assert_eq!(vec![None], container_tags(payload).await);
    control.shutdown().await.expect("encoder should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn a_large_attribute_exceeding_the_byte_cap_does_not_wait() {
    // A large byte attribute must count against the cap, even though its inline representation is small (#2396).
    let provider = Arc::new(ScriptedProvider::default());
    provider.set_tags(CONTAINER_ID, &["image_tag:1"]);
    let (control, mut payloads) = start_encoder(&provider, true).await;
    let mut event = trace(1, CONTAINER_ID);
    let Event::Trace(trace) = &mut event else {
        unreachable!()
    };
    trace.spans_mut()[0].attributes.insert(
        MetaString::from_static("large"),
        AttributeValue::Bytes(vec![0; 2_000_000]),
    );
    let start = Instant::now();

    control.send_event(event).await;
    let payload = payloads.next().await.expect("the encoder should send a payload");

    assert!(
        start.elapsed() < Duration::from_secs(1),
        "the trace exceeded the cap but waited"
    );
    assert_eq!(vec![Some("image_tag:1".to_string())], container_tags(payload).await);
    control.shutdown().await.expect("encoder should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn the_buffer_holds_a_trace_until_its_container_has_a_kube_tag() {
    let provider = Arc::new(ScriptedProvider::default());
    provider.set_tags(CONTAINER_ID, &["image_tag:1"]);
    let (control, mut payloads) = start_encoder(&provider, true).await;

    control.send_event(trace(1, CONTAINER_ID)).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(payloads.try_next().is_none(), "the container has no kube_ tag yet");

    provider.set_tags(CONTAINER_ID, &["image_tag:1", "kube_namespace:prod"]);
    let start = Instant::now();
    let payload = payloads.next().await.expect("the encoder should send a payload");

    assert!(
        start.elapsed() <= Duration::from_secs(2),
        "the next retry sends the trace"
    );
    assert_eq!(
        vec![Some("image_tag:1,kube_namespace:prod".to_string())],
        container_tags(payload).await
    );
    control.shutdown().await.expect("encoder should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn the_buffer_sends_a_trace_after_twelve_seconds_with_the_tags_it_has() {
    let provider = Arc::new(ScriptedProvider::default());
    provider.set_tags(CONTAINER_ID, &["image_tag:1"]);
    let (control, mut payloads) = start_encoder(&provider, true).await;
    let start = Instant::now();

    control.send_event(trace(1, CONTAINER_ID)).await;
    let payload = payloads.next().await.expect("the encoder should send a payload");

    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_secs(12) && waited < Duration::from_secs(14),
        "the trace waited {waited:?}"
    );
    assert_eq!(vec![Some("image_tag:1".to_string())], container_tags(payload).await);

    // The container is denied now, so its next trace doesn't wait.
    let start = Instant::now();
    control.send_event(trace(2, CONTAINER_ID)).await;
    let payload = payloads.next().await.expect("the encoder should send a payload");
    assert!(start.elapsed() < Duration::from_secs(1), "the denied container waited");
    assert_eq!(vec![Some("image_tag:1".to_string())], container_tags(payload).await);
    control.shutdown().await.expect("encoder should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn the_buffer_sends_held_traces_when_the_encoder_stops() {
    let provider = Arc::new(ScriptedProvider::default());
    provider.set_tags(CONTAINER_ID, &["image_tag:1"]);
    let (control, mut payloads) = start_encoder(&provider, true).await;

    // The encoder stops right after the input, long before the 12-second wait ends.
    control.send_event(trace(1, CONTAINER_ID)).await;
    let start = Instant::now();
    let (result, collected) = tokio::join!(control.shutdown(), payloads.collect());
    result.expect("encoder should stop cleanly");

    assert!(
        start.elapsed() < Duration::from_secs(5),
        "the stop waited for the container"
    );
    let mut tags = Vec::new();
    for payload in collected {
        tags.extend(container_tags(payload).await);
    }
    assert_eq!(vec![Some("image_tag:1".to_string())], tags);
}

#[tokio::test(start_paused = true)]
async fn a_trace_without_a_container_id_never_waits() {
    let provider = Arc::new(ScriptedProvider::default());
    let (control, mut payloads) = start_encoder(&provider, true).await;
    let start = Instant::now();

    control.send_event(trace(1, "")).await;
    let payload = payloads.next().await.expect("the encoder should send a payload");

    assert!(start.elapsed() < Duration::from_secs(1), "the encoder waited");
    assert_eq!(vec![None], container_tags(payload).await);
    assert_eq!(0, provider.lookups(), "a trace without a container needs no lookup");
    control.shutdown().await.expect("encoder should stop cleanly");
}

#[test]
fn the_memory_bounds_declare_the_cap_of_the_buffer_once_it_is_on() {
    let firm_limit = |buffer: Option<ContainerTagsBuffer>| {
        let config = DatadogTraceConfiguration {
            container_tags_buffer: buffer,
            ..trace_configuration(AgentEnv::resolve(None, None, &[], &[]))
        };
        let registry = ComponentRegistry::default();
        config.specify_bounds(&mut registry.bounds_builder(&SubsystemIdentifier::from_dotted("test")));
        registry.as_bounds().total_firm_limit_bytes()
    };

    let without = firm_limit(None);
    let with = firm_limit(Some(ContainerTagsBuffer::new(12_345)));

    assert_eq!(without + 12_345, with);
}
