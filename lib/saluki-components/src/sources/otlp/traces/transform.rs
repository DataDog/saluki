#![allow(dead_code)]

use std::convert::TryFrom;

use opentelemetry_semantic_conventions::resource::{
    CONTAINER_ID, DEPLOYMENT_ENVIRONMENT_NAME, HOST_NAME, K8S_POD_UID, SERVICE_NAME, SERVICE_VERSION,
};
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
use crate::sources::otlp::traces::normalize::{normalize_name, normalize_service, normalize_tag_value};

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
const DB_NAMESPACE_KEY: &str = "db.namespace";
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
const INTERNAL_DD_HOSTNAME_KEY: &str = "_dd.hostname";
const LEGACY_DEPLOYMENT_ENVIRONMENT_KEY: &str = "deployment.environment";
const DATADOG_HOSTNAME_ATTR: &str = "datadog.host.name";

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
    ignore_missing_fields: bool,
) -> DdSpan {
    let span_attributes = &otel_span.attributes;
    let resource_attributes = &otel_resource.attributes;

    let (dd_span, mut meta, mut metrics) =
        otel_to_dd_span_minimal(otel_span, otel_resource, instrumentation_scope, ignore_missing_fields);

    // 1) DD namespaced keys take precedence over OTLP keys, so use them first
    for (dd_key, apm_key) in DD_NAMESPACED_TO_APM_CONVENTIONS {
        if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, dd_key) {
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

    if !meta.contains_key("db.name") {
        if let Some(db_namespace) =
            use_both_maps(resource_attributes, span_attributes, false, DB_NAMESPACE_KEY)
        {
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

    let error = if let Some(value) = get_int_attribute(span_attributes, KEY_DATADOG_ERROR) {
        *value as i32
    } else if let Some(value) = get_string_attribute(span_attributes, KEY_DATADOG_ERROR) {
        value.parse::<i32>().unwrap_or(0)
    } else if let Some(status) = &otel_span.status {
        if status.code() == StatusCode::Error {
            1
        } else {
            0
        }
    } else {
        0
    };

    let incoming_span_kind = use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_SPAN_KIND);
    if let Some(value) = incoming_span_kind {
        meta.entry(SPAN_KIND_META_KEY.into()).or_insert(value);
    } else {
        meta.entry(SPAN_KIND_META_KEY.into()).or_insert_with(|| {
            MetaString::from(span_kind_name(
                SpanKind::try_from(otel_span.kind).unwrap_or(SpanKind::Unspecified),
            ))
        });
    }

    let mut service =
        use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_SERVICE).unwrap_or_else(MetaString::empty);
    let mut name =
        use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_NAME).unwrap_or_else(MetaString::empty);
    let mut resource =
        use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_RESOURCE).unwrap_or_else(MetaString::empty);
    let mut span_type =
        use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_TYPE).unwrap_or_else(MetaString::empty);

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

    
    if let Some(status_code) = get_otel_status_code(span_attributes, resource_attributes, ignore_missing_fields) {
        metrics.entry(HTTP_STATUS_CODE_KEY.into()).or_insert(status_code as f64);
    }

    (dd_span, meta, metrics)
}

/// Returns the DD service name based on OTel span and resource attributes.
fn get_otel_service(span_attributes: &[KeyValue], resource_attributes: &[KeyValue], normalize: bool) -> MetaString {
    let service = use_both_maps(span_attributes, resource_attributes, true, SERVICE_NAME).unwrap_or_else(|| {
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
    if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, OPERATION_NAME_KEY) {
        return normalize_name(value);
    }

    let span_kind = SpanKind::try_from(otel_span.kind).unwrap_or(SpanKind::Unspecified);
    let is_client = matches!(span_kind, SpanKind::Client);
    let is_server = matches!(span_kind, SpanKind::Server);

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
    if let Some(db_system) = use_both_maps(span_attributes, resource_attributes, true, DB_SYSTEM_KEY) {
        if is_client {
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
        if let Some(protocol) =
            use_both_maps(span_attributes, resource_attributes, true, NETWORK_PROTOCOL_NAME_KEY)
        {
            return MetaString::from(format!("{protocol}.server.request"));
        }
        return MetaString::from_static("server.request");
    }
    if is_client {
        if let Some(protocol) =
            use_both_maps(span_attributes, resource_attributes, true, NETWORK_PROTOCOL_NAME_KEY)
        {
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
    if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, RESOURCE_NAME_KEY) {
        return value;
    }

    if let Some(method) = use_both_maps_any(span_attributes, resource_attributes, HTTP_REQUEST_METHOD_KEYS) {
        let mut resource_name = if method.as_ref() == "_OTHER" {
            String::from("HTTP")
        } else {
            method.as_ref().to_string()
        };
        if span_kind == SpanKind::Server {
            if let Some(route) = use_both_maps(span_attributes, resource_attributes, true, HTTP_ROUTE_KEY) {
                if !route.is_empty() {
                    resource_name.push(' ');
                    resource_name.push_str(route.as_ref());
                }
            }
        }
        return MetaString::from(resource_name);
    }

    if let Some(operation) = use_both_maps(span_attributes, resource_attributes, true, MESSAGING_OPERATION_KEY) {
        let mut resource_name = operation.as_ref().to_string();
        if let Some(dest) = use_both_maps_any(span_attributes, resource_attributes, MESSAGING_DESTINATION_KEYS) {
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

    if let Some(op_type) = use_both_maps(span_attributes, resource_attributes, true, GRAPHQL_OPERATION_TYPE_KEY) {
        let mut resource_name = op_type.as_ref().to_string();
        if let Some(op_name) =
            use_both_maps(span_attributes, resource_attributes, true, GRAPHQL_OPERATION_NAME_KEY)
        {
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

fn use_both_maps(map: &[KeyValue], map2: &[KeyValue], normalize: bool, key: &str) -> Option<MetaString> {
    if let Some(value) = get_string_attribute(map, key) {
        return Some(if normalize { normalize_tag_value(value) } else { MetaString::from(value) });
    }
    get_string_attribute(map2, key)
        .map(|value| if normalize { normalize_tag_value(value) } else { MetaString::from(value) })
}

fn use_both_maps_any(
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

    if let Some(value) =
        use_both_maps(span_attributes, resource_attributes, true, DEPLOYMENT_ENVIRONMENT_NAME)
    {
        return value;
    }

    if let Some(value) =
        use_both_maps(span_attributes, resource_attributes, true, LEGACY_DEPLOYMENT_ENVIRONMENT_KEY)
    {
        return value;
    }

    MetaString::empty()
}

fn get_otel_hostname(
    span_attributes: &[KeyValue], resource_attributes: &[KeyValue], fallback_host: &str,
    ignore_missing_fields: bool,
) -> MetaString {
    if let Some(value) = use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_HOST) {
        return value;
    }

    if ignore_missing_fields {
        return MetaString::empty();
    }

    if let Some(value) =
        use_both_maps(span_attributes, resource_attributes, false, DATADOG_HOSTNAME_ATTR)
    {
        return value;
    }

    if let Some(value) =
        use_both_maps(span_attributes, resource_attributes, false, INTERNAL_DD_HOSTNAME_KEY)
    {
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
    if let Some(value) =
        use_both_maps(span_attributes, resource_attributes, true, KEY_DATADOG_CONTAINER_ID)
    {
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
    use super::*;
    use otlp_protos::opentelemetry::proto::common::v1::{AnyValue, KeyValue};
    use otlp_protos::opentelemetry::proto::resource::v1::Resource;
    use otlp_protos::opentelemetry::proto::trace::v1::Span as OtlpSpan;

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
            let result =
                get_otel_container_id(&tc.span_attrs, &tc.resource_attrs, tc.ignore_missing_datadog_fields);
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
                span_attrs: vec![
                    kv_str("db.name", "existing-db"),
                    kv_str(SEMCONV_DB_NAMESPACE, "testdb"),
                ],
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

            let dd_span = otel_span_to_dd_span(&span, &resource, None, false);
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
