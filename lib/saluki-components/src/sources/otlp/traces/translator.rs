use opentelemetry_semantic_conventions::resource::{
    CONTAINER_ID, DEPLOYMENT_ENVIRONMENT_NAME, HOST_NAME, K8S_POD_UID, SERVICE_VERSION, TELEMETRY_SDK_LANGUAGE,
    TELEMETRY_SDK_VERSION,
};
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use otlp_protos::opentelemetry::proto::trace::v1::ResourceSpans;
use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::Span as dd_span;
use saluki_core::data_model::event::tracer_payload::{TraceChunk, TracerPayload};
use saluki_core::data_model::event::Event;
use stringtheory::MetaString;

use super::sampler::SamplingPriority;
use crate::sources::otlp::attributes::source::SourceKind;
use crate::sources::otlp::attributes::{get_string_attribute, resource_to_source, tags_from_attributes};
use crate::sources::otlp::traces::transform::{
    otel_span_to_dd_span, KEY_DATADOG_CONTAINER_ID, KEY_DATADOG_CONTAINER_TAGS, KEY_DATADOG_ENVIRONMENT,
    KEY_DATADOG_HOST, KEY_DATADOG_VERSION, SAMPLING_PRIORITY_METRIC_KEY,
};

const CONTAINER_TAGS_META_KEY: &str = "_dd.tags.container";

pub fn convert_trace_id(trace_id: &[u8]) -> u64 {
    if trace_id.len() < 8 {
        return 0;
    }
    u64::from_be_bytes((&trace_id[(trace_id.len() - 8)..]).try_into().unwrap())
}

pub fn convert_span_id(span_id: &[u8]) -> u64 {
    if span_id.len() != 8 {
        return 0;
    }
    u64::from_be_bytes(span_id.try_into().unwrap())
}

pub struct OtlpTracesTranslator;

impl OtlpTracesTranslator {
    pub fn translate_resource_spans(resource_spans: ResourceSpans) -> Vec<Event> {
        let resource = resource_spans.resource.unwrap_or_default();
        let mut traces_by_id: FastHashMap<u64, Vec<dd_span>> = FastHashMap::default();
        let mut priorities: FastHashMap<u64, SamplingPriority> = FastHashMap::default();

        for scope_spans in resource_spans.scope_spans {
            let scope = scope_spans.scope;
            let scope_ref = scope.as_ref();
            for span in scope_spans.spans {
                let trace_id = convert_trace_id(&span.trace_id);
                if trace_id == 0 {
                    continue;
                }
                // TODO: add ignore_missing_fields parameter
                let dd_span = otel_span_to_dd_span(&span, &resource, scope_ref, false);
                if let Some(priority) = sampling_priority_from_span(&dd_span) {
                    priorities.insert(trace_id, priority);
                }
                traces_by_id.entry(trace_id).or_default().push(dd_span);
            }
        }

        if traces_by_id.is_empty() {
            return Vec::new();
        }

        let chunks = build_trace_chunks(traces_by_id, priorities);
        if chunks.is_empty() {
            return Vec::new();
        }

        let payload = build_tracer_payload(&resource, chunks);
        vec![Event::TracerPayload(payload)]
    }
}

fn sampling_priority_from_span(span: &dd_span) -> Option<SamplingPriority> {
    span.metrics()
        .iter()
        .find(|(key, _)| key.as_ref() == SAMPLING_PRIORITY_METRIC_KEY)
        .and_then(|(_, value)| sampling_priority_from_value(*value))
}

fn sampling_priority_from_value(value: f64) -> Option<SamplingPriority> {
    let rounded = value.round() as i8;
    match rounded {
        -1 => Some(SamplingPriority::PriorityUserDrop),
        0 => Some(SamplingPriority::PriorityAutoDrop),
        1 => Some(SamplingPriority::PriorityAutoKeep),
        2 => Some(SamplingPriority::PriorityUserKeep),
        _ => None,
    }
}

fn build_trace_chunks(
    traces_by_id: FastHashMap<u64, Vec<dd_span>>, priorities: FastHashMap<u64, SamplingPriority>,
) -> Vec<TraceChunk> {
    let mut chunks = Vec::with_capacity(traces_by_id.len());
    for (trace_id, spans) in traces_by_id {
        if spans.is_empty() {
            continue;
        }
        let priority = priorities
            .get(&trace_id)
            .copied()
            .unwrap_or(SamplingPriority::PriorityNone);
        chunks.push(TraceChunk::new(spans).with_priority(priority as i32));
    }
    chunks
}

fn build_tracer_payload(resource: &Resource, chunks: Vec<TraceChunk>) -> TracerPayload {
    let mut payload = TracerPayload::new().with_chunks(chunks);

    if let Some(hostname) = resolve_hostname(resource) {
        payload = payload.with_hostname(hostname);
    }
    if let Some(env) = resolve_env(resource) {
        payload = payload.with_env(env);
    }
    if let Some(container_id) = resolve_container_id(resource) {
        payload = payload.with_container_id(container_id);
    }
    if let Some(app_version) = resolve_app_version(resource) {
        payload = payload.with_app_version(app_version);
    }
    if let Some(language) = resolve_language(resource) {
        payload = payload.with_language_name(language);
    }
    if let Some(tracer_version) = resolve_tracer_version(resource) {
        payload = payload.with_tracer_version(tracer_version);
    }
    if let Some(tags) = resolve_container_tags(resource) {
        let mut map = FastHashMap::default();
        map.insert(MetaString::from(CONTAINER_TAGS_META_KEY), tags);
        payload = payload.with_tags(Some(map));
    }

    payload
}

fn resolve_hostname(resource: &Resource) -> Option<MetaString> {
    if let Some(value) = get_string_attribute(&resource.attributes, KEY_DATADOG_HOST) {
        return Some(MetaString::from(value));
    }
    if let Some(source) = resource_to_source(resource) {
        if matches!(source.kind, SourceKind::HostnameKind) {
            return Some(MetaString::from(source.identifier));
        }
    }
    get_string_attribute(&resource.attributes, HOST_NAME).map(MetaString::from)
}

fn resolve_env(resource: &Resource) -> Option<MetaString> {
    if let Some(value) = get_string_attribute(&resource.attributes, KEY_DATADOG_ENVIRONMENT) {
        return Some(MetaString::from(value));
    }
    get_string_attribute(&resource.attributes, DEPLOYMENT_ENVIRONMENT_NAME).map(MetaString::from)
}

fn resolve_container_id(resource: &Resource) -> Option<MetaString> {
    if let Some(value) = get_string_attribute(&resource.attributes, KEY_DATADOG_CONTAINER_ID) {
        return Some(MetaString::from(value));
    }
    get_string_attribute(&resource.attributes, CONTAINER_ID)
        .or_else(|| get_string_attribute(&resource.attributes, K8S_POD_UID))
        .map(MetaString::from)
}

fn resolve_app_version(resource: &Resource) -> Option<MetaString> {
    if let Some(value) = get_string_attribute(&resource.attributes, KEY_DATADOG_VERSION) {
        return Some(MetaString::from(value));
    }
    get_string_attribute(&resource.attributes, SERVICE_VERSION).map(MetaString::from)
}

fn resolve_language(resource: &Resource) -> Option<MetaString> {
    get_string_attribute(&resource.attributes, TELEMETRY_SDK_LANGUAGE).map(MetaString::from)
}

fn resolve_tracer_version(resource: &Resource) -> Option<MetaString> {
    get_string_attribute(&resource.attributes, TELEMETRY_SDK_VERSION)
        .map(|sdk_version| MetaString::from(format!("otlp-{}", sdk_version)))
}

fn resolve_container_tags(resource: &Resource) -> Option<MetaString> {
    if let Some(tags) = get_string_attribute(&resource.attributes, KEY_DATADOG_CONTAINER_TAGS) {
        if !tags.is_empty() {
            return Some(MetaString::from(tags));
        }
    }

    let tagset = tags_from_attributes(&resource.attributes);
    if tagset.is_empty() {
        return None;
    }
    let flattened = tagset
        .into_iter()
        .map(|tag| tag.to_string())
        .collect::<Vec<_>>()
        .join(",");
    if flattened.is_empty() {
        None
    } else {
        Some(MetaString::from(flattened))
    }
}
