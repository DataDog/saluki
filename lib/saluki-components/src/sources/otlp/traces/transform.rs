#![allow(dead_code)]

use std::convert::TryFrom;

use base64::{engine::general_purpose, Engine as _};
use opentelemetry_semantic_conventions::resource::{
    CONTAINER_ID, DEPLOYMENT_ENVIRONMENT_NAME, HOST_NAME, K8S_POD_UID, SERVICE_NAME, SERVICE_VERSION,
};
use otlp_protos::opentelemetry::proto::common::v1::{
    any_value::Value as OtlpValue, InstrumentationScope as OtlpInstrumentationScope, KeyValue,
};
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use otlp_protos::opentelemetry::proto::trace::v1::{
    span::Event as OtlpSpanEvent, span::Link as OtlpSpanLink, span::SpanKind, status::StatusCode, Span as OtlpSpan,
    Status as OtlpStatus,
};
use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::Span as DdSpan;
use serde_json::{Map as JsonMap, Value as JsonValue};
use stringtheory::MetaString;
use tracing::error;

use crate::sources::otlp::attributes::{get_int_attribute, get_string_attribute, HTTP_MAPPINGS};
use crate::sources::otlp::traces::normalize::{normalize_service, normalize_tag_value};
use crate::sources::otlp::traces::normalize::{truncate_utf8, MAX_RESOURCE_LEN};
use crate::sources::otlp::traces::translator::{convert_span_id, convert_trace_id};
pub(crate) const KEY_DATADOG_SERVICE: &str = "datadog.service";
pub(crate) const KEY_DATADOG_NAME: &str = "datadog.name";
pub(crate) const KEY_DATADOG_RESOURCE: &str = "datadog.resource";
pub(crate) const KEY_DATADOG_SPAN_KIND: &str = "datadog.span.kind";
pub(crate) const KEY_DATADOG_TYPE: &str = "datadog.type";
pub(crate) const KEY_DATADOG_ERROR: &str = "datadog.error";
pub(crate) const KEY_DATADOG_ERROR_MSG: &str = "datadog.error.msg";
pub(crate) const KEY_DATADOG_ERROR_TYPE: &str = "datadog.error.type";
pub(crate) const KEY_DATADOG_ERROR_STACK: &str = "datadog.error.stack";
pub(crate) const KEY_DATADOG_VERSION: &str = "datadog.version";
pub(crate) const KEY_DATADOG_HTTP_STATUS_CODE: &str = "datadog.http_status_code";
pub(crate) const KEY_DATADOG_HOST: &str = "datadog.host";
pub(crate) const KEY_DATADOG_ENVIRONMENT: &str = "datadog.env";
pub(crate) const KEY_DATADOG_CONTAINER_ID: &str = "datadog.container_id";
pub(crate) const KEY_DATADOG_CONTAINER_TAGS: &str = "datadog.container_tags";

pub(crate) const SAMPLING_PRIORITY_METRIC_KEY: &str = "_sampling_priority_v1";
const EVENT_EXTRACTION_METRIC_KEY: &str = "_dd1.sr.eausr";
const ANALYTICS_EVENT_KEY: &str = "analytics.event";
const HTTP_REQUEST_HEADER_PREFIX: &str = "http.request.header.";
const HTTP_REQUEST_HEADERS_PREFIX: &str = "http.request.headers.";

const DEFAULT_SERVICE_NAME: &str = "otlpresourcenoservicename";
const OPERATION_NAME_KEY: &str = "operation.name";
const RESOURCE_NAME_KEY: &str = "resource.name";
const HTTP_REQUEST_METHOD_KEYS: &[&str] = &["http.request.method", "http.method"];
const HTTP_ROUTE_KEY: &str = "http.route";
const MESSAGING_SYSTEM_KEY: &str = "messaging.system";
const MESSAGING_OPERATION_KEY: &str = "messaging.operation";
const MESSAGING_DESTINATION_KEYS: &[&str] = &["messaging.destination", "messaging.destination.name"];
const RPC_SYSTEM_KEY: &str = "rpc.system";
const RPC_SERVICE_KEY: &str = "rpc.service";
const RPC_METHOD_KEY: &str = "rpc.method";
const DB_SYSTEM_KEY: &str = "db.system";
const DB_STATEMENT_KEY: &str = "db.statement";
const DB_QUERY_TEXT_KEY: &str = "db.query.text";
const DB_NAMESPACE_KEY: &str = "db.namespace";
const GRAPHQL_OPERATION_TYPE_KEY: &str = "graphql.operation.type";
const GRAPHQL_OPERATION_NAME_KEY: &str = "graphql.operation.name";
const FAAS_INVOKED_PROVIDER_KEY: &str = "faas.invoked_provider";
const FAAS_INVOKED_NAME_KEY: &str = "faas.invoked_name";
const FAAS_TRIGGER_KEY: &str = "faas.trigger";
const NETWORK_PROTOCOL_NAME_KEY: &str = "network.protocol.name";
const HTTP_STATUS_CODE_KEY: &str = "http.status_code";
const HTTP_RESPONSE_STATUS_CODE_KEY: &str = "http.response.status_code";
const SPAN_KIND_META_KEY: &str = "span.kind";
const OTEL_TRACE_ID_META_KEY: &str = "otel.trace_id";
const W3C_TRACESTATE_META_KEY: &str = "w3c.tracestate";
const OTEL_LIBRARY_NAME_META_KEY: &str = "otel.library.name";
const OTEL_LIBRARY_VERSION_META_KEY: &str = "otel.library.version";
const OTEL_STATUS_CODE_META_KEY: &str = "otel.status_code";
const OTEL_STATUS_DESCRIPTION_META_KEY: &str = "otel.status_description";
const INTERNAL_DD_HOSTNAME_KEY: &str = "_dd.hostname";
const LEGACY_DEPLOYMENT_ENVIRONMENT_KEY: &str = "deployment.environment";
const DATADOG_HOSTNAME_ATTR: &str = "datadog.host.name";
const EXCEPTION_MESSAGE_KEY: &str = "exception.message";
const EXCEPTION_TYPE_KEY: &str = "exception.type";
const EXCEPTION_STACKTRACE_KEY: &str = "exception.stacktrace";

const DD_NAMESPACED_TO_APM_CONVENTIONS: &[(&str, &str)] = &[
    (KEY_DATADOG_ENVIRONMENT, "env"),
    (KEY_DATADOG_VERSION, "version"),
    (KEY_DATADOG_ERROR_MSG, "error.msg"),
    (KEY_DATADOG_ERROR_TYPE, "error.type"),
    (KEY_DATADOG_ERROR_STACK, "error.stack"),
    (KEY_DATADOG_HTTP_STATUS_CODE, HTTP_STATUS_CODE_KEY),
];

// otel_span_to_dd_span converts an OTLP span to DD span and is based on the logic defined in the agent.
// https://github.com/DataDog/datadog-agent/blob/instrument-otlp-traffic/pkg/trace/transform/transform.go#L357
pub fn otel_span_to_dd_span(
    otel_span: &OtlpSpan, otel_resource: &Resource, instrumentation_scope: Option<&OtlpInstrumentationScope>,
    ignore_missing_fields: bool, compute_top_level_by_span_kind: bool,
) -> DdSpan {
    let span_attributes = &otel_span.attributes;
    let resource_attributes = &otel_resource.attributes;
    let (mut dd_span, mut meta, mut metrics) = otel_to_dd_span_minimal(
        otel_span,
        otel_resource,
        instrumentation_scope,
        ignore_missing_fields,
        compute_top_level_by_span_kind,
    );

    for (dd_key, apm_key) in DD_NAMESPACED_TO_APM_CONVENTIONS {
        if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, dd_key) {
            meta.insert((*apm_key).into(), value);
        }
    }

    for attribute in span_attributes {
        map_attribute_generic(attribute, &mut meta, &mut metrics, ignore_missing_fields);
    }

    if !otel_span.trace_id.is_empty() {
        meta.insert(
            OTEL_TRACE_ID_META_KEY.into(),
            bytes_to_hex_lowercase(&otel_span.trace_id).into(),
        );
    }

    if !meta.contains_key("version") {
        let version = get_otel_version(span_attributes, resource_attributes, ignore_missing_fields);
        if !version.is_empty() {
            meta.insert("version".into(), version);
        }
    }

    if let Some(events_json) = marshal_events(&otel_span.events) {
        meta.insert("events".into(), events_json.into());
    }
    if span_contains_exception_event(&otel_span.events) {
        meta.insert("_dd.span_events.has_exception".into(), MetaString::from_static("true"));
    }
    if let Some(links_json) = marshal_links(&otel_span.links) {
        meta.insert("_dd.span_links".into(), links_json.into());
    }

    if !otel_span.trace_state.is_empty() {
        meta.insert(W3C_TRACESTATE_META_KEY.into(), otel_span.trace_state.as_str().into());
    }

    if let Some(scope) = instrumentation_scope {
        if !scope.name.is_empty() {
            meta.insert(OTEL_LIBRARY_NAME_META_KEY.into(), scope.name.as_str().into());
        }
        if !scope.version.is_empty() {
            meta.insert(OTEL_LIBRARY_VERSION_META_KEY.into(), scope.version.as_str().into());
        }
    }

    let status = otel_span.status.as_ref();
    let status_code = status
        .and_then(|s| StatusCode::try_from(s.code).ok())
        .unwrap_or(StatusCode::Unset);
    meta.insert(
        OTEL_STATUS_CODE_META_KEY.into(),
        MetaString::from(status_code_to_string(status_code)),
    );
    if let Some(status) = status {
        if !status.message.is_empty() {
            meta.insert(OTEL_STATUS_DESCRIPTION_META_KEY.into(), status.message.as_str().into());
        }
    }

    if !ignore_missing_fields {
        if !meta.contains_key("error.msg") || !meta.contains_key("error.type") || !meta.contains_key("error.stack") {
            let error = status_to_error(status, &otel_span.events, &mut meta);
            if error != 0 {
                dd_span = dd_span.with_error(error);
            }
        }

        if !meta.contains_key("env") {
            let env = get_otel_env(span_attributes, resource_attributes, ignore_missing_fields);
            if !env.is_empty() {
                meta.insert("env".into(), env);
            }
        }
    }

    for attribute in resource_attributes {
        let Some(value) = attribute.value.as_ref().and_then(|wrapper| wrapper.value.as_ref()) else {
            continue;
        };
        if let Some(serialized) = otlp_value_to_string(value) {
            conditionally_map_otlp_attribute_to_meta(
                attribute.key.as_str(),
                &serialized,
                &mut meta,
                &mut metrics,
                ignore_missing_fields,
            );
        }
    }

    if let Some(scope) = instrumentation_scope {
        instrumentation_scope_attributes_to_meta(scope, &mut meta);
    }

    if !meta.contains_key("db.name") {
        if let Some(db_namespace) = use_both_maps(resource_attributes, span_attributes, false, DB_NAMESPACE_KEY) {
            meta.insert("db.name".into(), db_namespace);
        }
    }

    dd_span.with_meta(Some(meta)).with_metrics(Some(metrics))
}

// OtelSpanToDDSpanMinimal otelSpanToDDSpan converts an OTel span to a DD span.
// The converted DD span only has the minimal number of fields for APM stats calculation and is only meant
// to be used in OTLPTracesToConcentratorInputs. Do not use them for other purposes.
pub fn otel_to_dd_span_minimal(
    otel_span: &OtlpSpan, otel_resource: &Resource, _instrumentation_scope: Option<&OtlpInstrumentationScope>,
    ignore_missing_fields: bool, compute_top_level_by_span_kind: bool,
) -> (
    DdSpan,
    FastHashMap<MetaString, MetaString>,
    FastHashMap<MetaString, f64>,
) {
    let span_attributes = &otel_span.attributes;
    let resource_attributes = &otel_resource.attributes;
    let mut dd_span = DdSpan::default();

    let trace_id = convert_trace_id(&otel_span.trace_id);
    let span_id = convert_span_id(&otel_span.span_id);
    let parent_id = convert_span_id(&otel_span.parent_span_id);
    let start = otel_span.start_time_unix_nano as i64;
    let duration = (otel_span.end_time_unix_nano - otel_span.start_time_unix_nano) as i64;
    let mut meta: FastHashMap<MetaString, MetaString> = FastHashMap::default();
    meta.reserve(span_attributes.len() + resource_attributes.len());
    let mut metrics: FastHashMap<MetaString, f64> = FastHashMap::default();
    let is_top_level = compute_top_level_by_span_kind && otel_span.parent_span_id.is_empty()
        || otel_span.kind() == SpanKind::Server
        || otel_span.kind() == SpanKind::Consumer;

    if let Some(value) = get_int_attribute(span_attributes, KEY_DATADOG_ERROR) {
        dd_span = dd_span.with_error(*value as i32);
    } else if let Some(value) = get_string_attribute(span_attributes, KEY_DATADOG_ERROR) {
        dd_span = dd_span.with_error(value.parse::<i32>().unwrap_or(0));
    } else if let Some(status) = &otel_span.status {
        if status.code() == StatusCode::Error {
            dd_span = dd_span.with_error(1);
        }
    }

    if is_top_level {
        meta.insert(MetaString::from("_top_level"), MetaString::from("1"));
    }

    if get_string_attribute(span_attributes, "_dd.measured").is_some_and(|v| *v == *"1")
        || (compute_top_level_by_span_kind
            && (otel_span.kind() == SpanKind::Client || otel_span.kind() == SpanKind::Producer))
    {
        metrics.insert(MetaString::from("_dd.measured"), 1.0);
    }

    let span_kind = use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_SPAN_KIND)
        .unwrap_or_else(|| MetaString::from(SpanKind::Unspecified.as_str_name()));
    meta.insert(MetaString::from(SPAN_KIND_META_KEY), span_kind);

    let mut service =
        use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_SERVICE).unwrap_or_default();
    let mut name = use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_NAME).unwrap_or_default();
    let mut resource =
        use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_RESOURCE).unwrap_or_default();
    let mut span_type = use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_TYPE).unwrap_or_default();

    if !ignore_missing_fields {
        // the functions below are based off the V2 agent functions as they are used by default
        // TODO: allow the user to opt out of V2 via config and also implement the V1 versions of the functions
        if service.is_empty() {
            service = get_otel_service(span_attributes, resource_attributes, true);
        }
        if name.is_empty() {
            name = get_otel_operation_name_v2(otel_span, span_attributes, resource_attributes);
        }
        if resource.is_empty() {
            resource = get_otel_resource_v2_truncated(otel_span, span_attributes, resource_attributes);
        }
        if span_type.is_empty() {
            span_type = get_otel_span_type(otel_span, span_attributes, resource_attributes);
        }
    }

    dd_span = dd_span
        .with_service(service)
        .with_name(name)
        .with_resource(resource)
        .with_span_type(span_type)
        .with_trace_id(trace_id)
        .with_span_id(span_id)
        .with_parent_id(parent_id)
        .with_start(start)
        .with_duration(duration);

    if let Some(status_code) = get_otel_status_code(span_attributes, resource_attributes, ignore_missing_fields) {
        metrics.insert(HTTP_STATUS_CODE_KEY.into(), status_code as f64);
    }

    // TODO: add peer key tags (unfinished in the agent as well)

    (dd_span, meta, metrics)
}

/// Returns the DD service name based on OTel span and resource attributes.
fn get_otel_service(span_attributes: &[KeyValue], resource_attributes: &[KeyValue], normalize: bool) -> MetaString {
    let service = use_both_maps(span_attributes, resource_attributes, true, SERVICE_NAME)
        .unwrap_or_else(|| MetaString::from_static(DEFAULT_SERVICE_NAME));

    if normalize {
        normalize_service(&service)
    } else {
        service
    }
}

// GetOTelOperationNameV2 returns the DD operation name based on OTel span and resource attributes and given configs.
// based on code from https://github.com/DataDog/datadog-agent/blob/instrument-otlp-traffic/pkg/trace/traceutil/otel_util.go#L424
fn get_otel_operation_name_v2(
    otel_span: &OtlpSpan, span_attributes: &[KeyValue], resource_attributes: &[KeyValue],
) -> MetaString {
    if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, OPERATION_NAME_KEY) {
        return value;
    }

    let span_kind = SpanKind::try_from(otel_span.kind).unwrap_or(SpanKind::Unspecified);
    let is_client = matches!(span_kind, SpanKind::Client);
    let is_server = matches!(span_kind, SpanKind::Server);

    // http
    for http_request_method_key in HTTP_REQUEST_METHOD_KEYS {
        if use_both_maps(span_attributes, resource_attributes, true, http_request_method_key).is_some() {
            if is_server {
                return MetaString::from_static("http.server.request");
            }
            if is_client {
                return MetaString::from_static("http.client.request");
            }
        }
    }

    // database
    if is_client {
        if let Some(db_system) = use_both_maps(span_attributes, resource_attributes, true, DB_SYSTEM_KEY) {
            return MetaString::from(format!("{db_system}.query"));
        }
    }

    // messaging
    if let (Some(system), Some(operation)) = (
        use_both_maps(span_attributes, resource_attributes, true, MESSAGING_SYSTEM_KEY),
        use_both_maps(span_attributes, resource_attributes, true, MESSAGING_OPERATION_KEY),
    ) {
        match span_kind {
            SpanKind::Client | SpanKind::Server | SpanKind::Consumer | SpanKind::Producer => {
                return MetaString::from(format!("{system}.{operation}"))
            }
            _ => {}
        }
    }

    // RPC & AWS
    if let Some(rpc_system) = use_both_maps(span_attributes, resource_attributes, true, RPC_SYSTEM_KEY) {
        let is_aws = rpc_system == "aws-api";
        if is_aws && is_client {
            if let Some(service) = use_both_maps(span_attributes, resource_attributes, true, RPC_SERVICE_KEY) {
                return MetaString::from(format!("aws.{service}.request"));
            }
            return MetaString::from("aws.client.request");
        }

        if is_client {
            return MetaString::from(format!("{rpc_system}.client.request"));
        }
        if is_server {
            return MetaString::from(format!("{rpc_system}.server.request"));
        }
    }

    // FAAS client
    if is_client {
        if let (Some(provider), Some(invoked)) = (
            use_both_maps(span_attributes, resource_attributes, true, FAAS_INVOKED_PROVIDER_KEY),
            use_both_maps(span_attributes, resource_attributes, true, FAAS_INVOKED_NAME_KEY),
        ) {
            return MetaString::from(format!("{provider}.{invoked}.invoke"));
        }
    }
    // FAAS server
    if is_server {
        if let Some(trigger) = use_both_maps(span_attributes, resource_attributes, true, FAAS_TRIGGER_KEY) {
            return MetaString::from(format!("{trigger}.invoke"));
        }
    }

    if use_both_maps(span_attributes, resource_attributes, true, GRAPHQL_OPERATION_TYPE_KEY).is_some() {
        return MetaString::from_static("graphql.server.request");
    }

    if is_server {
        if let Some(protocol) = use_both_maps(span_attributes, resource_attributes, true, NETWORK_PROTOCOL_NAME_KEY) {
            return MetaString::from(format!("{protocol}.server.request"));
        }
        return MetaString::from_static("server.request");
    }
    if is_client {
        if let Some(protocol) = use_both_maps(span_attributes, resource_attributes, true, NETWORK_PROTOCOL_NAME_KEY) {
            return MetaString::from(format!("{protocol}.client.request"));
        }
        return MetaString::from_static("client.request");
    }

    let fallback_kind = if span_kind == SpanKind::Unspecified {
        SpanKind::Internal
    } else {
        span_kind
    };
    MetaString::from(span_kind_name(fallback_kind))
}

// GetOTelResourceV2 returns the DD resource name based on OTel span and resource attributes.
// based on this code https://github.com/DataDog/datadog-agent/blob/instrument-otlp-traffic/pkg/trace/traceutil/otel_util.go#L348
fn get_otel_resource_v2(
    otel_span: &OtlpSpan, span_attributes: &[KeyValue], resource_attributes: &[KeyValue],
) -> MetaString {
    let span_kind = SpanKind::try_from(otel_span.kind).unwrap_or(SpanKind::Unspecified);
    if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, RESOURCE_NAME_KEY) {
        return value;
    }

    if span_kind == SpanKind::Server {
        if let Some(method) = use_both_maps_key_list(span_attributes, resource_attributes, HTTP_REQUEST_METHOD_KEYS) {
            let mut resource_name = if method.as_ref() == "_OTHER" {
                String::from("HTTP")
            } else {
                method.as_ref().to_string()
            };
            if let Some(route) = use_both_maps(span_attributes, resource_attributes, true, HTTP_ROUTE_KEY) {
                resource_name.push(' ');
                resource_name.push_str(route.as_ref());
            }
            return MetaString::from(resource_name);
        }
    }

    if let Some(operation) = use_both_maps(span_attributes, resource_attributes, true, MESSAGING_OPERATION_KEY) {
        let mut resource_name = operation.as_ref().to_string();
        if let Some(dest) = use_both_maps_key_list(span_attributes, resource_attributes, MESSAGING_DESTINATION_KEYS) {
            if !dest.is_empty() {
                resource_name.push(' ');
                resource_name.push_str(dest.as_ref());
            }
        }
        return MetaString::from(resource_name);
    }

    if let Some(method) = use_both_maps(span_attributes, resource_attributes, true, RPC_METHOD_KEY) {
        let mut resource_name = method.as_ref().to_string();
        if let Some(service) = use_both_maps(span_attributes, resource_attributes, true, RPC_SERVICE_KEY) {
            resource_name.push(' ');
            resource_name.push_str(service.as_ref());
        }
        return MetaString::from(resource_name);
    }

    // Enrich GraphQL query resource names.
    // See https://github.com/open-telemetry/semantic-conventions/blob/v1.29.0/docs/graphql/graphql-spans.md
    if let Some(op_type) = use_both_maps(span_attributes, resource_attributes, true, GRAPHQL_OPERATION_TYPE_KEY) {
        let mut resource_name = op_type.as_ref().to_string();
        if let Some(op_name) = use_both_maps(span_attributes, resource_attributes, true, GRAPHQL_OPERATION_NAME_KEY) {
            resource_name.push(' ');
            resource_name.push_str(op_name.as_ref());
        }
        return MetaString::from(resource_name);
    }

    if use_both_maps(span_attributes, resource_attributes, true, DB_SYSTEM_KEY).is_some() {
        if let Some(statement) = use_both_maps(span_attributes, resource_attributes, true, DB_STATEMENT_KEY) {
            return statement;
        }
        if let Some(query) = use_both_maps(span_attributes, resource_attributes, true, DB_QUERY_TEXT_KEY) {
            return query;
        }
    }

    if !otel_span.name.is_empty() {
        return MetaString::from(otel_span.name.as_str());
    }
    MetaString::empty()
}

fn get_otel_resource_v2_truncated(
    otel_span: &OtlpSpan, span_attributes: &[KeyValue], resource_attributes: &[KeyValue],
) -> MetaString {
    let res_name = get_otel_resource_v2(otel_span, span_attributes, resource_attributes);
    if res_name.len() > MAX_RESOURCE_LEN {
        MetaString::from(truncate_utf8(&res_name, MAX_RESOURCE_LEN))
    } else {
        res_name
    }
}

fn get_otel_span_type(
    otel_span: &OtlpSpan, span_attributes: &[KeyValue], resource_attributes: &[KeyValue],
) -> MetaString {
    if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, "span.type") {
        return value;
    }

    let span_kind = SpanKind::try_from(otel_span.kind).unwrap_or(SpanKind::Unspecified);
    let span_type = match span_kind {
        SpanKind::Server => "web",
        SpanKind::Client => {
            if let Some(db_system) = use_both_maps(span_attributes, resource_attributes, true, DB_SYSTEM_KEY) {
                map_db_system_to_span_type(db_system.as_ref())
            } else {
                "http"
            }
        }
        _ => "custom",
    };

    MetaString::from(span_type)
}

fn map_db_system_to_span_type(db_system: &str) -> &'static str {
    match db_system {
        "redis" => "redis",
        "memcached" => "memcached",
        "mongodb" => "mongodb",
        "elasticsearch" => "elasticsearch",
        "opensearch" => "opensearch",
        "cassandra" => "cassandra",
        system if SQL_DB_SYSTEMS.contains(&system) => "sql",
        _ => "db",
    }
}

const SQL_DB_SYSTEMS: &[&str] = &[
    "other_sql",
    "mssql",
    "mysql",
    "oracle",
    "db2",
    "postgresql",
    "redshift",
    "cloudscape",
    "hsqldb",
    "maxdb",
    "ingres",
    "firstsql",
    "edb",
    "cache",
    "firebird",
    "derby",
    "informix",
    "mariadb",
    "sqlite",
    "sybase",
    "teradata",
    "vertica",
    "h2",
    "coldfusion",
    "cockroachdb",
    "progress",
    "hana",
    "adabas",
    "filemaker",
    "instantdb",
    "interbase",
    "netezza",
    "pervasive",
    "pointbase",
    "clickhouse",
];

fn map_attribute_generic(
    attribute: &KeyValue, meta: &mut FastHashMap<MetaString, MetaString>, metrics: &mut FastHashMap<MetaString, f64>,
    ignore_missing_fields: bool,
) {
    let Some(value) = attribute.value.as_ref().and_then(|wrapper| wrapper.value.as_ref()) else {
        return;
    };

    match value {
        OtlpValue::StringValue(s) => {
            conditionally_map_otlp_attribute_to_meta(
                attribute.key.as_str(),
                s.as_str(),
                meta,
                metrics,
                ignore_missing_fields,
            );
        }
        OtlpValue::BoolValue(b) => {
            let bool_value = if *b { "true" } else { "false" };
            conditionally_map_otlp_attribute_to_meta(
                attribute.key.as_str(),
                bool_value,
                meta,
                metrics,
                ignore_missing_fields,
            );
        }
        OtlpValue::BytesValue(bytes) => {
            let placeholder = format!("<{} bytes>", bytes.len());
            conditionally_map_otlp_attribute_to_meta(
                attribute.key.as_str(),
                &placeholder,
                meta,
                metrics,
                ignore_missing_fields,
            );
        }
        OtlpValue::IntValue(i) => {
            conditionally_map_otlp_attribute_to_metric(
                attribute.key.as_str(),
                *i as f64,
                metrics,
                ignore_missing_fields,
            );
        }
        OtlpValue::DoubleValue(d) => {
            conditionally_map_otlp_attribute_to_metric(attribute.key.as_str(), *d, metrics, ignore_missing_fields);
        }
        _ => {
            // Skip complex values for now.
        }
    }
}

fn instrumentation_scope_attributes_to_meta(
    scope: &OtlpInstrumentationScope, meta: &mut FastHashMap<MetaString, MetaString>,
) {
    for attribute in &scope.attributes {
        let Some(value) = attribute.value.as_ref().and_then(|wrapper| wrapper.value.as_ref()) else {
            continue;
        };
        if let Some(serialized) = otlp_value_to_string(value) {
            meta.insert(attribute.key.as_str().into(), serialized.into());
        }
    }
}
// span_contains_exception_event checks if the span contains at least one exception span event.
fn span_contains_exception_event(events: &[OtlpSpanEvent]) -> bool {
    events.iter().any(|event| event.name == "exception")
}

// MarshalEvents marshals events into JSON.
fn marshal_events(events: &[OtlpSpanEvent]) -> Option<String> {
    if events.is_empty() {
        return None;
    }
    let mut serialized = Vec::with_capacity(events.len());
    for event in events {
        let mut obj = JsonMap::new();
        if event.time_unix_nano != 0 {
            obj.insert("time_unix_nano".to_string(), JsonValue::from(event.time_unix_nano));
        }
        if !event.name.is_empty() {
            obj.insert("name".to_string(), JsonValue::String(event.name.clone()));
        }
        if let Some(attributes) = key_values_to_json_object(&event.attributes) {
            obj.insert("attributes".to_string(), JsonValue::Object(attributes));
        }
        if event.dropped_attributes_count != 0 {
            obj.insert(
                "dropped_attributes_count".to_string(),
                JsonValue::from(u64::from(event.dropped_attributes_count)),
            );
        }
        serialized.push(JsonValue::Object(obj));
    }
    serde_json::to_string(&serialized).ok()
}

fn marshal_links(links: &[OtlpSpanLink]) -> Option<String> {
    if links.is_empty() {
        return None;
    }
    let mut serialized = Vec::with_capacity(links.len());
    for link in links {
        let mut obj = JsonMap::new();
        obj.insert(
            "trace_id".to_string(),
            JsonValue::String(bytes_to_hex_lowercase(&link.trace_id)),
        );
        obj.insert(
            "span_id".to_string(),
            JsonValue::String(bytes_to_hex_lowercase(&link.span_id)),
        );
        if !link.trace_state.is_empty() {
            obj.insert("tracestate".to_string(), JsonValue::String(link.trace_state.clone()));
        }
        if let Some(attributes) = key_values_to_json_object(&link.attributes) {
            obj.insert("attributes".to_string(), JsonValue::Object(attributes));
        }
        if link.dropped_attributes_count != 0 {
            obj.insert(
                "dropped_attributes_count".to_string(),
                JsonValue::from(u64::from(link.dropped_attributes_count)),
            );
        }
        serialized.push(JsonValue::Object(obj));
    }
    serde_json::to_string(&serialized).ok()
}

fn status_code_to_string(code: StatusCode) -> &'static str {
    match code {
        StatusCode::Ok => "StatusCodeOk",
        StatusCode::Error => "StatusCodeError",
        StatusCode::Unset => "StatusCodeUnset",
    }
}

// Status2Error checks the given status and events and applies any potential error and messages
// to the given span attributes.
fn status_to_error(
    status: Option<&OtlpStatus>, events: &[OtlpSpanEvent], meta: &mut FastHashMap<MetaString, MetaString>,
) -> i32 {
    let Some(status) = status else {
        return 0;
    };
    let status_code = StatusCode::try_from(status.code).unwrap_or(StatusCode::Unset);
    if status_code != StatusCode::Error {
        return 0;
    }
    for event in events {
        if !event.name.eq_ignore_ascii_case("exception") {
            continue;
        }
        for attribute in &event.attributes {
            let Some(value) = attribute.value.as_ref().and_then(|wrapper| wrapper.value.as_ref()) else {
                continue;
            };
            if let Some(serialized) = otlp_value_to_string(value) {
                let meta_value: MetaString = serialized.clone().into();
                match attribute.key.as_str() {
                    EXCEPTION_MESSAGE_KEY => {
                        meta.insert("error.msg".into(), meta_value);
                    }
                    EXCEPTION_TYPE_KEY => {
                        meta.insert("error.type".into(), serialized.into());
                    }
                    EXCEPTION_STACKTRACE_KEY => {
                        meta.insert("error.stack".into(), serialized.into());
                    }
                    _ => {}
                }
            }
        }
    }
    if !meta.contains_key("error.msg") {
        if !status.message.is_empty() {
            meta.insert("error.msg".into(), status.message.as_str().into());
        } else if let Some(http_code) =
            get_first_from_meta(meta, &[HTTP_RESPONSE_STATUS_CODE_KEY, HTTP_STATUS_CODE_KEY])
        {
            let mut message = http_code.as_ref().to_string();
            if let Some(http_text) = meta.get("http.status_text") {
                message.push(' ');
                message.push_str(http_text.as_ref());
            }
            meta.insert("error.msg".into(), message.into());
        }
    }
    1
}

fn get_first_from_meta(meta: &FastHashMap<MetaString, MetaString>, keys: &[&str]) -> Option<MetaString> {
    for key in keys {
        if let Some(value) = meta.get(*key) {
            return Some(value.clone());
        }
    }
    None
}

fn key_values_to_json_object(attributes: &[KeyValue]) -> Option<JsonMap<String, JsonValue>> {
    if attributes.is_empty() {
        return None;
    }
    let mut map = JsonMap::new();
    for attribute in attributes {
        if attribute.key.is_empty() {
            continue;
        }
        let Some(value) = attribute.value.as_ref().and_then(|wrapper| wrapper.value.as_ref()) else {
            continue;
        };
        if let Some(json_value) = otlp_value_to_json_value(value) {
            map.insert(attribute.key.clone(), json_value);
        }
    }
    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

fn otlp_value_to_json_value(value: &OtlpValue) -> Option<JsonValue> {
    match value {
        OtlpValue::StringValue(v) => Some(JsonValue::String(v.clone())),
        OtlpValue::BoolValue(v) => Some(JsonValue::Bool(*v)),
        OtlpValue::IntValue(v) => Some(JsonValue::Number((*v).into())),
        OtlpValue::DoubleValue(v) => serde_json::Number::from_f64(*v).map(JsonValue::Number),
        OtlpValue::BytesValue(bytes) => Some(JsonValue::String(general_purpose::STANDARD.encode(bytes))),
        OtlpValue::ArrayValue(array) => {
            let mut arr = Vec::with_capacity(array.values.len());
            for item in &array.values {
                if let Some(inner) = item.value.as_ref().and_then(otlp_value_to_json_value) {
                    arr.push(inner);
                }
            }
            Some(JsonValue::Array(arr))
        }
        OtlpValue::KvlistValue(kvlist) => {
            let mut obj = JsonMap::new();
            for kv in &kvlist.values {
                if let Some(inner) = kv
                    .value
                    .as_ref()
                    .and_then(|wrapper| wrapper.value.as_ref())
                    .and_then(otlp_value_to_json_value)
                {
                    obj.insert(kv.key.clone(), inner);
                }
            }
            Some(JsonValue::Object(obj))
        }
    }
}

fn otlp_value_to_string(value: &OtlpValue) -> Option<String> {
    match value {
        OtlpValue::StringValue(v) => Some(v.clone()),
        OtlpValue::BoolValue(v) => Some(if *v { "true" } else { "false" }.to_string()),
        OtlpValue::IntValue(v) => Some(v.to_string()),
        OtlpValue::DoubleValue(v) => Some(v.to_string()),
        OtlpValue::BytesValue(bytes) => Some(format!("<{} bytes>", bytes.len())),
        OtlpValue::ArrayValue(_) | OtlpValue::KvlistValue(_) => {
            otlp_value_to_json_value(value).map(|json| json.to_string())
        }
    }
}

fn conditionally_map_otlp_attribute_to_meta(
    key: &str, value: &str, meta: &mut FastHashMap<MetaString, MetaString>, metrics: &mut FastHashMap<MetaString, f64>,
    ignore_missing_fields: bool,
) {
    if value.is_empty() {
        return;
    }
    if let Some(mapped_key) = get_dd_key_for_otlp_attribute(key) {
        if meta.contains_key(&mapped_key) {
            return;
        }
        if ignore_missing_fields && has_dd_namespaced_equivalent(mapped_key.as_ref()) {
            return;
        }
        set_meta_field_otlp_if_empty(mapped_key, value, meta, metrics);
    }
}

fn conditionally_map_otlp_attribute_to_metric(
    key: &str, value: f64, metrics: &mut FastHashMap<MetaString, f64>, ignore_missing_fields: bool,
) {
    if let Some(mapped_key) = get_dd_key_for_otlp_attribute(key) {
        if metrics.contains_key(&mapped_key) {
            return;
        }
        if ignore_missing_fields && has_dd_namespaced_equivalent(mapped_key.as_ref()) {
            return;
        }
        set_metric_field_otlp_if_empty(mapped_key, value, metrics);
    }
}

// SetMetaOTLPIfEmpty sets the k/v OTLP attribute pair as a tag on span s, if the corresponding value hasn't been set already.
// based off of the code from the agent https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/transform/transform.go#L612
fn set_meta_field_otlp_if_empty(
    key: MetaString, value: &str, meta: &mut FastHashMap<MetaString, MetaString>,
    metrics: &mut FastHashMap<MetaString, f64>,
) {
    match key.as_ref() {
        "service.name" | "operation.name" | "resource.name" | "span.type" => {
            // handled elsewhere
        }
        ANALYTICS_EVENT_KEY => {
            if metrics.contains_key(EVENT_EXTRACTION_METRIC_KEY) {
                return;
            }
            if let Some(parsed) = parse_bool(value) {
                metrics
                    .entry(EVENT_EXTRACTION_METRIC_KEY.into())
                    .or_insert(if parsed { 1.0 } else { 0.0 });
            }
        }
        _ => {
            meta.entry(key).or_insert_with(|| value.into());
        }
    }
}

fn set_metric_field_otlp_if_empty(key: MetaString, value: f64, metrics: &mut FastHashMap<MetaString, f64>) {
    let storage_key = if key.as_ref() == "sampling.priority" {
        MetaString::from_static(SAMPLING_PRIORITY_METRIC_KEY)
    } else {
        key
    };
    metrics.entry(storage_key).or_insert(value);
}

// GetDDKeyForOTLPAttribute looks for a key in the Datadog HTTP convention that matches the given key from the
// OTLP HTTP convention. Otherwise, check if it is a Datadog APM convention key - if it is, it will be handled with
// specialized logic elsewhere, so return None. If it isn't, return the original key.
// based on the logic from the agent code https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/transform/transform.go#L179
fn get_dd_key_for_otlp_attribute(key: &str) -> Option<MetaString> {
    if let Some(mapped) = HTTP_MAPPINGS.get(key) {
        return Some(MetaString::from_static(mapped));
    }
    if let Some(header_suffix) = key.strip_prefix(HTTP_REQUEST_HEADER_PREFIX) {
        let mapped_key = format!("{HTTP_REQUEST_HEADERS_PREFIX}{header_suffix}");
        return Some(MetaString::from(mapped_key));
    }
    if !is_datadog_apm_convention_key(key) {
        return Some(MetaString::from(key));
    }
    None
}

fn is_datadog_apm_convention_key(key: &str) -> bool {
    matches!(key, "service.name" | "operation.name" | "resource.name" | "span.type") || key.starts_with("datadog.")
}

fn has_dd_namespaced_equivalent(key: &str) -> bool {
    matches!(
        key,
        "env" | "version" | HTTP_STATUS_CODE_KEY | "error.msg" | "error.type" | "error.stack"
    )
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

fn span_kind_name(kind: SpanKind) -> &'static str {
    match kind {
        SpanKind::Client => "client",
        SpanKind::Server => "server",
        SpanKind::Consumer => "consumer",
        SpanKind::Producer => "producer",
        SpanKind::Internal => "internal",
        SpanKind::Unspecified => "unspecified",
    }
}

fn use_both_maps(map: &[KeyValue], map2: &[KeyValue], normalize: bool, key: &str) -> Option<MetaString> {
    if let Some(value) = get_string_attribute(map, key) {
        return Some(if normalize {
            normalize_tag_value(value)
        } else {
            MetaString::from(value)
        });
    }
    get_string_attribute(map2, key).map(|value| {
        if normalize {
            normalize_tag_value(value)
        } else {
            MetaString::from(value)
        }
    })
}

fn use_both_maps_key_list(
    span_attributes: &[KeyValue], resource_attributes: &[KeyValue], keys: &[&str],
) -> Option<MetaString> {
    for key in keys {
        if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, key) {
            return Some(value);
        }
    }
    None
}

fn get_otel_env(
    span_attributes: &[KeyValue], resource_attributes: &[KeyValue], ignore_missing_fields: bool,
) -> MetaString {
    if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_ENVIRONMENT) {
        return value;
    }

    if ignore_missing_fields {
        return MetaString::empty();
    }

    if let Some(value) = use_both_maps_key_list(
        span_attributes,
        resource_attributes,
        &[DEPLOYMENT_ENVIRONMENT_NAME, LEGACY_DEPLOYMENT_ENVIRONMENT_KEY],
    ) {
        return value;
    }

    MetaString::empty()
}

fn get_otel_hostname(
    span_attributes: &[KeyValue], resource_attributes: &[KeyValue], fallback_host: &str, ignore_missing_fields: bool,
) -> MetaString {
    if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_HOST) {
        return value;
    }

    if ignore_missing_fields {
        return MetaString::empty();
    }

    if let Some(value) = use_both_maps(span_attributes, resource_attributes, false, DATADOG_HOSTNAME_ATTR) {
        return value;
    }

    if let Some(value) = use_both_maps(span_attributes, resource_attributes, false, INTERNAL_DD_HOSTNAME_KEY) {
        return value;
    }

    if let Some(value) = use_both_maps(span_attributes, resource_attributes, false, HOST_NAME) {
        return value;
    }

    if fallback_host.is_empty() {
        MetaString::empty()
    } else {
        MetaString::from(fallback_host)
    }
}

// GetOTelVersion returns the version based on OTel span and resource attributes, with span taking precedence.
fn get_otel_version(
    span_attributes: &[KeyValue], resource_attributes: &[KeyValue], ignore_missing_fields: bool,
) -> MetaString {
    if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_VERSION) {
        return value;
    }

    if ignore_missing_fields {
        return MetaString::empty();
    }

    if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, SERVICE_VERSION) {
        return value;
    }

    MetaString::empty()
}

fn get_otel_container_id(
    span_attributes: &[KeyValue], resource_attributes: &[KeyValue], ignore_missing_fields: bool,
) -> MetaString {
    if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_CONTAINER_ID) {
        return value;
    }

    if ignore_missing_fields {
        return MetaString::empty();
    }

    if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, CONTAINER_ID) {
        return value;
    }

    if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, K8S_POD_UID) {
        return value;
    }

    MetaString::empty()
}
// GetOTelStatusCode returns the HTTP status code based on OTel span and resource attributes, with span taking precedence.
fn get_otel_status_code(
    span_attributes: &[KeyValue], resource_attributes: &[KeyValue], ignore_missing_fields: bool,
) -> Option<i64> {
    if let Some(value) = get_int_attribute(span_attributes, KEY_DATADOG_HTTP_STATUS_CODE) {
        return Some(*value);
    }
    if let Some(value) = get_int_attribute(resource_attributes, KEY_DATADOG_HTTP_STATUS_CODE) {
        return Some(*value);
    }
    if !ignore_missing_fields {
        return get_int_attribute(span_attributes, HTTP_STATUS_CODE_KEY)
            .or_else(|| get_int_attribute(span_attributes, "http.response.status_code"))
            .or_else(|| get_int_attribute(resource_attributes, HTTP_STATUS_CODE_KEY))
            .or_else(|| get_int_attribute(resource_attributes, "http.response.status_code"))
            .copied();
    }
    None
}

fn bytes_to_hex_lowercase(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    let mut output = vec![0u8; bytes.len() * 2];
    if faster_hex::hex_encode(bytes, &mut output).is_err() {
        error!("Failed to encode bytes to hex");
        return String::new();
    }

    // SAFETY: faster_hex guarantees ASCII characters.
    unsafe { String::from_utf8_unchecked(output) }
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::common::v1::{AnyValue, KeyValue};
    use otlp_protos::opentelemetry::proto::resource::v1::Resource;
    use otlp_protos::opentelemetry::proto::trace::v1::Span as OtlpSpan;

    use super::*;

    // Helper to create a KeyValue with a string value
    fn kv_str(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(OtlpValue::StringValue(value.to_string())),
            }),
        }
    }

    // Helper to create a KeyValue with an int value
    fn kv_int(key: &str, value: i64) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(OtlpValue::IntValue(value)),
            }),
        }
    }

    fn kv_bool(key: &str, value: bool) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(OtlpValue::BoolValue(value)),
            }),
        }
    }

    // Semantic convention keys (matching Go's semconv117 and semconv127)
    const SEMCONV_DEPLOYMENT_ENVIRONMENT_NAME: &str = "deployment.environment.name"; // semconv127
    const SEMCONV_DEPLOYMENT_ENVIRONMENT: &str = "deployment.environment"; // semconv117
    const SEMCONV_SERVICE_VERSION: &str = "service.version";
    const SEMCONV_CONTAINER_ID: &str = "container.id";
    const SEMCONV_HOST_NAME: &str = "host.name";
    const SEMCONV_HTTP_STATUS_CODE: &str = "http.status_code";
    const SEMCONV_HTTP_RESPONSE_STATUS_CODE: &str = "http.response.status_code";
    const SEMCONV_DB_NAMESPACE: &str = "db.namespace";

    /// TestGetOTelEnv - Tests GetOTelEnv function
    /// Note: The Rust implementation doesn't have a direct GetOTelEnv function,
    /// but the logic is embedded in otel_span_to_dd_span. This test verifies
    /// the expected behavior based on Go's test cases.
    #[test]
    fn test_get_otel_env() {
        struct TestCase {
            name: &'static str,
            span_attrs: Vec<KeyValue>,
            resource_attrs: Vec<KeyValue>,
            expected: &'static str,
            ignore_missing_datadog_fields: bool,
        }

        let test_cases = vec![
            TestCase {
                name: "neither set",
                span_attrs: vec![],
                resource_attrs: vec![],
                expected: "",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "only in resource (semconv127)",
                span_attrs: vec![],
                resource_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT_NAME, "env-res-127")],
                expected: "env-res-127",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "only in resource (semconv117)",
                span_attrs: vec![],
                resource_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "env-res")],
                expected: "env-res",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "only in span (semconv127)",
                span_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT_NAME, "env-span-127")],
                resource_attrs: vec![],
                expected: "env-span-127",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "only in span (semconv117)",
                span_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "env-span")],
                resource_attrs: vec![],
                expected: "env-span",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "both set (span wins)",
                span_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "env-span")],
                resource_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "env-res")],
                expected: "env-span",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "normalization",
                span_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "  ENV ")],
                resource_attrs: vec![],
                expected: "_env",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "ignore missing datadog fields",
                span_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "env-span")],
                resource_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "env-span")],
                expected: "",
                ignore_missing_datadog_fields: true,
            },
            TestCase {
                name: "read from datadog fields",
                span_attrs: vec![
                    kv_str(KEY_DATADOG_ENVIRONMENT, "env-span"),
                    kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "env-span-semconv117"),
                ],
                resource_attrs: vec![
                    kv_str(KEY_DATADOG_ENVIRONMENT, "env-res"),
                    kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "env-res-semconv117"),
                ],
                expected: "env-span",
                ignore_missing_datadog_fields: false,
            },
        ];

        for tc in test_cases {
            let result = get_otel_env(&tc.span_attrs, &tc.resource_attrs, tc.ignore_missing_datadog_fields);
            assert_eq!(result.as_ref(), tc.expected, "test case: {}", tc.name);
        }
    }

    #[test]
    fn test_map_attribute_generic_matches_agent_rules() {
        let mut meta = FastHashMap::default();
        let mut metrics = FastHashMap::default();

        let http_attr = kv_str("http.request.method", "GET");
        map_attribute_generic(&http_attr, &mut meta, &mut metrics, false);
        assert_eq!(meta.get("http.method").map(|v| v.as_ref()), Some("GET"));

        let sampling_attr = kv_int("sampling.priority", 2);
        map_attribute_generic(&sampling_attr, &mut meta, &mut metrics, false);
        assert_eq!(metrics.get(SAMPLING_PRIORITY_METRIC_KEY), Some(&2.0));

        let analytics_attr = kv_bool(ANALYTICS_EVENT_KEY, true);
        map_attribute_generic(&analytics_attr, &mut meta, &mut metrics, false);
        assert_eq!(metrics.get(EVENT_EXTRACTION_METRIC_KEY), Some(&1.0));

        let dd_attr = kv_str("datadog.service", "svc");
        map_attribute_generic(&dd_attr, &mut meta, &mut metrics, false);
        assert!(!meta.contains_key("datadog.service"));

        let mut meta_ignore = FastHashMap::default();
        let mut metrics_ignore = FastHashMap::default();
        let env_attr = kv_str("env", "prod");
        map_attribute_generic(&env_attr, &mut meta_ignore, &mut metrics_ignore, true);
        assert!(meta_ignore.is_empty());
    }

    /// TestGetOTelHostname - Tests GetOTelHostname function
    #[test]
    fn test_get_otel_hostname() {
        struct TestCase {
            name: &'static str,
            span_attrs: Vec<KeyValue>,
            resource_attrs: Vec<KeyValue>,
            fallback_host: &'static str,
            expected: &'static str,
            ignore_missing_datadog_fields: bool,
        }

        let test_cases = vec![
            TestCase {
                name: "datadog.host.name",
                span_attrs: vec![],
                resource_attrs: vec![kv_str("datadog.host.name", "test-host")],
                fallback_host: "",
                expected: "test-host",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "_dd.hostname",
                span_attrs: vec![],
                resource_attrs: vec![kv_str("_dd.hostname", "test-host")],
                fallback_host: "",
                expected: "test-host",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "fallback hostname",
                span_attrs: vec![],
                resource_attrs: vec![],
                fallback_host: "test-host",
                expected: "test-host",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "ignore missing datadog fields",
                span_attrs: vec![],
                resource_attrs: vec![kv_str(SEMCONV_HOST_NAME, "test-host")],
                fallback_host: "",
                expected: "",
                ignore_missing_datadog_fields: true,
            },
            TestCase {
                name: "read from datadog fields",
                span_attrs: vec![
                    kv_str(KEY_DATADOG_HOST, "test-host"),
                    kv_str(SEMCONV_HOST_NAME, "test-host-semconv117"),
                ],
                resource_attrs: vec![
                    kv_str(KEY_DATADOG_HOST, "test-host"),
                    kv_str(SEMCONV_HOST_NAME, "test-host-semconv117"),
                ],
                fallback_host: "",
                expected: "test-host",
                ignore_missing_datadog_fields: false,
            },
        ];

        for tc in test_cases {
            let result = get_otel_hostname(
                &tc.span_attrs,
                &tc.resource_attrs,
                tc.fallback_host,
                tc.ignore_missing_datadog_fields,
            );
            assert_eq!(result.as_ref(), tc.expected, "test case: {}", tc.name);
        }
    }

    /// TestGetOTelVersion - Tests GetOTelVersion function
    #[test]
    fn test_get_otel_version() {
        struct TestCase {
            name: &'static str,
            span_attrs: Vec<KeyValue>,
            resource_attrs: Vec<KeyValue>,
            expected: &'static str,
            ignore_missing_datadog_fields: bool,
        }

        let test_cases = vec![
            TestCase {
                name: "neither set",
                span_attrs: vec![],
                resource_attrs: vec![],
                expected: "",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "only in resource",
                span_attrs: vec![],
                resource_attrs: vec![kv_str(SEMCONV_SERVICE_VERSION, "v1")],
                expected: "v1",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "only in span",
                span_attrs: vec![kv_str(SEMCONV_SERVICE_VERSION, "v3")],
                resource_attrs: vec![],
                expected: "v3",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "both set (span wins)",
                span_attrs: vec![kv_str(SEMCONV_SERVICE_VERSION, "v3")],
                resource_attrs: vec![kv_str(SEMCONV_SERVICE_VERSION, "v4")],
                expected: "v3",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "normalization",
                span_attrs: vec![kv_str(SEMCONV_SERVICE_VERSION, "  V1 ")],
                resource_attrs: vec![],
                expected: "_v1",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "ignore missing datadog fields",
                span_attrs: vec![kv_str(SEMCONV_SERVICE_VERSION, "v3")],
                resource_attrs: vec![kv_str(SEMCONV_SERVICE_VERSION, "v4")],
                expected: "",
                ignore_missing_datadog_fields: true,
            },
            TestCase {
                name: "read from datadog fields",
                span_attrs: vec![
                    kv_str(KEY_DATADOG_VERSION, "v3"),
                    kv_str(SEMCONV_SERVICE_VERSION, "v3-semconv117"),
                ],
                resource_attrs: vec![
                    kv_str(KEY_DATADOG_VERSION, "v4"),
                    kv_str(SEMCONV_SERVICE_VERSION, "v4-semconv117"),
                ],
                expected: "v3",
                ignore_missing_datadog_fields: false,
            },
        ];

        for tc in test_cases {
            let result = get_otel_version(&tc.span_attrs, &tc.resource_attrs, tc.ignore_missing_datadog_fields);
            assert_eq!(result.as_ref(), tc.expected, "test case: {}", tc.name);
        }
    }

    /// TestGetOTelContainerID - Tests GetOTelContainerID function
    #[test]
    fn test_get_otel_container_id() {
        struct TestCase {
            name: &'static str,
            span_attrs: Vec<KeyValue>,
            resource_attrs: Vec<KeyValue>,
            expected: &'static str,
            ignore_missing_datadog_fields: bool,
        }

        let test_cases = vec![
            TestCase {
                name: "neither set",
                span_attrs: vec![],
                resource_attrs: vec![],
                expected: "",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "only in resource",
                span_attrs: vec![],
                resource_attrs: vec![kv_str(SEMCONV_CONTAINER_ID, "cid-res")],
                expected: "cid-res",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "only in span",
                span_attrs: vec![kv_str(SEMCONV_CONTAINER_ID, "cid-span")],
                resource_attrs: vec![],
                expected: "cid-span",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "both set (span wins)",
                span_attrs: vec![kv_str(SEMCONV_CONTAINER_ID, "cid-span")],
                resource_attrs: vec![kv_str(SEMCONV_CONTAINER_ID, "cid-res")],
                expected: "cid-span",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "normalization",
                span_attrs: vec![kv_str(SEMCONV_CONTAINER_ID, "  CID ")],
                resource_attrs: vec![],
                expected: "_cid",
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "ignore missing datadog fields",
                span_attrs: vec![kv_str(SEMCONV_CONTAINER_ID, "cid-span")],
                resource_attrs: vec![kv_str(SEMCONV_CONTAINER_ID, "cid-span")],
                expected: "",
                ignore_missing_datadog_fields: true,
            },
            TestCase {
                name: "read from datadog fields",
                span_attrs: vec![
                    kv_str(KEY_DATADOG_CONTAINER_ID, "cid-span"),
                    kv_str(SEMCONV_CONTAINER_ID, "cid-span-semconv117"),
                ],
                resource_attrs: vec![
                    kv_str(KEY_DATADOG_CONTAINER_ID, "cid-res"),
                    kv_str(SEMCONV_CONTAINER_ID, "cid-res-semconv117"),
                ],
                expected: "cid-span",
                ignore_missing_datadog_fields: false,
            },
        ];

        for tc in test_cases {
            let result = get_otel_container_id(&tc.span_attrs, &tc.resource_attrs, tc.ignore_missing_datadog_fields);
            assert_eq!(result.as_ref(), tc.expected, "test case: {}", tc.name);
        }
    }

    /// TestGetOTelStatusCode - Tests GetOTelStatusCode function
    #[test]
    fn test_get_otel_status_code() {
        struct TestCase {
            name: &'static str,
            span_attrs: Vec<KeyValue>,
            resource_attrs: Vec<KeyValue>,
            expected: Option<i64>,
            ignore_missing_datadog_fields: bool,
        }

        let test_cases = vec![
            TestCase {
                name: "neither set",
                span_attrs: vec![],
                resource_attrs: vec![],
                expected: None,
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "only in span, only semconv117.HTTPStatusCodeKey",
                span_attrs: vec![kv_int(SEMCONV_HTTP_STATUS_CODE, 200)],
                resource_attrs: vec![],
                expected: Some(200),
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "only in span, both semconv117.HTTPStatusCodeKey and http.response.status_code, semconv117.HTTPStatusCodeKey wins",
                span_attrs: vec![
                    kv_int(SEMCONV_HTTP_STATUS_CODE, 200),
                    kv_int(SEMCONV_HTTP_RESPONSE_STATUS_CODE, 201),
                ],
                resource_attrs: vec![],
                expected: Some(200),
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "only in resource, only semconv117.HTTPStatusCodeKey",
                span_attrs: vec![],
                resource_attrs: vec![kv_int(SEMCONV_HTTP_STATUS_CODE, 201)],
                expected: Some(201),
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "only in resource, both semconv117.HTTPStatusCodeKey and http.response.status_code, semconv117.HTTPStatusCodeKey wins",
                span_attrs: vec![],
                resource_attrs: vec![
                    kv_int(SEMCONV_HTTP_STATUS_CODE, 201),
                    kv_int(SEMCONV_HTTP_RESPONSE_STATUS_CODE, 202),
                ],
                expected: Some(201),
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "both set (span wins)",
                span_attrs: vec![kv_int(SEMCONV_HTTP_STATUS_CODE, 203)],
                resource_attrs: vec![kv_int(SEMCONV_HTTP_STATUS_CODE, 204)],
                expected: Some(203),
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "ignore missing datadog fields",
                span_attrs: vec![kv_int(SEMCONV_HTTP_STATUS_CODE, 205)],
                resource_attrs: vec![],
                expected: None,
                ignore_missing_datadog_fields: true,
            },
            TestCase {
                name: "read from datadog fields",
                span_attrs: vec![
                    kv_int(KEY_DATADOG_HTTP_STATUS_CODE, 206),
                    kv_int(SEMCONV_HTTP_STATUS_CODE, 210),
                ],
                resource_attrs: vec![
                    kv_int(KEY_DATADOG_HTTP_STATUS_CODE, 207),
                    kv_int(SEMCONV_HTTP_STATUS_CODE, 211),
                ],
                expected: Some(206),
                ignore_missing_datadog_fields: false,
            },
        ];

        for tc in test_cases {
            let result = get_otel_status_code(&tc.span_attrs, &tc.resource_attrs, tc.ignore_missing_datadog_fields);
            assert_eq!(result, tc.expected, "test case: {}", tc.name);
        }
    }

    /// TestOtelSpanToDDSpanDBNameMapping - Tests db.namespace to db.name mapping
    #[test]
    fn test_otel_span_to_dd_span_db_name_mapping() {
        struct TestCase {
            name: &'static str,
            span_attrs: Vec<KeyValue>,
            resource_attrs: Vec<KeyValue>,
            expected_name: &'static str,
            should_map: bool,
        }

        let test_cases = vec![
            TestCase {
                name: "db.namespace in span attributes, no db.name",
                span_attrs: vec![kv_str(SEMCONV_DB_NAMESPACE, "testdb")],
                resource_attrs: vec![],
                expected_name: "testdb",
                should_map: true,
            },
            TestCase {
                name: "db.namespace in resource attributes, no db.name",
                span_attrs: vec![],
                resource_attrs: vec![kv_str(SEMCONV_DB_NAMESPACE, "testdb")],
                expected_name: "testdb",
                should_map: true,
            },
            TestCase {
                name: "db.namespace in both, resource takes precedence",
                span_attrs: vec![kv_str(SEMCONV_DB_NAMESPACE, "span-db")],
                resource_attrs: vec![kv_str(SEMCONV_DB_NAMESPACE, "resource-db")],
                expected_name: "resource-db",
                should_map: true,
            },
            TestCase {
                name: "db.name already exists, should not map",
                span_attrs: vec![kv_str("db.name", "existing-db"), kv_str(SEMCONV_DB_NAMESPACE, "testdb")],
                resource_attrs: vec![],
                expected_name: "existing-db",
                should_map: false,
            },
            TestCase {
                name: "no db.namespace, should not map",
                span_attrs: vec![],
                resource_attrs: vec![],
                expected_name: "",
                should_map: false,
            },
        ];

        for tc in test_cases {
            let span = OtlpSpan {
                name: "test-span".to_string(),
                attributes: tc.span_attrs,
                ..Default::default()
            };
            let resource = Resource {
                attributes: tc.resource_attrs,
                ..Default::default()
            };

            let dd_span = otel_span_to_dd_span(&span, &resource, None, false, true);
            let meta = dd_span.meta();

            if tc.should_map {
                assert_eq!(
                    meta.get("db.name").map(|s| s.as_ref()),
                    Some(tc.expected_name),
                    "test case: {}",
                    tc.name
                );
            } else if !tc.expected_name.is_empty() {
                assert_eq!(
                    meta.get("db.name").map(|s| s.as_ref()),
                    Some(tc.expected_name),
                    "test case: {}",
                    tc.name
                );
            } else {
                assert!(
                    meta.get("db.name").is_none() || meta.get("db.name").map(|s| s.as_ref()) == Some(""),
                    "test case: {}",
                    tc.name
                );
            }
        }
    }
}
