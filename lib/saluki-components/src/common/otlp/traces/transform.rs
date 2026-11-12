#![allow(dead_code)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fmt::Write as _;
use std::sync::Arc;

use base64::{engine::general_purpose, Engine as _};
use datadog_agent_apm::semantics::{lookup_int64, lookup_string, Accessor, Concept, Registry};
use opentelemetry_semantic_conventions::resource::{
    CONTAINER_ID, DEPLOYMENT_ENVIRONMENT_NAME, K8S_POD_UID, SERVICE_NAME, SERVICE_VERSION,
};
use otlp_protos::opentelemetry::proto::common::v1::{
    any_value::Value as OtlpValue, AnyValue, InstrumentationScope as OtlpInstrumentationScope, KeyValue,
};
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use otlp_protos::opentelemetry::proto::trace::v1::{
    span::Event as OtlpSpanEvent, span::Link as OtlpSpanLink, span::SpanKind, status::StatusCode, Span as OtlpSpan,
    Status as OtlpStatus,
};
use saluki_common::collections::FastHashMap;
use saluki_common::strings::StringBuilder;
use saluki_core::data_model::event::trace::{AttributeValue, Span as DdSpan, SpanEvent, SpanLink};
use serde_json::{Map as JsonMap, Value as JsonValue};
use stringtheory::interning::{GenericMapInterner, Interner};
use stringtheory::MetaString;
use tracing::error;

use crate::common::datadog::{OTEL_TRACE_ID_META_KEY, SAMPLING_PRIORITY_METRIC_KEY};
use crate::common::otlp::attributes::{get_int_attribute, HTTP_MAPPINGS};
use crate::common::otlp::semantics::{DdSpanAccessor, OtelSpanAccessor, OtlpAttributesAccessor};
use crate::common::otlp::traces::normalize::{
    is_normalized_tag_value, is_structured_meta_key, needs_name_normalization, normalize_name,
    normalize_peer_service_into, normalize_service_into, normalize_tag_into, normalize_tag_value_append_unchecked,
    normalize_tag_value_into_unchecked, truncate_utf8, truncate_with_ellipses, validate_and_fix_duration,
    validate_and_fix_start_time, PeerServiceChange, MAX_META_KEY_LEN, MAX_META_VAL_LEN, MAX_TYPE_LEN,
};
use crate::common::otlp::traces::sampling::sample_rate_from_tracestate;
use crate::common::otlp::traces::translator::{convert_span_id, convert_trace_id, convert_trace_id_high};
use crate::common::otlp::util::get_string_attribute;
use crate::common::otlp::util::{
    DEPLOYMENT_ENVIRONMENT_KEY, KEY_DATADOG_CONTAINER_ID, KEY_DATADOG_ENVIRONMENT, KEY_DATADOG_VERSION,
};
use crate::common::otlp::Metrics;

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
const LINK_NAME_KEY: &str = "link.name";
const OPERATION_NAME_KEY: &str = "operation.name";
const RESOURCE_NAME_KEY: &str = "resource.name";
const HTTP_REQUEST_METHOD_KEY: &str = "http.request.method";
const HTTP_METHOD_KEY: &str = "http.method";
const HTTP_REQUEST_METHOD_KEYS: &[&str] = &[HTTP_REQUEST_METHOD_KEY, HTTP_METHOD_KEY];
const HTTP_ROUTE_KEY: &str = "http.route";
const GRPC_PATH_KEY: &str = "grpc.path";
const SPAN_TYPE_KEY: &str = "span.type";
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
const SAMPLE_RATE_KEY: &str = "_sample_rate";
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

// From 7.87 the Agent sends the links and events of an OTLP span as span links and span events. Before that it wrote
// them as the `_dd.span_links` and `events` JSON tags, the legacy form. Regular CI builds ADP against the last Agent
// release and nightly CI against the nightly Agent, so ADP follows whichever it is paired with.
// TODO(#3069): remove this gate, the legacy tag path, and `marshal_links`/`marshal_events` once regular CI's
// `PUBLIC_DD_AGENT_VERSION` reaches 7.87.
const LEGACY_TAG_LINKS_AND_EVENTS: bool = !datadog_agent_commons::agent_version::meets(7, 87, 0);

/// How OTLP spans are converted, fixed for the life of a translator.
#[derive(Clone, Debug)]
pub struct SpanConversion {
    /// Whether only `datadog.*` attributes are trusted, without falling back to OTel semantic conventions.
    pub ignore_missing_fields: bool,

    /// Whether spans are marked top-level and measured by their span kind.
    pub compute_top_level_by_span_kind: bool,

    /// Maximum length of a span's resource name, in bytes.
    pub max_resource_len: usize,

    /// How operation and resource names are derived.
    pub naming: SpanNaming,

    /// Whether the `otel.scope.*` tags are emitted next to the deprecated `otel.library.*` tags.
    ///
    /// Only the V2 receive path emits them.
    pub emit_otel_scope: bool,
}

/// How operation and resource names are derived.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum SpanNaming {
    /// Names derived from semantic conventions, such as `http.server.request`.
    #[default]
    V2,

    /// Names derived from the instrumentation library and span kind, such as `io.opentelemetry.tomcat.server`, or
    /// from the span name.
    V1 {
        /// Whether the span name, rather than the instrumentation library name and span kind, becomes the operation
        /// name of a span without `operation.name`.
        span_name_as_resource_name: bool,

        /// Preferred operation names, keyed by the operation name V1 derives.
        span_name_remappings: FastHashMap<String, MetaString>,
    },
}

impl SpanNaming {
    /// Selects V1 naming when either span naming setting is set, or V2 naming is disabled.
    pub fn select(
        span_name_as_resource_name: bool, span_name_remappings: &HashMap<String, String>,
        disable_operation_and_resource_name_logic_v2: bool,
    ) -> Self {
        if !span_name_as_resource_name
            && span_name_remappings.is_empty()
            && !disable_operation_and_resource_name_logic_v2
        {
            return Self::V2;
        }
        Self::V1 {
            span_name_as_resource_name,
            span_name_remappings: span_name_remappings
                .iter()
                .map(|(from, to)| (from.clone(), MetaString::from(Arc::<str>::from(to.as_str()))))
                .collect(),
        }
    }
}

// Bit 31 of a span link's flags marks the flags as present, telling "not sampled" apart from "unknown".
const SPAN_LINK_FLAGS_PRESENT: u32 = 1 << 31;
// Only the low 8 bits of an OTLP link's flags are W3C trace flags; bits 8 and 9 are OTLP's `is_remote`.
const SPAN_LINK_TRACE_FLAGS_MASK: u32 = 0xff;

/// Resolves `peer.service` and `_dd.base_service` on a converted span and writes each normalized
/// value back under its canonical key.
///
/// Resolution goes through the semantic registry, so registered equivalent attributes are
/// honored; a value that normalizes to empty is written as empty, with no substitute.
pub fn normalize_peer_service_tags(
    span: &mut DdSpan, registry: &Registry, interner: &GenericMapInterner,
    string_builder: &mut StringBuilder<GenericMapInterner>, metrics: &Metrics,
) {
    normalize_peer_service_tag(span, registry, Concept::PeerService, interner, string_builder, metrics);
    normalize_peer_service_tag(
        span,
        registry,
        Concept::DdBaseService,
        interner,
        string_builder,
        metrics,
    );
}

fn normalize_peer_service_tag(
    span: &mut DdSpan, registry: &Registry, concept: Concept, interner: &GenericMapInterner,
    string_builder: &mut StringBuilder<GenericMapInterner>, metrics: &Metrics,
) {
    let Some(value) = lookup_string(registry, &DdSpanAccessor::new(span), concept) else {
        return;
    };
    if value.is_empty() {
        return;
    }

    let change = normalize_peer_service_into(&value, string_builder);
    let (truncate, invalid) = match concept {
        Concept::PeerService => (
            metrics.spans_malformed_peer_service_truncate(),
            metrics.spans_malformed_peer_service_invalid(),
        ),
        Concept::DdBaseService => (
            metrics.spans_malformed_base_service_truncate(),
            metrics.spans_malformed_base_service_invalid(),
        ),
        _ => return,
    };
    match change {
        PeerServiceChange::Truncated => truncate.increment(1),
        PeerServiceChange::Invalid => invalid.increment(1),
        PeerServiceChange::Unchanged => {}
    }

    let normalized = interner
        .try_intern(string_builder.as_str())
        .map(MetaString::from)
        .unwrap_or_else(|| MetaString::from(string_builder.as_str()));
    span.attributes.insert(
        MetaString::from_static(concept.as_str()),
        AttributeValue::String(normalized),
    );
}

// otel_span_to_dd_span converts an OTLP span to DD span and is based on the logic defined in the agent.
// https://github.com/DataDog/datadog-agent/blob/instrument-otlp-traffic/pkg/trace/transform/transform.go#L357
#[allow(clippy::too_many_arguments)]
pub fn otel_span_to_dd_span(
    otel_span: &OtlpSpan, otel_resource: &Resource, instrumentation_scope: Option<&OtlpInstrumentationScope>,
    conversion: &SpanConversion, registry: &Registry, interner: &GenericMapInterner,
    string_builder: &mut StringBuilder<GenericMapInterner>, trace_id_hex: Option<&MetaString>,
) -> DdSpan {
    let span_attributes = &otel_span.attributes;
    let resource_attributes = &otel_resource.attributes;
    let ignore_missing_fields = conversion.ignore_missing_fields;
    let (mut dd_span, mut attrs) = otel_to_dd_span_minimal(
        otel_span,
        otel_resource,
        instrumentation_scope,
        conversion,
        registry,
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
            attrs
                .entry(MetaString::from_static(apm_key))
                .or_insert(AttributeValue::String(value));
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

    set_links_and_events(
        &mut dd_span,
        &mut attrs,
        otel_span,
        LEGACY_TAG_LINKS_AND_EVENTS,
        interner,
    );
    if span_contains_exception_event(&otel_span.events) {
        attrs.insert(
            MetaString::from_static("_dd.span_events.has_exception"),
            AttributeValue::String(MetaString::from_static("true")),
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
            if conversion.emit_otel_scope {
                attrs.insert(MetaString::from_static(OTEL_SCOPE_NAME_META_KEY), name.clone());
            }
            attrs.insert(MetaString::from_static(OTEL_LIBRARY_NAME_META_KEY), name);
        }
        if !scope.version.is_empty() {
            let version = AttributeValue::String(scope.version.as_str().into());
            if conversion.emit_otel_scope {
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
    let grpc_name = lookup_int64(registry, &combined, Concept::RpcGrpcStatusCode)
        .and_then(|code| {
            let name = grpc_status_code_name(code as u8);
            if name.is_empty() {
                None
            } else {
                Some(name)
            }
        })
        .or_else(|| {
            lookup_string(registry, &combined, Concept::RpcGrpcStatusCode)
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

    truncate_attributes(&mut attrs);

    dd_span.attributes = attrs;
    dd_span
}

/// Converts an OTLP span on the V1 receive path.
///
/// The V1 receive path copies attributes more literally than [`otel_span_to_dd_span`]:
///
/// - Resource attributes are copied first, and span attributes override them. Numeric span attributes become
///   metrics; everything else becomes a string tag. Only the HTTP method and status code are mapped to Datadog
///   conventions, and `datadog.*` attributes stay ordinary tags.
/// - `operation.name`, `service.name`, `resource.name`, and `span.type` set their fields from either map, and missing
///   fields fall back to V1 defaults. V2 naming still overrides the operation name.
/// - Only the deprecated `otel.library.*` scope tags are emitted, and instrumentation scope attributes are dropped.
///
/// `ignore_missing_fields` doesn't apply. The span is normalized as on the V2 path.
pub fn otel_span_to_dd_span_v1(
    otel_span: &OtlpSpan, otel_resource: &Resource, instrumentation_scope: Option<&OtlpInstrumentationScope>,
    conversion: &SpanConversion, interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
    trace_id_hex: Option<&MetaString>,
) -> DdSpan {
    let mut span = SpanV1::new(
        otel_span,
        &otel_resource.attributes,
        instrumentation_scope,
        conversion,
        interner,
        string_builder,
    );
    span.copy_resource_attributes();
    span.add_span_tags(trace_id_hex);
    span.copy_span_attributes();
    span.add_derived_tags();
    span.fill_missing_fields();
    span.finish()
}

/// A span being converted on the V1 receive path.
///
/// Each step writes over what the steps before it wrote, so their order sets which values take precedence.
struct SpanV1<'a> {
    otel_span: &'a OtlpSpan,
    resource_attributes: &'a [KeyValue],
    instrumentation_scope: Option<&'a OtlpInstrumentationScope>,
    conversion: &'a SpanConversion,
    interner: &'a GenericMapInterner,
    string_builder: &'a mut StringBuilder<GenericMapInterner>,
    kind: SpanKind,
    fields: SpanFieldsV1,
    attrs: FastHashMap<MetaString, AttributeValue>,
    dd_span: DdSpan,
    error: i32,
}

/// The span fields that `operation.name`, `service.name`, `resource.name`, and `span.type` set instead of a tag.
#[derive(Default)]
struct SpanFieldsV1 {
    service: MetaString,
    name: MetaString,
    resource: MetaString,
    span_type: MetaString,
}

impl<'a> SpanV1<'a> {
    fn new(
        otel_span: &'a OtlpSpan, resource_attributes: &'a [KeyValue],
        instrumentation_scope: Option<&'a OtlpInstrumentationScope>, conversion: &'a SpanConversion,
        interner: &'a GenericMapInterner, string_builder: &'a mut StringBuilder<GenericMapInterner>,
    ) -> Self {
        let mut attrs = FastHashMap::default();
        attrs.reserve(otel_span.attributes.len() + resource_attributes.len());
        Self {
            otel_span,
            resource_attributes,
            instrumentation_scope,
            conversion,
            interner,
            string_builder,
            kind: SpanKind::try_from(otel_span.kind).unwrap_or(SpanKind::Unspecified),
            fields: SpanFieldsV1::default(),
            attrs,
            dd_span: DdSpan::default(),
            error: 0,
        }
    }

    /// Copies every resource attribute as a string tag, or as the field it names.
    fn copy_resource_attributes(&mut self) {
        for attribute in self.resource_attributes {
            let value = otlp_value_as_string(attribute.value.as_ref().and_then(|wrapper| wrapper.value.as_ref()));
            self.set_tag_or_field(&attribute.key, &value);
        }
    }

    /// Adds the tags taken from the span itself: top-level and measured marks, trace ID, span kind, version, links,
    /// and events.
    fn add_span_tags(&mut self, trace_id_hex: Option<&MetaString>) {
        if self.conversion.compute_top_level_by_span_kind {
            let is_root = convert_span_id(&self.otel_span.parent_span_id) == 0;
            if is_root || matches!(self.kind, SpanKind::Server | SpanKind::Consumer) {
                self.attrs
                    .insert(MetaString::from_static("_top_level"), AttributeValue::Float(1.0));
            }
            if matches!(self.kind, SpanKind::Client | SpanKind::Producer) {
                self.attrs
                    .insert(MetaString::from_static("_dd.measured"), AttributeValue::Float(1.0));
            }
        }

        let trace_id_hex = match trace_id_hex {
            Some(trace_id_hex) => trace_id_hex.clone(),
            None => bytes_to_hex_lowercase(&self.otel_span.trace_id).into(),
        };
        if !trace_id_hex.is_empty() {
            self.insert_tag(OTEL_TRACE_ID_META_KEY, trace_id_hex);
        }
        self.insert_tag(SPAN_KIND_META_KEY, MetaString::from_static(span_kind_name(self.kind)));
        if !has_meta(&self.attrs, "version") {
            let version = first_present_attribute(self.resource_attributes, &[SERVICE_VERSION]);
            if !version.is_empty() {
                let version = MetaString::from_interner(&version, self.interner);
                self.insert_tag("version", version);
            }
        }

        set_links_and_events(
            &mut self.dd_span,
            &mut self.attrs,
            self.otel_span,
            LEGACY_TAG_LINKS_AND_EVENTS,
            self.interner,
        );
        if span_contains_exception_event(&self.otel_span.events) {
            self.insert_tag("_dd.span_events.has_exception", MetaString::from_static("true"));
        }
    }

    /// Copies every span attribute over the resource attributes: numbers as metrics, everything else as string tags,
    /// with the HTTP method and status code mapped onto their Datadog keys.
    fn copy_span_attributes(&mut self) {
        let mut method_from_new_convention = false;
        let mut status_code_from_new_convention = false;
        for attribute in &self.otel_span.attributes {
            let key = attribute.key.as_str();
            let value = attribute.value.as_ref().and_then(|wrapper| wrapper.value.as_ref());
            let numeric = match value {
                Some(OtlpValue::DoubleValue(value)) => Some(AttributeValue::Float(*value)),
                Some(OtlpValue::IntValue(value)) => Some(AttributeValue::Int(*value)),
                _ => None,
            };
            if let Some(numeric) = numeric {
                // A number replaces a resource tag of the same key, even when it is stored under another key.
                self.attrs.remove(key);
                let metric_key = if key == "sampling.priority" {
                    MetaString::from_static(SAMPLING_PRIORITY_METRIC_KEY)
                } else {
                    MetaString::from_interner(key, self.interner)
                };
                self.attrs.insert(metric_key, numeric);
            } else if key != HTTP_METHOD_KEY && key != HTTP_STATUS_CODE_KEY {
                self.set_tag_or_field(key, &otlp_value_as_string(value));
            }

            // The current semantic convention keys take precedence over the older ones.
            if key == HTTP_REQUEST_METHOD_KEY || (key == HTTP_METHOD_KEY && !method_from_new_convention) {
                method_from_new_convention |= key == HTTP_REQUEST_METHOD_KEY;
                self.set_tag_or_field(HTTP_METHOD_KEY, &otlp_value_as_string(value));
            }
            if key == HTTP_RESPONSE_STATUS_CODE_KEY || (key == HTTP_STATUS_CODE_KEY && !status_code_from_new_convention)
            {
                status_code_from_new_convention |= key == HTTP_RESPONSE_STATUS_CODE_KEY;
                self.set_tag_or_field(HTTP_STATUS_CODE_KEY, &otlp_value_as_string(value));
            }
        }
    }

    /// Adds the tags derived from what was copied, the instrumentation scope, and the status, and sets the error flag.
    fn add_derived_tags(&mut self) {
        if !has_meta(&self.attrs, "env") {
            if let Some(env) = first_meta_value(&self.attrs, &[DEPLOYMENT_ENVIRONMENT_NAME, DEPLOYMENT_ENVIRONMENT_KEY])
            {
                normalize_tag_into(&env, self.string_builder);
                let env = self.string_builder.to_meta_string();
                self.insert_tag("env", env);
            }
        }
        if !has_meta(&self.attrs, "db.name") {
            let db_namespace = resource_then_span_attribute(
                self.resource_attributes,
                &self.otel_span.attributes,
                &[DB_NAMESPACE_KEY],
            );
            if !db_namespace.is_empty() {
                let db_namespace = MetaString::from_interner(&db_namespace, self.interner);
                self.insert_tag("db.name", db_namespace);
            }
        }
        if !self.otel_span.trace_state.is_empty() {
            self.insert_tag(W3C_TRACESTATE_META_KEY, self.otel_span.trace_state.as_str().into());
        }
        if let Some(scope) = self.instrumentation_scope {
            if !scope.name.is_empty() {
                self.insert_tag(OTEL_LIBRARY_NAME_META_KEY, scope.name.as_str().into());
            }
            if !scope.version.is_empty() {
                self.insert_tag(OTEL_LIBRARY_VERSION_META_KEY, scope.version.as_str().into());
            }
        }

        let status = self.otel_span.status.as_ref();
        let status_code = status
            .and_then(|status| StatusCode::try_from(status.code).ok())
            .unwrap_or(StatusCode::Unset);
        self.insert_tag(
            OTEL_STATUS_CODE_META_KEY,
            MetaString::from_static(status_code_to_string(status_code)),
        );
        if let Some(status) = status.filter(|status| !status.message.is_empty()) {
            self.insert_tag(OTEL_STATUS_DESCRIPTION_META_KEY, status.message.as_str().into());
        }
        // This path takes no error fields from exception events.
        self.error = status_to_error(status, &[], &mut self.attrs);
    }

    /// Fills the operation name, service, resource, and span type that no attribute set.
    fn fill_missing_fields(&mut self) {
        let otel_span = self.otel_span;
        let fields = &mut self.fields;
        match &self.conversion.naming {
            SpanNaming::V2 => {
                fields.name = get_otel_operation_name_v2(
                    otel_span,
                    &otel_span.attributes,
                    self.resource_attributes,
                    self.interner,
                    self.string_builder,
                );
            }
            SpanNaming::V1 {
                span_name_as_resource_name,
                span_name_remappings,
            } => {
                // Unlike V1 naming on the V2 receive path, a name set by `operation.name` is never remapped.
                if fields.name.is_empty() {
                    let builder = &mut *self.string_builder;
                    builder.clear();
                    if *span_name_as_resource_name {
                        let _ = builder.push_str(&otel_span.name);
                    } else {
                        let library = self.instrumentation_scope.map_or("", |scope| scope.name.as_str());
                        let _ = builder.push_str(if library.is_empty() { "opentelemetry" } else { library });
                        let _ = builder.push('.');
                        let _ = builder.push_str(span_kind_name(self.kind));
                    }
                    fields.name = match span_name_remappings.get(builder.as_str()) {
                        Some(remapped) => remapped.clone(),
                        None => builder.to_meta_string(),
                    };
                }
            }
        }
        if fields.service.is_empty() {
            fields.service = MetaString::from_static("OTLPResourceNoServiceName");
        }
        if fields.resource.is_empty() {
            fields.resource = match &self.conversion.naming {
                SpanNaming::V2 => get_otel_resource_v2(
                    otel_span,
                    &otel_span.attributes,
                    self.resource_attributes,
                    self.interner,
                    self.string_builder,
                ),
                SpanNaming::V1 { .. } => resource_from_tags(&self.attrs, self.interner, self.string_builder)
                    .unwrap_or_else(|| MetaString::from(otel_span.name.as_str())),
            };
        }
        if fields.span_type.is_empty() {
            fields.span_type = MetaString::from_static(span_kind_to_type_v1(
                self.kind,
                &otel_span.attributes,
                self.resource_attributes,
                self.string_builder,
            ));
        }
    }

    /// Normalizes the fields as the V2 path does, and builds the span.
    fn finish(self) -> DdSpan {
        let SpanFieldsV1 {
            service,
            name,
            mut resource,
            mut span_type,
        } = self.fields;
        normalize_service_into(&service, self.string_builder);
        let service = self.string_builder.to_meta_string();
        let name = normalize_name(name);
        if resource.is_empty() {
            resource = name.clone();
        }
        if resource.len() > self.conversion.max_resource_len {
            resource = MetaString::from(truncate_utf8(&resource, self.conversion.max_resource_len));
        }
        if span_type.len() > MAX_TYPE_LEN {
            span_type = MetaString::from(truncate_utf8(&span_type, MAX_TYPE_LEN).to_owned());
        }
        let span_id = convert_span_id(&self.otel_span.span_id);
        let parent_id = convert_span_id(&self.otel_span.parent_span_id);
        let parent_id = if parent_id == span_id { 0 } else { parent_id };
        let start = self.otel_span.start_time_unix_nano;
        let duration = validate_and_fix_duration(start, self.otel_span.end_time_unix_nano.wrapping_sub(start));
        let start = validate_and_fix_start_time(start, duration);
        let mut attrs = self.attrs;
        truncate_attributes(&mut attrs);

        let mut dd_span = self
            .dd_span
            .with_service(service)
            .with_name(name)
            .with_resource(resource)
            .with_span_type(span_type)
            .with_span_id(span_id)
            .with_parent_id(parent_id)
            .with_start(start)
            .with_duration(duration)
            .with_error(self.error);
        dd_span.attributes = attrs;
        dd_span
    }

    /// Sets an attribute as a string tag, or as the span field it names.
    ///
    /// A string tag never replaces a number stored under the same key, such as a computed `_top_level` or a numeric
    /// `http.status_code`: a Datadog span keeps string tags and numbers in separate maps, so the number would survive.
    fn set_tag_or_field(&mut self, key: &str, value: &str) {
        let field = match key {
            OPERATION_NAME_KEY => &mut self.fields.name,
            SERVICE_NAME => &mut self.fields.service,
            RESOURCE_NAME_KEY => &mut self.fields.resource,
            SPAN_TYPE_KEY => &mut self.fields.span_type,
            ANALYTICS_EVENT_KEY => {
                if let Some(enabled) = parse_bool(value) {
                    self.attrs.insert(
                        MetaString::from_static(EVENT_EXTRACTION_METRIC_KEY),
                        AttributeValue::Float(if enabled { 1.0 } else { 0.0 }),
                    );
                }
                return;
            }
            _ => {
                if self.attrs.get(key).is_none_or(|existing| existing.as_num().is_none()) {
                    self.attrs.insert(
                        MetaString::from_interner(key, self.interner),
                        AttributeValue::String(MetaString::from_interner(value, self.interner)),
                    );
                }
                return;
            }
        };
        *field = MetaString::from_interner(value, self.interner);
    }

    fn insert_tag(&mut self, key: &'static str, value: MetaString) {
        self.attrs
            .insert(MetaString::from_static(key), AttributeValue::String(value));
    }
}

/// Returns whether `key` is set as a string tag, even an empty one.
fn has_meta(attrs: &FastHashMap<MetaString, AttributeValue>, key: &str) -> bool {
    attrs.get(key).is_some_and(|value| value.as_string().is_some())
}

/// Returns the string tag `key`, if it is set and not empty.
fn meta_value<'a>(attrs: &'a FastHashMap<MetaString, AttributeValue>, key: &str) -> Option<&'a MetaString> {
    attrs
        .get(key)
        .and_then(AttributeValue::as_string)
        .filter(|value| !value.is_empty())
}

/// Returns the first of `keys` set as a non-empty string tag.
fn first_meta_value(attrs: &FastHashMap<MetaString, AttributeValue>, keys: &[&str]) -> Option<MetaString> {
    keys.iter().find_map(|key| meta_value(attrs, key)).cloned()
}

/// Derives a resource name from the string tags: the HTTP method and route, the messaging operation and destination,
/// the RPC method and service, or the GraphQL operation type and name, whichever comes first.
fn resource_from_tags(
    attrs: &FastHashMap<MetaString, AttributeValue>, interner: &GenericMapInterner,
    string_builder: &mut StringBuilder<GenericMapInterner>,
) -> Option<MetaString> {
    let (first, second) = [
        (HTTP_REQUEST_METHOD_KEYS, &[HTTP_ROUTE_KEY, GRPC_PATH_KEY][..]),
        (&[MESSAGING_OPERATION_KEY][..], MESSAGING_DESTINATION_KEYS),
        (&[RPC_METHOD_KEY][..], &[RPC_SERVICE_KEY][..]),
        (&[GRAPHQL_OPERATION_TYPE_KEY][..], &[GRAPHQL_OPERATION_NAME_KEY][..]),
    ]
    .into_iter()
    .find_map(|(first_keys, second_keys)| {
        first_meta_value(attrs, first_keys).map(|first| (first, first_meta_value(attrs, second_keys)))
    })?;
    Some(match second {
        Some(second) => join_resource_parts(&first, &second, interner, string_builder),
        None => first,
    })
}

/// Returns the span type of the V1 receive path: `web` for servers, `http`, `cache`, or `db` for clients, and `custom`
/// otherwise.
fn span_kind_to_type_v1(
    kind: SpanKind, span_attributes: &[KeyValue], resource_attributes: &[KeyValue],
    string_builder: &mut StringBuilder<GenericMapInterner>,
) -> &'static str {
    match kind {
        SpanKind::Server => "web",
        SpanKind::Client => {
            // Each map's value is normalized before falling back to the next map.
            for attributes in [resource_attributes, span_attributes] {
                normalize_tag_value_into_unchecked(
                    &first_present_attribute(attributes, &[DB_SYSTEM_KEY]),
                    string_builder,
                );
                match string_builder.as_str() {
                    "" => continue,
                    "redis" | "memcached" => return "cache",
                    _ => return "db",
                }
            }
            "http"
        }
        _ => "custom",
    }
}

/// Joins two resource name parts with a space.
fn join_resource_parts(
    first: &str, second: &str, interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
) -> MetaString {
    if second.is_empty() {
        return MetaString::from_interner(first, interner);
    }
    string_builder.clear();
    let _ = string_builder.push_str(first);
    let _ = string_builder.push(' ');
    let _ = string_builder.push_str(second);
    string_builder.to_meta_string()
}

/// Truncates oversized attribute keys and string values in-place, leaving everything else untouched.
///
/// Keys longer than `MAX_META_KEY_LEN` and string values longer than `MAX_META_VAL_LEN` are
/// truncated to the limit and suffixed with `...`. Structured meta keys
/// (`_dd.*.{json,msgpack,protobuf}`) are exempt. Only oversized entries are rewritten.
fn truncate_attributes(attrs: &mut FastHashMap<MetaString, AttributeValue>) {
    let oversized_keys: Vec<MetaString> = attrs
        .iter()
        .filter(|(key, value)| {
            if is_structured_meta_key(key) {
                return false;
            }
            key.len() > MAX_META_KEY_LEN || matches!(value, AttributeValue::String(s) if s.len() > MAX_META_VAL_LEN)
        })
        .map(|(key, _)| key.clone())
        .collect();

    for key in oversized_keys {
        let Some(mut value) = attrs.remove(&key) else {
            continue;
        };
        let new_key = if key.len() > MAX_META_KEY_LEN {
            MetaString::from(truncate_with_ellipses(key.as_ref(), MAX_META_KEY_LEN).into_owned())
        } else {
            key
        };
        if let AttributeValue::String(s) = &mut value {
            if s.len() > MAX_META_VAL_LEN {
                *s = MetaString::from(truncate_with_ellipses(s.as_ref(), MAX_META_VAL_LEN).into_owned());
            }
        }
        attrs.insert(new_key, value);
    }
}

// OtelSpanToDDSpanMinimal otelSpanToDDSpan converts an OTel span to a DD span.
// The converted DD span only has the minimal number of fields for APM stats calculation and is only meant
// to be used in OTLPTracesToConcentratorInputs. Do not use them for other purposes.
pub fn otel_to_dd_span_minimal(
    otel_span: &OtlpSpan, otel_resource: &Resource, instrumentation_scope: Option<&OtlpInstrumentationScope>,
    conversion: &SpanConversion, registry: &Registry, interner: &GenericMapInterner,
    string_builder: &mut StringBuilder<GenericMapInterner>,
) -> (DdSpan, FastHashMap<MetaString, AttributeValue>) {
    let SpanConversion {
        ignore_missing_fields,
        compute_top_level_by_span_kind,
        max_resource_len,
        ..
    } = *conversion;
    let span_attributes = &otel_span.attributes;
    let resource_attributes = &otel_resource.attributes;
    let mut dd_span = DdSpan::default();

    let span_id = convert_span_id(&otel_span.span_id);
    let mut parent_id = convert_span_id(&otel_span.parent_span_id);

    // Mirrors `normalizeV1` (`pkg/trace/agent/normalizer.go`): a span that is its own parent is
    // treated as a root span.
    if parent_id == span_id {
        parent_id = 0;
    }

    // Mirrors `validateAndFixDurationV1` and `validateAndFixStartTimeV1`, in that order: the duration
    // is fixed first using the original start, then the start is fixed using the corrected duration.
    // Without the duration fix, a tracer reporting an end time before its start time wraps the `u64`
    // subtraction and the span carries a duration near `u64::MAX`.
    let start = otel_span.start_time_unix_nano;
    let duration = validate_and_fix_duration(start, otel_span.end_time_unix_nano.wrapping_sub(start));
    let start = validate_and_fix_start_time(start, duration);

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
        if service.is_empty() {
            service = get_otel_service(span_attributes, resource_attributes, true, interner, string_builder);
        }
        match &conversion.naming {
            SpanNaming::V2 => {
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
                    resource = get_otel_resource_v2(
                        otel_span,
                        span_attributes,
                        resource_attributes,
                        interner,
                        string_builder,
                    );
                }
            }
            SpanNaming::V1 {
                span_name_as_resource_name,
                span_name_remappings,
            } => {
                if name.is_empty() {
                    name = get_otel_operation_name_v1(
                        otel_span,
                        resource_attributes,
                        instrumentation_scope,
                        *span_name_as_resource_name,
                        span_name_remappings,
                        interner,
                        string_builder,
                    );
                }
                if resource.is_empty() {
                    resource = get_otel_resource_v1(otel_span, resource_attributes, interner, string_builder);
                }
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

    // Mirrors `validateAndFixType` (`pkg/trace/agent/normalizer.go`): truncate `span.type` at
    // `MaxTypeLen` bytes, cutting back to a valid UTF-8 boundary.
    if span_type.len() > MAX_TYPE_LEN {
        span_type = MetaString::from(truncate_utf8(&span_type, MAX_TYPE_LEN).to_owned());
    }

    name = normalize_name(name);

    // An empty resource falls back to the span name.
    if !ignore_missing_fields && resource.is_empty() {
        resource = name.clone();
    }

    if resource.len() > max_resource_len {
        resource = MetaString::from(truncate_utf8(&resource, max_resource_len));
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

    if let Some(status_code) =
        get_otel_status_code(registry, span_attributes, resource_attributes, ignore_missing_fields)
    {
        if status_code >= 0 {
            attrs.insert(
                MetaString::from_static(HTTP_STATUS_CODE_KEY),
                AttributeValue::Float(status_code as f64),
            );
        }
    }

    // An explicit numeric `_sample_rate` span attribute wins over the rate decoded from the tracestate. It is
    // applied here, ahead of the generic attribute mapping, because that mapping never overwrites an existing key.
    let explicit_sample_rate = span_attributes
        .iter()
        .find(|attribute| attribute.key == SAMPLE_RATE_KEY)
        .and_then(|attribute| match attribute.value.as_ref()?.value.as_ref()? {
            OtlpValue::DoubleValue(rate) => Some(*rate),
            OtlpValue::IntValue(rate) => Some(*rate as f64),
            _ => None,
        });
    if let Some(rate) = explicit_sample_rate.or_else(|| sample_rate_from_tracestate(&otel_span.trace_state)) {
        attrs.insert(MetaString::from_static(SAMPLE_RATE_KEY), AttributeValue::Float(rate));
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

/// Returns the operation name of V1 naming: `operation.name`, or else the span name or `<library>.<kind>`.
///
/// `span_name_remappings` applies to the result, including a name taken from `operation.name`. The caller
/// normalizes it.
fn get_otel_operation_name_v1(
    otel_span: &OtlpSpan, resource_attributes: &[KeyValue], instrumentation_scope: Option<&OtlpInstrumentationScope>,
    span_name_as_resource_name: bool, span_name_remappings: &FastHashMap<String, MetaString>,
    interner: &GenericMapInterner, string_builder: &mut StringBuilder<GenericMapInterner>,
) -> MetaString {
    string_builder.clear();
    let operation_name =
        resource_then_span_attribute(resource_attributes, &otel_span.attributes, &[OPERATION_NAME_KEY]);
    if !operation_name.is_empty() {
        let _ = string_builder.push_str(&operation_name);
    } else if span_name_as_resource_name {
        let _ = string_builder.push_str(&otel_span.name);
    } else {
        // An unknown span kind contributes an empty name.
        let kind = SpanKind::try_from(otel_span.kind).map_or("", span_kind_name);
        let library = instrumentation_scope.map_or("", |scope| scope.name.as_str());
        let _ = string_builder.push_str(if library.is_empty() { "opentelemetry" } else { library });
        let _ = string_builder.push('.');
        let _ = string_builder.push_str(kind);
    }

    if let Some(remapped) = span_name_remappings.get(string_builder.as_str()) {
        return remapped.clone();
    }
    MetaString::from_interner(string_builder.as_str(), interner)
}

/// Returns the resource name of V1 naming.
///
/// Unlike V2, the HTTP route applies to every span kind, and neither database statements nor GraphQL values are used
/// or normalized. The caller truncates the result.
fn get_otel_resource_v1(
    otel_span: &OtlpSpan, resource_attributes: &[KeyValue], interner: &GenericMapInterner,
    string_builder: &mut StringBuilder<GenericMapInterner>,
) -> MetaString {
    let span_attributes = &otel_span.attributes;
    let attribute = |keys: &[&str]| resource_then_span_attribute(resource_attributes, span_attributes, keys);

    let resource_name = attribute(&[RESOURCE_NAME_KEY]);
    if !resource_name.is_empty() {
        return MetaString::from_interner(&resource_name, interner);
    }

    let (first, second) = [
        (HTTP_REQUEST_METHOD_KEYS, &[HTTP_ROUTE_KEY][..]),
        (&[MESSAGING_OPERATION_KEY][..], MESSAGING_DESTINATION_KEYS),
        (&[RPC_METHOD_KEY][..], &[RPC_SERVICE_KEY][..]),
        (&[GRAPHQL_OPERATION_TYPE_KEY][..], &[GRAPHQL_OPERATION_NAME_KEY][..]),
    ]
    .into_iter()
    .find_map(|(first_keys, second_keys)| {
        let first = attribute(first_keys);
        (!first.is_empty()).then(|| (first, attribute(second_keys)))
    })
    .unwrap_or((Cow::Borrowed(otel_span.name.as_str()), Cow::Borrowed("")));
    join_resource_parts(&first, &second, interner, string_builder)
}

/// Returns the first of `keys` present in the resource attributes, or else in the span attributes, as a string.
///
/// V1 naming looks attributes up this way: resource attributes take precedence, and a key that is present with an
/// empty value hides the later keys of the same map.
fn resource_then_span_attribute<'a>(
    resource_attributes: &'a [KeyValue], span_attributes: &'a [KeyValue], keys: &[&str],
) -> Cow<'a, str> {
    let value = first_present_attribute(resource_attributes, keys);
    if !value.is_empty() {
        return value;
    }
    first_present_attribute(span_attributes, keys)
}

/// Returns the first of `keys` present in `attributes`, as `pcommon.Value.AsString` renders it.
fn first_present_attribute<'a>(attributes: &'a [KeyValue], keys: &[&str]) -> Cow<'a, str> {
    keys.iter()
        .find_map(|key| attributes.iter().find(|attribute| attribute.key == *key))
        .map_or(Cow::Borrowed(""), |attribute| {
            otlp_value_as_string(attribute.value.as_ref().and_then(|wrapper| wrapper.value.as_ref()))
        })
}

/// Renders an OTLP value the way `pcommon.Value.AsString` does. Arrays and maps become JSON.
fn otlp_value_as_string(value: Option<&OtlpValue>) -> Cow<'_, str> {
    match value {
        Some(value @ (OtlpValue::ArrayValue(_) | OtlpValue::KvlistValue(_))) => {
            otlp_value_to_string(value).map_or(Cow::Borrowed(""), Cow::Owned)
        }
        scalar => otlp_scalar_as_string(scalar),
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

/// Records the links and events of `otel_span` on `dd_span`.
///
/// They become the span's links and events, unless `legacy_tags` is set, in which case they go in `attrs` as the
/// `_dd.span_links` and `events` JSON tags.
fn set_links_and_events(
    dd_span: &mut DdSpan, attrs: &mut FastHashMap<MetaString, AttributeValue>, otel_span: &OtlpSpan, legacy_tags: bool,
    interner: &GenericMapInterner,
) {
    if !legacy_tags {
        *dd_span.span_links_mut() = otlp_links_to_span_links(&otel_span.links, interner);
        *dd_span.span_events_mut() = otlp_events_to_span_events(&otel_span.events, interner);
        return;
    }

    if let Some(events_json) = marshal_events(&otel_span.events) {
        attrs.insert(
            MetaString::from_static("events"),
            AttributeValue::String(events_json.into()),
        );
    }
    if let Some(links_json) = marshal_links(&otel_span.links) {
        attrs.insert(
            MetaString::from_static("_dd.span_links"),
            AttributeValue::String(links_json.into()),
        );
    }
}

/// Converts OTLP span links into [`SpanLink`]s.
///
/// Mirrors the Agent's `OTelSpanLinksToDD` (`pkg/trace/transform/transform.go`). Link attributes are strings in the
/// Datadog model, so arrays are flattened into `key.0`, `key.1`, and so on, and maps into `key.subkey`. The `link.name`
/// attribute is normalized here, since nothing downstream does it for ADP.
pub(crate) fn otlp_links_to_span_links(links: &[OtlpSpanLink], interner: &GenericMapInterner) -> Vec<SpanLink> {
    // One buffer builds every flattened key, so flattening allocates only for what the interner keeps.
    let mut key = String::new();
    links
        .iter()
        .map(|link| {
            let mut span_link = SpanLink::new(convert_trace_id(&link.trace_id), convert_span_id(&link.span_id))
                .with_trace_id_high(convert_trace_id_high(&link.trace_id));
            if !link.trace_state.is_empty() {
                span_link = span_link.with_tracestate(MetaString::from_interner(link.trace_state.as_str(), interner));
            }
            if link.flags != 0 {
                span_link = span_link.with_flags((link.flags & SPAN_LINK_TRACE_FLAGS_MASK) | SPAN_LINK_FLAGS_PRESENT);
            }
            if !link.attributes.is_empty() {
                let mut attributes = FastHashMap::default();
                for attribute in &link.attributes {
                    let value = attribute.value.as_ref().and_then(|wrapper| wrapper.value.as_ref());
                    key.clear();
                    key.push_str(&attribute.key);
                    flatten_link_attribute(&mut attributes, &mut key, value, interner);
                }
                if let Some(AttributeValue::String(link_name)) = attributes.get_mut(LINK_NAME_KEY) {
                    if needs_name_normalization(link_name.as_ref()) {
                        *link_name = normalize_name(link_name.clone());
                    }
                }
                span_link = span_link.with_attributes(attributes);
            }
            span_link
        })
        .collect()
}

/// Sets `key` in the string-valued attributes of a [`SpanLink`], flattening arrays and maps.
fn flatten_link_attribute(
    attributes: &mut FastHashMap<MetaString, AttributeValue>, key: &mut String, value: Option<&OtlpValue>,
    interner: &GenericMapInterner,
) {
    match value {
        Some(OtlpValue::ArrayValue(array)) => {
            let base = key.len();
            for (index, item) in array.values.iter().enumerate() {
                let _ = write!(key, ".{index}");
                flatten_link_attribute(attributes, key, item.value.as_ref(), interner);
                key.truncate(base);
            }
        }
        Some(OtlpValue::KvlistValue(list)) => {
            let base = key.len();
            for entry in &list.values {
                key.push('.');
                key.push_str(&entry.key);
                let value = entry.value.as_ref().and_then(|wrapper| wrapper.value.as_ref());
                flatten_link_attribute(attributes, key, value, interner);
                key.truncate(base);
            }
        }
        scalar => {
            attributes.insert(
                MetaString::from_interner(key.as_str(), interner),
                AttributeValue::String(MetaString::from_interner(&otlp_scalar_as_string(scalar), interner)),
            );
        }
    }
}

/// Renders a scalar OTLP value the way `pcommon.Value.AsString` does. An absent value is the empty string.
fn otlp_scalar_as_string(value: Option<&OtlpValue>) -> Cow<'_, str> {
    match value {
        Some(OtlpValue::StringValue(value)) => Cow::Borrowed(value.as_str()),
        Some(OtlpValue::BoolValue(value)) => Cow::Borrowed(if *value { "true" } else { "false" }),
        Some(OtlpValue::IntValue(value)) => Cow::Owned(value.to_string()),
        Some(OtlpValue::DoubleValue(value)) => Cow::Owned(float64_as_string(*value)),
        Some(OtlpValue::BytesValue(value)) => Cow::Owned(general_purpose::STANDARD.encode(value)),
        // Callers flatten arrays and maps, so they never reach this point.
        Some(OtlpValue::ArrayValue(_) | OtlpValue::KvlistValue(_)) | None => Cow::Borrowed(""),
    }
}

/// Renders a double like `pcommon`'s `float64AsString` in `pdata` v1.70.0, the version the Agent pins: ES6
/// number-to-string, which is decimal from 1e-6 up to 1e21 and exponent form outside that range, with `NaN`,
/// `Infinity`, and `-Infinity` spelled out.
fn float64_as_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }

    let magnitude = value.abs();
    if magnitude != 0.0 && !(1e-6..1e21).contains(&magnitude) {
        // Rust writes `1e21` and `1.5e-7`; ES6 writes a sign on positive exponents, as in `1e+21`.
        let exponent_form = format!("{value:e}");
        return match exponent_form.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => format!("{mantissa}e+{exponent}"),
            _ => exponent_form,
        };
    }
    value.to_string()
}

/// Converts OTLP span events into [`SpanEvent`]s.
///
/// Mirrors the Agent's `OTelSpanEventsToDD` (`pkg/trace/transform/transform.go`). String, bool, int, and double
/// attributes keep their type, and so do arrays whose values all share one of those types. Bytes become base64
/// strings, other arrays are flattened into `key.0`, `key.1`, and so on, maps into `key.subkey`, and empty values are
/// dropped.
pub(crate) fn otlp_events_to_span_events(events: &[OtlpSpanEvent], interner: &GenericMapInterner) -> Vec<SpanEvent> {
    // One buffer builds every flattened key, so flattening allocates only for what the interner keeps.
    let mut key = String::new();
    events
        .iter()
        .map(|event| {
            let mut attributes = FastHashMap::default();
            for attribute in &event.attributes {
                let value = attribute.value.as_ref().and_then(|wrapper| wrapper.value.as_ref());
                key.clear();
                key.push_str(&attribute.key);
                set_event_attribute(&mut attributes, &mut key, value, interner);
            }
            SpanEvent::new(
                event.time_unix_nano,
                MetaString::from_interner(event.name.as_str(), interner),
            )
            .with_attributes(attributes)
        })
        .collect()
}

/// Sets `key` in the attributes of a [`SpanEvent`].
fn set_event_attribute(
    attributes: &mut FastHashMap<MetaString, AttributeValue>, key: &mut String, value: Option<&OtlpValue>,
    interner: &GenericMapInterner,
) {
    let typed = match value {
        None => return,
        Some(OtlpValue::StringValue(value)) => {
            AttributeValue::String(MetaString::from_interner(value.as_str(), interner))
        }
        Some(OtlpValue::BoolValue(value)) => AttributeValue::Bool(*value),
        Some(OtlpValue::IntValue(value)) => AttributeValue::Int(*value),
        Some(OtlpValue::DoubleValue(value)) => AttributeValue::Float(*value),
        Some(OtlpValue::BytesValue(value)) => AttributeValue::String(MetaString::from_interner(
            general_purpose::STANDARD.encode(value).as_str(),
            interner,
        )),
        Some(OtlpValue::ArrayValue(array)) => {
            if let Some(values) = event_attribute_array(&array.values, interner) {
                AttributeValue::Array(values)
            } else {
                let base = key.len();
                for (index, item) in array.values.iter().enumerate() {
                    let _ = write!(key, ".{index}");
                    set_event_attribute(attributes, key, item.value.as_ref(), interner);
                    key.truncate(base);
                }
                return;
            }
        }
        Some(OtlpValue::KvlistValue(list)) => {
            let base = key.len();
            for entry in &list.values {
                key.push('.');
                key.push_str(&entry.key);
                let value = entry.value.as_ref().and_then(|wrapper| wrapper.value.as_ref());
                set_event_attribute(attributes, key, value, interner);
                key.truncate(base);
            }
            return;
        }
    };
    attributes.insert(MetaString::from_interner(key.as_str(), interner), typed);
}

/// Converts `values` into a typed array attribute, which is empty for an empty slice.
///
/// Returns `None` unless every value is a string, a bool, an int, or a double, all of the same type.
fn event_attribute_array(values: &[AnyValue], interner: &GenericMapInterner) -> Option<Vec<AttributeValue>> {
    let Some(first) = values.first() else {
        return Some(Vec::new());
    };
    let kind = std::mem::discriminant(first.value.as_ref()?);
    values
        .iter()
        .map(|item| {
            match item
                .value
                .as_ref()
                .filter(|value| std::mem::discriminant(*value) == kind)?
            {
                OtlpValue::StringValue(value) => Some(AttributeValue::String(MetaString::from_interner(
                    value.as_str(),
                    interner,
                ))),
                OtlpValue::BoolValue(value) => Some(AttributeValue::Bool(*value)),
                OtlpValue::IntValue(value) => Some(AttributeValue::Int(*value)),
                OtlpValue::DoubleValue(value) => Some(AttributeValue::Float(*value)),
                _ => None,
            }
        })
        .collect()
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
        if let Some(mut attributes) = key_values_to_json_object(&link.attributes) {
            if let Some(JsonValue::String(link_name)) = attributes.get_mut(LINK_NAME_KEY) {
                if needs_name_normalization(link_name) {
                    let normalized = normalize_name(MetaString::from(link_name.as_str()));
                    *link_name = normalized.as_ref().to_string();
                }
            }
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
        if let Some(value) = attrs.get(*key) {
            if let Some(s) = value.as_string() {
                return Some(s.clone());
            }
            // `http.status_code` is emitted as a numeric (Float/Int) value; format it as an
            // integer string so the error-message fallback can parse it as a status code.
            if let Some(n) = value.as_num() {
                return Some(MetaString::from((n as i64).to_string()));
            }
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
    registry: &Registry, span_attributes: &[KeyValue], resource_attributes: &[KeyValue], ignore_missing_fields: bool,
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
    lookup_int64(registry, &combined, Concept::HttpStatusCode)
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

    use datadog_agent_apm::semantics::{test_util::registry_json, EMBEDDED_REGISTRY};
    use otlp_protos::opentelemetry::proto::common::v1::{AnyValue, ArrayValue, KeyValue, KeyValueList};
    use otlp_protos::opentelemetry::proto::resource::v1::Resource;
    use otlp_protos::opentelemetry::proto::trace::v1::Span as OtlpSpan;

    use super::*;
    use crate::common::otlp::traces::normalize::YEAR_2000_NANOSEC_TS;

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

    /// The conversion of a translator built from the configuration defaults.
    fn conversion() -> SpanConversion {
        SpanConversion {
            ignore_missing_fields: false,
            compute_top_level_by_span_kind: true,
            max_resource_len: 5000,
            naming: SpanNaming::V2,
            emit_otel_scope: true,
        }
    }

    fn meta_str<'a>(dd_span: &'a DdSpan, key: &str) -> Option<&'a str> {
        dd_span
            .attributes
            .get(key)
            .and_then(AttributeValue::as_string)
            .map(|value| value.as_ref())
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

    /// The shared signature of the `get_otel_*` string attribute extractors driven by
    /// [`run_attribute_extraction_cases`].
    type AttributeExtractor =
        fn(&[KeyValue], &[KeyValue], bool, &GenericMapInterner, &mut StringBuilder<GenericMapInterner>) -> MetaString;

    /// Drives `extractor` over every case with a shared interner/string builder and asserts the extracted string
    /// equals the case's `expected` value, reporting the failing case name.
    fn run_attribute_extraction_cases(extractor: AttributeExtractor, cases: Vec<AttributeExtractionCase>) {
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
            let result = get_otel_status_code(
                &EMBEDDED_REGISTRY,
                &tc.span_attrs,
                &tc.resource_attrs,
                tc.ignore_missing_datadog_fields,
            );
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
            &conversion(),
            &EMBEDDED_REGISTRY,
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
                &conversion(),
                &EMBEDDED_REGISTRY,
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
    fn otel_span_to_dd_span_emits_scope_tags_unless_the_scope_convention_is_disabled() {
        let (interner, mut sb) = extraction_env();
        let span = OtlpSpan {
            name: "scoped-span".to_string(),
            ..Default::default()
        };
        let scope = OtlpInstrumentationScope {
            name: "com.example.products".to_string(),
            version: "1.0.0".to_string(),
            ..Default::default()
        };

        for emit_otel_scope in [true, false] {
            let conversion = SpanConversion {
                emit_otel_scope,
                ..conversion()
            };
            let dd_span = otel_span_to_dd_span(
                &span,
                &Resource::default(),
                Some(&scope),
                &conversion,
                &EMBEDDED_REGISTRY,
                &interner,
                &mut sb,
                None,
            );

            // The deprecated `otel.library.*` tags are always emitted.
            assert_eq!(meta_str(&dd_span, "otel.library.name"), Some("com.example.products"));
            assert_eq!(meta_str(&dd_span, "otel.library.version"), Some("1.0.0"));
            let (name, version) = if emit_otel_scope {
                (Some("com.example.products"), Some("1.0.0"))
            } else {
                (None, None)
            };
            assert_eq!(meta_str(&dd_span, "otel.scope.name"), name, "{emit_otel_scope}");
            assert_eq!(meta_str(&dd_span, "otel.scope.version"), version, "{emit_otel_scope}");
        }
    }

    // ===============================================================================================
    // Branching translation logic: the V2 service/operation-name/resource/span-type resolvers, the
    // status-to-error mapping, and the full `otel_span_to_dd_span` assembly entry point.
    //
    // Ported from the Go trace agent's `GetOTelService`/`GetOTelOperationNameV2`/`GetOTelResourceV2`/
    // `GetOTelSpanType`/`status2Error` in:
    // https://github.com/DataDog/datadog-agent/blob/instrument-otlp-traffic/pkg/trace/traceutil/otel_util.go
    // https://github.com/DataDog/datadog-agent/blob/instrument-otlp-traffic/pkg/trace/transform/transform.go
    // ===============================================================================================

    fn peer_service_span(attrs: &[(&str, &str)]) -> DdSpan {
        let attributes = attrs
            .iter()
            .map(|(k, v)| (MetaString::from(*k), AttributeValue::String(MetaString::from(*v))))
            .collect::<FastHashMap<MetaString, AttributeValue>>();
        DdSpan::new("svc", "op", "res", "web", 1, 0, 0, 1, 0).with_attributes(attributes)
    }

    #[test]
    fn peer_service_resolves_registered_equivalents_not_literal_keys() {
        let registry = Registry::from_json(&registry_json(
            r#"{"peer.service":{"canonical":"peer.service","fallbacks":[
                {"name":"service.peer","provider":"otel","type":"string"},
                {"name":"peer.service","provider":"datadog","type":"string"}]}}"#,
        ))
        .expect("test registry parses");

        let mut span = peer_service_span(&[("service.peer", "checkouts")]);
        let (interner, mut string_builder) = extraction_env();
        normalize_peer_service_tags(
            &mut span,
            &registry,
            &interner,
            &mut string_builder,
            &Metrics::for_tests(),
        );

        assert_eq!(
            span.attributes
                .get("peer.service")
                .and_then(|v| v.as_string())
                .map(|s| s.as_ref()),
            Some("checkouts"),
            "the equivalent attribute resolves and is written back under the canonical key"
        );
        assert_eq!(
            span.attributes
                .get("service.peer")
                .and_then(|v| v.as_string())
                .map(|s| s.as_ref()),
            Some("checkouts"),
            "the source attribute is left as-is"
        );
    }

    #[test]
    fn peer_service_caps_at_the_service_length_limit() {
        use metrics::set_default_local_recorder;
        use saluki_core::components::ComponentContext;
        use saluki_metrics::test::TestRecorder;

        use crate::common::otlp::build_metrics;

        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);
        let metrics = build_metrics(&ComponentContext::test_source("otlp_test"));

        let mut span = peer_service_span(&[("peer.service", &"a".repeat(120))]);
        let (interner, mut string_builder) = extraction_env();
        normalize_peer_service_tags(&mut span, &EMBEDDED_REGISTRY, &interner, &mut string_builder, &metrics);

        let capped = span
            .attributes
            .get("peer.service")
            .and_then(|v| v.as_string())
            .expect("canonical key is set");
        assert_eq!(
            capped.as_ref().len(),
            100,
            "the value is capped at the service length limit"
        );

        let tags: &[(&str, &str)] = &[
            ("component_id", "otlp_test"),
            ("component_type", "source"),
            ("tag", "peer.service"),
            ("reason", "truncate"),
        ];
        assert_eq!(recorder.counter(("component_spans_malformed_total", tags)), Some(1));
    }

    #[test]
    fn peer_service_value_that_normalizes_to_empty_is_not_substituted() {
        use metrics::set_default_local_recorder;
        use saluki_core::components::ComponentContext;
        use saluki_metrics::test::TestRecorder;

        use crate::common::otlp::build_metrics;

        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);
        let metrics = build_metrics(&ComponentContext::test_source("otlp_test"));

        let mut span = peer_service_span(&[("peer.service", "!!!")]);
        let (interner, mut string_builder) = extraction_env();
        normalize_peer_service_tags(&mut span, &EMBEDDED_REGISTRY, &interner, &mut string_builder, &metrics);

        assert_eq!(
            span.attributes
                .get("peer.service")
                .and_then(|v| v.as_string())
                .map(|s| s.as_ref()),
            Some(""),
            "the value is dropped, not replaced"
        );

        let tags: &[(&str, &str)] = &[
            ("component_id", "otlp_test"),
            ("component_type", "source"),
            ("tag", "peer.service"),
            ("reason", "invalid"),
        ];
        assert_eq!(recorder.counter(("component_spans_malformed_total", tags)), Some(1));
    }

    #[test]
    fn absent_peer_service_tags_are_left_unset() {
        let mut span = peer_service_span(&[("http.url", "/pay")]);
        let (interner, mut string_builder) = extraction_env();
        normalize_peer_service_tags(
            &mut span,
            &EMBEDDED_REGISTRY,
            &interner,
            &mut string_builder,
            &Metrics::for_tests(),
        );

        assert!(!span.attributes.contains_key("peer.service"));
        assert!(!span.attributes.contains_key("_dd.base_service"));
    }

    #[test]
    fn base_service_normalizes_like_a_tag_value() {
        let mut span = peer_service_span(&[("_dd.base_service", "payments db")]);
        let (interner, mut string_builder) = extraction_env();
        normalize_peer_service_tags(
            &mut span,
            &EMBEDDED_REGISTRY,
            &interner,
            &mut string_builder,
            &Metrics::for_tests(),
        );

        assert_eq!(
            span.attributes
                .get("_dd.base_service")
                .and_then(|v| v.as_string())
                .map(|s| s.as_ref()),
            Some("payments_db"),
            "the canonical key holds the normalized value"
        );
    }

    fn extraction_env() -> (GenericMapInterner, StringBuilder<GenericMapInterner>) {
        let interner = test_interner();
        let string_builder = StringBuilder::new().with_interner(interner.clone());
        (interner, string_builder)
    }

    fn service_for(span_attrs: &[KeyValue], resource_attrs: &[KeyValue]) -> String {
        let (interner, mut sb) = extraction_env();
        get_otel_service(span_attrs, resource_attrs, true, &interner, &mut sb)
            .as_ref()
            .to_string()
    }

    fn operation_name_for(kind: SpanKind, span_attrs: &[KeyValue]) -> String {
        let (interner, mut sb) = extraction_env();
        let span = OtlpSpan {
            kind: kind as i32,
            ..Default::default()
        };
        get_otel_operation_name_v2(&span, span_attrs, &[], &interner, &mut sb)
            .as_ref()
            .to_string()
    }

    fn resource_name_for(kind: SpanKind, span_name: &str, span_attrs: &[KeyValue]) -> String {
        let (interner, mut sb) = extraction_env();
        let span = OtlpSpan {
            name: span_name.to_string(),
            kind: kind as i32,
            ..Default::default()
        };
        get_otel_resource_v2(&span, span_attrs, &[], &interner, &mut sb)
            .as_ref()
            .to_string()
    }

    fn span_type_for(kind: SpanKind, span_attrs: &[KeyValue]) -> String {
        let (interner, mut sb) = extraction_env();
        let span = OtlpSpan {
            kind: kind as i32,
            ..Default::default()
        };
        get_otel_span_type(&span, span_attrs, &[], &interner, &mut sb)
            .as_ref()
            .to_string()
    }

    #[test]
    fn get_otel_service_resolves_by_precedence_with_default_and_normalization() {
        // No service anywhere falls back to the documented default.
        assert_eq!(service_for(&[], &[]), "otlpresourcenoservicename");
        // Span attributes win over resource attributes.
        assert_eq!(service_for(&[kv_str(SERVICE_NAME, "span-svc")], &[]), "span-svc");
        assert_eq!(service_for(&[], &[kv_str(SERVICE_NAME, "res-svc")]), "res-svc");
        assert_eq!(
            service_for(&[kv_str(SERVICE_NAME, "span-svc")], &[kv_str(SERVICE_NAME, "res-svc")]),
            "span-svc"
        );
        // The resolved service is normalized (lowercased, illegal-character runs collapsed to `_`).
        assert_eq!(service_for(&[kv_str(SERVICE_NAME, "My Service!")], &[]), "my_service");
    }

    #[test]
    fn get_otel_operation_name_v2_covers_semantic_convention_branches() {
        struct Case {
            name: &'static str,
            kind: SpanKind,
            attrs: Vec<KeyValue>,
            expected: &'static str,
        }

        let cases = vec![
            Case {
                name: "operation.name override",
                kind: SpanKind::Internal,
                attrs: vec![kv_str("operation.name", "custom.op")],
                expected: "custom.op",
            },
            Case {
                name: "http server",
                kind: SpanKind::Server,
                attrs: vec![kv_str("http.request.method", "GET")],
                expected: "http.server.request",
            },
            Case {
                name: "http client",
                kind: SpanKind::Client,
                attrs: vec![kv_str("http.request.method", "GET")],
                expected: "http.client.request",
            },
            Case {
                name: "database client",
                kind: SpanKind::Client,
                attrs: vec![kv_str("db.system", "postgresql")],
                expected: "postgresql.query",
            },
            Case {
                name: "messaging producer",
                kind: SpanKind::Producer,
                attrs: vec![
                    kv_str("messaging.system", "kafka"),
                    kv_str("messaging.operation", "publish"),
                ],
                expected: "kafka.publish",
            },
            Case {
                name: "rpc client",
                kind: SpanKind::Client,
                attrs: vec![kv_str("rpc.system", "grpc")],
                expected: "grpc.client.request",
            },
            Case {
                name: "rpc server",
                kind: SpanKind::Server,
                attrs: vec![kv_str("rpc.system", "grpc")],
                expected: "grpc.server.request",
            },
            Case {
                name: "aws rpc client with service",
                kind: SpanKind::Client,
                // The RPC service is tag-normalized (lowercased) into the operation name.
                attrs: vec![kv_str("rpc.system", "aws-api"), kv_str("rpc.service", "S3")],
                expected: "aws.s3.request",
            },
            Case {
                name: "aws rpc client without service",
                kind: SpanKind::Client,
                attrs: vec![kv_str("rpc.system", "aws-api")],
                expected: "aws.client.request",
            },
            Case {
                name: "faas client",
                kind: SpanKind::Client,
                attrs: vec![
                    kv_str("faas.invoked_provider", "aws"),
                    kv_str("faas.invoked_name", "lambda"),
                ],
                expected: "aws.lambda.invoke",
            },
            Case {
                name: "faas server",
                kind: SpanKind::Server,
                attrs: vec![kv_str("faas.trigger", "http")],
                expected: "http.invoke",
            },
            Case {
                name: "graphql",
                kind: SpanKind::Server,
                attrs: vec![kv_str("graphql.operation.type", "query")],
                expected: "graphql.server.request",
            },
            Case {
                name: "network protocol server",
                kind: SpanKind::Server,
                attrs: vec![kv_str("network.protocol.name", "amqp")],
                expected: "amqp.server.request",
            },
            Case {
                name: "plain server",
                kind: SpanKind::Server,
                attrs: vec![],
                expected: "server.request",
            },
            Case {
                name: "plain client",
                kind: SpanKind::Client,
                attrs: vec![],
                expected: "client.request",
            },
            Case {
                name: "internal fallback uses capitalized span kind",
                kind: SpanKind::Internal,
                attrs: vec![],
                expected: "Internal",
            },
        ];

        for case in cases {
            assert_eq!(
                operation_name_for(case.kind, &case.attrs),
                case.expected,
                "case: {}",
                case.name
            );
        }
    }

    #[test]
    fn get_otel_resource_v2_covers_semantic_convention_branches() {
        struct Case {
            name: &'static str,
            kind: SpanKind,
            span_name: &'static str,
            attrs: Vec<KeyValue>,
            expected: &'static str,
        }

        let cases = vec![
            Case {
                name: "resource.name override",
                kind: SpanKind::Server,
                span_name: "span-name",
                attrs: vec![kv_str("resource.name", "GET /custom")],
                expected: "GET /custom",
            },
            Case {
                name: "http server with route",
                kind: SpanKind::Server,
                span_name: "span-name",
                attrs: vec![kv_str("http.request.method", "GET"), kv_str("http.route", "/users/:id")],
                expected: "GET /users/:id",
            },
            Case {
                name: "http client without route",
                kind: SpanKind::Client,
                span_name: "span-name",
                attrs: vec![kv_str("http.request.method", "POST")],
                expected: "POST",
            },
            Case {
                name: "http _OTHER method",
                kind: SpanKind::Server,
                span_name: "span-name",
                attrs: vec![kv_str("http.request.method", "_OTHER")],
                expected: "HTTP",
            },
            Case {
                name: "messaging operation with destination",
                kind: SpanKind::Producer,
                span_name: "span-name",
                attrs: vec![
                    kv_str("messaging.operation", "publish"),
                    kv_str("messaging.destination", "topic"),
                ],
                expected: "publish topic",
            },
            Case {
                name: "rpc method with service",
                kind: SpanKind::Client,
                span_name: "span-name",
                attrs: vec![kv_str("rpc.method", "GetObject"), kv_str("rpc.service", "S3")],
                expected: "GetObject S3",
            },
            Case {
                name: "database statement is normalized",
                kind: SpanKind::Client,
                span_name: "span-name",
                attrs: vec![kv_str("db.system", "postgresql"), kv_str("db.statement", "SELECT 1")],
                expected: "select_1",
            },
            Case {
                name: "falls back to span name",
                kind: SpanKind::Internal,
                span_name: "my-span",
                attrs: vec![],
                expected: "my-span",
            },
        ];

        for case in cases {
            assert_eq!(
                resource_name_for(case.kind, case.span_name, &case.attrs),
                case.expected,
                "case: {}",
                case.name
            );
        }
    }

    #[test]
    fn get_otel_span_type_covers_kind_and_db_system_branches() {
        struct Case {
            name: &'static str,
            kind: SpanKind,
            attrs: Vec<KeyValue>,
            expected: &'static str,
        }

        let cases = vec![
            Case {
                name: "span.type override",
                kind: SpanKind::Client,
                attrs: vec![kv_str("span.type", "myspantype")],
                expected: "myspantype",
            },
            Case {
                name: "server is web",
                kind: SpanKind::Server,
                attrs: vec![],
                expected: "web",
            },
            Case {
                name: "client without db is http",
                kind: SpanKind::Client,
                attrs: vec![],
                expected: "http",
            },
            Case {
                name: "client redis",
                kind: SpanKind::Client,
                attrs: vec![kv_str("db.system", "redis")],
                expected: "redis",
            },
            Case {
                name: "client sql-family db",
                kind: SpanKind::Client,
                attrs: vec![kv_str("db.system", "postgresql")],
                expected: "sql",
            },
            Case {
                name: "client unknown db defaults to db",
                kind: SpanKind::Client,
                attrs: vec![kv_str("db.system", "some-unknown-store")],
                expected: "db",
            },
            Case {
                name: "internal is custom",
                kind: SpanKind::Internal,
                attrs: vec![],
                expected: "custom",
            },
        ];

        for case in cases {
            assert_eq!(
                span_type_for(case.kind, &case.attrs),
                case.expected,
                "case: {}",
                case.name
            );
        }
    }

    fn error_from_status(
        code: StatusCode, message: &str, events: &[OtlpSpanEvent],
    ) -> (i32, FastHashMap<MetaString, AttributeValue>) {
        let status = OtlpStatus {
            code: code as i32,
            message: message.to_string(),
        };
        let mut attrs = FastHashMap::default();
        let error = status_to_error(Some(&status), events, &mut attrs);
        (error, attrs)
    }

    fn error_attr<'a>(attrs: &'a FastHashMap<MetaString, AttributeValue>, key: &str) -> Option<&'a str> {
        attrs.get(key).and_then(AttributeValue::as_string).map(|s| s.as_ref())
    }

    #[test]
    fn status_to_error_ignores_non_error_status() {
        let (error, attrs) = error_from_status(StatusCode::Unset, "", &[]);
        assert_eq!(error, 0);
        assert!(attrs.is_empty());
    }

    #[test]
    fn status_to_error_extracts_exception_event_fields() {
        let exception = OtlpSpanEvent {
            name: "exception".to_string(),
            attributes: vec![
                kv_str("exception.message", "boom"),
                kv_str("exception.type", "RuntimeError"),
                kv_str("exception.stacktrace", "at main()"),
            ],
            ..Default::default()
        };

        let (error, attrs) = error_from_status(StatusCode::Error, "", std::slice::from_ref(&exception));
        assert_eq!(error, 1);
        assert_eq!(error_attr(&attrs, "error.msg"), Some("boom"));
        assert_eq!(error_attr(&attrs, "error.type"), Some("RuntimeError"));
        assert_eq!(error_attr(&attrs, "error.stack"), Some("at main()"));
    }

    #[test]
    fn status_to_error_falls_back_to_status_message() {
        // No exception event, so the status message becomes error.msg.
        let (error, attrs) = error_from_status(StatusCode::Error, "db failed", &[]);
        assert_eq!(error, 1);
        assert_eq!(error_attr(&attrs, "error.msg"), Some("db failed"));
    }

    #[test]
    fn status_to_error_sets_error_without_message_when_none_available() {
        let (error, attrs) = error_from_status(StatusCode::Error, "", &[]);
        assert_eq!(error, 1);
        assert_eq!(error_attr(&attrs, "error.msg"), None);
    }

    fn build_dd_span(
        kind: SpanKind, span_attrs: Vec<KeyValue>, status: Option<OtlpStatus>, events: Vec<OtlpSpanEvent>,
    ) -> DdSpan {
        let (interner, mut sb) = extraction_env();
        let span = OtlpSpan {
            name: "span-name".to_string(),
            kind: kind as i32,
            attributes: span_attrs,
            status,
            events,
            ..Default::default()
        };
        let resource = Resource::default();
        otel_span_to_dd_span(
            &span,
            &resource,
            None,
            &conversion(),
            &EMBEDDED_REGISTRY,
            &interner,
            &mut sb,
            None,
        )
    }

    #[test]
    fn otel_span_to_dd_span_assembles_http_server_span() {
        let dd_span = build_dd_span(
            SpanKind::Server,
            vec![
                kv_str(SERVICE_NAME, "checkout"),
                kv_str("http.request.method", "GET"),
                kv_str("http.route", "/pay"),
            ],
            None,
            vec![],
        );

        assert_eq!(dd_span.service(), "checkout");
        assert_eq!(dd_span.name(), "http.server.request");
        assert_eq!(dd_span.resource(), "GET /pay");
        assert_eq!(dd_span.span_type(), "web");
        assert_eq!(dd_span.error(), 0);
    }

    #[test]
    fn otel_span_to_dd_span_assembles_db_client_error_span() {
        let exception = OtlpSpanEvent {
            name: "exception".to_string(),
            attributes: vec![kv_str("exception.message", "deadlock")],
            ..Default::default()
        };
        let dd_span = build_dd_span(
            SpanKind::Client,
            vec![kv_str("db.system", "postgresql"), kv_str("db.statement", "SELECT 1")],
            Some(OtlpStatus {
                code: StatusCode::Error as i32,
                message: String::new(),
            }),
            vec![exception],
        );

        // No service attribute => documented default service name.
        assert_eq!(dd_span.service(), "otlpresourcenoservicename");
        assert_eq!(dd_span.name(), "postgresql.query");
        assert_eq!(dd_span.resource(), "select_1");
        assert_eq!(dd_span.span_type(), "sql");
        assert_eq!(dd_span.error(), 1);
        assert_eq!(
            dd_span
                .attributes
                .get("error.msg")
                .and_then(AttributeValue::as_string)
                .map(|s| s.as_ref()),
            Some("deadlock")
        );
    }

    #[test]
    fn otel_span_to_dd_span_normalizes_span_name() {
        // `datadog.name` values are tag-normalized on the way in, but tag rules differ from span
        // name rules: colons are valid in tags and invalid in names.
        let dd_span = build_dd_span(SpanKind::Internal, vec![kv_str("datadog.name", "fun:ky")], None, vec![]);
        assert_eq!(dd_span.name(), "fun_ky");

        // Already-valid names pass through unchanged.
        let dd_span = build_dd_span(
            SpanKind::Internal,
            vec![kv_str("datadog.name", "good.name")],
            None,
            vec![],
        );
        assert_eq!(dd_span.name(), "good.name");

        // Over-length names are truncated to the name limit before normalization.
        let dd_span = build_dd_span(
            SpanKind::Internal,
            vec![kv_str("datadog.name", "a".repeat(150).as_str())],
            None,
            vec![],
        );
        assert_eq!(dd_span.name(), "a".repeat(100));
    }

    #[test]
    fn otel_span_to_dd_span_truncates_oversized_attributes() {
        let long_key = "k".repeat(MAX_META_KEY_LEN + 50);
        let long_value = "v".repeat(MAX_META_VAL_LEN + 100);
        let dd_span = build_dd_span(
            SpanKind::Internal,
            vec![kv_str(&long_key, "value"), kv_str("small_key", &long_value)],
            None,
            vec![],
        );

        let expected_key = format!("{}...", "k".repeat(MAX_META_KEY_LEN));
        assert!(dd_span.attributes.contains_key(expected_key.as_str()));
        assert!(!dd_span.attributes.contains_key(long_key.as_str()));

        let value = dd_span
            .attributes
            .get("small_key")
            .and_then(AttributeValue::as_string)
            .expect("value attribute should exist");
        assert_eq!(value.len(), MAX_META_VAL_LEN + 3);
        assert!(value.as_ref().ends_with("..."));
    }

    #[test]
    fn otel_span_to_dd_span_preserves_structured_meta_keys() {
        let big_payload = "x".repeat(MAX_META_VAL_LEN + 1000);
        let dd_span = build_dd_span(
            SpanKind::Internal,
            vec![kv_str("_dd.appsec.json", &big_payload)],
            None,
            vec![],
        );

        let value = dd_span
            .attributes
            .get("_dd.appsec.json")
            .and_then(AttributeValue::as_string)
            .expect("structured key should survive");
        assert_eq!(
            value.len(),
            big_payload.len(),
            "structured meta values are never truncated"
        );
    }

    fn kv_value(key: &str, value: OtlpValue) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue { value: Some(value) }),
        }
    }

    fn kv_empty(key: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue { value: None }),
        }
    }

    fn any_value(value: OtlpValue) -> AnyValue {
        AnyValue { value: Some(value) }
    }

    fn array_value(values: Vec<OtlpValue>) -> OtlpValue {
        OtlpValue::ArrayValue(ArrayValue {
            values: values.into_iter().map(any_value).collect(),
        })
    }

    fn map_value(entries: Vec<KeyValue>) -> OtlpValue {
        OtlpValue::KvlistValue(KeyValueList { values: entries })
    }

    fn link_with(trace_id: Vec<u8>, flags: u32, attributes: Vec<KeyValue>) -> OtlpSpanLink {
        OtlpSpanLink {
            trace_id,
            span_id: vec![0, 0, 0, 0, 0, 0, 0, 9],
            attributes,
            flags,
            ..Default::default()
        }
    }

    fn converted_link_attributes(attributes: Vec<KeyValue>) -> Vec<(String, String)> {
        let (interner, _) = extraction_env();
        let links = otlp_links_to_span_links(&[link_with(vec![1; 16], 0, attributes)], &interner);
        let mut pairs: Vec<_> = links[0]
            .attributes()
            .iter()
            .map(|(key, value)| {
                (
                    key.as_ref().to_string(),
                    value
                        .as_string()
                        .expect("link attributes are strings")
                        .as_ref()
                        .to_string(),
                )
            })
            .collect();
        pairs.sort();
        pairs
    }

    fn converted_event_attributes(attributes: Vec<KeyValue>) -> Vec<(String, AttributeValue)> {
        let (interner, _) = extraction_env();
        let event = OtlpSpanEvent {
            attributes,
            ..Default::default()
        };
        let events = otlp_events_to_span_events(&[event], &interner);
        let mut pairs: Vec<_> = events[0]
            .attributes()
            .iter()
            .map(|(key, value)| (key.as_ref().to_string(), value.clone()))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs
    }

    fn string_value(value: &str) -> AttributeValue {
        AttributeValue::String(MetaString::from(value))
    }

    fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn span_links_split_the_trace_id_and_keep_the_span_id_and_tracestate() {
        let trace_id: Vec<u8> = (1..=16).collect();
        let mut link = link_with(trace_id, 0, vec![]);
        link.trace_state = "vendor=value".to_string();
        let (interner, _) = extraction_env();

        let links = otlp_links_to_span_links(&[link], &interner);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].trace_id_high(), 0x0102_0304_0506_0708);
        assert_eq!(links[0].trace_id(), 0x090a_0b0c_0d0e_0f10);
        assert_eq!(links[0].span_id(), 9);
        assert_eq!(links[0].tracestate(), "vendor=value");
        assert_eq!(links[0].flags(), 0);
        assert!(links[0].attributes().is_empty());
    }

    #[test]
    fn span_link_flags_keep_the_trace_flags_and_mark_them_present() {
        let (interner, _) = extraction_env();
        // (OTLP flags, Datadog flags): unset stays unset; any set flags keep their low 8 bits and gain bit 31, and
        // the `is_remote` bits (8 and 9) are dropped.
        for (otlp, datadog) in [
            (0, 0),
            (0x01, 0x8000_0001),
            (0x300 | 0x01, 0x8000_0001),
            (0x100, 0x8000_0000),
            (0xff, 0x8000_00ff),
        ] {
            let links = otlp_links_to_span_links(&[link_with(vec![1; 16], otlp, vec![])], &interner);
            assert_eq!(links[0].flags(), datadog, "OTLP flags {otlp:#x}");
        }
    }

    #[test]
    fn span_link_attributes_are_strings_and_flatten_arrays_and_maps() {
        let attributes = converted_link_attributes(vec![
            kv_str("s", "text"),
            kv_value("b", OtlpValue::BoolValue(true)),
            kv_value("i", OtlpValue::IntValue(-42)),
            kv_value("d", OtlpValue::DoubleValue(1.5)),
            kv_value("bytes", OtlpValue::BytesValue(b"hi".to_vec())),
            kv_value(
                "arr",
                array_value(vec![
                    OtlpValue::StringValue("a".to_string()),
                    OtlpValue::IntValue(2),
                    array_value(vec![OtlpValue::BoolValue(false)]),
                ]),
            ),
            kv_value(
                "m",
                map_value(vec![kv_str("x", "1"), kv_value("y", map_value(vec![kv_str("z", "2")]))]),
            ),
            kv_empty("empty"),
        ]);

        assert_eq!(
            attributes,
            pairs(&[
                ("arr.0", "a"),
                ("arr.1", "2"),
                ("arr.2.0", "false"),
                ("b", "true"),
                ("bytes", "aGk="),
                ("d", "1.5"),
                ("empty", ""),
                ("i", "-42"),
                ("m.x", "1"),
                ("m.y.z", "2"),
                ("s", "text"),
            ])
        );
    }

    #[test]
    fn float64_as_string_matches_pcommon() {
        for (value, expected) in [
            (0.0, "0"),
            (-0.0, "-0"),
            (1.0, "1"),
            (1.5, "1.5"),
            (-2.25, "-2.25"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1.5e-7, "1.5e-7"),
            (1e-10, "1e-10"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (1.5e300, "1.5e+300"),
            (-1e21, "-1e+21"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ] {
            assert_eq!(float64_as_string(value), expected, "{value:?}");
        }
    }

    #[test]
    fn span_links_normalize_link_name() {
        let (interner, _) = extraction_env();
        for (raw, normalized) in [("bad link name!", "bad_link_name"), ("中文link", "link")] {
            let links = otlp_links_to_span_links(
                &[link_with(
                    vec![1; 16],
                    0,
                    vec![kv_str("link.name", raw), kv_str("other.attribute", "untouched")],
                )],
                &interner,
            );

            let attributes = links[0].attributes();
            assert_eq!(attributes.get("link.name"), Some(&string_value(normalized)), "{raw}");
            assert_eq!(attributes.get("other.attribute"), Some(&string_value("untouched")));
        }
    }

    #[test]
    fn span_events_keep_name_time_and_typed_attributes() {
        let (interner, _) = extraction_env();
        let event = OtlpSpanEvent {
            time_unix_nano: 1_700_000_000_123,
            name: "exception".to_string(),
            attributes: vec![
                kv_str("s", "text"),
                kv_value("b", OtlpValue::BoolValue(true)),
                kv_value("i", OtlpValue::IntValue(7)),
                kv_value("d", OtlpValue::DoubleValue(2.5)),
                kv_value("bytes", OtlpValue::BytesValue(b"hi".to_vec())),
                kv_str("blank", ""),
            ],
            // The Datadog model has no place for the count of dropped attributes.
            dropped_attributes_count: 3,
        };

        let events = otlp_events_to_span_events(&[event], &interner);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].time_unix_nano(), 1_700_000_000_123);
        assert_eq!(events[0].name(), "exception");
        let mut attributes: Vec<_> = events[0]
            .attributes()
            .iter()
            .map(|(k, v)| (k.as_ref().to_string(), v.clone()))
            .collect();
        attributes.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            attributes,
            vec![
                ("b".to_string(), AttributeValue::Bool(true)),
                ("blank".to_string(), string_value("")),
                ("bytes".to_string(), string_value("aGk=")),
                ("d".to_string(), AttributeValue::Float(2.5)),
                ("i".to_string(), AttributeValue::Int(7)),
                ("s".to_string(), string_value("text")),
            ]
        );
    }

    #[test]
    fn span_event_arrays_stay_typed_only_when_every_value_has_one_scalar_type() {
        let attributes = converted_event_attributes(vec![
            kv_value(
                "strs",
                array_value(vec![
                    OtlpValue::StringValue("a".to_string()),
                    OtlpValue::StringValue("b".to_string()),
                ]),
            ),
            kv_value(
                "ints",
                array_value(vec![OtlpValue::IntValue(1), OtlpValue::IntValue(2)]),
            ),
            kv_value("doubles", array_value(vec![OtlpValue::DoubleValue(0.5)])),
            kv_value(
                "bools",
                array_value(vec![OtlpValue::BoolValue(true), OtlpValue::BoolValue(false)]),
            ),
            kv_value("none", array_value(vec![])),
            // Mixed types, arrays of arrays, and bytes can't be a typed array, so they flatten. Each flattened
            // element is then converted on its own, so an inner array that is typed stays typed.
            kv_value(
                "mixed",
                array_value(vec![OtlpValue::IntValue(1), OtlpValue::StringValue("a".to_string())]),
            ),
            kv_value("nested", array_value(vec![array_value(vec![OtlpValue::IntValue(5)])])),
            kv_value(
                "deep",
                array_value(vec![array_value(vec![
                    OtlpValue::IntValue(1),
                    OtlpValue::StringValue("a".to_string()),
                ])]),
            ),
            kv_value("blobs", array_value(vec![OtlpValue::BytesValue(b"hi".to_vec())])),
        ]);

        assert_eq!(
            attributes,
            vec![
                ("blobs.0".to_string(), string_value("aGk=")),
                (
                    "bools".to_string(),
                    AttributeValue::Array(vec![AttributeValue::Bool(true), AttributeValue::Bool(false)])
                ),
                ("deep.0.0".to_string(), AttributeValue::Int(1)),
                ("deep.0.1".to_string(), string_value("a")),
                (
                    "doubles".to_string(),
                    AttributeValue::Array(vec![AttributeValue::Float(0.5)])
                ),
                (
                    "ints".to_string(),
                    AttributeValue::Array(vec![AttributeValue::Int(1), AttributeValue::Int(2)])
                ),
                ("mixed.0".to_string(), AttributeValue::Int(1)),
                ("mixed.1".to_string(), string_value("a")),
                (
                    "nested.0".to_string(),
                    AttributeValue::Array(vec![AttributeValue::Int(5)])
                ),
                ("none".to_string(), AttributeValue::Array(vec![])),
                (
                    "strs".to_string(),
                    AttributeValue::Array(vec![string_value("a"), string_value("b")])
                ),
            ]
        );
    }

    #[test]
    fn span_event_maps_flatten_and_empty_values_are_dropped() {
        let attributes = converted_event_attributes(vec![
            kv_value(
                "m",
                map_value(vec![
                    kv_value("a", OtlpValue::IntValue(1)),
                    kv_value("b", map_value(vec![kv_str("c", "x")])),
                    kv_empty("gone"),
                ]),
            ),
            kv_value("empty_map", map_value(vec![])),
            kv_value("only_empty", array_value(vec![])),
            kv_empty("empty"),
        ]);

        assert_eq!(
            attributes,
            vec![
                ("m.a".to_string(), AttributeValue::Int(1)),
                ("m.b.c".to_string(), string_value("x")),
                // An empty array is a real value, unlike an empty map.
                ("only_empty".to_string(), AttributeValue::Array(vec![])),
            ]
        );
    }

    fn span_with_links_and_events() -> OtlpSpan {
        OtlpSpan {
            name: "span-name".to_string(),
            kind: SpanKind::Server as i32,
            links: vec![link_with(vec![1; 16], 0, vec![kv_str("k", "v")])],
            events: vec![OtlpSpanEvent {
                name: "exception".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn links_and_events_become_span_links_and_events_without_tags_by_default() {
        let (interner, _) = extraction_env();
        let mut attributes = FastHashMap::default();

        let mut dd_span = DdSpan::default();
        set_links_and_events(
            &mut dd_span,
            &mut attributes,
            &span_with_links_and_events(),
            false,
            &interner,
        );

        assert_eq!(dd_span.span_links().len(), 1);
        assert_eq!(dd_span.span_events().len(), 1);
        assert!(!attributes.contains_key("_dd.span_links"));
        assert!(!attributes.contains_key("events"));
    }

    #[test]
    fn links_and_events_go_in_tags_without_span_links_and_events_when_legacy() {
        let (interner, _) = extraction_env();
        let mut attributes = FastHashMap::default();

        let mut dd_span = DdSpan::default();
        set_links_and_events(
            &mut dd_span,
            &mut attributes,
            &span_with_links_and_events(),
            true,
            &interner,
        );

        assert!(dd_span.span_links().is_empty());
        assert!(dd_span.span_events().is_empty());
        assert!(attributes.contains_key("_dd.span_links"));
        assert!(attributes.contains_key("events"));
    }

    #[test]
    fn legacy_tags_are_used_exactly_for_agents_before_7_87() {
        // `DD_AGENT_VERSION` is the build input the gate reads. An unknown version means the newest behavior, and so
        // does a development build such as `nightly`.
        let version =
            option_env!("DD_AGENT_VERSION").and_then(datadog_agent_commons::agent_version::AgentVersion::parse);
        let expected = version.is_some_and(|version| !version.meets(7, 87, 0));

        assert_eq!(LEGACY_TAG_LINKS_AND_EVENTS, expected);
    }

    #[test]
    fn otel_span_to_dd_span_follows_the_legacy_tags_gate_and_always_flags_exceptions() {
        let (interner, mut sb) = extraction_env();
        let dd_span = otel_span_to_dd_span(
            &span_with_links_and_events(),
            &Resource::default(),
            None,
            &conversion(),
            &EMBEDDED_REGISTRY,
            &interner,
            &mut sb,
            None,
        );

        // Whichever path the Agent version selects, the other one is empty.
        assert_eq!(!dd_span.span_links().is_empty(), !LEGACY_TAG_LINKS_AND_EVENTS);
        assert_eq!(!dd_span.span_events().is_empty(), !LEGACY_TAG_LINKS_AND_EVENTS);
        assert_eq!(
            dd_span.attributes.contains_key("_dd.span_links"),
            LEGACY_TAG_LINKS_AND_EVENTS
        );
        assert_eq!(dd_span.attributes.contains_key("events"), LEGACY_TAG_LINKS_AND_EVENTS);
        // The exception flag is a tag on both paths.
        assert_eq!(
            dd_span
                .attributes
                .get("_dd.span_events.has_exception")
                .and_then(AttributeValue::as_string)
                .map(AsRef::as_ref),
            Some("true")
        );
    }

    #[test]
    fn tag_span_links_normalize_link_name() {
        let link = OtlpSpanLink {
            trace_id: vec![1; 16],
            span_id: vec![2; 8],
            attributes: vec![
                kv_str("link.name", "bad link name!"),
                kv_str("other.attribute", "untouched"),
            ],
            ..Default::default()
        };
        let span = OtlpSpan {
            name: "span-name".to_string(),
            kind: SpanKind::Server as i32,
            links: vec![link],
            ..Default::default()
        };
        let (interner, _) = extraction_env();
        let mut attributes = FastHashMap::default();
        set_links_and_events(&mut DdSpan::default(), &mut attributes, &span, true, &interner);

        let links_json = attributes
            .get("_dd.span_links")
            .and_then(AttributeValue::as_string)
            .map(|s| s.as_ref().to_owned())
            .expect("span links should be marshaled");
        assert!(
            links_json.contains("bad_link_name"),
            "link name is normalized: {links_json}"
        );
        assert!(
            !links_json.contains("bad link name"),
            "raw link name is gone: {links_json}"
        );
        assert!(links_json.contains("untouched"), "other link attributes are untouched");
    }

    #[test]
    fn otel_span_to_dd_span_repairs_unicode_leading_names() {
        // A datadog.name starting with a multi-byte character is repaired, not a panic.
        let dd_span = build_dd_span(
            SpanKind::Internal,
            vec![kv_str("datadog.name", "中文query")],
            None,
            vec![],
        );
        assert_eq!(dd_span.name(), "query");
    }

    #[test]
    fn tag_span_links_repair_unicode_leading_link_names() {
        let link = OtlpSpanLink {
            trace_id: vec![1; 16],
            span_id: vec![2; 8],
            attributes: vec![kv_str("link.name", "中文link")],
            ..Default::default()
        };
        let span = OtlpSpan {
            name: "span-name".to_string(),
            kind: SpanKind::Server as i32,
            links: vec![link],
            ..Default::default()
        };
        let (interner, _) = extraction_env();
        let mut attributes = FastHashMap::default();
        set_links_and_events(&mut DdSpan::default(), &mut attributes, &span, true, &interner);

        let links_json = attributes
            .get("_dd.span_links")
            .and_then(AttributeValue::as_string)
            .map(|s| s.as_ref().to_owned())
            .expect("span links should be marshaled");
        assert!(links_json.contains("\"link\""), "repaired link name: {links_json}");
        assert!(
            !links_json.contains("中文"),
            "multi-byte characters are repaired away: {links_json}"
        );
    }

    #[test]
    fn otel_span_to_dd_span_truncates_resource_to_configured_limit() {
        let long_route = "/r".repeat(4000);
        let span_attrs = vec![kv_str("http.request.method", "GET"), kv_str("http.route", &long_route)];

        // Default cap: the resolved resource (method + route) is truncated to 5000 bytes.
        let dd_span = build_dd_span(SpanKind::Server, span_attrs.clone(), None, vec![]);
        assert_eq!(dd_span.resource().len(), 5000);

        // The configured cap overrides the default.
        let (interner, mut sb) = extraction_env();
        let span = OtlpSpan {
            name: "span-name".to_string(),
            kind: SpanKind::Server as i32,
            attributes: span_attrs,
            ..Default::default()
        };
        let dd_span = otel_span_to_dd_span(
            &span,
            &Resource::default(),
            None,
            &SpanConversion {
                max_resource_len: 100,
                ..conversion()
            },
            &EMBEDDED_REGISTRY,
            &interner,
            &mut sb,
            None,
        );
        assert_eq!(dd_span.resource().len(), 100);
        assert!(dd_span.resource().starts_with("GET /r"));
    }

    /// `http.status_code` must be emitted as a numeric (float) attribute, mirroring the Agent's
    /// `Metrics[traceutil.TagStatusCode]`, so the encoded span carries a `DoubleValue` rather than
    /// a string reference. See https://github.com/DataDog/saluki/issues/2379.
    #[test]
    fn otel_span_to_dd_span_emits_http_status_code_as_numeric() {
        let dd_span = build_dd_span(
            SpanKind::Server,
            vec![
                kv_str(SERVICE_NAME, "checkout"),
                kv_str("http.request.method", "GET"),
                kv_int(SEMCONV_HTTP_RESPONSE_STATUS_CODE, 200),
            ],
            None,
            vec![],
        );

        let status_code = dd_span
            .attributes
            .get(HTTP_STATUS_CODE_KEY)
            .expect("http.status_code should be present");
        assert_eq!(status_code.as_num(), Some(200.0), "http.status_code must be numeric");
        assert!(
            status_code.as_string().is_none(),
            "http.status_code must not be a string reference"
        );
    }

    /// A negative resolved status code produces no `http.status_code` attribute, matching the
    /// Agent's `code >= 0` guard.
    #[test]
    fn otel_to_dd_span_minimal_skips_negative_http_status_code() {
        let (interner, mut sb) = extraction_env();
        let span = OtlpSpan {
            name: "span-name".to_string(),
            attributes: vec![kv_int(SEMCONV_HTTP_STATUS_CODE, -1)],
            ..Default::default()
        };
        let resource = Resource::default();
        let (_dd_span, attrs) = otel_to_dd_span_minimal(
            &span,
            &resource,
            None,
            &conversion(),
            &EMBEDDED_REGISTRY,
            &interner,
            &mut sb,
        );
        assert!(
            !attrs.contains_key(HTTP_STATUS_CODE_KEY),
            "negative status code must not produce an http.status_code attribute"
        );
    }

    fn kv_double(key: &str, value: f64) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(OtlpValue::DoubleValue(value)),
            }),
        }
    }

    /// Converts a span with the given attributes and tracestate through the full conversion.
    fn dd_span_with_tracestate(span_attrs: Vec<KeyValue>, trace_state: &str) -> DdSpan {
        let (interner, mut sb) = extraction_env();
        let span = OtlpSpan {
            name: "span-name".to_string(),
            attributes: span_attrs,
            trace_state: trace_state.to_string(),
            ..Default::default()
        };
        otel_span_to_dd_span(
            &span,
            &Resource::default(),
            None,
            &conversion(),
            &EMBEDDED_REGISTRY,
            &interner,
            &mut sb,
            None,
        )
    }

    #[test]
    fn sample_rate_is_set_from_tracestate_threshold() {
        let dd_span = dd_span_with_tracestate(vec![], "ot=th:8");
        let rate = dd_span
            .attributes
            .get(SAMPLE_RATE_KEY)
            .expect("_sample_rate should be set");
        assert!(matches!(rate, AttributeValue::Float(_)), "_sample_rate must be a float");
        assert_eq!(rate.as_num(), Some(0.5));
        assert_eq!(
            dd_span
                .attributes
                .get(W3C_TRACESTATE_META_KEY)
                .and_then(AttributeValue::as_string)
                .map(|s| s.as_ref()),
            Some("ot=th:8"),
            "the raw tracestate is still carried on the span"
        );
    }

    #[test]
    fn sample_rate_is_set_from_tracestate_power_of_two() {
        let dd_span = dd_span_with_tracestate(vec![], "ot=p:4;r:4");
        assert_eq!(
            dd_span.attributes.get(SAMPLE_RATE_KEY).and_then(AttributeValue::as_num),
            Some(0.0625)
        );
    }

    #[test]
    fn sample_rate_is_not_set_without_a_usable_tracestate() {
        for trace_state in ["", "zz=vendor", "ot=rv:abcdefabcdefab", "ot=th:zz", "ot=p:63"] {
            let dd_span = dd_span_with_tracestate(vec![], trace_state);
            assert!(
                !dd_span.attributes.contains_key(SAMPLE_RATE_KEY),
                "tracestate {trace_state:?} must not set _sample_rate"
            );
        }
    }

    #[test]
    fn explicit_sample_rate_attribute_wins_over_tracestate() {
        let dd_span = dd_span_with_tracestate(vec![kv_double(SAMPLE_RATE_KEY, 0.25)], "ot=th:8");
        assert_eq!(
            dd_span.attributes.get(SAMPLE_RATE_KEY).and_then(AttributeValue::as_num),
            Some(0.25)
        );

        let dd_span = dd_span_with_tracestate(vec![kv_int(SAMPLE_RATE_KEY, 1)], "ot=th:8");
        let rate = dd_span
            .attributes
            .get(SAMPLE_RATE_KEY)
            .expect("_sample_rate should be set");
        assert!(matches!(rate, AttributeValue::Float(_)), "_sample_rate must be a float");
        assert_eq!(rate.as_num(), Some(1.0));
    }

    #[test]
    fn explicit_sample_rate_attribute_is_kept_without_tracestate() {
        let dd_span = dd_span_with_tracestate(vec![kv_double(SAMPLE_RATE_KEY, 0.25)], "");
        assert_eq!(
            dd_span.attributes.get(SAMPLE_RATE_KEY).and_then(AttributeValue::as_num),
            Some(0.25)
        );
    }

    #[test]
    fn non_numeric_sample_rate_attribute_does_not_block_tracestate() {
        let dd_span = dd_span_with_tracestate(vec![kv_str(SAMPLE_RATE_KEY, "0.25")], "ot=th:8");
        assert_eq!(
            dd_span.attributes.get(SAMPLE_RATE_KEY).and_then(AttributeValue::as_num),
            Some(0.5)
        );
    }

    /// The error-message fallback in `status_to_error` still derives `error.msg` from the HTTP
    /// status code now that it is stored numerically (regression guard for lading-style string
    /// status codes on error spans).
    #[test]
    fn status_to_error_builds_error_msg_from_numeric_http_status_code() {
        let dd_span = build_dd_span(
            SpanKind::Server,
            vec![
                kv_str(SERVICE_NAME, "checkout"),
                kv_str("http.request.method", "GET"),
                kv_str(SEMCONV_HTTP_RESPONSE_STATUS_CODE, "500"),
            ],
            Some(OtlpStatus {
                code: StatusCode::Error as i32,
                message: String::new(),
            }),
            vec![],
        );

        assert_eq!(dd_span.error(), 1);
        assert_eq!(
            dd_span
                .attributes
                .get("error.msg")
                .and_then(AttributeValue::as_string)
                .map(|s| s.as_ref()),
            Some("500 Internal Server Error")
        );
    }

    /// An end time before the start time must yield a zero duration, not a wrapped `u64` near
    /// `u64::MAX`. Mirrors `validateAndFixDurationV1` (`pkg/trace/agent/normalizer.go`).
    /// https://github.com/DataDog/saluki/issues/2382
    #[test]
    fn otel_to_dd_span_zeroes_duration_when_end_time_precedes_start_time() {
        let (interner, mut sb) = extraction_env();
        let span = OtlpSpan {
            name: "span-name".to_string(),
            start_time_unix_nano: YEAR_2000_NANOSEC_TS + 1_000_000_000,
            end_time_unix_nano: YEAR_2000_NANOSEC_TS,
            ..Default::default()
        };
        let resource = Resource::default();
        let dd_span = otel_span_to_dd_span(
            &span,
            &resource,
            None,
            &conversion(),
            &EMBEDDED_REGISTRY,
            &interner,
            &mut sb,
            None,
        );

        assert_eq!(dd_span.duration(), 0);
        // The start is above the year-2000 floor, so it must survive untouched.
        assert_eq!(dd_span.start(), YEAR_2000_NANOSEC_TS + 1_000_000_000);
    }

    /// A start time before the year-2000 floor is replaced with the receive time minus the duration.
    /// Mirrors `validateAndFixStartTimeV1` (`pkg/trace/agent/normalizer.go`).
    /// https://github.com/DataDog/saluki/issues/2382
    #[test]
    fn otel_to_dd_span_rewrites_pre_year_2000_start_time() {
        let (interner, mut sb) = extraction_env();
        let duration = 1_000_000_000;
        let span = OtlpSpan {
            name: "span-name".to_string(),
            start_time_unix_nano: 1,
            end_time_unix_nano: 1 + duration,
            ..Default::default()
        };
        let resource = Resource::default();
        let dd_span = otel_span_to_dd_span(
            &span,
            &resource,
            None,
            &conversion(),
            &EMBEDDED_REGISTRY,
            &interner,
            &mut sb,
            None,
        );

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let start = dd_span.start();
        assert!(
            start >= YEAR_2000_NANOSEC_TS,
            "rewritten start must clear the year-2000 floor"
        );
        assert!(
            start <= now && now - start < 5_000_000_000,
            "start: {start}, now: {now}"
        );
        assert_eq!(dd_span.duration(), duration, "a valid duration must survive");
    }

    /// A parent ID equal to the span ID is cleared to zero, mirroring `normalizeV1`
    /// (`pkg/trace/agent/normalizer.go`). https://github.com/DataDog/saluki/issues/2382
    #[test]
    fn otel_to_dd_span_clears_self_referencing_parent_id() {
        let (interner, mut sb) = extraction_env();
        let span = OtlpSpan {
            name: "span-name".to_string(),
            span_id: [1u8; 8].to_vec(),
            parent_span_id: [1u8; 8].to_vec(),
            ..Default::default()
        };
        let resource = Resource::default();
        let dd_span = otel_span_to_dd_span(
            &span,
            &resource,
            None,
            &conversion(),
            &EMBEDDED_REGISTRY,
            &interner,
            &mut sb,
            None,
        );

        assert_eq!(dd_span.parent_id(), 0);
        assert_eq!(dd_span.span_id(), u64::from_be_bytes([1u8; 8]));
    }

    /// A distinct parent ID survives the self-referencing check.
    #[test]
    fn otel_to_dd_span_keeps_distinct_parent_id() {
        let (interner, mut sb) = extraction_env();
        let span = OtlpSpan {
            name: "span-name".to_string(),
            span_id: [1u8; 8].to_vec(),
            parent_span_id: [2u8; 8].to_vec(),
            ..Default::default()
        };
        let resource = Resource::default();
        let dd_span = otel_span_to_dd_span(
            &span,
            &resource,
            None,
            &conversion(),
            &EMBEDDED_REGISTRY,
            &interner,
            &mut sb,
            None,
        );

        assert_eq!(dd_span.parent_id(), u64::from_be_bytes([2u8; 8]));
    }

    /// A `span.type` longer than 100 bytes is truncated at a UTF-8 boundary, mirroring
    /// `validateAndFixType` (`pkg/trace/agent/normalizer.go`).
    /// https://github.com/DataDog/saluki/issues/2382
    #[test]
    fn otel_to_dd_span_truncates_long_span_type() {
        let (interner, mut sb) = extraction_env();
        // 150 ASCII bytes, well over the limit, and a multibyte character straddling the boundary:
        // 98 bytes of 'a' plus two 4-byte crabs cut back to the last valid boundary at 98 bytes.
        let long_ascii = "t".repeat(150);
        let long_multibyte = "a".repeat(98) + "🦀🦀";
        for (label, span_type, expected_len) in [("ascii", long_ascii, 100), ("multibyte", long_multibyte, 98)] {
            let span = OtlpSpan {
                name: "span-name".to_string(),
                attributes: vec![kv_str(KEY_DATADOG_TYPE, span_type.as_str())],
                ..Default::default()
            };
            let resource = Resource::default();
            let dd_span = otel_span_to_dd_span(
                &span,
                &resource,
                None,
                &conversion(),
                &EMBEDDED_REGISTRY,
                &interner,
                &mut sb,
                None,
            );

            assert_eq!(
                dd_span.span_type().len(),
                expected_len,
                "{label}: span type must be truncated at a UTF-8 boundary"
            );
            // The right length from the wrong bytes would still be wrong: the result must be a prefix
            // of the input.
            assert!(
                span_type.as_bytes().starts_with(dd_span.span_type().as_bytes()),
                "{label}: truncated span type must be a prefix of the input"
            );
        }
    }

    /// A `span.type` at or below the limit is left untouched.
    #[test]
    fn otel_to_dd_span_keeps_span_type_within_limit() {
        let (interner, mut sb) = extraction_env();
        let span_type = "x".repeat(MAX_TYPE_LEN);
        let span = OtlpSpan {
            name: "span-name".to_string(),
            attributes: vec![kv_str(KEY_DATADOG_TYPE, span_type.as_str())],
            ..Default::default()
        };
        let resource = Resource::default();
        let dd_span = otel_span_to_dd_span(
            &span,
            &resource,
            None,
            &conversion(),
            &EMBEDDED_REGISTRY,
            &interner,
            &mut sb,
            None,
        );

        assert_eq!(dd_span.span_type(), span_type);
    }

    // ===============================================================================================
    // V1 naming and the V1 receive path.
    //
    // Cases marked as ported come from `testOTelSpanToDDSpan`, `testOTLPConvertSpan`, and `testOTLPNameRemapping` in
    // https://github.com/DataDog/datadog-agent/blob/00b7aa9aa8/pkg/trace/api/otlp_test.go
    // Those tests check spans before normalization, so the expected names here are normalized.

    /// The trace and span IDs of the ported fixtures.
    const FIXTURE_TRACE_ID: [u8; 16] = [
        0x72, 0xdf, 0x52, 0x0a, 0xf2, 0xbd, 0xe7, 0xa5, 0x24, 0x00, 0x31, 0xea, 0xd7, 0x50, 0xe5, 0xf3,
    ];
    const FIXTURE_SPAN_ID: [u8; 8] = [0x24, 0x00, 0x31, 0xea, 0xd7, 0x50, 0xe5, 0xf3];
    const FIXTURE_START: u64 = 1_700_000_000_000_000_000;

    fn fixture_span(name: &str, kind: SpanKind, attributes: Vec<KeyValue>) -> OtlpSpan {
        OtlpSpan {
            trace_id: FIXTURE_TRACE_ID.to_vec(),
            span_id: FIXTURE_SPAN_ID.to_vec(),
            name: name.to_string(),
            kind: kind as i32,
            start_time_unix_nano: FIXTURE_START,
            end_time_unix_nano: FIXTURE_START + 200_000_000,
            attributes,
            ..Default::default()
        }
    }

    fn scope(name: &str, version: &str) -> OtlpInstrumentationScope {
        OtlpInstrumentationScope {
            name: name.to_string(),
            version: version.to_string(),
            ..Default::default()
        }
    }

    fn resource(attributes: Vec<KeyValue>) -> Resource {
        Resource {
            attributes,
            ..Default::default()
        }
    }

    /// The conversion with V1 naming, selected by `disable_operation_and_resource_name_logic_v2`.
    fn v1_naming(span_name_as_resource_name: bool, remappings: &[(&str, &str)]) -> SpanConversion {
        let remappings = remappings
            .iter()
            .map(|(from, to)| (from.to_string(), to.to_string()))
            .collect();
        SpanConversion {
            naming: SpanNaming::select(span_name_as_resource_name, &remappings, true),
            ..conversion()
        }
    }

    fn convert_v1(
        span: &OtlpSpan, resource: &Resource, scope: Option<&OtlpInstrumentationScope>, conversion: &SpanConversion,
    ) -> DdSpan {
        let (interner, mut sb) = extraction_env();
        otel_span_to_dd_span_v1(span, resource, scope, conversion, &interner, &mut sb, None)
    }

    fn convert_v2(
        span: &OtlpSpan, resource: &Resource, scope: Option<&OtlpInstrumentationScope>, conversion: &SpanConversion,
    ) -> DdSpan {
        let (interner, mut sb) = extraction_env();
        otel_span_to_dd_span(
            span,
            resource,
            scope,
            conversion,
            &EMBEDDED_REGISTRY,
            &interner,
            &mut sb,
            None,
        )
    }

    fn string(value: &str) -> AttributeValue {
        AttributeValue::String(MetaString::from(value))
    }

    #[track_caller]
    fn assert_attributes(dd_span: &DdSpan, expected: &[(&str, AttributeValue)]) {
        let expected: FastHashMap<MetaString, AttributeValue> = expected
            .iter()
            .map(|(key, value)| (MetaString::from(*key), value.clone()))
            .collect();
        assert_eq!(dd_span.attributes, expected);
    }

    #[test]
    fn span_naming_selects_v1_when_any_v1_setting_is_set() {
        let remappings = HashMap::from([("old".to_string(), "new".to_string())]);
        let none = HashMap::new();

        assert_eq!(SpanNaming::select(false, &none, false), SpanNaming::V2);
        for (span_name_as_resource_name, remappings, disable_v2) in
            [(true, &none, false), (false, &remappings, false), (false, &none, true)]
        {
            assert_eq!(
                SpanNaming::select(span_name_as_resource_name, remappings, disable_v2),
                SpanNaming::V1 {
                    span_name_as_resource_name,
                    span_name_remappings: remappings
                        .iter()
                        .map(|(from, to)| (from.clone(), MetaString::from(to.as_str())))
                        .collect(),
                },
                "{span_name_as_resource_name} {remappings:?} {disable_v2}"
            );
        }
    }

    #[test]
    fn v1_naming_on_the_v2_receive_path_matches_the_fixtures() {
        struct Case {
            name: &'static str,
            library: &'static str,
            kind: SpanKind,
            span_name: &'static str,
            resource_attrs: Vec<KeyValue>,
            span_attrs: Vec<KeyValue>,
            conversion: SpanConversion,
            operation_name: &'static str,
            resource_name: &'static str,
        }

        let cases = [
            // Ported from `testOTelSpanToDDSpan`.
            Case {
                name: "library and kind, RPC resource",
                library: "go.opentelemetry.io/contrib/instrumentation/google.golang.org/grpc/otelgrpc",
                kind: SpanKind::Server,
                span_name: "opentelemetry.proto.collector.trace.v1.TraceService/Export",
                resource_attrs: vec![],
                span_attrs: vec![
                    kv_str("rpc.method", "Export"),
                    kv_str("rpc.service", "opentelemetry.proto.collector.trace.v1.TraceService"),
                ],
                conversion: v1_naming(false, &[]),
                operation_name: "go.opentelemetry.io_contrib_instrumentation_google.golang.org_grpc_otelgrpc.server",
                resource_name: "Export opentelemetry.proto.collector.trace.v1.TraceService",
            },
            Case {
                name: "resource attributes take precedence",
                library: "ddtracer",
                kind: SpanKind::Server,
                span_name: "/path",
                resource_attrs: vec![kv_str("operation.name", "res-op"), kv_str("resource.name", "res-res")],
                span_attrs: vec![kv_str("operation.name", "span-op"), kv_str("resource.name", "span-res")],
                conversion: v1_naming(false, &[]),
                operation_name: "res_op",
                resource_name: "res-res",
            },
            Case {
                name: "operation.name keeps its case",
                library: "ddtracer",
                kind: SpanKind::Unspecified,
                span_name: "/path",
                resource_attrs: vec![],
                span_attrs: vec![kv_str("operation.name", "READ"), kv_str("resource.name", "/path")],
                conversion: v1_naming(false, &[]),
                operation_name: "READ",
                resource_name: "/path",
            },
            Case {
                name: "HTTP method and route",
                library: "ddtracer",
                kind: SpanKind::Server,
                span_name: "/path",
                resource_attrs: vec![],
                span_attrs: vec![kv_str("http.method", "GET"), kv_str("http.route", "/path")],
                conversion: v1_naming(false, &[]),
                operation_name: "ddtracer.server",
                resource_name: "GET /path",
            },
            // Ported from `testOTLPNameRemapping`.
            Case {
                name: "remapped library and kind",
                library: "libname",
                kind: SpanKind::Unspecified,
                span_name: "asd",
                resource_attrs: vec![],
                span_attrs: vec![],
                conversion: v1_naming(false, &[("libname.unspecified", "new")]),
                operation_name: "new",
                resource_name: "asd",
            },
            // The remaining cases cover the other branches of V1 naming.
            Case {
                name: "remapped operation.name",
                library: "ddtracer",
                kind: SpanKind::Internal,
                span_name: "span",
                resource_attrs: vec![],
                span_attrs: vec![kv_str("operation.name", "legacy.op")],
                conversion: v1_naming(false, &[("legacy.op", "modern.op")]),
                operation_name: "modern.op",
                resource_name: "span",
            },
            Case {
                name: "span name as operation name",
                library: "ddtracer",
                kind: SpanKind::Internal,
                span_name: "checkout",
                resource_attrs: vec![],
                span_attrs: vec![],
                conversion: v1_naming(true, &[]),
                operation_name: "checkout",
                resource_name: "checkout",
            },
            Case {
                name: "no library",
                library: "",
                kind: SpanKind::Consumer,
                span_name: "span",
                resource_attrs: vec![],
                span_attrs: vec![
                    kv_str("messaging.operation", "process"),
                    kv_str("messaging.destination.name", "orders"),
                ],
                conversion: v1_naming(false, &[]),
                operation_name: "opentelemetry.consumer",
                resource_name: "process orders",
            },
            Case {
                name: "HTTP route on a client span",
                library: "ddtracer",
                kind: SpanKind::Client,
                span_name: "span",
                resource_attrs: vec![],
                span_attrs: vec![kv_str("http.request.method", "GET"), kv_str("http.route", "/users")],
                conversion: v1_naming(false, &[]),
                operation_name: "ddtracer.client",
                resource_name: "GET /users",
            },
            Case {
                name: "an empty resource attribute hides later keys of the resource only",
                library: "ddtracer",
                kind: SpanKind::Internal,
                span_name: "span",
                resource_attrs: vec![kv_str("http.request.method", ""), kv_str("http.method", "PUT")],
                span_attrs: vec![kv_str("http.method", "GET")],
                conversion: v1_naming(false, &[]),
                operation_name: "ddtracer.internal",
                resource_name: "GET",
            },
            Case {
                name: "GraphQL values are not normalized",
                library: "ddtracer",
                kind: SpanKind::Server,
                span_name: "span",
                resource_attrs: vec![],
                span_attrs: vec![
                    kv_str("graphql.operation.type", "Query"),
                    kv_str("graphql.operation.name", "GetUser"),
                ],
                conversion: v1_naming(false, &[]),
                operation_name: "ddtracer.server",
                resource_name: "Query GetUser",
            },
        ];

        for case in cases {
            let span = fixture_span(case.span_name, case.kind, case.span_attrs);
            let library = scope(case.library, "");
            let dd_span = convert_v2(&span, &resource(case.resource_attrs), Some(&library), &case.conversion);

            assert_eq!(dd_span.name(), case.operation_name, "{}", case.name);
            assert_eq!(dd_span.resource(), case.resource_name, "{}", case.name);
        }
    }

    #[test]
    fn v1_receive_path_matches_the_fixtures() {
        let trace_id = string("72df520af2bde7a5240031ead750e5f3");

        // Ported from `testOTLPConvertSpan`: Datadog fields set by attributes.
        let span = fixture_span(
            "/path",
            SpanKind::Unspecified,
            vec![
                kv_str("service.name", "mongo"),
                kv_str("operation.name", "READ"),
                kv_str("resource.name", "/path"),
                kv_str("span.type", "db"),
                kv_str("name", "john"),
                kv_str("container.id", "cid"),
                kv_str("k8s.container.name", "k8s-container"),
                kv_str("http.method", "GET"),
                kv_str("http.route", "/path"),
                kv_value("approx", OtlpValue::DoubleValue(1.2)),
                kv_int("count", 2),
                kv_bool("analytics.event", true),
            ],
        );
        let resource_attrs = resource(vec![kv_str("env", "staging")]);
        let library = scope("ddtracer", "v2");
        for (conversion, operation_name) in [(v1_naming(false, &[]), "READ"), (conversion(), "read")] {
            let dd_span = convert_v1(&span, &resource_attrs, Some(&library), &conversion);
            assert_eq!(dd_span.service(), "mongo");
            assert_eq!(dd_span.name(), operation_name);
            assert_eq!(dd_span.resource(), "/path");
            assert_eq!(dd_span.span_type(), "db");
            assert_eq!(dd_span.error(), 0);
            assert_eq!(dd_span.span_id(), 2594128270069917171);
            assert_eq!(dd_span.parent_id(), 0);
            assert_eq!(dd_span.start(), FIXTURE_START);
            assert_eq!(dd_span.duration(), 200_000_000);
            assert_attributes(
                &dd_span,
                &[
                    ("env", string("staging")),
                    ("container.id", string("cid")),
                    ("k8s.container.name", string("k8s-container")),
                    ("http.method", string("GET")),
                    ("http.route", string("/path")),
                    ("otel.status_code", string("Unset")),
                    ("otel.library.name", string("ddtracer")),
                    ("otel.library.version", string("v2")),
                    ("name", string("john")),
                    ("otel.trace_id", trace_id.clone()),
                    ("span.kind", string("unspecified")),
                    ("approx", AttributeValue::Float(1.2)),
                    ("count", AttributeValue::Int(2)),
                    (EVENT_EXTRACTION_METRIC_KEY, AttributeValue::Float(1.0)),
                    ("_top_level", AttributeValue::Float(1.0)),
                ],
            );
        }

        // Ported from `testOTLPConvertSpan`: an HTTP server error with the current semantic conventions.
        let mut span = fixture_span(
            "POST /uploads/:document_id",
            SpanKind::Unspecified,
            vec![
                kv_str("operation.name", "ddtracer.server"),
                kv_str("http.request.method", "POST"),
                kv_str("url.path", "/uploads/4"),
                kv_str("url.scheme", "https"),
                kv_str("http.route", "/uploads/:document_id"),
                kv_str("http.response.status_code", "201"),
                kv_str("error.type", "WebSocketDisconnect"),
            ],
        );
        span.status = Some(OtlpStatus {
            code: StatusCode::Error as i32,
            ..Default::default()
        });
        let resource_attrs = resource(vec![
            kv_str("env", "staging"),
            kv_str("service.name", "document-uploader"),
        ]);
        for (conversion, resource_name) in [
            (v1_naming(false, &[]), "POST /uploads/:document_id"),
            (conversion(), "POST"),
        ] {
            let dd_span = convert_v1(&span, &resource_attrs, Some(&library), &conversion);
            assert_eq!(dd_span.service(), "document-uploader");
            assert_eq!(dd_span.name(), "ddtracer.server");
            assert_eq!(dd_span.resource(), resource_name);
            assert_eq!(dd_span.span_type(), "custom");
            assert_eq!(dd_span.error(), 1);
            assert_attributes(
                &dd_span,
                &[
                    ("env", string("staging")),
                    ("otel.library.name", string("ddtracer")),
                    ("otel.library.version", string("v2")),
                    ("otel.status_code", string("Error")),
                    ("error.msg", string("201 Created")),
                    ("http.request.method", string("POST")),
                    ("http.method", string("POST")),
                    ("url.path", string("/uploads/4")),
                    ("url.scheme", string("https")),
                    ("http.route", string("/uploads/:document_id")),
                    ("http.response.status_code", string("201")),
                    ("http.status_code", string("201")),
                    ("error.type", string("WebSocketDisconnect")),
                    ("otel.trace_id", trace_id.clone()),
                    ("span.kind", string("unspecified")),
                    ("_top_level", AttributeValue::Float(1.0)),
                ],
            );
        }
    }

    #[test]
    fn v1_receive_path_overrides_resource_values_with_span_values() {
        let span = fixture_span(
            "span",
            SpanKind::Internal,
            vec![
                kv_str("service.name", "span-service"),
                kv_int("sampling.priority", 2),
                kv_int("retries", 7),
                kv_int("http.status_code", 200),
                kv_str("http.response.status_code", "201"),
            ],
        );
        let resource_attrs = resource(vec![
            kv_str("service.name", "resource-service"),
            kv_str("sampling.priority", "1"),
            kv_str("retries", "resource-value"),
            kv_str("team", "core"),
        ]);
        let dd_span = convert_v1(&span, &resource_attrs, None, &v1_naming(false, &[]));

        assert_eq!(dd_span.service(), "span-service");
        assert_eq!(
            dd_span.attributes.get(SAMPLING_PRIORITY_METRIC_KEY),
            Some(&AttributeValue::Int(2))
        );
        // The resource's `sampling.priority` tag is dropped, though the number is stored under another key.
        assert_eq!(dd_span.attributes.get("sampling.priority"), None);
        assert_eq!(dd_span.attributes.get("retries"), Some(&AttributeValue::Int(7)));
        assert_eq!(meta_str(&dd_span, "team"), Some("core"));
        // `http.response.status_code` is mapped onto a string tag, but the numeric `http.status_code` stays a number.
        assert_eq!(
            dd_span.attributes.get("http.status_code"),
            Some(&AttributeValue::Int(200))
        );
        assert_eq!(meta_str(&dd_span, "http.response.status_code"), Some("201"));
    }

    #[test]
    fn v1_receive_path_names_only_spans_without_operation_name() {
        let library = scope("libname", "1.2");
        let remapped = v1_naming(false, &[("libname.unspecified", "new"), ("legacy.op", "modern.op")]);

        // Ported from `testOTLPNameRemapping`.
        let span = fixture_span("asd", SpanKind::Unspecified, vec![]);
        assert_eq!(
            convert_v1(&span, &resource(vec![]), Some(&library), &remapped).name(),
            "new"
        );

        // Unlike V1 naming on the V2 receive path, an `operation.name` is not remapped.
        let span = fixture_span(
            "asd",
            SpanKind::Unspecified,
            vec![kv_str("operation.name", "legacy.op")],
        );
        assert_eq!(
            convert_v1(&span, &resource(vec![]), Some(&library), &remapped).name(),
            "legacy.op"
        );

        let span = fixture_span("checkout", SpanKind::Server, vec![]);
        assert_eq!(
            convert_v1(&span, &resource(vec![]), Some(&library), &v1_naming(true, &[])).name(),
            "checkout"
        );
        assert_eq!(
            convert_v1(&span, &resource(vec![]), None, &v1_naming(false, &[])).name(),
            "opentelemetry.server"
        );
    }

    #[test]
    fn v1_receive_path_falls_back_to_its_own_defaults() {
        // The database system is read from the resource first, and normalized.
        for (kind, resource_attrs, span_attrs, span_type) in [
            (SpanKind::Server, vec![], vec![], "web"),
            (SpanKind::Client, vec![], vec![], "http"),
            (
                SpanKind::Client,
                vec![],
                vec![kv_str("db.system", "memcached")],
                "cache",
            ),
            (
                SpanKind::Client,
                vec![kv_str("db.system", "Redis")],
                vec![kv_str("db.system", "mysql")],
                "cache",
            ),
            (SpanKind::Client, vec![], vec![kv_str("db.system", "mysql")], "db"),
            (SpanKind::Producer, vec![], vec![kv_str("db.system", "redis")], "custom"),
        ] {
            let span = fixture_span("span", kind, span_attrs);
            let dd_span = convert_v1(&span, &resource(resource_attrs), None, &v1_naming(false, &[]));
            assert_eq!(dd_span.span_type(), span_type, "{kind:?}");
            assert_eq!(dd_span.service(), "otlpresourcenoservicename");
        }

        // The resource is derived from the converted tags, so span attributes take precedence.
        for (resource_attrs, span_attrs, resource_name) in [
            (
                vec![kv_str("http.method", "PUT")],
                vec![kv_str("http.method", "GET"), kv_str("grpc.path", "/svc/Method")],
                "GET /svc/Method",
            ),
            (vec![kv_str("http.method", "PUT")], vec![], "PUT"),
            (
                vec![],
                vec![
                    kv_str("messaging.operation", "publish"),
                    kv_str("messaging.destination", "orders"),
                ],
                "publish orders",
            ),
            (
                vec![],
                vec![kv_str("rpc.method", "Get"), kv_str("rpc.service", "Users")],
                "Get Users",
            ),
            (vec![], vec![kv_str("graphql.operation.type", "query")], "query"),
            (vec![], vec![], "span"),
        ] {
            let span = fixture_span("span", SpanKind::Internal, span_attrs);
            let dd_span = convert_v1(&span, &resource(resource_attrs), None, &v1_naming(false, &[]));
            assert_eq!(dd_span.resource(), resource_name);
        }
    }

    #[test]
    fn v1_receive_path_derives_env_from_the_deployment_environment() {
        for (span_attrs, env) in [
            (
                vec![
                    kv_str("deployment.environment", "Staging"),
                    kv_str("deployment.environment.name", "1Prod Env"),
                ],
                Some("prod_env"),
            ),
            (vec![kv_str("deployment.environment", "Staging")], Some("staging")),
            (
                vec![kv_str("env", "Raw"), kv_str("deployment.environment", "Staging")],
                Some("Raw"),
            ),
            (vec![], None),
        ] {
            let span = fixture_span("span", SpanKind::Internal, span_attrs);
            let dd_span = convert_v1(&span, &resource(vec![]), None, &v1_naming(false, &[]));
            assert_eq!(meta_str(&dd_span, "env"), env);
        }
    }

    #[test]
    fn v1_receive_path_emits_only_the_deprecated_scope_tags() {
        let mut library = scope("ddtracer", "v2");
        library.attributes = vec![kv_str("scope.attribute", "dropped")];
        let span = fixture_span(
            "span",
            SpanKind::Internal,
            vec![kv_str("datadog.service", "dd-service"), kv_str("db.namespace", "users")],
        );
        let dd_span = convert_v1(&span, &resource(vec![]), Some(&library), &conversion());

        assert_eq!(meta_str(&dd_span, "otel.library.name"), Some("ddtracer"));
        assert_eq!(meta_str(&dd_span, "otel.library.version"), Some("v2"));
        assert_eq!(meta_str(&dd_span, "otel.scope.name"), None);
        assert_eq!(meta_str(&dd_span, "otel.scope.version"), None);
        assert_eq!(meta_str(&dd_span, "scope.attribute"), None);
        // `datadog.*` attributes are ordinary tags on this path.
        assert_eq!(meta_str(&dd_span, "datadog.service"), Some("dd-service"));
        assert_eq!(dd_span.service(), "otlpresourcenoservicename");
        assert_eq!(meta_str(&dd_span, "db.name"), Some("users"));
    }

    #[test]
    fn v1_receive_path_marks_top_level_and_measured_by_span_kind() {
        let parent = [9u8; 8];
        for (kind, has_parent, compute, top_level, measured) in [
            (SpanKind::Internal, false, true, true, false),
            (SpanKind::Server, true, true, true, false),
            (SpanKind::Consumer, true, true, true, false),
            (SpanKind::Client, true, true, false, true),
            (SpanKind::Producer, true, true, false, true),
            (SpanKind::Internal, true, true, false, false),
            (SpanKind::Server, false, false, false, false),
        ] {
            let mut span = fixture_span("span", kind, vec![]);
            if has_parent {
                span.parent_span_id = parent.to_vec();
            }
            let conversion = SpanConversion {
                compute_top_level_by_span_kind: compute,
                ..conversion()
            };
            let dd_span = convert_v1(&span, &resource(vec![]), None, &conversion);
            let case = format!("{kind:?} parent={has_parent} compute={compute}");
            assert_eq!(dd_span.attributes.contains_key("_top_level"), top_level, "{case}");
            assert_eq!(dd_span.attributes.contains_key("_dd.measured"), measured, "{case}");
        }
    }

    #[test]
    fn v1_receive_path_keeps_numbers_over_string_tags_of_the_same_key() {
        for kind in [SpanKind::Server, SpanKind::Client] {
            let span = fixture_span(
                "span",
                kind,
                vec![kv_str("_top_level", "0"), kv_str("_dd.measured", "0")],
            );
            let dd_span = convert_v1(&span, &resource(vec![]), None, &conversion());

            let marker = if kind == SpanKind::Server {
                "_top_level"
            } else {
                "_dd.measured"
            };
            assert_eq!(
                dd_span.attributes.get(marker),
                Some(&AttributeValue::Float(1.0)),
                "{kind:?}"
            );
        }

        // Without a computed marker, the attribute is an ordinary string tag.
        let span = fixture_span("span", SpanKind::Internal, vec![kv_str("_dd.measured", "0")]);
        let dd_span = convert_v1(&span, &resource(vec![]), None, &conversion());
        assert_eq!(meta_str(&dd_span, "_dd.measured"), Some("0"));
    }

    #[test]
    fn v1_receive_path_follows_the_legacy_tags_gate_and_takes_no_error_fields_from_events() {
        let mut span = span_with_links_and_events();
        span.events[0].attributes = vec![
            kv_str("exception.message", "Out of memory"),
            kv_str("exception.type", "mem"),
        ];
        span.status = Some(OtlpStatus {
            code: StatusCode::Error as i32,
            message: "boom".to_string(),
        });
        let dd_span = convert_v1(&span, &resource(vec![]), None, &conversion());

        assert_eq!(!dd_span.span_links().is_empty(), !LEGACY_TAG_LINKS_AND_EVENTS);
        assert_eq!(!dd_span.span_events().is_empty(), !LEGACY_TAG_LINKS_AND_EVENTS);
        assert_eq!(
            dd_span.attributes.contains_key("_dd.span_links"),
            LEGACY_TAG_LINKS_AND_EVENTS
        );
        assert_eq!(dd_span.attributes.contains_key("events"), LEGACY_TAG_LINKS_AND_EVENTS);
        assert_eq!(meta_str(&dd_span, "_dd.span_events.has_exception"), Some("true"));
        assert_eq!(meta_str(&dd_span, "error.msg"), Some("boom"));
        assert_eq!(meta_str(&dd_span, "error.type"), None);
        assert_eq!(dd_span.error(), 1);
    }
}
