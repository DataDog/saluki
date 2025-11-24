#![allow(dead_code)]

use std::convert::TryFrom;

use opentelemetry_semantic_conventions::resource::SERVICE_NAME;
use otlp_protos::opentelemetry::proto::common::v1::{
    any_value::Value as OtlpValue, InstrumentationScope as OtlpInstrumentationScope, KeyValue,
};
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use otlp_protos::opentelemetry::proto::trace::v1::{span::SpanKind, status::StatusCode, Span as OtlpSpan};
use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::Span as DdSpan;
use stringtheory::MetaString;
use tracing::error;

use super::translator::{convert_span_id, convert_trace_id};
use crate::sources::otlp::attributes::{get_int_attribute, get_string_attribute};
use crate::sources::otlp::traces::normalize::{normalize_name, normalize_service};

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
const GRAPHQL_OPERATION_TYPE_KEY: &str = "graphql.operation.type";
const GRAPHQL_OPERATION_NAME_KEY: &str = "graphql.operation.name";
const FAAS_INVOKED_PROVIDER_KEY: &str = "faas.invoked_provider";
const FAAS_INVOKED_NAME_KEY: &str = "faas.invoked_name";
const FAAS_TRIGGER_KEY: &str = "faas.trigger";
const NETWORK_PROTOCOL_NAME_KEY: &str = "network.protocol.name";
const HTTP_STATUS_CODE_KEY: &str = "http.status_code";
const SPAN_KIND_META_KEY: &str = "span.kind";
const OTEL_TRACE_ID_META_KEY: &str = "otel.trace_id";
const W3C_TRACESTATE_META_KEY: &str = "w3c.tracestate";
const OTEL_SCOPE_NAME_META_KEY: &str = "otel.scope.name";
const OTEL_SCOPE_VERSION_META_KEY: &str = "otel.scope.version";
const OTEL_STATUS_CODE_META_KEY: &str = "otel.status_code";
const OTEL_STATUS_DESCRIPTION_META_KEY: &str = "otel.status_description";

const DD_NAMESPACED_TO_APM_CONVENTIONS: &[(&str, &str)] = &[
    (KEY_DATADOG_ENVIRONMENT, "env"),
    (KEY_DATADOG_VERSION, "version"),
    (KEY_DATADOG_ERROR_MSG, "error.msg"),
    (KEY_DATADOG_ERROR_TYPE, "error.type"),
    (KEY_DATADOG_ERROR_STACK, "error.stack"),
    (KEY_DATADOG_HTTP_STATUS_CODE, HTTP_STATUS_CODE_KEY),
];

pub fn otel_span_to_dd_span(
    otel_span: &OtlpSpan, otel_resource: &Resource, instrumentation_scope: Option<&OtlpInstrumentationScope>,
    ignore_missing_fields: bool,
) -> DdSpan {
    let span_attributes = &otel_span.attributes;
    let resource_attributes = &otel_resource.attributes;

    let (dd_span, mut meta, mut metrics) =
        otel_to_dd_span_minimal(otel_span, otel_resource, instrumentation_scope, ignore_missing_fields);

    // 1) DD namespaced keys take precedence over OTLP keys, so use them first
    for (dd_key, apm_key) in DD_NAMESPACED_TO_APM_CONVENTIONS {
        if let Some(value) = use_both_maps(span_attributes, resource_attributes, dd_key) {
            meta.entry((*apm_key).into()).or_insert(value);
        }
    }

    // 2) Span attributes take precedence over resource attributes in the event of key collisions
    for attribute in span_attributes {
        map_attribute_generic(attribute, &mut meta, &mut metrics);
    }
    for attribute in resource_attributes {
        map_attribute_generic(attribute, &mut meta, &mut metrics);
    }

    if !otel_span.trace_id.is_empty() {
        meta.insert(
            OTEL_TRACE_ID_META_KEY.into(),
            bytes_to_hex_lowercase(&otel_span.trace_id).into(),
        );
    }

    if let Some(status) = otel_span.status.as_ref() {
        let code = StatusCode::try_from(status.code).unwrap_or(StatusCode::Unset);
        meta.entry(OTEL_STATUS_CODE_META_KEY.into())
            .or_insert_with(|| MetaString::from(code.as_str_name().to_ascii_lowercase()));
        if !status.message.is_empty() {
            meta.entry(OTEL_STATUS_DESCRIPTION_META_KEY.into())
                .or_insert_with(|| status.message.as_str().into());
        }
    }

    if let Some(scope) = instrumentation_scope {
        if !scope.name.is_empty() {
            meta.entry(OTEL_SCOPE_NAME_META_KEY.into())
                .or_insert_with(|| scope.name.as_str().into());
        }
        if !scope.version.is_empty() {
            meta.entry(OTEL_SCOPE_VERSION_META_KEY.into())
                .or_insert_with(|| scope.version.as_str().into());
        }
    }

    if !otel_span.trace_state.is_empty() {
        meta.entry(W3C_TRACESTATE_META_KEY.into())
            .or_insert_with(|| otel_span.trace_state.as_str().into());
    }

    dd_span.with_meta(Some(meta)).with_metrics(Some(metrics))
}

pub fn otel_to_dd_span_minimal(
    otel_span: &OtlpSpan, otel_resource: &Resource, _instrumentation_scope: Option<&OtlpInstrumentationScope>,
    ignore_missing_fields: bool,
) -> (
    DdSpan,
    FastHashMap<MetaString, MetaString>,
    FastHashMap<MetaString, f64>,
) {
    let span_attributes = &otel_span.attributes;
    let resource_attributes = &otel_resource.attributes;

    let trace_id = convert_trace_id(&otel_span.trace_id);
    let span_id = convert_span_id(&otel_span.span_id);
    let parent_id = convert_span_id(&otel_span.parent_span_id);
    let start = i64::try_from(otel_span.start_time_unix_nano).unwrap_or(i64::MAX);
    let duration_nanos = otel_span
        .end_time_unix_nano
        .saturating_sub(otel_span.start_time_unix_nano);
    let duration = i64::try_from(duration_nanos).unwrap_or(i64::MAX);
    let mut meta = FastHashMap::default();
    meta.reserve(span_attributes.len() + resource_attributes.len());
    let mut metrics = FastHashMap::default();

    let error = if let Some(value) = get_string_attribute(span_attributes, KEY_DATADOG_ERROR) {
        value.parse::<i32>().unwrap_or(0)
    } else if let Some(status) = &otel_span.status {
        if StatusCode::try_from(status.code).unwrap_or(StatusCode::Unset) == StatusCode::Error {
            1
        } else {
            0
        }
    } else {
        0
    };

    let mut service =
        use_both_maps(span_attributes, resource_attributes, KEY_DATADOG_SERVICE).unwrap_or_else(MetaString::empty);
    let mut name =
        use_both_maps(span_attributes, resource_attributes, KEY_DATADOG_NAME).unwrap_or_else(MetaString::empty);
    let mut resource =
        use_both_maps(span_attributes, resource_attributes, KEY_DATADOG_RESOURCE).unwrap_or_else(MetaString::empty);
    let mut span_type =
        use_both_maps(span_attributes, resource_attributes, KEY_DATADOG_TYPE).unwrap_or_else(MetaString::empty);

    if !ignore_missing_fields {
        if service.is_empty() {
            // Normalize enabled (true)
            service = get_otel_service(span_attributes, resource_attributes, true);
        }
        if name.is_empty() {
            // Returns normalized
            name = get_otel_operation_name_v2(otel_span, span_attributes, resource_attributes);
        }
        if resource.is_empty() {
            // Returns truncated/normalized
            resource = get_otel_resource_v2_truncated(otel_span, span_attributes, resource_attributes);
        }
        if span_type.is_empty() {
            span_type = get_otel_span_type(otel_span, span_attributes, resource_attributes);
        }
    }

    let dd_span = DdSpan::new(
        service, name, resource, span_type, trace_id, span_id, parent_id, start, duration, error,
    );

    let incoming_span_kind = use_both_maps(span_attributes, resource_attributes, KEY_DATADOG_SPAN_KIND);
    if let Some(value) = incoming_span_kind {
        meta.entry(SPAN_KIND_META_KEY.into()).or_insert(value);
    } else {
        meta.entry(SPAN_KIND_META_KEY.into()).or_insert_with(|| {
            MetaString::from(span_kind_name(
                SpanKind::try_from(otel_span.kind).unwrap_or(SpanKind::Unspecified),
            ))
        });
    }

    if let Some(status_code) = get_otel_status_code(span_attributes, resource_attributes, ignore_missing_fields) {
        metrics.entry(HTTP_STATUS_CODE_KEY.into()).or_insert(status_code as f64);
    }

    (dd_span, meta, metrics)
}

/// Returns the DD service name based on OTel span and resource attributes.
fn get_otel_service(span_attributes: &[KeyValue], resource_attributes: &[KeyValue], normalize: bool) -> MetaString {
    let service = use_both_maps(span_attributes, resource_attributes, SERVICE_NAME).unwrap_or_else(|| {
        // We do not fallback to `otel_span.name` here, as that is a legacy behavior that we want to avoid.
        MetaString::from_static(DEFAULT_SERVICE_NAME)
    });

    if normalize {
        let normalized = normalize_service(&service);
        if normalized == service {
            service
        } else {
            normalized
        }
    } else {
        service
    }
}

fn get_otel_operation_name_v2(
    otel_span: &OtlpSpan, span_attributes: &[KeyValue], resource_attributes: &[KeyValue],
) -> MetaString {
    if let Some(value) = use_both_maps(span_attributes, resource_attributes, OPERATION_NAME_KEY) {
        return normalize_name(&value);
    }

    let span_kind = SpanKind::try_from(otel_span.kind).unwrap_or(SpanKind::Unspecified);
    let is_client = matches!(span_kind, SpanKind::Client);
    let is_server = matches!(span_kind, SpanKind::Server);

    for http_request_method_key in HTTP_REQUEST_METHOD_KEYS {
        if use_both_maps(span_attributes, resource_attributes, http_request_method_key).is_some() {
            if is_server {
                return MetaString::from_static("http.server.request");
            }
            if is_client {
                return MetaString::from_static("http.client.request");
            }
        }
    }
    // database
    if let Some(db_system) = use_both_maps(span_attributes, resource_attributes, DB_SYSTEM_KEY) {
        if is_client {
            return MetaString::from(format!("{db_system}.query"));
        }
    }
    // messaging
    if let (Some(system), Some(operation)) = (
        use_both_maps(span_attributes, resource_attributes, MESSAGING_SYSTEM_KEY),
        use_both_maps(span_attributes, resource_attributes, MESSAGING_OPERATION_KEY),
    ) {
        match span_kind {
            SpanKind::Client | SpanKind::Server | SpanKind::Consumer | SpanKind::Producer => {
                return MetaString::from(format!("{system}.{operation}"))
            }
            _ => {}
        }
    }
    // RPC & AWS
    if let Some(rpc_system) = use_both_maps(span_attributes, resource_attributes, RPC_SYSTEM_KEY) {
        let is_aws = rpc_system == "aws-api";
        if is_aws && is_client {
            if let Some(service) = use_both_maps(span_attributes, resource_attributes, RPC_SERVICE_KEY) {
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
            // TODO: add normalization logic
            use_both_maps(span_attributes, resource_attributes, FAAS_INVOKED_PROVIDER_KEY),
            use_both_maps(span_attributes, resource_attributes, FAAS_INVOKED_NAME_KEY),
        ) {
            return MetaString::from(format!("{provider}.{invoked}.invoke"));
        }
    }
    // FAAS server
    if is_server {
        // TODO: add normalization logic
        if let Some(trigger) = use_both_maps(span_attributes, resource_attributes, FAAS_TRIGGER_KEY) {
            return MetaString::from(format!("{trigger}.invoke"));
        }
    }

    if use_both_maps(span_attributes, resource_attributes, GRAPHQL_OPERATION_TYPE_KEY).is_some() {
        return MetaString::from_static("graphql.server.request");
    }

    if is_server {
        if let Some(protocol) = get_attr_from_both(span_attributes, resource_attributes, NETWORK_PROTOCOL_NAME_KEY) {
            return MetaString::from(format!("{protocol}.server.request"));
        }
        return MetaString::from_static("server.request");
    }
    if is_client {
        if let Some(protocol) = get_attr_from_both(span_attributes, resource_attributes, NETWORK_PROTOCOL_NAME_KEY) {
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

fn get_otel_resource_v2(
    otel_span: &OtlpSpan, span_attributes: &[KeyValue], resource_attributes: &[KeyValue],
) -> MetaString {
    let span_kind = SpanKind::try_from(otel_span.kind).unwrap_or(SpanKind::Unspecified);
    if let Some(value) = get_attr_from_both(span_attributes, resource_attributes, RESOURCE_NAME_KEY) {
        return value.into();
    }

    if let Some(method) = get_attr_from_both_any(span_attributes, resource_attributes, HTTP_REQUEST_METHOD_KEYS) {
        let mut resource_name = if method == "_OTHER" {
            String::from("HTTP")
        } else {
            method.to_string()
        };
        if span_kind == SpanKind::Server {
            if let Some(route) = get_attr_from_both(span_attributes, resource_attributes, HTTP_ROUTE_KEY) {
                if !route.is_empty() {
                    resource_name.push(' ');
                    resource_name.push_str(route);
                }
            }
        }
        return MetaString::from(resource_name);
    }

    if let Some(operation) = get_attr_from_both(span_attributes, resource_attributes, MESSAGING_OPERATION_KEY) {
        let mut resource_name = operation.to_string();
        if let Some(dest) = get_attr_from_both_any(span_attributes, resource_attributes, MESSAGING_DESTINATION_KEYS) {
            if !dest.is_empty() {
                resource_name.push(' ');
                resource_name.push_str(dest);
            }
        }
        return MetaString::from(resource_name);
    }

    if let Some(method) = get_attr_from_both(span_attributes, resource_attributes, RPC_METHOD_KEY) {
        let mut resource_name = method.to_string();
        if let Some(service) = get_attr_from_both(span_attributes, resource_attributes, RPC_SERVICE_KEY) {
            resource_name.push(' ');
            resource_name.push_str(service);
        }
        return MetaString::from(resource_name);
    }

    if let Some(op_type) = get_attr_from_both(span_attributes, resource_attributes, GRAPHQL_OPERATION_TYPE_KEY) {
        let mut resource_name = op_type.to_string();
        if let Some(op_name) = get_attr_from_both(span_attributes, resource_attributes, GRAPHQL_OPERATION_NAME_KEY) {
            resource_name.push(' ');
            resource_name.push_str(op_name);
        }
        return MetaString::from(resource_name);
    }

    if get_attr_from_both(span_attributes, resource_attributes, DB_SYSTEM_KEY).is_some() {
        if let Some(statement) = get_attr_from_both(span_attributes, resource_attributes, DB_STATEMENT_KEY) {
            return MetaString::from(statement);
        }
        if let Some(query) = get_attr_from_both(span_attributes, resource_attributes, DB_QUERY_TEXT_KEY) {
            return MetaString::from(query);
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
    if res_name.len() > crate::sources::otlp::traces::normalize::MAX_RESOURCE_LEN {
        let truncated = &res_name[..crate::sources::otlp::traces::normalize::MAX_RESOURCE_LEN];
        // Ensure we don't split UTF-8 char
        let mut end = truncated.len();
        while end > 0 && !truncated.is_char_boundary(end) {
            end -= 1;
        }
        MetaString::from(&res_name[..end])
    } else {
        res_name
    }
}

fn get_otel_span_type(
    otel_span: &OtlpSpan, span_attributes: &[KeyValue], resource_attributes: &[KeyValue],
) -> MetaString {
    if let Some(value) = get_attr_from_both(span_attributes, resource_attributes, "span.type") {
        return value.into();
    }

    let span_kind = SpanKind::try_from(otel_span.kind).unwrap_or(SpanKind::Unspecified);
    let span_type = match span_kind {
        SpanKind::Server => "web",
        SpanKind::Client => {
            if let Some(db_system) = get_attr_from_both(span_attributes, resource_attributes, DB_SYSTEM_KEY) {
                map_db_system_to_span_type(db_system)
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

fn get_attr_from_both<'a>(map: &'a [KeyValue], map2: &'a [KeyValue], key: &str) -> Option<&'a str> {
    if let Some(value) = get_string_attribute(map, key) {
        return Some(value);
    }
    get_string_attribute(map2, key)
}

fn get_attr_from_both_any<'a>(
    span_attributes: &'a [KeyValue], resource_attributes: &'a [KeyValue], keys: &[&str],
) -> Option<&'a str> {
    for key in keys {
        if let Some(value) = get_attr_from_both(span_attributes, resource_attributes, key) {
            return Some(value);
        }
    }
    None
}

fn map_attribute_generic(
    attribute: &KeyValue, meta: &mut FastHashMap<MetaString, MetaString>, metrics: &mut FastHashMap<MetaString, f64>,
) {
    let Some(value) = attribute.value.as_ref().and_then(|wrapper| wrapper.value.as_ref()) else {
        return;
    };

    match value {
        OtlpValue::StringValue(s) => {
            set_meta_field_generic(attribute.key.as_str(), s.as_str(), meta, metrics);
        }
        OtlpValue::BoolValue(b) => {
            set_meta_field_generic(attribute.key.as_str(), if *b { "true" } else { "false" }, meta, metrics);
        }
        OtlpValue::IntValue(i) => {
            metrics.entry(attribute.key.as_str().into()).or_insert(*i as f64);
        }
        OtlpValue::DoubleValue(d) => {
            metrics.entry(attribute.key.as_str().into()).or_insert(*d);
        }
        OtlpValue::BytesValue(bytes) => {
            meta.entry(attribute.key.as_str().into())
                .or_insert_with(|| format!("<{} bytes>", bytes.len()).into());
        }
        _ => {
            // Skip complex values for now.
        }
    }
}

fn set_meta_field_generic(
    key: &str, value: &str, meta: &mut FastHashMap<MetaString, MetaString>, metrics: &mut FastHashMap<MetaString, f64>,
) {
    if value.is_empty() {
        return;
    }
    match key {
        "service.name" | "operation.name" | "resource.name" | "span.type" => {
            // These are handled by specific logic in `otel_to_dd_span_minimal` and should not be added to meta.
        }
        "sampling.priority" => {
            if let Ok(parsed) = value.parse::<f64>() {
                metrics.entry(SAMPLING_PRIORITY_METRIC_KEY.into()).or_insert(parsed);
            }
        }
        _ => {
            meta.entry(key.into()).or_insert_with(|| value.into());
        }
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

fn use_both_maps(map: &[KeyValue], map2: &[KeyValue], key: &str) -> Option<MetaString> {
    if let Some(value) = get_string_attribute(map, key) {
        return Some(MetaString::from(value));
    }
    get_string_attribute(map2, key).map(MetaString::from)
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
