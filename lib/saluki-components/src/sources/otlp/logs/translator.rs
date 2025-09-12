use opentelemetry_semantic_conventions::resource::{HOST_NAME, SERVICE_NAME};
use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;
use otlp_protos::opentelemetry::proto::logs::v1::ResourceLogs as OtlpResourceLogs;
use std::collections::HashMap;
use serde_json::Value as JsonValue;
use saluki_context::tags::{SharedTagSet, TagSet};
use saluki_core::data_model::event::log::Log as DdLog;
use saluki_core::data_model::event::Event;
use saluki_error::GenericError;
use stringtheory::MetaString;

use crate::sources::otlp::attributes::source::SourceKind;
use crate::sources::otlp::attributes::translator::AttributeTranslator;
use crate::sources::otlp::logs::transform::{
    any_value_to_json, any_value_to_message_string, derive_status, get_first_string_attr_case_insensitive,
    get_string_attribute, get_string_attribute_case_insensitive, to_hex_lower, be_u64_from_last_8,
    be_u64_from_first_8, decode_hex_exact, DDTAGS_ATTR, MESSAGE_KEYS, STATUS_KEYS, TRACE_ID_ATTR_KEYS,
    SPAN_ID_ATTR_KEYS,
};
use crate::sources::otlp::Metrics;
use tracing::info;

/// A translator for converting OTLP logs into DD native logs.
pub struct OtlpLogsTranslator {
    attribute_translator: AttributeTranslator,
}

impl OtlpLogsTranslator {
    pub fn new() -> Self {
        Self {
            attribute_translator: AttributeTranslator::new(),
        }
    }

    /// Translates a batch of OTLP ResourceLogs into a collection of DD native logs.
    pub fn map_logs(&mut self, resource_logs: OtlpResourceLogs, metrics: &Metrics) -> Result<Vec<Event>, GenericError> {
        let mut events = Vec::new();

        let resource = resource_logs.resource.unwrap_or_default();
        let source = self.attribute_translator.resource_to_source(&resource);

        let host: Option<String> = match &source {
            Some(src) if matches!(src.kind, SourceKind::HostnameKind) => Some(src.identifier.clone()),
            _ => None,
        };

        let service: Option<String> = get_string_attribute(&resource.attributes, SERVICE_NAME).map(|s| s.to_string());

        let base_resource_tags: SharedTagSet = self.attribute_translator.tags_from_attributes(&resource.attributes);

        // Build base tags once per resource, ensuring canonical service and adding otel_source
        let mut base_tags_owned = TagSet::default();
        base_tags_owned.merge_missing_shared(&base_resource_tags);
        // Remove any pre-existing service:* tags (e.g., from raw `service` resource attribute)
        base_tags_owned.remove_tags("service");
        // Prefer service.name for service tag
        if let Some(svc) = &service {
            if !svc.is_empty() {
                base_tags_owned.insert_tag(format!("service:{}", svc));
            }
        }
        // Add otel_source tag if a source is detected
        if let Some(src) = &source {
            base_tags_owned.insert_tag(format!("otel_source:{}", src.tag()));
        }
        let base_tags_for_resource: SharedTagSet = base_tags_owned.into_shared();

        for mut scope_logs in resource_logs.scope_logs {
            // Start with curated resource tags only
            let base_tags = base_tags_for_resource.clone();

            info!("\n\nWACK TEST logs_translator: scope has {} attributes and {} records\n\n", scope_logs.scope.as_ref().map(|s| s.attributes.len()).unwrap_or(0), scope_logs.log_records.len());

            for lr in scope_logs.log_records.drain(..) {
                metrics._logs_received().increment(1);

                // Tags start from curated resource tags only
                let mut tags = base_tags.clone();

                // Merge ddtags (append, not overwrite)
                if let Some(ddtags) = get_string_attribute_case_insensitive(&lr.attributes, DDTAGS_ATTR) {
                    let mut extra = TagSet::default();
                    for raw in ddtags.split(',') {
                        let s = raw.trim();
                        if !s.is_empty() {
                            extra.insert_tag(s.to_string());
                        }
                    }
                    tags.extend_from_shared(&extra.into_shared());
                }

                // Host/service fallbacks from record attributes if missing
                let mut host_for_record = host.clone();
                if host_for_record.is_none() {
                    host_for_record = get_string_attribute(&lr.attributes, HOST_NAME).map(|s| s.to_string());
                }
                let mut service_for_record = service.clone();
                if service_for_record.is_none() {
                    service_for_record = get_string_attribute(&lr.attributes, SERVICE_NAME).map(|s| s.to_string());
                }

                // Status derivation
                let status_text_from_attrs =
                    get_first_string_attr_case_insensitive(&lr.attributes, STATUS_KEYS).map(|s| s.to_string());
                let status = derive_status(
                    status_text_from_attrs.as_deref(),
                    lr.severity_text.as_str(),
                    lr.severity_number,
                );
                info!("\n\nWACK TEST logs_translator: status = {:?}\n\n", status);

                // Build additional properties map with resource, scope and record attributes
                let mut additional_properties = HashMap::<String, JsonValue>::new();

                // Helper to insert safely avoiding collisions with reserved top-level fields
                fn safe_insert(map: &mut HashMap<String, JsonValue>, key: &str, value: JsonValue) {
                    if key == "hostname" || key == "service" {
                        map.insert(format!("otel.{}", key), value);
                    } else {
                        map.insert(key.to_string(), value);
                    }
                }

                // Resource attributes
                for kv in &resource.attributes {
                    if let Some(av) = kv.value.as_ref() {
                        let val = match av.value.as_ref() {
                            Some(otlp_common::any_value::Value::StringValue(s)) => JsonValue::String(s.clone()),
                            _ => any_value_to_json(av),
                        };
                        if !val.is_null() {
                            safe_insert(&mut additional_properties, &kv.key, val);
                        }
                    }
                }

                // Scope attributes
                if let Some(scope) = &scope_logs.scope {
                    for kv in &scope.attributes {
                        if let Some(av) = kv.value.as_ref() {
                            let val = match av.value.as_ref() {
                                Some(otlp_common::any_value::Value::StringValue(s)) => JsonValue::String(s.clone()),
                                _ => any_value_to_json(av),
                            };
                            if !val.is_null() {
                                safe_insert(&mut additional_properties, &kv.key, val);
                            }
                        }
                    }
                }

                // Record attributes (skip keys that collide with message/status/tags/correlation)
                fn is_skip_key(key: &str) -> bool {
                    let k = key.to_ascii_lowercase();
                    k == DDTAGS_ATTR
                        || MESSAGE_KEYS.iter().any(|m| k == *m)
                        || STATUS_KEYS.iter().any(|s| k == *s)
                        || TRACE_ID_ATTR_KEYS.iter().any(|t| k == *t)
                        || SPAN_ID_ATTR_KEYS.iter().any(|s| k == *s)
                }

                for kv in &lr.attributes {
                    if is_skip_key(&kv.key) {
                        continue;
                    }
                    if let Some(av) = kv.value.as_ref() {
                        let val = match av.value.as_ref() {
                            Some(otlp_common::any_value::Value::StringValue(s)) => JsonValue::String(s.clone()),
                            _ => any_value_to_json(av),
                        };
                        if !val.is_null() {
                            safe_insert(&mut additional_properties, &kv.key, val);
                        }
                    }
                }

                // Correlation fields: from record bytes first
                if !lr.trace_id.is_empty() {
                    let hex = to_hex_lower(&lr.trace_id);
                    additional_properties.insert("otel.trace_id".to_string(), JsonValue::String(hex));
                    if lr.trace_id.len() >= 8 {
                        let dd = be_u64_from_last_8(&lr.trace_id);
                        additional_properties.insert("dd.trace_id".to_string(), JsonValue::from(dd));
                    }
                } else if let Some(trace_hex) = get_first_string_attr_case_insensitive(&lr.attributes, TRACE_ID_ATTR_KEYS) {
                    if let Some(bytes) = decode_hex_exact(trace_hex, 16) {
                        additional_properties.insert("otel.trace_id".to_string(), JsonValue::String(trace_hex.to_string()));
                        let dd = be_u64_from_last_8(&bytes);
                        additional_properties.insert("dd.trace_id".to_string(), JsonValue::from(dd));
                    }
                }

                if !lr.span_id.is_empty() {
                    let hex = to_hex_lower(&lr.span_id);
                    additional_properties.insert("otel.span_id".to_string(), JsonValue::String(hex));
                    if lr.span_id.len() == 8 {
                        let dd = be_u64_from_first_8(&lr.span_id);
                        additional_properties.insert("dd.span_id".to_string(), JsonValue::from(dd));
                    }
                } else if let Some(span_hex) = get_first_string_attr_case_insensitive(&lr.attributes, SPAN_ID_ATTR_KEYS) {
                    if let Some(bytes) = decode_hex_exact(span_hex, 8) {
                        additional_properties.insert("otel.span_id".to_string(), JsonValue::String(span_hex.to_string()));
                        let dd = be_u64_from_first_8(&bytes);
                        additional_properties.insert("dd.span_id".to_string(), JsonValue::from(dd));
                    }
                }

                // Message: prefer attributes (msg|message|log), else body
                let message_from_attrs =
                    get_first_string_attr_case_insensitive(&lr.attributes, MESSAGE_KEYS).map(|s| s.to_string());
                let message = match message_from_attrs {
                    Some(m) => m,
                    None => lr
                        .body
                        .as_ref()
                        .map(any_value_to_message_string)
                        .unwrap_or_else(String::new),
                };
                info!("\n\nWACK TEST logs_translator: message = '{}'\n\n", message);

                // Timestamp: prefer event time, else observed time; seconds
                let ts_ns = if lr.time_unix_nano != 0 {
                    lr.time_unix_nano
                } else {
                    lr.observed_time_unix_nano
                };
                let ts_s = if ts_ns != 0 { Some(ts_ns / 1_000_000_000) } else { None };
                info!("\n\nWACK TEST logs_translator: ts_ns = {}, ts_s = {:?}\n\n", ts_ns, ts_s);

                // Build Log -> Event::Log
                let log = DdLog::new(message)
                    .with_status(status)
                    .with_timestamp_unix_s(ts_s)
                    .with_hostname(host_for_record.as_deref().map(MetaString::from))
                    .with_service(service_for_record.as_deref().map(MetaString::from))
                    .with_ddtags(tags)
                    .with_additional_properties(Some(additional_properties));

                events.push(Event::Log(log));
            }
        }

        info!("\n\nWACK TEST logs_translator: end map_logs -> {} events\n\n", events.len());
        Ok(events)
    }
}
