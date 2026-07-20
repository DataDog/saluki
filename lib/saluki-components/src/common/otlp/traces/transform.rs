#![allow(dead_code)]

use std::convert::TryFrom;

use base64::{engine::general_purpose, Engine as _};
use opentelemetry_semantic_conventions::resource::{
    CONTAINER_ID, DEPLOYMENT_ENVIRONMENT_NAME, K8S_POD_UID, SERVICE_NAME, SERVICE_VERSION,
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
use saluki_common::strings::StringBuilder;
use saluki_core::data_model::event::trace::{AttributeValue, Span as DdSpan};
use serde_json::{Map as JsonMap, Value as JsonValue};
use stringtheory::interning::{GenericMapInterner, Interner};
use stringtheory::MetaString;
use tracing::error;

use crate::common::datadog::{OTEL_TRACE_ID_META_KEY, SAMPLING_PRIORITY_METRIC_KEY};
use crate::common::otlp::attributes::{get_int_attribute, HTTP_MAPPINGS};
use crate::common::otlp::semantics::{
    lookup_int64, lookup_string, Accessor, Concept, OtelSpanAccessor, OtlpAttributesAccessor, REGISTRY,
};
use crate::common::otlp::traces::normalize::{
    is_normalized_tag_value, normalize_service_into, normalize_tag_value_append_unchecked,
    normalize_tag_value_into_unchecked,
};
use crate::common::otlp::traces::normalize::{truncate_utf8, MAX_RESOURCE_LEN};
use crate::common::otlp::traces::translator::convert_span_id;
use crate::common::otlp::util::get_string_attribute;
use crate::common::otlp::util::{
    DEPLOYMENT_ENVIRONMENT_KEY, KEY_DATADOG_CONTAINER_ID, KEY_DATADOG_ENVIRONMENT, KEY_DATADOG_VERSION,
};

const EVENT_EXTRACTION_METRIC_KEY: &str = "_dd1.sr.eausr";
const ANALYTICS_EVENT_KEY: &str = "analytics.event";
const HTTP_REQUEST_HEADER_PREFIX: &str = "http.request.header.";
const HTTP_REQUEST_HEADERS_PREFIX: &str = "http.request.headers.";

// Datadog-specific attribute keys used only within this translator.
const KEY_DATADOG_SERVICE: &str = "datadog.service";
const KEY_DATADOG_NAME: &str = "datadog.name";
const KEY_DATADOG_RESOURCE: &str = "datadog.resource";
const KEY_DATADOG_SPAN_KIND: &str = "datadog.span.kind";
const KEY_DATADOG_TYPE: &str = "datadog.type";
const KEY_DATADOG_ERROR: &str = "datadog.error";
const KEY_DATADOG_ERROR_MSG: &str = "datadog.error.msg";
const KEY_DATADOG_ERROR_TYPE: &str = "datadog.error.type";
const KEY_DATADOG_ERROR_STACK: &str = "datadog.error.stack";
const KEY_DATADOG_HTTP_STATUS_CODE: &str = "datadog.http_status_code";

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
const GRPC_STATUS_CODE_META_KEY: &str = "rpc.grpc.status_code";
const W3C_TRACESTATE_META_KEY: &str = "w3c.tracestate";
const OTEL_LIBRARY_NAME_META_KEY: &str = "otel.library.name";
const OTEL_LIBRARY_VERSION_META_KEY: &str = "otel.library.version";
const OTEL_SCOPE_NAME_META_KEY: &str = "otel.scope.name";
const OTEL_SCOPE_VERSION_META_KEY: &str = "otel.scope.version";
const OTEL_STATUS_CODE_META_KEY: &str = "otel.status_code";
const OTEL_STATUS_DESCRIPTION_META_KEY: &str = "otel.status_description";
const INTERNAL_DD_HOSTNAME_KEY: &str = "_dd.hostname";
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

// Behavior gated on the Datadog Agent version ADP was built against. The Agent version is baked in at build time (see
// `datadog_agent_commons::agent_version`), so each gate resolves to a compile-time constant and the unused branch is
// eliminated. Add further version-gated toggles here as the Agent's output evolves.

// `otel.scope.{name,version}` span meta were added to the Agent's OTLP trace conversion in 7.82; only emit them when
// the Agent version meets that threshold.
const EMIT_OTEL_SCOPE_META: bool = datadog_agent_commons::agent_version::meets(7, 82, 0);

// otel_span_to_dd_span converts an OTLP span to DD span and is based on the logic defined in the agent.
// https://github.com/DataDog/datadog-agent/blob/instrument-otlp-traffic/pkg/trace/transform/transform.go#L357
pub fn otel_span_to_dd_span(
    otel_span: &OtlpSpan, otel_resource: &Resource, instrumentation_scope: Option<&OtlpInstrumentationScope>,
    ignore_missing_fields: bool, compute_top_level_by_span_kind: bool, interner: &GenericMapInterner,
    string_builder: &mut StringBuilder<GenericMapInterner>, trace_id_hex: Option<&MetaString>,
) -> DdSpan {
    let span_attributes = &otel_span.attributes;
    let resource_attributes = &otel_resource.attributes;
    let (mut dd_span, mut attrs) = otel_to_dd_span_minimal(
        otel_span,
        otel_resource,
        instrumentation_scope,
        ignore_missing_fields,
        compute_top_level_by_span_kind,
        interner,
        string_builder,
    );

    for (dd_key, apm_key) in DD_NAMESPACED_TO_APM_CONVENTIONS {
        if let Some(value) = use_both_maps(
            span_attributes,
            resource_attributes,
            true,
            dd_key,
            interner,
            string_builder,
        ) {
            attrs.insert(MetaString::from_static(apm_key), AttributeValue::String(value));
        }
    }

    for attribute in span_attributes {
        map_attribute_generic(attribute, &mut attrs, ignore_missing_fields, interner, string_builder);
    }

    if let Some(trace_id_hex) = trace_id_hex {
        if !trace_id_hex.is_empty() {
            attrs.insert(
                MetaString::from_static(OTEL_TRACE_ID_META_KEY),
                AttributeValue::String(trace_id_hex.clone()),
            );
        }
    } else if !otel_span.trace_id.is_empty() {
        attrs.insert(
            MetaString::from_static(OTEL_TRACE_ID_META_KEY),
            AttributeValue::String(bytes_to_hex_lowercase(&otel_span.trace_id).into()),
        );
    }

    if !attrs.contains_key("version") {
        let version = get_otel_version(
            span_attributes,
            resource_attributes,
            ignore_missing_fields,
            interner,
            string_builder,
        );
        if !version.is_empty() {
            attrs.insert(MetaString::from_static("version"), AttributeValue::String(version));
        }
    }

    if let Some(events_json) = marshal_events(&otel_span.events) {
        attrs.insert(
            MetaString::from_static("events"),
            AttributeValue::String(events_json.into()),
        );
    }
    if span_contains_exception_event(&otel_span.events) {
        attrs.insert(
            MetaString::from_static("_dd.span_events.has_exception"),
            AttributeValue::String(MetaString::from_static("true")),
        );
    }
    if let Some(links_json) = marshal_links(&otel_span.links) {
        attrs.insert(
            MetaString::from_static("_dd.span_links"),
            AttributeValue::String(links_json.into()),
        );
    }

    if !otel_span.trace_state.is_empty() {
        attrs.insert(
            MetaString::from_static(W3C_TRACESTATE_META_KEY),
            AttributeValue::String(otel_span.trace_state.as_str().into()),
        );
    }

    if let Some(scope) = instrumentation_scope {
        if !scope.name.is_empty() {
            // Build the value once and reuse it for both the deprecated `otel.library.*` and current `otel.scope.*`
            // keys; cloning a `MetaString`-backed `AttributeValue` is cheaper than re-converting the source string.
            let name = AttributeValue::String(scope.name.as_str().into());
            if EMIT_OTEL_SCOPE_META {
                attrs.insert(MetaString::from_static(OTEL_SCOPE_NAME_META_KEY), name.clone());
            }
            attrs.insert(MetaString::from_static(OTEL_LIBRARY_NAME_META_KEY), name);
        }
        if !scope.version.is_empty() {
            let version = AttributeValue::String(scope.version.as_str().into());
            if EMIT_OTEL_SCOPE_META {
                attrs.insert(MetaString::from_static(OTEL_SCOPE_VERSION_META_KEY), version.clone());
            }
            attrs.insert(MetaString::from_static(OTEL_LIBRARY_VERSION_META_KEY), version);
        }
    }

    let status = otel_span.status.as_ref();
    let status_code = status
        .and_then(|s| StatusCode::try_from(s.code).ok())
        .unwrap_or(StatusCode::Unset);
    attrs.insert(
        MetaString::from_static(OTEL_STATUS_CODE_META_KEY),
        AttributeValue::String(MetaString::from_static(status_code_to_string(status_code))),
    );
    if let Some(status) = status {
        if !status.message.is_empty() {
            attrs.insert(
                MetaString::from_static(OTEL_STATUS_DESCRIPTION_META_KEY),
                AttributeValue::String(status.message.as_str().into()),
            );
        }
    }

    if !ignore_missing_fields {
        if !attrs.contains_key("error.msg") || !attrs.contains_key("error.type") || !attrs.contains_key("error.stack") {
            let error = status_to_error(status, &otel_span.events, &mut attrs);
            if error != 0 {
                dd_span = dd_span.with_error(error);
            }
        }

        if !attrs.contains_key("env") {
            let env = get_otel_env(
                span_attributes,
                resource_attributes,
                ignore_missing_fields,
                interner,
                string_builder,
            );
            if !env.is_empty() {
                attrs.insert(MetaString::from_static("env"), AttributeValue::String(env));
            }
        }
    }

    for attribute in resource_attributes {
        let Some(value) = attribute.value.as_ref().and_then(|wrapper| wrapper.value.as_ref()) else {
            continue;
        };
        if let Some(serialized) = otlp_value_to_string(value) {
            conditionally_map_otlp_attribute(
                attribute.key.as_str(),
                AttributeValue::String(MetaString::from_interner(&serialized, interner)),
                &mut attrs,
                ignore_missing_fields,
                interner,
                string_builder,
            );
        }
    }

    if let Some(scope) = instrumentation_scope {
        instrumentation_scope_attributes_to_attrs(scope, &mut attrs, interner);
    }

    // Mirror Agent 7.80.1's explicit Meta["rpc.grpc.status_code"] mapping. Integer codes are
    // converted to canonical names (e.g., 14 → "UNAVAILABLE"); string-typed attributes that
    // are already a name (e.g., "UNAVAILABLE") pass through via grpc_status_code_name_from_str.
    // OTel semconv v1.39+ uses rpc.response.status_code instead of rpc.grpc.status_code for
    // gRPC spans; that key is checked last, conditionally when the span is identified as gRPC
    // via rpc.system.name or rpc.system (matching the Agent 7.80.1 fallback behavior).
    let combined = OtelSpanAccessor::new(span_attributes, resource_attributes);
    let grpc_name = lookup_int64(&REGISTRY, &combined, Concept::RpcGrpcStatusCode)
        .and_then(|code| {
            let name = grpc_status_code_name(code as u8);
            if name.is_empty() {
                None
            } else {
                Some(name)
            }
        })
        .or_else(|| {
            lookup_string(&REGISTRY, &combined, Concept::RpcGrpcStatusCode)
                .filter(|s| !s.is_empty())
                .and_then(|s| {
                    // Parse string (either decimal "14" or canonical "UNAVAILABLE") to name.
                    let name = grpc_status_code_name_from_str(&s);
                    if name.is_empty() {
                        None
                    } else {
                        Some(name)
                    }
                })
        })
        .or_else(|| {
            // rpc.system.name is used by lading's OpentelemetryTraces generator (OTel semconv
            // v1.39+); rpc.system is the older OTel key. Mirror the Agent's conditional: only
            // apply rpc.response.status_code when the span is explicitly identified as gRPC.
            let system_name = get_both_string_attribute(span_attributes, resource_attributes, "rpc.system.name");
            let rpc_system = get_both_string_attribute(span_attributes, resource_attributes, "rpc.system");
            let is_grpc = system_name == Some("grpc") || (system_name.is_none() && rpc_system == Some("grpc"));
            if !is_grpc {
                return None;
            }
            get_both_string_attribute(span_attributes, resource_attributes, "rpc.response.status_code")
                .filter(|s| !s.is_empty())
                .and_then(|s| {
                    let name = grpc_status_code_name_from_str(s);
                    if name.is_empty() {
                        None
                    } else {
                        Some(name)
                    }
                })
        });
    if let Some(name) = grpc_name {
        attrs.insert(
            MetaString::from_static(GRPC_STATUS_CODE_META_KEY),
            AttributeValue::String(MetaString::from_static(name)),
        );
    }

    if !attrs.contains_key("db.name") {
        if let Some(db_namespace) = use_both_maps(
            resource_attributes,
            span_attributes,
            false,
            DB_NAMESPACE_KEY,
            interner,
            string_builder,
        ) {
            attrs.insert(MetaString::from_static("db.name"), AttributeValue::String(db_namespace));
        }
    }

    dd_span.attributes = attrs;
    dd_span
}

// OtelSpanToDDSpanMinimal otelSpanToDDSpan converts an OTel span to a DD span.
// The converted DD span only has the minimal number of fields for APM stats calculation and is only meant
// to be used in OTLPTracesToConcentratorInputs. Do not use them for other purposes.
pub fn otel_to_dd_span_minimal(
    otel_span: &OtlpSpan, otel_resource: &Resource, _instrumentation_scope: Option<&OtlpInstrumentationScope>,
    ignore_missing_fields: bool, compute_top_level_by_span_kind: bool, interner: &GenericMapInterner,
    string_builder: &mut StringBuilder<GenericMapInterner>,
) -> (DdSpan, FastHashMap<MetaString, AttributeValue>) {
    let span_attributes = &otel_span.attributes;
    let resource_attributes = &otel_resource.attributes;
    let mut dd_span = DdSpan::default();

    let span_id = convert_span_id(&otel_span.span_id);
    let parent_id = convert_span_id(&otel_span.parent_span_id);
    let start = otel_span.start_time_unix_nano;
    let duration = otel_span.end_time_unix_nano - otel_span.start_time_unix_nano;
    let mut attrs: FastHashMap<MetaString, AttributeValue> = FastHashMap::default();
    attrs.reserve(span_attributes.len() + resource_attributes.len());
    let is_top_level = compute_top_level_by_span_kind
        && (otel_span.parent_span_id.is_empty()
            || otel_span.kind() == SpanKind::Server
            || otel_span.kind() == SpanKind::Consumer);

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
        attrs.insert(MetaString::from_static("_top_level"), AttributeValue::Float(1.0));
    }

    if use_both_maps(
        span_attributes,
        resource_attributes,
        false,
        "_dd.measured",
        interner,
        string_builder,
    )
    .is_some_and(|v| *v == *"1")
        || (compute_top_level_by_span_kind
            && (otel_span.kind() == SpanKind::Client || otel_span.kind() == SpanKind::Producer))
    {
        attrs.insert(MetaString::from_static("_dd.measured"), AttributeValue::Float(1.0));
    }

    let span_kind = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        KEY_DATADOG_SPAN_KIND,
        interner,
        string_builder,
    )
    .unwrap_or_else(|| {
        let kind = SpanKind::try_from(otel_span.kind).unwrap_or(SpanKind::Unspecified);
        MetaString::from_static(span_kind_name(kind))
    });
    attrs.insert(
        MetaString::from_static(SPAN_KIND_META_KEY),
        AttributeValue::String(span_kind),
    );

    let mut service = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        KEY_DATADOG_SERVICE,
        interner,
        string_builder,
    )
    .unwrap_or_default();
    let mut name = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        KEY_DATADOG_NAME,
        interner,
        string_builder,
    )
    .unwrap_or_default();
    let mut resource = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        KEY_DATADOG_RESOURCE,
        interner,
        string_builder,
    )
    .unwrap_or_default();
    let mut span_type = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        KEY_DATADOG_TYPE,
        interner,
        string_builder,
    )
    .unwrap_or_default();

    if !ignore_missing_fields {
        // the functions below are based off the V2 agent functions as they are used by default
        // TODO: allow the user to opt out of V2 via config and also implement the V1 versions of the functions
        if service.is_empty() {
            service = get_otel_service(span_attributes, resource_attributes, true, interner, string_builder);
        }
        if name.is_empty() {
            name = get_otel_operation_name_v2(
                otel_span,
                span_attributes,
                resource_attributes,
                interner,
                string_builder,
            );
        }
        if resource.is_empty() {
            resource = get_otel_resource_v2_truncated(
                otel_span,
                span_attributes,
                resource_attributes,
                interner,
                string_builder,
            );
            // Agent normalizer sets resource = name when resource is empty
            // https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/agent/normalizer.go#L245-248
            if resource.is_empty() {
                resource = name.clone();
            }
        }
        if span_type.is_empty() {
            span_type = get_otel_span_type(
                otel_span,
                span_attributes,
                resource_attributes,
                interner,
                string_builder,
            );
        }
    }

    dd_span = dd_span
        .with_service(service)
        .with_name(name)
        .with_resource(resource)
        .with_span_type(span_type)
        .with_span_id(span_id)
        .with_parent_id(parent_id)
        .with_start(start)
        .with_duration(duration);

    if let Some(status_code) = get_otel_status_code(span_attributes, resource_attributes, ignore_missing_fields) {
        attrs.insert(
            MetaString::from_static(HTTP_STATUS_CODE_KEY),
            AttributeValue::String(MetaString::from(status_code.to_string())),
        );
    }

    // TODO: add peer key tags (unfinished in the agent as well)

    (dd_span, attrs)
}

/// Returns the DD service name based on OTel span and resource attributes.
fn get_otel_service(
    span_attributes: &[KeyValue], resource_attributes: &[KeyValue], normalize: bool, interner: &GenericMapInterner,
    string_builder: &mut StringBuilder<GenericMapInterner>,
) -> MetaString {
    let service = get_string_attribute(span_attributes, SERVICE_NAME)
        .filter(|s| !s.is_empty())
        .or_else(|| get_string_attribute(resource_attributes, SERVICE_NAME).filter(|s| !s.is_empty()))
        .unwrap_or(DEFAULT_SERVICE_NAME);

    if normalize {
        normalize_service_into(service, string_builder);
        interner
            .try_intern(string_builder.as_str())
            .map(MetaString::from)
            .unwrap_or_else(|| MetaString::from(string_builder.as_str()))
    } else {
        MetaString::from_interner(service, interner)
    }
}

// GetOTelOperationNameV2 returns the DD operation name based on OTel span and resource attributes and given configs.
// based on code from https://github.com/DataDog/datadog-agent/blob/instrument-otlp-traffic/pkg/trace/traceutil/otel_util.go#L424
fn get_otel_operation_name_v2(
    otel_span: &OtlpSpan, span_attributes: &[KeyValue], resource_attributes: &[KeyValue],
    interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
) -> MetaString {
    if let Some(value) = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        OPERATION_NAME_KEY,
        interner,
        string_builder,
    ) {
        if !value.is_empty() {
            return value;
        }
    }

    let span_kind = SpanKind::try_from(otel_span.kind).unwrap_or(SpanKind::Unspecified);
    let is_client = matches!(span_kind, SpanKind::Client);
    let is_server = matches!(span_kind, SpanKind::Server);

    // http
    for http_request_method_key in HTTP_REQUEST_METHOD_KEYS {
        if get_both_string_attribute(span_attributes, resource_attributes, http_request_method_key).is_some() {
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
        if let Some(db_system) = use_both_maps(
            span_attributes,
            resource_attributes,
            true,
            DB_SYSTEM_KEY,
            interner,
            string_builder,
        ) {
            string_builder.clear();
            let _ = string_builder.push_str(db_system.as_ref());
            let _ = string_builder.push_str(".query");
            return string_builder.to_meta_string();
        }
    }

    // messaging
    let system = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        MESSAGING_SYSTEM_KEY,
        interner,
        string_builder,
    );
    let operation = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        MESSAGING_OPERATION_KEY,
        interner,
        string_builder,
    );
    if let (Some(system), Some(operation)) = (system, operation) {
        match span_kind {
            SpanKind::Client | SpanKind::Server | SpanKind::Consumer | SpanKind::Producer => {
                string_builder.clear();
                let _ = string_builder.push_str(system.as_ref());
                let _ = string_builder.push('.');
                let _ = string_builder.push_str(operation.as_ref());
                return string_builder.to_meta_string();
            }
            _ => {}
        }
    }

    // RPC & AWS
    if let Some(rpc_system) = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        RPC_SYSTEM_KEY,
        interner,
        string_builder,
    ) {
        let is_aws = rpc_system == "aws-api";
        if is_aws && is_client {
            if let Some(service) = use_both_maps(
                span_attributes,
                resource_attributes,
                true,
                RPC_SERVICE_KEY,
                interner,
                string_builder,
            ) {
                string_builder.clear();
                let _ = string_builder.push_str("aws.");
                let _ = string_builder.push_str(service.as_ref());
                let _ = string_builder.push_str(".request");
                return string_builder.to_meta_string();
            }
            return MetaString::from_static("aws.client.request");
        }

        if is_client {
            string_builder.clear();
            let _ = string_builder.push_str(rpc_system.as_ref());
            let _ = string_builder.push_str(".client.request");
            return string_builder.to_meta_string();
        }
        if is_server {
            string_builder.clear();
            let _ = string_builder.push_str(rpc_system.as_ref());
            let _ = string_builder.push_str(".server.request");
            return string_builder.to_meta_string();
        }
    }

    // FAAS client
    if is_client {
        let provider = use_both_maps(
            span_attributes,
            resource_attributes,
            true,
            FAAS_INVOKED_PROVIDER_KEY,
            interner,
            string_builder,
        );
        let invoked = use_both_maps(
            span_attributes,
            resource_attributes,
            true,
            FAAS_INVOKED_NAME_KEY,
            interner,
            string_builder,
        );
        if let (Some(provider), Some(invoked)) = (provider, invoked) {
            string_builder.clear();
            let _ = string_builder.push_str(provider.as_ref());
            let _ = string_builder.push('.');
            let _ = string_builder.push_str(invoked.as_ref());
            let _ = string_builder.push_str(".invoke");
            return string_builder.to_meta_string();
        }
    }
    // FAAS server
    if is_server {
        if let Some(trigger) = use_both_maps(
            span_attributes,
            resource_attributes,
            true,
            FAAS_TRIGGER_KEY,
            interner,
            string_builder,
        ) {
            string_builder.clear();
            let _ = string_builder.push_str(trigger.as_ref());
            let _ = string_builder.push_str(".invoke");
            return string_builder.to_meta_string();
        }
    }

    if get_both_string_attribute(span_attributes, resource_attributes, GRAPHQL_OPERATION_TYPE_KEY).is_some() {
        return MetaString::from_static("graphql.server.request");
    }

    if is_server {
        if let Some(protocol) = use_both_maps(
            span_attributes,
            resource_attributes,
            true,
            NETWORK_PROTOCOL_NAME_KEY,
            interner,
            string_builder,
        ) {
            string_builder.clear();
            let _ = string_builder.push_str(protocol.as_ref());
            let _ = string_builder.push_str(".server.request");
            return string_builder.to_meta_string();
        }
        return MetaString::from_static("server.request");
    }
    if is_client {
        if let Some(protocol) = use_both_maps(
            span_attributes,
            resource_attributes,
            true,
            NETWORK_PROTOCOL_NAME_KEY,
            interner,
            string_builder,
        ) {
            string_builder.clear();
            let _ = string_builder.push_str(protocol.as_ref());
            let _ = string_builder.push_str(".client.request");
            return string_builder.to_meta_string();
        }
        return MetaString::from_static("client.request");
    }

    let fallback_kind = if span_kind == SpanKind::Unspecified {
        SpanKind::Internal
    } else {
        span_kind
    };
    // Use capitalized span kind name for operation  (for example, "Internal", "Client", "Server")
    MetaString::from_static(span_kind_name_capitalized(fallback_kind))
}

// GetOTelResourceV2 returns the DD resource name based on OTel span and resource attributes.
// based on this code https://github.com/DataDog/datadog-agent/blob/instrument-otlp-traffic/pkg/trace/traceutil/otel_util.go#L348
fn get_otel_resource_v2(
    otel_span: &OtlpSpan, span_attributes: &[KeyValue], resource_attributes: &[KeyValue],
    interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
) -> MetaString {
    let span_kind = SpanKind::try_from(otel_span.kind).unwrap_or(SpanKind::Unspecified);
    // Resource names use raw attribute values without tag normalization,
    // matching the Go Agent's GetOTelResource behavior.
    if let Some(value) = use_both_maps(
        span_attributes,
        resource_attributes,
        false,
        RESOURCE_NAME_KEY,
        interner,
        string_builder,
    ) {
        if !value.is_empty() {
            return value;
        }
    }

    if let Some(method) = use_both_maps_key_list(
        span_attributes,
        resource_attributes,
        false,
        HTTP_REQUEST_METHOD_KEYS,
        interner,
        string_builder,
    ) {
        let route = if span_kind == SpanKind::Server {
            use_both_maps(
                span_attributes,
                resource_attributes,
                false,
                HTTP_ROUTE_KEY,
                interner,
                string_builder,
            )
        } else {
            None
        };

        string_builder.clear();
        if method.as_ref() == "_OTHER" {
            let _ = string_builder.push_str("HTTP");
        } else {
            let _ = string_builder.push_str(method.as_ref());
        }
        if let Some(route) = route {
            let _ = string_builder.push(' ');
            let _ = string_builder.push_str(route.as_ref());
        }
        return string_builder.to_meta_string();
    }

    if let Some(operation) = use_both_maps(
        span_attributes,
        resource_attributes,
        false,
        MESSAGING_OPERATION_KEY,
        interner,
        string_builder,
    ) {
        let dest = use_both_maps_key_list(
            span_attributes,
            resource_attributes,
            false,
            MESSAGING_DESTINATION_KEYS,
            interner,
            string_builder,
        );

        string_builder.clear();
        let _ = string_builder.push_str(operation.as_ref());
        if let Some(dest) = dest {
            if !dest.is_empty() {
                let _ = string_builder.push(' ');
                let _ = string_builder.push_str(dest.as_ref());
            }
        }
        return string_builder.to_meta_string();
    }

    if let Some(method) = use_both_maps(
        span_attributes,
        resource_attributes,
        false,
        RPC_METHOD_KEY,
        interner,
        string_builder,
    ) {
        let service = use_both_maps(
            span_attributes,
            resource_attributes,
            false,
            RPC_SERVICE_KEY,
            interner,
            string_builder,
        );

        string_builder.clear();
        let _ = string_builder.push_str(method.as_ref());
        if let Some(service) = service {
            let _ = string_builder.push(' ');
            let _ = string_builder.push_str(service.as_ref());
        }
        return string_builder.to_meta_string();
    }

    // Enrich GraphQL query resource names.
    // See https://github.com/open-telemetry/semantic-conventions/blob/v1.29.0/docs/graphql/graphql-spans.md
    if let Some(op_type) = get_both_string_attribute(span_attributes, resource_attributes, GRAPHQL_OPERATION_TYPE_KEY) {
        string_builder.clear();
        if is_normalized_tag_value(op_type) {
            let _ = string_builder.push_str(op_type);
        } else {
            normalize_tag_value_append_unchecked(op_type, string_builder);
        }

        if let Some(op_name) =
            get_both_string_attribute(span_attributes, resource_attributes, GRAPHQL_OPERATION_NAME_KEY)
        {
            let _ = string_builder.push(' ');
            if is_normalized_tag_value(op_name) {
                let _ = string_builder.push_str(op_name);
            } else {
                normalize_tag_value_append_unchecked(op_name, string_builder);
            }
        }
        return string_builder.to_meta_string();
    }

    if get_both_string_attribute(span_attributes, resource_attributes, DB_SYSTEM_KEY).is_some() {
        if let Some(statement) = get_both_string_attribute(span_attributes, resource_attributes, DB_STATEMENT_KEY) {
            normalize_tag_value_into_unchecked(statement, string_builder);
            return string_builder.to_meta_string();
        }
        if let Some(query) = get_both_string_attribute(span_attributes, resource_attributes, DB_QUERY_TEXT_KEY) {
            normalize_tag_value_into_unchecked(query, string_builder);
            return string_builder.to_meta_string();
        }
    }

    if !otel_span.name.is_empty() {
        return MetaString::from(otel_span.name.as_str());
    }
    MetaString::empty()
}

fn get_otel_resource_v2_truncated(
    otel_span: &OtlpSpan, span_attributes: &[KeyValue], resource_attributes: &[KeyValue],
    interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
) -> MetaString {
    let res_name = get_otel_resource_v2(
        otel_span,
        span_attributes,
        resource_attributes,
        interner,
        string_builder,
    );
    if res_name.len() > MAX_RESOURCE_LEN {
        MetaString::from(truncate_utf8(&res_name, MAX_RESOURCE_LEN))
    } else {
        res_name
    }
}

fn get_otel_span_type(
    otel_span: &OtlpSpan, span_attributes: &[KeyValue], resource_attributes: &[KeyValue],
    interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
) -> MetaString {
    if let Some(value) = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        "span.type",
        interner,
        string_builder,
    ) {
        if !value.is_empty() {
            return value;
        }
    }

    let span_kind = SpanKind::try_from(otel_span.kind).unwrap_or(SpanKind::Unspecified);
    let span_type = match span_kind {
        SpanKind::Server => "web",
        SpanKind::Client => {
            if let Some(db_system) = use_both_maps(
                span_attributes,
                resource_attributes,
                true,
                DB_SYSTEM_KEY,
                interner,
                string_builder,
            ) {
                map_db_system_to_span_type(db_system.as_ref())
            } else {
                "http"
            }
        }
        _ => "custom",
    };

    MetaString::from_static(span_type)
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
    attribute: &KeyValue, attrs: &mut FastHashMap<MetaString, AttributeValue>, ignore_missing_fields: bool,
    interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
) {
    if attribute.key.is_empty() {
        return;
    }

    let Some(value) = attribute.value.as_ref().and_then(|wrapper| wrapper.value.as_ref()) else {
        return;
    };

    match value {
        OtlpValue::StringValue(s) => {
            conditionally_map_otlp_attribute(
                attribute.key.as_str(),
                AttributeValue::String(MetaString::from_interner(s.as_str(), interner)),
                attrs,
                ignore_missing_fields,
                interner,
                string_builder,
            );
        }
        OtlpValue::BoolValue(b) => {
            conditionally_map_otlp_attribute(
                attribute.key.as_str(),
                AttributeValue::Bool(*b),
                attrs,
                ignore_missing_fields,
                interner,
                string_builder,
            );
        }
        OtlpValue::BytesValue(bytes) => {
            conditionally_map_otlp_attribute(
                attribute.key.as_str(),
                AttributeValue::Bytes(bytes.clone()),
                attrs,
                ignore_missing_fields,
                interner,
                string_builder,
            );
        }
        OtlpValue::IntValue(i) => {
            conditionally_map_otlp_attribute(
                attribute.key.as_str(),
                AttributeValue::Int(*i),
                attrs,
                ignore_missing_fields,
                interner,
                string_builder,
            );
        }
        OtlpValue::DoubleValue(d) => {
            conditionally_map_otlp_attribute(
                attribute.key.as_str(),
                AttributeValue::Float(*d),
                attrs,
                ignore_missing_fields,
                interner,
                string_builder,
            );
        }
        _ => {
            // Skip complex values for now.
        }
    }
}

fn instrumentation_scope_attributes_to_attrs(
    scope: &OtlpInstrumentationScope, attrs: &mut FastHashMap<MetaString, AttributeValue>,
    interner: &GenericMapInterner,
) {
    for attribute in &scope.attributes {
        let Some(value) = attribute.value.as_ref().and_then(|wrapper| wrapper.value.as_ref()) else {
            continue;
        };
        if let Some(serialized) = otlp_value_to_string(value) {
            let key = MetaString::from_interner(attribute.key.as_str(), interner);
            let value = MetaString::from_interner(serialized.as_str(), interner);
            attrs.insert(key, AttributeValue::String(value));
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
        StatusCode::Ok => "Ok",
        StatusCode::Error => "Error",
        StatusCode::Unset => "Unset",
    }
}

// Status2Error checks the given status and events and applies any potential error and messages
// to the given span attributes.
fn status_to_error(
    status: Option<&OtlpStatus>, events: &[OtlpSpanEvent], attrs: &mut FastHashMap<MetaString, AttributeValue>,
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
                        attrs.insert(MetaString::from_static("error.msg"), AttributeValue::String(meta_value));
                    }
                    EXCEPTION_TYPE_KEY => {
                        attrs.insert(
                            MetaString::from_static("error.type"),
                            AttributeValue::String(serialized.into()),
                        );
                    }
                    EXCEPTION_STACKTRACE_KEY => {
                        attrs.insert(
                            MetaString::from_static("error.stack"),
                            AttributeValue::String(serialized.into()),
                        );
                    }
                    _ => {}
                }
            }
        }
    }
    if !attrs.contains_key("error.msg") {
        if !status.message.is_empty() {
            attrs.insert(
                MetaString::from_static("error.msg"),
                AttributeValue::String(status.message.as_str().into()),
            );
        } else if let Some(http_code) =
            get_first_from_attrs(attrs, &[HTTP_RESPONSE_STATUS_CODE_KEY, HTTP_STATUS_CODE_KEY])
        {
            let mut message = http_code.as_ref().to_string();
            if let Some(http_text) = attrs.get("http.status_text").and_then(AttributeValue::as_string) {
                message.push(' ');
                message.push_str(http_text.as_ref());
            } else if let Ok(status_code) = http_code.as_ref().parse::<u16>() {
                if let Ok(status) = http::StatusCode::from_u16(status_code) {
                    if let Some(reason) = status.canonical_reason() {
                        message.push(' ');
                        message.push_str(reason);
                    }
                }
            }
            attrs.insert(
                MetaString::from_static("error.msg"),
                AttributeValue::String(message.into()),
            );
        }
    }
    1
}

fn get_first_from_attrs(attrs: &FastHashMap<MetaString, AttributeValue>, keys: &[&str]) -> Option<MetaString> {
    for key in keys {
        if let Some(value) = attrs.get(*key).and_then(AttributeValue::as_string) {
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

pub(super) fn otlp_value_to_string(value: &OtlpValue) -> Option<String> {
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

// Mirrors SetMetaOTLPIfEmpty / SetMetricOTLPIfEmpty from the Go agent
// (pkg/trace/transform/transform.go). Checks whether `key` maps to a
// Datadog convention key, then inserts `value` into `attrs` if not already
// present, with key-specific special-case handling.
fn conditionally_map_otlp_attribute(
    key: &str, value: AttributeValue, attrs: &mut FastHashMap<MetaString, AttributeValue>, ignore_missing_fields: bool,
    interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
) {
    let Some(mapped_key) = get_dd_key_for_otlp_attribute(key, interner, string_builder) else {
        return;
    };
    if attrs.contains_key(&mapped_key) {
        return;
    }
    if ignore_missing_fields && has_dd_namespaced_equivalent(mapped_key.as_ref()) {
        return;
    }
    match mapped_key.as_ref() {
        "service.name" | "operation.name" | "resource.name" | "span.type" => {
            // handled elsewhere
        }
        ANALYTICS_EVENT_KEY => {
            if !attrs.contains_key(EVENT_EXTRACTION_METRIC_KEY) {
                let parsed = match &value {
                    AttributeValue::Bool(b) => Some(*b),
                    AttributeValue::String(s) => parse_bool(s.as_ref()),
                    _ => None,
                };
                if let Some(parsed) = parsed {
                    attrs
                        .entry(MetaString::from_static(EVENT_EXTRACTION_METRIC_KEY))
                        .or_insert(AttributeValue::Float(if parsed { 1.0 } else { 0.0 }));
                }
            }
        }
        _ => {
            let storage_key = if mapped_key.as_ref() == "sampling.priority" {
                MetaString::from_static(SAMPLING_PRIORITY_METRIC_KEY)
            } else {
                mapped_key
            };
            attrs.entry(storage_key).or_insert(value);
        }
    }
}

// GetDDKeyForOTLPAttribute looks for a key in the Datadog HTTP convention that matches the given key from the
// OTLP HTTP convention. Otherwise, check if it is a Datadog APM convention key - if it is, it will be handled with
// specialized logic elsewhere, so return None. If it isn't, return the original key.
// based on the logic from the agent code https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/transform/transform.go#L179
fn get_dd_key_for_otlp_attribute(
    key: &str, interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
) -> Option<MetaString> {
    if let Some(mapped) = HTTP_MAPPINGS.get(key) {
        return Some(MetaString::from_static(mapped));
    }
    if let Some(header_suffix) = key.strip_prefix(HTTP_REQUEST_HEADER_PREFIX) {
        string_builder.clear();
        let _ = string_builder.push_str(HTTP_REQUEST_HEADERS_PREFIX);
        let _ = string_builder.push_str(header_suffix);
        return Some(string_builder.to_meta_string());
    }
    if !is_datadog_apm_convention_key(key) {
        return Some(MetaString::from_interner(key, interner));
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

fn span_kind_name_capitalized(kind: SpanKind) -> &'static str {
    match kind {
        SpanKind::Client => "Client",
        SpanKind::Server => "Server",
        SpanKind::Consumer => "Consumer",
        SpanKind::Producer => "Producer",
        SpanKind::Internal => "Internal",
        SpanKind::Unspecified => "Unspecified",
    }
}

fn intern_attribute_value(
    value: &str, normalize: bool, interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
) -> MetaString {
    if normalize {
        if is_normalized_tag_value(value) {
            return MetaString::from_interner(value, interner);
        }

        normalize_tag_value_into_unchecked(value, string_builder);
        return interner
            .try_intern(string_builder.as_str())
            .map(MetaString::from)
            .unwrap_or_else(|| MetaString::from(string_builder.as_str()));
    }

    MetaString::from_interner(value, interner)
}

fn use_both_maps(
    map: &[KeyValue], map2: &[KeyValue], normalize: bool, key: &str, interner: &GenericMapInterner,
    string_builder: &mut StringBuilder<GenericMapInterner>,
) -> Option<MetaString> {
    if let Some(value) = get_string_attribute(map, key) {
        if !value.is_empty() {
            return Some(intern_attribute_value(value, normalize, interner, string_builder));
        }
    }
    get_string_attribute(map2, key).and_then(|value| {
        if !value.is_empty() {
            Some(intern_attribute_value(value, normalize, interner, string_builder))
        } else {
            None
        }
    })
}

fn get_both_string_attribute<'a>(map: &'a [KeyValue], map2: &'a [KeyValue], key: &str) -> Option<&'a str> {
    get_string_attribute(map, key)
        .filter(|value| !value.is_empty())
        .or_else(|| get_string_attribute(map2, key).filter(|value| !value.is_empty()))
}

fn use_both_maps_key_list(
    span_attributes: &[KeyValue], resource_attributes: &[KeyValue], normalize: bool, keys: &[&str],
    interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
) -> Option<MetaString> {
    for key in keys {
        if let Some(value) = use_both_maps(
            span_attributes,
            resource_attributes,
            normalize,
            key,
            interner,
            string_builder,
        ) {
            return Some(value);
        }
    }
    None
}

pub(crate) fn get_otel_env(
    span_attributes: &[KeyValue], resource_attributes: &[KeyValue], ignore_missing_fields: bool,
    interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
) -> MetaString {
    if let Some(value) = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        KEY_DATADOG_ENVIRONMENT,
        interner,
        string_builder,
    ) {
        return value;
    }

    if ignore_missing_fields {
        return MetaString::empty();
    }

    if let Some(value) = use_both_maps_key_list(
        span_attributes,
        resource_attributes,
        true,
        &[DEPLOYMENT_ENVIRONMENT_NAME, DEPLOYMENT_ENVIRONMENT_KEY],
        interner,
        string_builder,
    ) {
        return value;
    }

    MetaString::empty()
}

// GetOTelVersion returns the version based on OTel span and resource attributes, with span taking precedence.
pub(crate) fn get_otel_version(
    span_attributes: &[KeyValue], resource_attributes: &[KeyValue], ignore_missing_fields: bool,
    interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
) -> MetaString {
    if let Some(value) = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        KEY_DATADOG_VERSION,
        interner,
        string_builder,
    ) {
        return value;
    }

    if ignore_missing_fields {
        return MetaString::empty();
    }

    if let Some(value) = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        SERVICE_VERSION,
        interner,
        string_builder,
    ) {
        return value;
    }

    MetaString::empty()
}

pub(crate) fn get_otel_container_id(
    span_attributes: &[KeyValue], resource_attributes: &[KeyValue], ignore_missing_fields: bool,
    interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
) -> MetaString {
    if let Some(value) = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        KEY_DATADOG_CONTAINER_ID,
        interner,
        string_builder,
    ) {
        return value;
    }

    if ignore_missing_fields {
        return MetaString::empty();
    }

    if let Some(value) = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        CONTAINER_ID,
        interner,
        string_builder,
    ) {
        return value;
    }

    if let Some(value) = use_both_maps(
        span_attributes,
        resource_attributes,
        true,
        K8S_POD_UID,
        interner,
        string_builder,
    ) {
        return value;
    }

    MetaString::empty()
}
// GetOTelStatusCode returns the HTTP status code based on OTel span and resource attributes, with span taking precedence.
//
// The ADP-specific `datadog.http_status_code` override is checked first; it is
// not part of upstream's semantic registry but is load-bearing for the
// `DD_NAMESPACED_TO_APM_CONVENTIONS` meta-copy path. Everything else flows
// through the semantic registry, mirroring upstream's
// `semantics.LookupInt64(_, _, ConceptHTTPStatusCode)`.
fn get_otel_status_code(
    span_attributes: &[KeyValue], resource_attributes: &[KeyValue], ignore_missing_fields: bool,
) -> Option<i64> {
    let span = OtlpAttributesAccessor::new(span_attributes);
    let resource = OtlpAttributesAccessor::new(resource_attributes);

    if let Some(value) = parse_int_like(&span, KEY_DATADOG_HTTP_STATUS_CODE)
        .or_else(|| parse_int_like(&resource, KEY_DATADOG_HTTP_STATUS_CODE))
    {
        return Some(value);
    }

    if ignore_missing_fields {
        return None;
    }

    let combined = OtelSpanAccessor::new(span_attributes, resource_attributes);
    lookup_int64(&REGISTRY, &combined, Concept::HttpStatusCode)
}

// Returns an i64 from either an IntValue attribute or a StringValue attribute
// parsed via `i64::from_str`. Used for ADP-local `datadog.*`-namespaced
// overrides that are not represented in the upstream semantic registry.
fn parse_int_like<A: Accessor>(accessor: &A, key: &str) -> Option<i64> {
    if let Some(v) = accessor.get_int64(key) {
        return Some(v);
    }
    accessor.get_string(key).and_then(|s| s.parse::<i64>().ok())
}

/// Maps a `tonic::Code` to its canonical OTel uppercase name.
///
/// Mirrors the names used by the Datadog Agent's `Meta["rpc.grpc.status_code"]` field.
///
/// Status code values: https://github.com/open-telemetry/opentelemetry-go/blob/dcfc16cebd0674eaaa7fe5316be8abe8a0f6d35e/semconv/v1.19.0/trace.go#L2024-L2059
fn grpc_code_to_canonical_name(code: tonic::Code) -> &'static str {
    match code {
        tonic::Code::Ok => "OK",
        tonic::Code::Cancelled => "CANCELLED",
        tonic::Code::Unknown => "UNKNOWN",
        tonic::Code::InvalidArgument => "INVALID_ARGUMENT",
        tonic::Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
        tonic::Code::NotFound => "NOT_FOUND",
        tonic::Code::AlreadyExists => "ALREADY_EXISTS",
        tonic::Code::PermissionDenied => "PERMISSION_DENIED",
        tonic::Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
        tonic::Code::FailedPrecondition => "FAILED_PRECONDITION",
        tonic::Code::Aborted => "ABORTED",
        tonic::Code::OutOfRange => "OUT_OF_RANGE",
        tonic::Code::Unimplemented => "UNIMPLEMENTED",
        tonic::Code::Internal => "INTERNAL",
        tonic::Code::Unavailable => "UNAVAILABLE",
        tonic::Code::DataLoss => "DATA_LOSS",
        tonic::Code::Unauthenticated => "UNAUTHENTICATED",
    }
}

/// Maps a gRPC status code integer to its canonical uppercase name, or `""` for out-of-range codes.
fn grpc_status_code_name(code: u8) -> &'static str {
    // tonic::Code::from_i32 maps anything outside 0–16 to Code::Unknown (which is code 2), so
    // guard the range first to distinguish "code 2 = UNKNOWN" from "code 99 = invalid".
    if code > 16 {
        return "";
    }
    grpc_code_to_canonical_name(tonic::Code::from_i32(code as i32))
}

/// Parses a gRPC status code string (either decimal `"14"` or canonical `"UNAVAILABLE"`) to its
/// canonical uppercase name, returning `""` for unrecognized inputs.
fn grpc_status_code_name_from_str(s: &str) -> &'static str {
    if let Ok(code) = s.parse::<u8>() {
        return grpc_status_code_name(code);
    }
    let upper = s.to_uppercase();
    let normalized = upper.strip_prefix("STATUSCODE.").unwrap_or(upper.as_str());
    let code = match normalized {
        "OK" => tonic::Code::Ok,
        "CANCELLED" | "CANCELED" => tonic::Code::Cancelled,
        "UNKNOWN" => tonic::Code::Unknown,
        "INVALID_ARGUMENT" | "INVALIDARGUMENT" => tonic::Code::InvalidArgument,
        "DEADLINE_EXCEEDED" | "DEADLINEEXCEEDED" => tonic::Code::DeadlineExceeded,
        "NOT_FOUND" | "NOTFOUND" => tonic::Code::NotFound,
        "ALREADY_EXISTS" | "ALREADYEXISTS" => tonic::Code::AlreadyExists,
        "PERMISSION_DENIED" | "PERMISSIONDENIED" => tonic::Code::PermissionDenied,
        "RESOURCE_EXHAUSTED" | "RESOURCEEXHAUSTED" => tonic::Code::ResourceExhausted,
        "FAILED_PRECONDITION" | "FAILEDPRECONDITION" => tonic::Code::FailedPrecondition,
        "ABORTED" => tonic::Code::Aborted,
        "OUT_OF_RANGE" | "OUTOFRANGE" => tonic::Code::OutOfRange,
        "UNIMPLEMENTED" => tonic::Code::Unimplemented,
        "INTERNAL" => tonic::Code::Internal,
        "UNAVAILABLE" => tonic::Code::Unavailable,
        "DATA_LOSS" | "DATALOSS" => tonic::Code::DataLoss,
        "UNAUTHENTICATED" => tonic::Code::Unauthenticated,
        _ => return "",
    };
    grpc_code_to_canonical_name(code)
}

pub(super) fn bytes_to_hex_lowercase(bytes: &[u8]) -> String {
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
    use std::num::NonZeroUsize;

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

    fn test_interner() -> GenericMapInterner {
        GenericMapInterner::new(NonZeroUsize::new(64 * 1024).unwrap())
    }

    // Semantic convention keys (matching Go's semconv117 and semconv127)
    const SEMCONV_DEPLOYMENT_ENVIRONMENT_NAME: &str = "deployment.environment.name"; // semconv127
    const SEMCONV_DEPLOYMENT_ENVIRONMENT: &str = "deployment.environment"; // semconv117
    const SEMCONV_SERVICE_VERSION: &str = "service.version";
    const SEMCONV_CONTAINER_ID: &str = "container.id";
    const SEMCONV_HTTP_STATUS_CODE: &str = "http.status_code";
    const SEMCONV_HTTP_RESPONSE_STATUS_CODE: &str = "http.response.status_code";
    const SEMCONV_DB_NAMESPACE: &str = "db.namespace";

    /// A single attribute-precedence case for the `get_otel_*` string extractors.
    ///
    /// `get_otel_env`, `get_otel_version`, and `get_otel_container_id` share an identical signature and precedence
    /// contract (span attributes win over resource attributes, Datadog-namespaced keys are honored, and
    /// `ignore_missing_datadog_fields` suppresses the semantic-convention fallback), so their tests share this case
    /// shape and the `run_attribute_extraction_cases` runner rather than each redefining an identical harness.
    struct AttributeExtractionCase {
        name: &'static str,
        span_attrs: Vec<KeyValue>,
        resource_attrs: Vec<KeyValue>,
        expected: &'static str,
        ignore_missing_datadog_fields: bool,
    }

    /// Drives `extractor` over every case with a shared interner/string builder and asserts the extracted string
    /// equals the case's `expected` value, reporting the failing case name.
    #[allow(clippy::type_complexity)]
    fn run_attribute_extraction_cases(
        extractor: fn(
            &[KeyValue],
            &[KeyValue],
            bool,
            &GenericMapInterner,
            &mut StringBuilder<GenericMapInterner>,
        ) -> MetaString,
        cases: Vec<AttributeExtractionCase>,
    ) {
        let interner = test_interner();
        let mut string_builder = StringBuilder::new().with_interner(interner.clone());
        for tc in cases {
            let result = extractor(
                &tc.span_attrs,
                &tc.resource_attrs,
                tc.ignore_missing_datadog_fields,
                &interner,
                &mut string_builder,
            );
            assert_eq!(result.as_ref(), tc.expected, "test case: {}", tc.name);
        }
    }

    /// TestGetOTelEnv - Tests GetOTelEnv function
    /// Note: The Rust implementation doesn't have a direct GetOTelEnv function,
    /// but the logic is embedded in `otel_span_to_dd_span`. This test verifies
    /// the expected behavior based on Go's test cases.
    #[test]
    fn test_get_otel_env() {
        let test_cases = vec![
            AttributeExtractionCase {
                name: "neither set",
                span_attrs: vec![],
                resource_attrs: vec![],
                expected: "",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "only in resource (semconv127)",
                span_attrs: vec![],
                resource_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT_NAME, "env-res-127")],
                expected: "env-res-127",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "only in resource (semconv117)",
                span_attrs: vec![],
                resource_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "env-res")],
                expected: "env-res",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "only in span (semconv127)",
                span_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT_NAME, "env-span-127")],
                resource_attrs: vec![],
                expected: "env-span-127",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "only in span (semconv117)",
                span_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "env-span")],
                resource_attrs: vec![],
                expected: "env-span",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "both set (span wins)",
                span_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "env-span")],
                resource_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "env-res")],
                expected: "env-span",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "normalization",
                span_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "  ENV ")],
                resource_attrs: vec![],
                expected: "_env",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "ignore missing datadog fields",
                span_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "env-span")],
                resource_attrs: vec![kv_str(SEMCONV_DEPLOYMENT_ENVIRONMENT, "env-span")],
                expected: "",
                ignore_missing_datadog_fields: true,
            },
            AttributeExtractionCase {
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

        run_attribute_extraction_cases(get_otel_env, test_cases);
    }

    #[test]
    fn test_map_attribute_generic_matches_agent_rules() {
        use saluki_core::data_model::event::trace::AttributeValue;

        let mut attrs: FastHashMap<MetaString, AttributeValue> = FastHashMap::default();
        let interner = test_interner();
        let mut string_builder = StringBuilder::new().with_interner(interner.clone());

        let http_attr = kv_str("http.request.method", "GET");
        map_attribute_generic(&http_attr, &mut attrs, false, &interner, &mut string_builder);
        assert_eq!(
            attrs
                .get("http.method")
                .and_then(AttributeValue::as_string)
                .map(|v| v.as_ref()),
            Some("GET")
        );

        let sampling_attr = kv_int("sampling.priority", 2);
        map_attribute_generic(&sampling_attr, &mut attrs, false, &interner, &mut string_builder);
        assert_eq!(
            attrs.get(SAMPLING_PRIORITY_METRIC_KEY).and_then(AttributeValue::as_num),
            Some(2.0)
        );

        let analytics_attr = kv_bool(ANALYTICS_EVENT_KEY, true);
        map_attribute_generic(&analytics_attr, &mut attrs, false, &interner, &mut string_builder);
        assert_eq!(
            attrs
                .get(EVENT_EXTRACTION_METRIC_KEY)
                .and_then(AttributeValue::as_float),
            Some(1.0)
        );

        let dd_attr = kv_str("datadog.service", "svc");
        map_attribute_generic(&dd_attr, &mut attrs, false, &interner, &mut string_builder);
        assert!(!attrs.contains_key("datadog.service"));

        let mut attrs_ignore: FastHashMap<MetaString, AttributeValue> = FastHashMap::default();
        let env_attr = kv_str("env", "prod");
        map_attribute_generic(&env_attr, &mut attrs_ignore, true, &interner, &mut string_builder);
        assert!(attrs_ignore.is_empty());
    }

    /// TestGetOTelVersion - Tests GetOTelVersion function
    #[test]
    fn test_get_otel_version() {
        let test_cases = vec![
            AttributeExtractionCase {
                name: "neither set",
                span_attrs: vec![],
                resource_attrs: vec![],
                expected: "",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "only in resource",
                span_attrs: vec![],
                resource_attrs: vec![kv_str(SEMCONV_SERVICE_VERSION, "v1")],
                expected: "v1",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "only in span",
                span_attrs: vec![kv_str(SEMCONV_SERVICE_VERSION, "v3")],
                resource_attrs: vec![],
                expected: "v3",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "both set (span wins)",
                span_attrs: vec![kv_str(SEMCONV_SERVICE_VERSION, "v3")],
                resource_attrs: vec![kv_str(SEMCONV_SERVICE_VERSION, "v4")],
                expected: "v3",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "normalization",
                span_attrs: vec![kv_str(SEMCONV_SERVICE_VERSION, "  V1 ")],
                resource_attrs: vec![],
                expected: "_v1",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "ignore missing datadog fields",
                span_attrs: vec![kv_str(SEMCONV_SERVICE_VERSION, "v3")],
                resource_attrs: vec![kv_str(SEMCONV_SERVICE_VERSION, "v4")],
                expected: "",
                ignore_missing_datadog_fields: true,
            },
            AttributeExtractionCase {
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

        run_attribute_extraction_cases(get_otel_version, test_cases);
    }

    /// TestGetOTelContainerID - Tests GetOTelContainerID function
    #[test]
    fn test_get_otel_container_id() {
        let test_cases = vec![
            AttributeExtractionCase {
                name: "neither set",
                span_attrs: vec![],
                resource_attrs: vec![],
                expected: "",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "only in resource",
                span_attrs: vec![],
                resource_attrs: vec![kv_str(SEMCONV_CONTAINER_ID, "cid-res")],
                expected: "cid-res",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "only in span",
                span_attrs: vec![kv_str(SEMCONV_CONTAINER_ID, "cid-span")],
                resource_attrs: vec![],
                expected: "cid-span",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "both set (span wins)",
                span_attrs: vec![kv_str(SEMCONV_CONTAINER_ID, "cid-span")],
                resource_attrs: vec![kv_str(SEMCONV_CONTAINER_ID, "cid-res")],
                expected: "cid-span",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "normalization",
                span_attrs: vec![kv_str(SEMCONV_CONTAINER_ID, "  CID ")],
                resource_attrs: vec![],
                expected: "_cid",
                ignore_missing_datadog_fields: false,
            },
            AttributeExtractionCase {
                name: "ignore missing datadog fields",
                span_attrs: vec![kv_str(SEMCONV_CONTAINER_ID, "cid-span")],
                resource_attrs: vec![kv_str(SEMCONV_CONTAINER_ID, "cid-span")],
                expected: "",
                ignore_missing_datadog_fields: true,
            },
            AttributeExtractionCase {
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

        run_attribute_extraction_cases(get_otel_container_id, test_cases);
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
            TestCase {
                name: "string http.response.status_code on span (lading repro)",
                span_attrs: vec![kv_str(SEMCONV_HTTP_RESPONSE_STATUS_CODE, "500")],
                resource_attrs: vec![],
                expected: Some(500),
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "string http.status_code on span",
                span_attrs: vec![kv_str(SEMCONV_HTTP_STATUS_CODE, "200")],
                resource_attrs: vec![],
                expected: Some(200),
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "string http.response.status_code on resource only",
                span_attrs: vec![],
                resource_attrs: vec![kv_str(SEMCONV_HTTP_RESPONSE_STATUS_CODE, "418")],
                expected: Some(418),
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "string datadog.http_status_code override",
                span_attrs: vec![kv_str(KEY_DATADOG_HTTP_STATUS_CODE, "206")],
                resource_attrs: vec![],
                expected: Some(206),
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "string datadog.http_status_code honored with ignore_missing_datadog_fields",
                span_attrs: vec![
                    kv_str(KEY_DATADOG_HTTP_STATUS_CODE, "403"),
                    kv_int(SEMCONV_HTTP_STATUS_CODE, 200),
                ],
                resource_attrs: vec![],
                expected: Some(403),
                ignore_missing_datadog_fields: true,
            },
            TestCase {
                name: "malformed string returns none",
                span_attrs: vec![kv_str(SEMCONV_HTTP_STATUS_CODE, "not-a-number")],
                resource_attrs: vec![],
                expected: None,
                ignore_missing_datadog_fields: false,
            },
            TestCase {
                name: "int attribute wins over string attribute at same name",
                // Per the mappings.json precedence list, int-typed tags for
                // http.status_code are tried before the string-typed tag.
                span_attrs: vec![
                    kv_int(SEMCONV_HTTP_STATUS_CODE, 201),
                    kv_str(SEMCONV_HTTP_RESPONSE_STATUS_CODE, "500"),
                ],
                resource_attrs: vec![],
                expected: Some(201),
                ignore_missing_datadog_fields: false,
            },
        ];

        for tc in test_cases {
            let result = get_otel_status_code(&tc.span_attrs, &tc.resource_attrs, tc.ignore_missing_datadog_fields);
            assert_eq!(result, tc.expected, "test case: {}", tc.name);
        }
    }

    /// Verifies that `rpc.grpc.status_code` is promoted to a string in Meta,
    /// matching Agent 7.80.1's explicit `Meta["rpc.grpc.status_code"]` mapping.
    #[test]
    fn test_otel_span_to_dd_span_grpc_status_code_meta() {
        let interner = test_interner();
        let mut string_builder = StringBuilder::new().with_interner(interner.clone());

        let span = OtlpSpan {
            name: "grpc-span".to_string(),
            attributes: vec![kv_int("rpc.grpc.status_code", 14)],
            ..Default::default()
        };
        let resource = Resource::default();

        let dd_span = otel_span_to_dd_span(
            &span,
            &resource,
            None,
            false,
            true,
            &interner,
            &mut string_builder,
            None,
        );

        use saluki_core::data_model::event::trace::AttributeValue;
        assert_eq!(
            dd_span
                .attributes
                .get("rpc.grpc.status_code")
                .and_then(AttributeValue::as_string)
                .map(|s| s.as_ref()),
            Some("UNAVAILABLE"),
            "rpc.grpc.status_code must be the canonical name, not the decimal code"
        );
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

        let interner = test_interner();
        let mut string_builder = StringBuilder::new().with_interner(interner.clone());
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

            let dd_span = otel_span_to_dd_span(
                &span,
                &resource,
                None,
                false,
                true,
                &interner,
                &mut string_builder,
                None,
            );
            use saluki_core::data_model::event::trace::AttributeValue;
            if tc.should_map {
                assert_eq!(
                    dd_span
                        .attributes
                        .get("db.name")
                        .and_then(AttributeValue::as_string)
                        .map(|s| s.as_ref()),
                    Some(tc.expected_name),
                    "test case: {}",
                    tc.name
                );
            } else if !tc.expected_name.is_empty() {
                assert_eq!(
                    dd_span
                        .attributes
                        .get("db.name")
                        .and_then(AttributeValue::as_string)
                        .map(|s| s.as_ref()),
                    Some(tc.expected_name),
                    "test case: {}",
                    tc.name
                );
            } else {
                assert!(
                    dd_span
                        .attributes
                        .get("db.name")
                        .and_then(AttributeValue::as_string)
                        .map(|s| s.as_ref())
                        .unwrap_or("")
                        .is_empty(),
                    "test case: {}",
                    tc.name
                );
            }
        }
    }

    #[test]
    fn test_otel_span_to_dd_span_scope_name_version_meta() {
        let interner = test_interner();
        let mut string_builder = StringBuilder::new().with_interner(interner.clone());

        let span = OtlpSpan {
            name: "scoped-span".to_string(),
            ..Default::default()
        };
        let resource = Resource::default();
        let scope = OtlpInstrumentationScope {
            name: "com.example.products".to_string(),
            version: "1.0.0".to_string(),
            ..Default::default()
        };

        let dd_span = otel_span_to_dd_span(
            &span,
            &resource,
            Some(&scope),
            false,
            true,
            &interner,
            &mut string_builder,
            None,
        );

        use saluki_core::data_model::event::trace::AttributeValue;
        let meta = |key: &str| {
            dd_span
                .attributes
                .get(key)
                .and_then(AttributeValue::as_string)
                .map(|s| s.as_ref().to_string())
        };

        // The deprecated `otel.library.*` keys are always emitted.
        assert_eq!(meta("otel.library.name").as_deref(), Some("com.example.products"));
        assert_eq!(meta("otel.library.version").as_deref(), Some("1.0.0"));

        // The `otel.scope.*` keys are gated on the Agent version this crate was built against (7.82.0+). Since the gate
        // is resolved at build time, key the expectation off the same constant rather than assuming a build config.
        if EMIT_OTEL_SCOPE_META {
            assert_eq!(meta("otel.scope.name").as_deref(), Some("com.example.products"));
            assert_eq!(meta("otel.scope.version").as_deref(), Some("1.0.0"));
        } else {
            assert_eq!(meta("otel.scope.name"), None);
            assert_eq!(meta("otel.scope.version"), None);
        }
    }
}
