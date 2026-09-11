use std::collections::hash_map::IntoIter;
use std::sync::Arc;

use agent_data_plane_config::domains;
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
/// Returns 0 if the trace ID is shorter than 16 bytes (for example, a 64-bit-only ID).
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
    attributes: Arc<FastHashMap<MetaString, AttributeValue>>,
}

/// Extracts unified trace-level fields from OTLP resource attributes.
///
/// Mirrors the field extraction performed by `receiveResourceSpansV2` in the Go trace agent
/// (`pkg/trace/api/otlp.go`): env from deployment environment semantic conventions, container ID from
/// container semantic conventions, hostname from `datadog.host.name`, language/version from telemetry SDK attributes.
/// All known fields are also inserted into the returned `attributes` map so that downstream code
/// can use a single map lookup regardless of whether a field is explicitly modelled on `Trace`.
///
/// **Hostname**: we capture only `datadog.host.name` here. The Go agent resolves hostname
/// through up to six fallback steps (cloud-provider EC2/GCP/Azure, K8s node name, `host.name`,
/// etc.). The encoder covers `host.name` + AWS ECS Fargate via `attributes_to_source`, but the
/// cloud-provider and K8s steps are not yet implemented.
/// TODO: implement full hostnameFromAttributes parity.
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

        // Scalar types are stored in their native AttributeValue variant so downstream
        // code (e.g. the encoder) can coerce at the output boundary. Arrays and KVLists
        // are stringified via JSON because no wire format accepts them natively.
        // TODO: when implementing the new indexed format this will no longer be necessary.
        let attr_value = match value {
            OtlpValue::StringValue(s) => AttributeValue::String(MetaString::from_interner(s.as_str(), interner)),
            OtlpValue::IntValue(i) => AttributeValue::Int(*i),
            OtlpValue::DoubleValue(d) => AttributeValue::Float(*d),
            OtlpValue::BoolValue(b) => AttributeValue::Bool(*b),
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
        attributes: Arc::new(attr_map),
    }
}

struct TraceEntry {
    spans: Vec<DdSpan>,
    priority: Option<i32>,
    trace_id_hex: Option<MetaString>,
    /// High 8 bytes of the 128-bit trace ID (captured from the first span).
    trace_id_high: u64,
    /// Whether the group failed the full trace ID consistency check and must be dropped whole.
    rejected: bool,
}

pub struct OtlpTracesTranslator {
    config: domains::otlp::Traces,
    interner: GenericMapInterner,
    string_builder: StringBuilder<GenericMapInterner>,
}

impl OtlpTracesTranslator {
    pub fn new(config: domains::otlp::Traces) -> Self {
        let interner = GenericMapInterner::new(config.string_interner_size);
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
        let compute_top_level = self.config.enable_compute_top_level_by_span_kind;
        let interner = &self.interner;
        let string_builder = &mut self.string_builder;

        // Build unified resource metadata for the new Trace fields.
        let resource_meta =
            extract_resource_meta(&resource.attributes, ignore_missing_fields, interner, string_builder);

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
                    rejected: false,
                });

                // Trace ID validity: a trace ID is either empty or exactly 16 bytes, and its
                // low half must be nonzero. An invalid ID rejects the whole group.
                if (!span.trace_id.is_empty() && span.trace_id.len() != 16) || trace_id == 0 {
                    entry.rejected = true;
                }

                // Full trace ID consistency: spans in a group must carry the same full trace ID
                // as the group's first span.
                if entry.trace_id_high != trace_id_high {
                    entry.rejected = true;
                }

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

                // Malformed self-parented spans (parent == span == trace ID) get their parent
                // cleared so root selection treats them as roots.
                let dd_span = if dd_span.parent_id() == trace_id && dd_span.parent_id() == dd_span.span_id() {
                    dd_span.with_parent_id(0)
                } else {
                    dd_span
                };

                // A zero span ID rejects the whole group; a wrong-length OTLP span ID also
                // decodes to zero.
                if dd_span.span_id() == 0 {
                    entry.rejected = true;
                }

                // Track last-seen priority for this trace (overwrites previous values)
                if let Some(priority) = dd_span
                    .attributes
                    .get(SAMPLING_PRIORITY_METRIC_KEY)
                    .and_then(AttributeValue::as_num)
                {
                    entry.priority = Some(priority as i32);
                }

                entry.spans.push(dd_span);
            }
        }

        OtlpTraceEventsIter {
            resource_meta,
            entries: traces_by_id.into_iter(),
            metrics: metrics.clone(),
        }
    }
}

struct OtlpTraceEventsIter {
    resource_meta: OtlpResourceMeta,
    entries: IntoIter<u64, TraceEntry>,
    metrics: Metrics,
}

impl Iterator for OtlpTraceEventsIter {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        for (trace_id_low, entry) in self.entries.by_ref() {
            if entry.rejected {
                // Two different traces glued together by a shared low half: drop the group whole.
                self.metrics
                    .spans_dropped_foreign_trace()
                    .increment(entry.spans.len() as u64);
                continue;
            }

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
            trace.attributes = Arc::clone(&self.resource_meta.attributes);

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

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::common::v1::any_value::Value;
    use otlp_protos::opentelemetry::proto::common::v1::{AnyValue, KeyValue};
    use otlp_protos::opentelemetry::proto::resource::v1::Resource;
    use otlp_protos::opentelemetry::proto::trace::v1::{ResourceSpans, ScopeSpans, Span as OtlpSpan};

    use super::*;
    use crate::common::otlp::Metrics;

    fn string_kv(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(value.to_string())),
            }),
        }
    }

    fn int_kv(key: &str, value: i64) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(Value::IntValue(value)),
            }),
        }
    }

    fn span(trace_id: [u8; 16], span_id: [u8; 8], attributes: Vec<KeyValue>) -> OtlpSpan {
        span_raw(trace_id.to_vec(), span_id, attributes)
    }

    fn span_with_parent(
        trace_id: [u8; 16], span_id: [u8; 8], parent_span_id: [u8; 8], attributes: Vec<KeyValue>,
    ) -> OtlpSpan {
        OtlpSpan {
            trace_id: trace_id.to_vec(),
            span_id: span_id.to_vec(),
            parent_span_id: parent_span_id.to_vec(),
            name: "span".to_string(),
            end_time_unix_nano: 2,
            attributes,
            ..Default::default()
        }
    }

    fn span_raw(trace_id: Vec<u8>, span_id: [u8; 8], attributes: Vec<KeyValue>) -> OtlpSpan {
        OtlpSpan {
            trace_id,
            span_id: span_id.to_vec(),
            name: "span".to_string(),
            end_time_unix_nano: 2,
            attributes,
            ..Default::default()
        }
    }

    /// Builds a 16-byte big-endian trace ID from its high and low u64 halves.
    fn trace_id16(high: u64, low: u64) -> [u8; 16] {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&high.to_be_bytes());
        id[8..].copy_from_slice(&low.to_be_bytes());
        id
    }

    fn build_resource_spans(resource_attrs: Vec<KeyValue>, spans: Vec<OtlpSpan>) -> ResourceSpans {
        ResourceSpans {
            resource: Some(Resource {
                attributes: resource_attrs,
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn translate(resource_spans: ResourceSpans) -> Vec<Trace> {
        let mut translator = OtlpTracesTranslator::new(domains::otlp::Traces {
            string_interner_size: std::num::NonZeroUsize::new(64 * 1024).unwrap(),
            enable_compute_top_level_by_span_kind: true,
            ..Default::default()
        });
        let metrics = Metrics::for_tests();
        translator
            .translate_spans(resource_spans, &metrics)
            .filter_map(Event::try_into_trace)
            .collect()
    }

    #[test]
    fn translate_spans_resolves_hostname_from_datadog_host_name() {
        let rs = build_resource_spans(
            vec![string_kv("datadog.host.name", "my-host")],
            vec![span([1u8; 16], [1u8; 8], vec![])],
        );

        let traces = translate(rs);
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].payload.hostname.as_ref(), "my-host");
    }

    #[test]
    fn translate_spans_leaves_hostname_empty_without_datadog_host_name() {
        // Only `datadog.host.name` is honored here (the single documented fallback); absent it, the
        // hostname is left empty rather than guessed from other attributes.
        let rs = build_resource_spans(
            vec![string_kv("host.name", "ignored")],
            vec![span([1u8; 16], [1u8; 8], vec![])],
        );

        let traces = translate(rs);
        assert_eq!(traces.len(), 1);
        assert!(traces[0].payload.hostname.as_ref().is_empty());
    }

    #[test]
    fn translate_spans_groups_spans_by_trace_id() {
        let trace_a = [0xAAu8; 16];
        let trace_b = [0xBBu8; 16];
        let rs = build_resource_spans(
            vec![],
            vec![
                span(trace_a, [1u8; 8], vec![]),
                span(trace_a, [2u8; 8], vec![]),
                span(trace_b, [3u8; 8], vec![]),
            ],
        );

        let mut traces = translate(rs);
        assert_eq!(traces.len(), 2, "expected one trace per distinct trace ID");

        traces.sort_by_key(|t| t.spans().len());
        assert_eq!(traces[0].spans().len(), 1);

        let grouped = &traces[1];
        assert_eq!(grouped.spans().len(), 2, "spans sharing a trace ID group together");
        // Trace ID low/high bytes are captured from the 16-byte OTLP trace ID.
        assert_eq!(grouped.trace_id_low, u64::from_be_bytes([0xAA; 8]));
        assert_eq!(grouped.trace_id_high, u64::from_be_bytes([0xAA; 8]));
    }

    #[test]
    fn translate_spans_tracks_last_seen_sampling_priority() {
        let trace = [0xCCu8; 16];
        let rs = build_resource_spans(
            vec![],
            vec![
                span(trace, [1u8; 8], vec![int_kv("sampling.priority", 1)]),
                span(trace, [2u8; 8], vec![int_kv("sampling.priority", 2)]),
            ],
        );

        let traces = translate(rs);
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].priority, Some(2), "the last span's sampling priority wins");
    }

    #[test]
    fn translate_spans_populates_resource_metadata() {
        let rs = build_resource_spans(
            vec![
                string_kv("deployment.environment", "prod"),
                string_kv("service.version", "1.2.3"),
                string_kv("container.id", "abc123"),
                string_kv("telemetry.sdk.language", "go"),
                string_kv("telemetry.sdk.version", "1.0"),
            ],
            vec![span([1u8; 16], [1u8; 8], vec![])],
        );

        let traces = translate(rs);
        assert_eq!(traces.len(), 1);
        let payload = &traces[0].payload;
        assert_eq!(payload.env.as_ref(), "prod");
        assert_eq!(payload.app_version.as_ref(), "1.2.3");
        assert_eq!(payload.container_id.as_ref(), "abc123");
        assert_eq!(payload.language_name.as_ref(), "go");
        assert_eq!(payload.tracer_version.as_ref(), "1.0");
    }

    #[test]
    fn convert_trace_id_uses_agent_grouping_key() {
        // Pins the grouping key we derive from the upstream OTLP ingest: the low 8 bytes of the
        // 16-byte big-endian trace ID.
        let id: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        assert_eq!(
            convert_trace_id(&id),
            u64::from_be_bytes([8, 9, 10, 11, 12, 13, 14, 15])
        );
        assert_eq!(convert_trace_id_high(&id), u64::from_be_bytes([0, 1, 2, 3, 4, 5, 6, 7]));

        // A zero-padded 64-bit trace ID: its value lives entirely in the low half.
        let id64 = trace_id16(0, 0x1234_5678_9abc_def0);
        assert_eq!(convert_trace_id(&id64), 0x1234_5678_9abc_def0);
        assert_eq!(convert_trace_id_high(&id64), 0);

        assert_eq!(convert_trace_id(&[1, 2, 3]), 0);
        assert_eq!(convert_trace_id_high(&[0xAA; 8]), 0);
    }

    #[test]
    fn translate_spans_rejects_groups_with_mixed_full_trace_ids() {
        let low = u64::from_be_bytes([0xAA; 8]);
        let rs = build_resource_spans(
            vec![],
            vec![
                span(trace_id16(0x0101_0101_0101_0101, low), [1u8; 8], vec![]),
                span(trace_id16(0x0202_0202_0202_0202, low), [2u8; 8], vec![]),
                span(trace_id16(0, 0x0BBB_BBBB_BBBB_BBBB), [3u8; 8], vec![]),
            ],
        );

        let traces = translate(rs);
        assert_eq!(
            traces.len(),
            1,
            "the mixed group is dropped; the unrelated trace survives"
        );
        assert_eq!(traces[0].trace_id_low, 0x0BBB_BBBB_BBBB_BBBB);
    }

    #[test]
    fn translate_spans_rejects_mixed_zero_and_nonzero_high_halves() {
        // A zero high half and a nonzero high half are different full trace IDs, in either span
        // order. Both IDs are canonical 16-byte OTLP IDs here; the OTLP spans always carry the
        // full 128-bit ID, so unlike legacy tracer chunks the high halves are always compared.
        let id_zero_high = trace_id16(0, u64::from_be_bytes([0xAA; 8])).to_vec();
        let id_full = trace_id16(0x0101_0101_0101_0101, u64::from_be_bytes([0xAA; 8])).to_vec();

        let zero_high_first = build_resource_spans(
            vec![],
            vec![
                span_raw(id_zero_high.clone(), [1u8; 8], vec![]),
                span_raw(id_full.clone(), [2u8; 8], vec![]),
            ],
        );
        assert_eq!(translate(zero_high_first).len(), 0);

        let full_first = build_resource_spans(
            vec![],
            vec![
                span_raw(id_full, [1u8; 8], vec![]),
                span_raw(id_zero_high, [2u8; 8], vec![]),
            ],
        );
        assert_eq!(translate(full_first).len(), 0);
    }

    #[test]
    fn translate_spans_accepts_consistent_zero_high_halves() {
        // Two 16-byte IDs with the same zero high half and same low half are one trace.
        let id_zero_high = trace_id16(0, u64::from_be_bytes([0xAA; 8]));

        let rs = build_resource_spans(
            vec![],
            vec![
                span(id_zero_high, [1u8; 8], vec![]),
                span(id_zero_high, [2u8; 8], vec![]),
            ],
        );
        let traces = translate(rs);
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].spans().len(), 2);
        assert_eq!(traces[0].trace_id_high, 0);
    }

    #[test]
    fn translate_spans_rejects_non_canonical_length_trace_ids() {
        // A trace ID is valid only when it is empty or exactly 16 bytes; anything else is
        // decode-invalid, and the trace is dropped.
        for raw_id in [vec![0xAA; 8], vec![0xAA; 12], vec![0xAA; 20]] {
            let rs = build_resource_spans(vec![], vec![span_raw(raw_id.clone(), [1u8; 8], vec![])]);
            assert_eq!(
                translate(rs).len(),
                0,
                "non-canonical length {} is dropped",
                raw_id.len()
            );
        }

        // A short ID in a group also rejects the whole group, not just the malformed span.
        let rs = build_resource_spans(
            vec![],
            vec![
                span_raw(vec![0xAA; 8], [1u8; 8], vec![]),
                span(trace_id16(0, u64::from_be_bytes([0xAA; 8])), [2u8; 8], vec![]),
            ],
        );
        assert_eq!(
            translate(rs).len(),
            0,
            "the group containing a short trace ID is dropped whole"
        );
    }

    #[test]
    fn translate_spans_rejects_zero_trace_ids() {
        // A zero low half is invalid regardless of the high half: a 16-byte ID with a zero
        // low half is zero, and an empty ID decodes to zero.
        for raw_id in [
            trace_id16(0x0101_0101_0101_0101, 0).to_vec(),
            trace_id16(0, 0).to_vec(),
            Vec::new(),
        ] {
            let rs = build_resource_spans(vec![], vec![span_raw(raw_id, [1u8; 8], vec![])]);
            assert_eq!(translate(rs).len(), 0, "zero trace IDs are dropped");
        }
    }

    #[test]
    fn translate_spans_rejects_zero_span_ids() {
        // A zero span ID drops the whole trace; a wrong-length OTLP span ID also decodes to
        // zero. Duplicate span IDs, by contrast, are only counted as malformed, so they stay
        // accepted.
        let rs = build_resource_spans(vec![], vec![span(trace_id16(0, 1), [0u8; 8], vec![])]);
        assert_eq!(translate(rs).len(), 0, "an all-zero span ID drops the trace");

        let mut zero_len_span = span(trace_id16(0, 1), [1u8; 8], vec![]);
        zero_len_span.span_id = vec![];
        let rs = build_resource_spans(vec![], vec![zero_len_span]);
        assert_eq!(
            translate(rs).len(),
            0,
            "an empty span ID decodes to zero and drops the trace"
        );

        let rs = build_resource_spans(
            vec![],
            vec![
                span(trace_id16(0, 1), [1u8; 8], vec![]),
                span(trace_id16(0, 1), [1u8; 8], vec![]),
            ],
        );
        let traces = translate(rs);
        assert_eq!(traces.len(), 1, "duplicate span IDs are tolerated");
        assert_eq!(traces_spans_len(&traces), 2);
    }

    fn traces_spans_len(traces: &[Trace]) -> usize {
        traces.iter().map(|t| t.spans().len()).sum()
    }

    #[test]
    fn translate_spans_repairs_self_parented_spans() {
        // parent == span == trace ID gets the parent cleared so the malformed span becomes a
        // root; other spans keep their parents.
        let trace = trace_id16(0, 1);
        let self_parented = span_with_parent(trace, [0, 0, 0, 0, 0, 0, 0, 1], [0, 0, 0, 0, 0, 0, 0, 1], vec![]);
        let child = span_with_parent(trace, [0, 0, 0, 0, 0, 0, 0, 2], [0, 0, 0, 0, 0, 0, 0, 1], vec![]);

        let rs = build_resource_spans(vec![], vec![self_parented, child]);
        let traces = translate(rs);
        assert_eq!(traces.len(), 1);

        let spans = traces[0].spans();
        let repaired = spans
            .iter()
            .find(|s| s.span_id() == 1)
            .expect("self-parented span is present");
        assert_eq!(repaired.parent_id(), 0, "the self-parented span's parent is cleared");

        let child = spans.iter().find(|s| s.span_id() == 2).expect("child is present");
        assert_eq!(child.parent_id(), 1, "the child's parent is untouched");
    }
}
