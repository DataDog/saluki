use saluki_core::data_model::event::trace::Span as dd_span;
use otlp_protos::opentelemetry::proto::{common::v1::InstrumentationScope, resource::v1::Resource, trace::v1::Span as otel_span};
use crate::sources::otlp::attributes::{get_string_attribute, get_int_attribute, resource_to_source, tags_from_attributes};
use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common, any_value::Value};
use stringtheory::MetaString;
use super::translator::{convert_span_id, convert_trace_id};
use saluki_common::collections::FastHashMap;


// KEY_DATADOG_SERVICE is the key for the service name in the Datadog namespace
const KEY_DATADOG_SERVICE: &str = "datadog.service";
// KEY_DATADOG_NAME is the key for the operation name in the Datadog namespace
const KEY_DATADOG_NAME: &str = "datadog.name";
// KEY_DATADOG_RESOURCE is the key for the resource name in the Datadog namespace
const KEY_DATADOG_RESOURCE: &str = "datadog.resource";
// KEY_DATADOG_SPAN_KIND is the key for the span kind in the Datadog namespace
const KEY_DATADOG_SPAN_KIND: &str = "datadog.span.kind";
// KEY_DATADOG_TYPE is the key for the span type in the Datadog namespace
const KEY_DATADOG_TYPE: &str = "datadog.type";
// KEY_DATADOG_ERROR is the key for the error flag in the Datadog namespace
const KEY_DATADOG_ERROR: &str = "datadog.error";
// KEY_DATADOG_ERROR_MSG is the key for the error message in the Datadog namespace
const KEY_DATADOG_ERROR_MSG: &str = "datadog.error.msg";
// KEY_DATADOG_ERROR_TYPE is the key for the error type in the Datadog namespace
const KEY_DATADOG_ERROR_TYPE: &str = "datadog.error.type";
// KEY_DATADOG_ERROR_STACK is the key for the error stack in the Datadog namespace
const KEY_DATADOG_ERROR_STACK: &str = "datadog.error.stack";
// KEY_DATADOG_VERSION is the key for the version in the Datadog namespace
const KEY_DATADOG_VERSION: &str = "datadog.version";
// KEY_DATADOG_HTTP_STATUS_CODE is the key for the HTTP status code in the Datadog namespace
const KEY_DATADOG_HTTP_STATUS_CODE: &str = "datadog.http_status_code";
// KEY_DATADOG_HOST is the key for the host in the Datadog namespace
const KEY_DATADOG_HOST: &str = "datadog.host";
// KEY_DATADOG_ENVIRONMENT is the key for the environment in the Datadog namespace
const KEY_DATADOG_ENVIRONMENT: &str = "datadog.env";
// KEY_DATADOG_CONTAINER_ID is the key for the container ID in the Datadog namespace
const KEY_DATADOG_CONTAINER_ID: &str = "datadog.container_id";
// KEY_DATADOG_CONTAINER_TAGS is the key for the container tags in the Datadog namespace
const KEY_DATADOG_CONTAINER_TAGS: &str = "datadog.container_tags";


pub fn otel_span_to_dd_span(otel_span: otel_span, otel_resource: &Resource, instrumentation_scope: Option<InstrumentationScope>) -> dd_span {
    let span_kind = otel_span.kind;
    // TODO: add conf for enable_otlp_compute_top_level_by_span_kind
    let mut dd_span = minimal_otel_span_to_dd_span(otel_span, otel_resource, instrumentation_scope);
}

pub fn minimal_otel_span_to_dd_span(otel_span: otel_span, otel_resource: Resource, instrumentation_scope: Option<InstrumentationScope>) -> dd_span {
    let span_attributes = otel_span.attributes;
    let resource_attributes = otel_resource.attributes;
    let service = use_both_maps(&span_attributes, &resource_attributes, KEY_DATADOG_SERVICE);
    let name = use_both_maps(&span_attributes, &resource_attributes, KEY_DATADOG_NAME);
    let resource = use_both_maps(&span_attributes, &resource_attributes, KEY_DATADOG_RESOURCE);
    let span_type = use_both_maps(&span_attributes, &resource_attributes, KEY_DATADOG_TYPE);
    let trace_id = convert_trace_id(&otel_span.trace_id);
    let span_id = convert_span_id(&otel_span.span_id);
    let parent_id = convert_span_id(&otel_span.parent_span_id);
    let start = match i64::try_from(otel_span.start_time_unix_nano) {
        Ok(ts) => ts,
        Err(_) => i64::MAX,
    };
    let duration_nanos = otel_span
        .end_time_unix_nano
        .saturating_sub(otel_span.start_time_unix_nano);
    let duration = match i64::try_from(duration_nanos) {
        Ok(ns) => ns,
        Err(_) => i64::MAX,
    };
    let mut meta: FastHashMap<MetaString, MetaString> = FastHashMap::default();
    meta.reserve(span_attributes.len() + resource_attributes.len());
    let mut metrics: FastHashMap<MetaString, f64> = FastHashMap::default();  
    let mut dd_span = dd_span::new(service.unwrap_or(MetaString::empty()), name.unwrap_or(MetaString::empty()), resource.unwrap_or(MetaString::empty()), span_type.unwrap_or(MetaString::empty()), trace_id, span_id); 
    let error = get_string_attribute(&span_attributes, KEY_DATADOG_ERROR)
        .and_then(|err| err.parse::<i32>().ok())
        .unwrap_or(1); 
    dd_span.with_error(error);
    let span_kind = use_both_maps(&span_attributes, &resource_attributes, KEY_DATADOG_SPAN_KIND);
    if let Some(value) = span_kind {
        meta.insert(MetaString::from("span.kind"), value);
    }

    let code = get_otel_status_code(&span_attributes, &resource_attributes);
    if code != &0 {
        metrics.insert(MetaString::from("http.status_code"), code.clone() as f64);
    }

    if dd_span.service().is_empty() {
        dd_span.with_service()
    }
    
    return dd_span;
}

fn use_both_maps(map: &[otlp_common::KeyValue], map2: &[otlp_common::KeyValue], key: &str) -> Option<MetaString> {
    if let Some(value) = get_string_attribute(map, key) {
        return Some(MetaString::from(value));
    }
    get_string_attribute(map2, key).map(MetaString::from)
}

fn get_otel_status_code<'a>(span_attributes: &'a [otlp_common::KeyValue], resource_attributes: &'a [otlp_common::KeyValue]) -> &'a i64{
    if let Some(value) = get_int_attribute(&span_attributes, KEY_DATADOG_HTTP_STATUS_CODE) {
        return value;
    } else if let Some(value) = get_int_attribute(&resource_attributes, KEY_DATADOG_HTTP_STATUS_CODE) {
        return value;
    } else if let Some(value) = get_int_attribute(&span_attributes, "http.status_code") { 
        return value;
    } else if let Some(value) = get_int_attribute(&resource_attributes, "http.status_code") {
        return value;
    } else if let Some(value) = get_int_attribute(&span_attributes, "http.response.status_code"){
        return value;
    } else if let Some(value) = get_int_attribute(&resource_attributes, "http.response.status_code"){
        return value;
    }
    return &0;
}