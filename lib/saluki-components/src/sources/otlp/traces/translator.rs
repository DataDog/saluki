use std::env;

use libc::{c_char, size_t};
use opentelemetry_semantic_conventions::resource::{
    CONTAINER_ID, DEPLOYMENT_ENVIRONMENT_NAME, K8S_POD_UID, SERVICE_VERSION, TELEMETRY_SDK_LANGUAGE,
    TELEMETRY_SDK_VERSION,
};
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use otlp_protos::opentelemetry::proto::trace::v1::ResourceSpans;
use saluki_common::collections::FastHashMap;
use saluki_context::tags::TagSet;
use saluki_core::data_model::event::trace::Span as dd_span;
use saluki_core::data_model::event::tracer_payload::{TraceChunk, TracerPayload};
use saluki_core::data_model::event::Event;
use stringtheory::MetaString;

use super::config::OtlpTracesTranslatorConfig;
use super::sampler::SamplingPriority;
use crate::sources::otlp::attributes::source::{Source, SourceKind};
use crate::sources::otlp::attributes::{
    extract_container_tags_from_resource_attributes, get_string_attribute, resource_to_source,
};
use crate::sources::otlp::traces::transform::{
    otel_span_to_dd_span, KEY_DATADOG_CONTAINER_ID, KEY_DATADOG_CONTAINER_TAGS, KEY_DATADOG_ENVIRONMENT,
    KEY_DATADOG_HOST, KEY_DATADOG_VERSION, SAMPLING_PRIORITY_METRIC_KEY,
};

const CONTAINER_TAGS_META_KEY: &str = "_dd.tags.container";
const DEPLOYMENT_ENVIRONMENT_KEY: &str = "deployment.environment";

pub fn convert_trace_id(trace_id: &[u8]) -> u64 {
    if trace_id.len() < 8 {
        return 0;
    }
    u64::from_be_bytes((&trace_id[(trace_id.len() - 8)..]).try_into().unwrap_or_default())
}

pub fn convert_span_id(span_id: &[u8]) -> u64 {
    if span_id.len() != 8 {
        return 0;
    }
    u64::from_be_bytes(span_id.try_into().unwrap_or_default())
}

pub struct OtlpTracesTranslator {
    config: OtlpTracesTranslatorConfig,
    default_hostname: Option<MetaString>,
}

impl OtlpTracesTranslator {
    pub fn new(config: OtlpTracesTranslatorConfig) -> Self {
        Self {
            config,
            default_hostname: detect_default_hostname(),
        }
    }

    pub fn translate_resource_spans(&self, resource_spans: ResourceSpans) -> Vec<Event> {
        let resource = resource_spans.resource.unwrap_or_default();
        let mut traces_by_id: FastHashMap<u64, Vec<dd_span>> = FastHashMap::default();
        let mut priorities: FastHashMap<u64, SamplingPriority> = FastHashMap::default();
        let ignore_missing_fields = self.config.ignore_missing_datadog_fields;

        for scope_spans in resource_spans.scope_spans {
            let scope = scope_spans.scope;
            let scope_ref = scope.as_ref();
            for span in scope_spans.spans {
                let trace_id = convert_trace_id(&span.trace_id);
                let dd_span = otel_span_to_dd_span(
                    &span,
                    &resource,
                    scope_ref,
                    ignore_missing_fields,
                    self.config.compute_top_level_by_span_kind,
                );
                if let Some(priority) = dd_span
                    .metrics()
                    .get(SAMPLING_PRIORITY_METRIC_KEY)
                    .and_then(|value| sampling_priority_from_value(*value))
                {
                    priorities.insert(trace_id, priority);
                }
                traces_by_id.entry(trace_id).or_default().push(dd_span);
            }
        }

        if traces_by_id.is_empty() {
            return Vec::new();
        }
        // TODO: add tag stats https://github.com/DataDog/datadog-agent/blob/instrument-otlp-traffic/pkg/trace/api/otlp.go#L300
        let chunks = build_trace_chunks(traces_by_id, priorities);
        if chunks.is_empty() {
            return Vec::new();
        }

        let source = resource_to_source(&resource);
        let payload = build_tracer_payload(
            &resource,
            source.as_ref(),
            self.default_hostname.as_ref(),
            chunks,
            ignore_missing_fields,
        );
        vec![Event::TracerPayload(payload)]
    }
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

fn build_tracer_payload(
    resource: &Resource, source: Option<&Source>, default_hostname: Option<&MetaString>, chunks: Vec<TraceChunk>,
    ignore_missing_fields: bool,
) -> TracerPayload {
    let mut payload = TracerPayload::new().with_chunks(chunks);

    if let Some(hostname) = resolve_hostname(resource, source, default_hostname, ignore_missing_fields) {
        payload = payload.with_hostname(hostname);
    }
    if let Some(env) = resolve_env(resource, ignore_missing_fields) {
        payload = payload.with_env(env);
    }

    if let Some(container_id) = resolve_container_id(resource, ignore_missing_fields) {
        payload = payload.with_container_id(container_id);
    }

    if let Some(app_version) = resolve_app_version(resource) {
        payload = payload.with_app_version(app_version);
    }
    // TODO: language and tracer version are calculated with tagstats so these can be removed from here once we add stats
    if let Some(language) = resolve_language(resource) {
        payload = payload.with_language_name(language);
    }
    if let Some(tracer_version) = resolve_tracer_version(resource) {
        payload = payload.with_tracer_version(tracer_version);
    }
    if let Some(tags) = resolve_container_tags(resource, source, ignore_missing_fields) {
        let mut map = FastHashMap::default();
        map.insert(MetaString::from(CONTAINER_TAGS_META_KEY), tags);
        payload = payload.with_tags(Some(map));
    }

    payload
}

// Get the hostname or set to empty if source is empty.
// logic is based off of the agent code: https://github.com/DataDog/datadog-agent/blob/instrument-otlp-traffic/pkg/trace/api/otlp.go#L317
fn resolve_hostname(
    resource: &Resource, source: Option<&Source>, default_hostname: Option<&MetaString>, ignore_missing_fields: bool,
) -> Option<MetaString> {
    let mut hostname = match source {
        Some(src) => match src.kind {
            SourceKind::HostnameKind => Some(MetaString::from(src.identifier.as_str())),
            // We are not on a hostname (serverless), hence the hostname is empty
            _ => Some(MetaString::empty()),
        },
        None => default_hostname.cloned(),
    };

    if ignore_missing_fields {
        hostname = Some(MetaString::empty());
    }

    if let Some(value) = get_string_attribute(&resource.attributes, KEY_DATADOG_HOST) {
        hostname = Some(MetaString::from(value));
    }

    hostname
}

fn resolve_env(resource: &Resource, ignore_missing_fields: bool) -> Option<MetaString> {
    if let Some(value) = get_string_attribute(&resource.attributes, KEY_DATADOG_ENVIRONMENT) {
        return Some(MetaString::from(value));
    }
    if ignore_missing_fields {
        return None;
    }
    if let Some(value) = get_string_attribute(&resource.attributes, DEPLOYMENT_ENVIRONMENT_NAME) {
        return Some(MetaString::from(value));
    }
    get_string_attribute(&resource.attributes, DEPLOYMENT_ENVIRONMENT_KEY).map(MetaString::from)
}

fn resolve_container_id(resource: &Resource, ignore_missing_fields: bool) -> Option<MetaString> {
    if let Some(value) = get_string_attribute(&resource.attributes, KEY_DATADOG_CONTAINER_ID) {
        return Some(MetaString::from(value));
    }
    if ignore_missing_fields {
        return None;
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

fn resolve_container_tags(
    resource: &Resource, source: Option<&Source>, ignore_missing_fields: bool,
) -> Option<MetaString> {
    if let Some(tags) = get_string_attribute(&resource.attributes, KEY_DATADOG_CONTAINER_TAGS) {
        if !tags.is_empty() {
            return Some(MetaString::from(tags));
        }
    }

    if ignore_missing_fields {
        return None;
    }
    let mut container_tags = TagSet::default();
    extract_container_tags_from_resource_attributes(&resource.attributes, &mut container_tags);
    let is_fargate_source = matches!(source, Some(src) if matches!(src.kind, SourceKind::AwsEcsFargateKind));
    if container_tags.is_empty() && !is_fargate_source {
        return None;
    }

    let mut flattened = flatten_container_tag(container_tags);
    if is_fargate_source {
        if let Some(src) = source {
            append_tags(&mut flattened, &src.tag());
        }
    }

    if flattened.is_empty() {
        None
    } else {
        Some(MetaString::from(flattened))
    }
}

fn flatten_container_tag(tags: TagSet) -> String {
    let mut flattened = String::new();
    for tag in tags {
        if !flattened.is_empty() {
            flattened.push(',');
        }
        flattened.push_str(tag.as_str());
    }
    flattened
}

fn append_tags(target: &mut String, tags: &str) {
    if tags.is_empty() {
        return;
    }
    if !target.is_empty() {
        target.push(',');
    }
    target.push_str(tags);
}

fn detect_default_hostname() -> Option<MetaString> {
    env::var("DD_HOSTNAME")
        .ok()
        .and_then(normalize_hostname)
        .or_else(|| env::var("HOSTNAME").ok().and_then(normalize_hostname))
        .or_else(|| system_hostname().and_then(normalize_hostname))
}

fn normalize_hostname(value: String) -> Option<MetaString> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.len() == value.len() {
        Some(MetaString::from(value))
    } else {
        Some(MetaString::from(trimmed.to_owned()))
    }
}

fn system_hostname() -> Option<String> {
    const HOSTNAME_BUFFER_LEN: usize = 256;
    let mut buffer = [0u8; HOSTNAME_BUFFER_LEN];
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr() as *mut c_char, buffer.len() as size_t) };
    if result != 0 {
        return None;
    }

    let len = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    if len == 0 {
        return None;
    }

    std::str::from_utf8(&buffer[..len]).ok().map(|s| s.to_string())
}
