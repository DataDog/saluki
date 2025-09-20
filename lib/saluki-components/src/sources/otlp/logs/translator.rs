use opentelemetry_semantic_conventions::resource::{HOST_NAME, SERVICE_NAME};
use otlp_protos::opentelemetry::proto::logs::v1::ResourceLogs as OtlpResourceLogs;
use saluki_context::tags::{SharedTagSet, TagSet};
use saluki_core::data_model::event::Event;
use saluki_error::GenericError;

use crate::sources::otlp::attributes::source::SourceKind;
use crate::sources::otlp::attributes::translator::AttributeTranslator;
use crate::sources::otlp::logs::transform::{get_string_attribute, LogRecordTransformer};
use crate::sources::otlp::Metrics;

/// A translator for converting OTLP logs into DD native logs.
pub struct OtlpLogsTranslator {
    attribute_translator: AttributeTranslator,
    record_transformer: LogRecordTransformer,
    otel_source: String,
}

impl OtlpLogsTranslator {
    pub fn new(otel_source: String) -> Self {
        Self {
            attribute_translator: AttributeTranslator::new(),
            record_transformer: LogRecordTransformer::new(),
            otel_source,
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

        // Build base tags once per resource and add otel_source
        let mut base_tags_owned = TagSet::default();
        base_tags_owned.merge_missing_shared(&self.attribute_translator.tags_from_attributes(&resource.attributes));
        base_tags_owned.insert_tag(format!("otel_source:{}", self.otel_source));

        let base_tags_for_resource: SharedTagSet = base_tags_owned.into_shared();

        for mut scope_logs in resource_logs.scope_logs {
            for lr in scope_logs.log_records.drain(..) {
                metrics._logs_received().increment(1);

                // Host/service fallbacks from record attributes if missing
                let mut host_for_record = host.clone();
                if host_for_record.is_none() {
                    host_for_record = get_string_attribute(&lr.attributes, HOST_NAME).map(|s| s.to_string());
                }
                let mut service_for_record = service.clone();
                if service_for_record.is_none() {
                    service_for_record = get_string_attribute(&lr.attributes, SERVICE_NAME).map(|s| s.to_string());
                }

                let log = self.record_transformer.transform(
                    lr,
                    &resource,
                    scope_logs.scope.as_ref(),
                    host_for_record,
                    service_for_record,
                    &base_tags_for_resource,
                );

                events.push(Event::Log(log));
            }
        }
        Ok(events)
    }
}
