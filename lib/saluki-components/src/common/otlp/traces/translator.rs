use std::collections::hash_map::IntoIter;
use std::num::NonZeroUsize;
use std::sync::Arc;

use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common, any_value::Value as OtlpValue};
use otlp_protos::opentelemetry::proto::resource::v1::Resource as OtlpResource;
use otlp_protos::opentelemetry::proto::trace::v1::ResourceSpans;
use saluki_common::collections::FastHashMap;
use saluki_common::strings::StringBuilder;
use saluki_core::data_model::event::trace::{AttributeValue, Span as DdSpan, Trace};
use saluki_core::data_model::event::Event;
use stringtheory::interning::GenericMapInterner;
use stringtheory::MetaString;

use crate::common::datadog::SAMPLING_PRIORITY_METRIC_KEY;
use crate::common::otlp::config::TracesConfig;
use crate::common::otlp::traces::transform::{
    bytes_to_hex_lowercase, get_otel_container_id, get_otel_env, get_otel_version, otel_span_to_dd_span,
    otlp_value_to_string,
};
use crate::common::otlp::util::get_string_attribute;
use crate::common::otlp::Metrics;

const DATADOG_HOSTNAME_ATTR: &str = "datadog.host.name";
const TELEMETRY_SDK_LANGUAGE: &str = "telemetry.sdk.language";
const TELEMETRY_SDK_VERSION: &str = "telemetry.sdk.version";

pub fn convert_trace_id(trace_id: &[u8]) -> u64 {
    if trace_id.len() < 8 {
        return 0;
    }
    u64::from_be_bytes((&trace_id[(trace_id.len() - 8)..]).try_into().unwrap_or_default())
}

/// Extracts the high 8 bytes of a 128-bit OTLP trace ID as a big-endian u64.
///
/// Returns 0 if the trace ID is shorter than 16 bytes (e.g. a 64-bit-only ID).
pub fn convert_trace_id_high(trace_id: &[u8]) -> u64 {
    if trace_id.len() < 16 {
        return 0;
    }
    u64::from_be_bytes((&trace_id[..8]).try_into().unwrap_or_default())
}

pub fn convert_span_id(span_id: &[u8]) -> u64 {
    if span_id.len() != 8 {
        return 0;
    }
    u64::from_be_bytes(span_id.try_into().unwrap_or_default())
}

/// Metadata extracted from OTLP resource attributes for the unified `Trace` fields.
///
/// Built once per `ResourceSpans` batch and shared across all traces derived from
/// the same resource.
struct OtlpResourceMeta {
    /// Resolved environment name.
    env: MetaString,
    /// Resolved hostname.
    hostname: MetaString,
    /// Resolved container ID.
    container_id: MetaString,
    /// Resolved application version.
    app_version: MetaString,
    /// Resolved tracer language name.
    language_name: MetaString,
    /// Resolved tracer SDK version.
    tracer_version: MetaString,
    /// All resource attributes as a typed map (for `Trace::attributes`).
    attributes: FastHashMap<MetaString, AttributeValue>,
}

/// Extracts unified trace-level fields from OTLP resource attributes.
///
/// Mirrors the field extraction performed by `receiveResourceSpansV2` in the Go trace agent
/// (`pkg/trace/api/otlp.go`): env from deployment env semconv, container ID from container
/// semconv, hostname from `datadog.host.name`, language/version from telemetry SDK attributes.
/// All known fields are also inserted into the returned `attributes` map so that downstream code
/// can use a single map lookup regardless of whether a field is explicitly modelled on `Trace`.
fn extract_resource_meta(
    attributes: &[otlp_common::KeyValue], ignore_missing_fields: bool, interner: &GenericMapInterner,
    string_builder: &mut StringBuilder<GenericMapInterner>,
) -> OtlpResourceMeta {
    // Reuse the existing normalizing helpers (span_attrs = empty, resource_attrs = full).
    let empty: &[otlp_common::KeyValue] = &[];

    let env = get_otel_env(attributes, empty, ignore_missing_fields, interner, string_builder);
    let app_version = get_otel_version(attributes, empty, ignore_missing_fields, interner, string_builder);
    let container_id = get_otel_container_id(attributes, empty, ignore_missing_fields, interner, string_builder);

    let hostname = get_string_attribute(attributes, DATADOG_HOSTNAME_ATTR)
        .filter(|s| !s.is_empty())
        .map(|s| MetaString::from_interner(s, interner))
        .unwrap_or_default();

    let language_name = get_string_attribute(attributes, TELEMETRY_SDK_LANGUAGE)
        .filter(|s| !s.is_empty())
        .map(|s| MetaString::from_interner(s, interner))
        .unwrap_or_default();

    let tracer_version = get_string_attribute(attributes, TELEMETRY_SDK_VERSION)
        .filter(|s| !s.is_empty())
        .map(|s| MetaString::from_interner(s, interner))
        .unwrap_or_default();
    // language_version is intentionally not populated for OTLP traces: OTLP has no standardised
    // attribute for the language runtime version, so we leave it empty rather than guess.

    // Build the typed attributes map from all resource attributes.
    let mut attr_map: FastHashMap<MetaString, AttributeValue> = FastHashMap::default();
    attr_map.reserve(attributes.len());
    for kv in attributes {
        if kv.key.is_empty() {
            continue;
        }
        let Some(wrapper) = &kv.value else { continue };
        let Some(value) = &wrapper.value else { continue };

        let attr_value = match value {
            OtlpValue::StringValue(s) => AttributeValue::String(MetaString::from_interner(s.as_str(), interner)),
            OtlpValue::IntValue(i) => AttributeValue::Float(*i as f64),
            OtlpValue::DoubleValue(d) => AttributeValue::Float(*d),
            OtlpValue::BoolValue(b) => {
                AttributeValue::String(MetaString::from_static(if *b { "true" } else { "false" }))
            }
            OtlpValue::BytesValue(b) => AttributeValue::Bytes(b.clone()),
            _ => {
                // Arrays and KVLists are stringified via JSON.
                if let Some(s) = otlp_value_to_string(value) {
                    AttributeValue::String(MetaString::from_interner(s.as_str(), interner))
                } else {
                    continue;
                }
            }
        };

        let key = MetaString::from_interner(kv.key.as_str(), interner);
        attr_map.insert(key, attr_value);
    }

    OtlpResourceMeta {
        env,
        hostname,
        container_id,
        app_version,
        language_name,
        tracer_version,
        attributes: attr_map,
    }
}

struct TraceEntry {
    spans: Vec<DdSpan>,
    priority: Option<i32>,
    trace_id_hex: Option<MetaString>,
    /// High 8 bytes of the 128-bit trace ID (captured from the first span).
    trace_id_high: u64,
}

pub struct OtlpTracesTranslator {
    config: TracesConfig,
    interner: GenericMapInterner,
    string_builder: StringBuilder<GenericMapInterner>,
}

impl OtlpTracesTranslator {
    pub fn new(config: TracesConfig, interner_size: NonZeroUsize) -> Self {
        let interner = GenericMapInterner::new(interner_size);
        let string_builder = StringBuilder::new().with_interner(interner.clone());
        Self {
            config,
            interner,
            string_builder,
        }
    }

    pub fn translate_spans(&mut self, resource_spans: ResourceSpans, metrics: &Metrics) -> impl Iterator<Item = Event> {
        let resource: OtlpResource = resource_spans.resource.unwrap_or_default();
        let ignore_missing_fields = self.config.ignore_missing_datadog_fields;
        let compute_top_level = self.config.enable_otlp_compute_top_level_by_span_kind;
        let interner = &self.interner;
        let string_builder = &mut self.string_builder;

        // Build unified resource metadata for the new Trace fields.
        let resource_meta = extract_resource_meta(&resource.attributes, ignore_missing_fields, interner, string_builder);

        let mut traces_by_id: FastHashMap<u64, TraceEntry> = FastHashMap::default();
        let trace_count_hint = resource_spans.scope_spans.len();
        traces_by_id.reserve(trace_count_hint);

        for scope_spans in resource_spans.scope_spans {
            let scope = scope_spans.scope;
            let scope_ref = scope.as_ref();
            metrics.spans_received().increment(scope_spans.spans.len() as u64);
            for span in scope_spans.spans {
                let trace_id = convert_trace_id(&span.trace_id);
                let trace_id_high = convert_trace_id_high(&span.trace_id);
                let entry = traces_by_id.entry(trace_id).or_insert_with(|| TraceEntry {
                    spans: Vec::new(),
                    priority: None,
                    trace_id_hex: None,
                    trace_id_high,
                });

                if entry.trace_id_hex.is_none() {
                    entry.trace_id_hex = trace_id_hex_meta(&span.trace_id);
                }

                let dd_span = otel_span_to_dd_span(
                    &span,
                    &resource,
                    scope_ref,
                    ignore_missing_fields,
                    compute_top_level,
                    interner,
                    string_builder,
                    entry.trace_id_hex.as_ref(),
                );

                // Track last-seen priority for this trace (overwrites previous values)
                if let Some(priority) = dd_span.attributes.get(SAMPLING_PRIORITY_METRIC_KEY).and_then(AttributeValue::as_float) {
                    entry.priority = Some(priority as i32);
                }

                entry.spans.push(dd_span);
            }
        }

        OtlpTraceEventsIter {
            resource_meta,
            entries: traces_by_id.into_iter(),
        }
    }
}

struct OtlpTraceEventsIter {
    resource_meta: OtlpResourceMeta,
    entries: IntoIter<u64, TraceEntry>,
}

impl Iterator for OtlpTraceEventsIter {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        for (trace_id_low, entry) in self.entries.by_ref() {
            if entry.spans.is_empty() {
                continue;
            }

            let mut trace = Trace::new(entry.spans);

            // Populate unified Trace fields here — after grouping spans by trace ID — because
            // this is the first point where a complete (spans + resource metadata + priority)
            // picture is available for a single trace. Resource metadata is shared across all
            // traces in a ResourceSpans batch, so it lives on the iterator rather than per-entry.
            trace.trace_id_low = trace_id_low;
            trace.trace_id_high = entry.trace_id_high;
            trace.priority = entry.priority;
            trace.payload.env = self.resource_meta.env.clone();
            trace.payload.hostname = self.resource_meta.hostname.clone();
            trace.payload.container_id = self.resource_meta.container_id.clone();
            trace.payload.app_version = self.resource_meta.app_version.clone();
            trace.payload.language_name = self.resource_meta.language_name.clone();
            trace.payload.tracer_version = self.resource_meta.tracer_version.clone();
            trace.attributes = self.resource_meta.attributes.clone();

            return Some(Event::Trace(trace));
        }

        None
    }
}

fn trace_id_hex_meta(trace_id: &[u8]) -> Option<MetaString> {
    if trace_id.is_empty() {
        return None;
    }

    let hex = bytes_to_hex_lowercase(trace_id);
    if hex.is_empty() {
        return None;
    }

    Some(MetaString::from(Arc::<str>::from(hex)))
}
