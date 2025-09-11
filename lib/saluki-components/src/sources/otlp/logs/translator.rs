use opentelemetry_semantic_conventions::resource::{HOST_NAME, SERVICE_NAME};
use otlp_protos::opentelemetry::proto::logs::v1::ResourceLogs as OtlpResourceLogs;
use saluki_context::tags::{SharedTagSet, TagSet};
use saluki_core::data_model::event::log::Log as DdLog;
use saluki_core::data_model::event::Event;
use saluki_error::GenericError;
use stringtheory::MetaString;

use crate::sources::otlp::attributes::source::SourceKind;
use crate::sources::otlp::attributes::translator::AttributeTranslator;
use crate::sources::otlp::logs::transform::{
    add_trace_span_tags_from_attributes, add_trace_span_tags_from_record, any_value_scalar_or_string,
    any_value_to_message_string, collect_flattened_attr_tags, derive_status, get_first_string_attr_case_insensitive,
    get_string_attribute, get_string_attribute_case_insensitive, DDTAGS_ATTR, MESSAGE_KEYS, STATUS_KEYS,
};
use crate::sources::otlp::metrics::config::OtlpTranslatorConfig;
use crate::sources::otlp::Metrics;

/// A translator for converting OTLP logs into DD native logs.
pub struct OtlpLogsTranslator {
    config: OtlpTranslatorConfig,
    attribute_translator: AttributeTranslator,
}

impl OtlpLogsTranslator {
    /// Creates a new OTLP Logs translator.
    pub fn new(config: OtlpTranslatorConfig) -> Self {
        Self {
            config,
            attribute_translator: AttributeTranslator::new(),
        }
    }

    /// Translates a batch of OTLP `ResourceLogs` into a collection of Saluki `Event::Log`s.
    pub fn map_logs(&mut self, resource_logs: OtlpResourceLogs, metrics: &Metrics) -> Result<Vec<Event>, GenericError> {
        let mut events = Vec::new();

        let resource = resource_logs.resource.unwrap_or_default();
        let source = self.attribute_translator.resource_to_source(&resource);

        // Hostname (resource-level)
        let host: Option<String> = match &source {
            Some(src) if matches!(src.kind, SourceKind::HostnameKind) => Some(src.identifier.clone()),
            _ => None,
        };

        // Service (resource-level)
        let service: Option<String> = get_string_attribute(&resource.attributes, SERVICE_NAME).map(|s| s.to_string());

        // Resource tags from attributes
        let base_resource_tags: SharedTagSet = self.attribute_translator.tags_from_attributes(&resource.attributes);

        // Additionally mirror Agent: copy resource attributes as tags, but avoid clobbering hostname/service fields.
        let mut resource_extra_tags = TagSet::default();
        for kv in &resource.attributes {
            if let Some(val) = kv.value.as_ref().and_then(|v| v.value.as_ref()) {
                let k = kv.key.as_str();
                let v_str = any_value_scalar_or_string(val);
                if v_str.is_empty() {
                    continue;
                }
                if k == "hostname" {
                    resource_extra_tags.insert_tag(format!("otel.hostname:{}", v_str));
                } else if k == "service" {
                    resource_extra_tags.insert_tag(format!("otel.service:{}", v_str));
                } else {
                    resource_extra_tags.insert_tag(format!("{}:{}", k, v_str));
                }
            }
        }
        let resource_extra_tags: SharedTagSet = resource_extra_tags.into_shared();

        for mut scope_logs in resource_logs.scope_logs {
            // Start with resource tags
            let mut base_tags = base_resource_tags.clone();

            // Optionally include instrumentation scope or library metadata as tags
            if self.config.instrumentation_scope_metadata_as_tags {
                if let Some(scope) = &scope_logs.scope {
                    let scope_tags = crate::sources::otlp::metrics::internal::instrumentationscope::
                        tags_from_instrumentation_scope_metadata(scope);
                    let mut ts = TagSet::default();
                    for t in scope_tags {
                        ts.insert_tag(t);
                    }
                    base_tags.extend_from_shared(&ts.into_shared());
                }
            } else if self.config.instrumentation_library_metadata_as_tags {
                if let Some(scope) = &scope_logs.scope {
                    let lib_tags = crate::sources::otlp::metrics::internal::instrumentationlibrary::
                        tags_from_instrumentation_library_metadata(scope);
                    let mut ts = TagSet::default();
                    for t in lib_tags {
                        ts.insert_tag(t);
                    }
                    base_tags.extend_from_shared(&ts.into_shared());
                }
            }

            // Always include raw scope attributes (Agent copies into additional properties)
            if let Some(scope) = &scope_logs.scope {
                let mut ts = TagSet::default();
                for kv in &scope.attributes {
                    if let Some(val) = kv.value.as_ref().and_then(|v| v.value.as_ref()) {
                        let v_str = any_value_scalar_or_string(val);
                        if !v_str.is_empty() {
                            ts.insert_tag(format!("{}:{}", kv.key, v_str));
                        }
                    }
                }
                base_tags.extend_from_shared(&ts.into_shared());
            }

            for lr in scope_logs.log_records.drain(..) {
                metrics._logs_received().increment(1);

                // Merge tags: resource + resource extras + flattened record attrs
                let mut tags = base_tags.clone();
                tags.extend_from_shared(&resource_extra_tags);

                let mut flattened_attr_tags = TagSet::default();
                collect_flattened_attr_tags(&lr.attributes, &mut flattened_attr_tags);
                tags.extend_from_shared(&flattened_attr_tags.into_shared());

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

                // Trace/span ids: from record bytes first
                add_trace_span_tags_from_record(&lr, &mut tags);

                // Trace/span ids: from attributes if not set yet
                add_trace_span_tags_from_attributes(&lr.attributes, &mut tags);

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

                // Timestamp: prefer event time, else observed time; seconds
                let ts_ns = if lr.time_unix_nano != 0 {
                    lr.time_unix_nano
                } else {
                    lr.observed_time_unix_nano
                };
                let ts_s = if ts_ns != 0 { Some(ts_ns / 1_000_000_000) } else { None };

                // Build Log -> Event::Log
                let log = DdLog::new(message)
                    .with_status(status)
                    .with_timestamp_unix_s(ts_s)
                    .with_hostname(host_for_record.as_deref().map(MetaString::from))
                    .with_service(service_for_record.as_deref().map(MetaString::from))
                    .with_ddtags(tags);

                events.push(Event::Log(log));
            }
        }

        Ok(events)
    }
}
