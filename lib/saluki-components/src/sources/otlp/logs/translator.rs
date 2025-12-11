use std::vec::IntoIter;

use opentelemetry_semantic_conventions::resource::{HOST_NAME, SERVICE_NAME};
use otlp_protos::opentelemetry::proto::common::v1::InstrumentationScope;
use otlp_protos::opentelemetry::proto::logs::v1::{LogRecord, ResourceLogs as OtlpResourceLogs, ScopeLogs};
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use saluki_context::origin::OriginTagsResolver;
use saluki_context::tags::{SharedTagSet, Tag};
use saluki_core::data_model::event::Event;
use stringtheory::MetaString;

use super::super::attributes::raw_origin_from_attributes;
use crate::sources::otlp::attributes::source::SourceKind;
use crate::sources::otlp::attributes::tags_from_attributes;
use crate::common::otlp::util::{get_string_attribute, resource_to_source};
use crate::sources::otlp::logs::transform::transform_log_record;
use crate::sources::otlp::origin::OtlpOriginTagResolver;

static OTEL_SOURCE_TAG: Tag = Tag::from_static("otel_source:datadog_agent");

/// A translator for converting OTLP logs into DD native logs.
pub struct OtlpLogsTranslator {
    resource: Resource,
    host: Option<MetaString>,
    service: Option<MetaString>,
    attribute_tags: SharedTagSet,
    scope_logs: IntoIter<ScopeLogs>,
    current_scope_logs: Option<(Option<InstrumentationScope>, IntoIter<LogRecord>)>,
}

impl OtlpLogsTranslator {
    pub fn from_resource_logs(
        resource_logs: OtlpResourceLogs, origin_tag_resolver: Option<&OtlpOriginTagResolver>,
    ) -> Self {
        let resource = resource_logs.resource.unwrap_or_default();
        let source = resource_to_source(&resource);
        let host = match &source {
            Some(src) if matches!(src.kind, SourceKind::HostnameKind) => {
                Some(MetaString::from(src.identifier.as_str()))
            }
            _ => None,
        };

        let service = get_string_attribute(&resource.attributes, SERVICE_NAME).map(MetaString::from);
        let mut attribute_tags = tags_from_attributes(&resource.attributes);
        attribute_tags.insert_tag(OTEL_SOURCE_TAG.clone());
        let mut shared_attribute_tags = attribute_tags.into_shared();
        let origin = raw_origin_from_attributes(&resource.attributes);
        if let Some(resolver) = origin_tag_resolver {
            let origin_tags = resolver.resolve_origin_tags(origin);
            shared_attribute_tags.extend_from_shared(&origin_tags);
        }

        Self {
            resource,
            host,
            service,
            attribute_tags: shared_attribute_tags,
            scope_logs: resource_logs.scope_logs.into_iter(),
            current_scope_logs: None,
        }
    }

    fn next_log(&mut self) -> Option<Event> {
        loop {
            let (current_scope, current_log_records) = match self.current_scope_logs.as_mut() {
                Some(current) => current,
                None => match self.scope_logs.next() {
                    Some(scope_logs) => {
                        self.current_scope_logs = Some((scope_logs.scope, scope_logs.log_records.into_iter()));
                        continue;
                    }
                    None => return None,
                },
            };

            match current_log_records.next() {
                Some(log_record) => {
                    let record_host = self
                        .host
                        .clone()
                        .or_else(|| get_string_attribute(&log_record.attributes, HOST_NAME).map(MetaString::from));
                    let record_service = self
                        .service
                        .clone()
                        .or_else(|| get_string_attribute(&log_record.attributes, SERVICE_NAME).map(MetaString::from));

                    let log = transform_log_record(
                        log_record,
                        &self.resource,
                        current_scope.as_ref(),
                        record_host,
                        record_service,
                        self.attribute_tags.clone(),
                    );

                    return Some(Event::Log(log));
                }
                None => {
                    self.current_scope_logs = None;
                }
            }
        }
    }
}

impl Iterator for OtlpLogsTranslator {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_log()
    }
}

#[cfg(test)]
mod tests {
    use otlp_common::any_value::Value::{KvlistValue, StringValue};
    use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common};
    use otlp_protos::opentelemetry::proto::logs::v1 as otlp_logs_v1;
    use otlp_protos::opentelemetry::proto::resource::v1 as otlp_resource_v1;
    use saluki_core::data_model::event::log::{Log, LogStatus};
    use saluki_core::data_model::event::Event;
    use serde_json::Value as JsonValue;

    use super::OtlpLogsTranslator;
    use super::SERVICE_NAME;
    use crate::sources::otlp::logs::transform::DDTAGS_ATTR;

    const TRACE_ID: [u8; 16] = [
        0x08, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00,
    ];
    const SPAN_ID: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00];

    fn key_value_string_to_otlp_common_key_value(key: &str, value: &str) -> otlp_common::KeyValue {
        otlp_common::KeyValue {
            key: key.to_string(),
            value: Some(otlp_common::AnyValue {
                value: Some(StringValue(value.to_string())),
            }),
        }
    }

    fn string_to_any_value(val: &str) -> otlp_common::AnyValue {
        otlp_common::AnyValue {
            value: Some(StringValue(val.to_string())),
        }
    }

    fn any_value_vec_to_map(entries: Vec<(&str, otlp_common::AnyValue)>) -> otlp_common::AnyValue {
        let kvs: Vec<otlp_common::KeyValue> = entries
            .into_iter()
            .map(|(k, v)| otlp_common::KeyValue {
                key: k.to_string(),
                value: Some(v),
            })
            .collect();
        otlp_common::AnyValue {
            value: Some(KvlistValue(otlp_common::KeyValueList { values: kvs })),
        }
    }

    fn empty_resource() -> otlp_resource_v1::Resource {
        otlp_resource_v1::Resource {
            attributes: vec![],
            dropped_attributes_count: 0,
            entity_refs: vec![],
        }
    }

    // Helper: run a single LogRecord through the translator with given resource/scope and return the Log event.
    fn translate_log(
        lr: otlp_logs_v1::LogRecord, resource: otlp_resource_v1::Resource,
        scope: Option<otlp_common::InstrumentationScope>,
    ) -> Log {
        let scope_logs = otlp_logs_v1::ScopeLogs {
            scope,
            log_records: vec![lr],
            schema_url: String::new(),
        };
        let resource_logs = otlp_logs_v1::ResourceLogs {
            resource: Some(resource),
            scope_logs: vec![scope_logs],
            schema_url: String::new(),
        };

        let translator = OtlpLogsTranslator::from_resource_logs(resource_logs, None);

        let mut events = translator.collect::<Vec<_>>();
        assert_eq!(events.len(), 1);

        match events.remove(0) {
            Event::Log(log) => log,
            other => panic!("expected log event, got {:?}", other),
        }
    }

    #[test]
    fn test_transform_trace_and_span_conversion() {
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: Some(otlp_common::AnyValue {
                value: Some(StringValue("test message".to_string())),
            }),
            attributes: vec![key_value_string_to_otlp_common_key_value("app", "test")],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: TRACE_ID.to_vec(),
            span_id: SPAN_ID.to_vec(),
            event_name: String::new(),
        };
        let log = translate_log(lr.clone(), empty_resource(), None);

        assert_eq!(log.message(), "test message");
        assert_eq!(log.status(), Some(LogStatus::Debug));
        let props = log.additional_properties();
        assert_eq!(props.get("app"), Some(&JsonValue::String("test".to_string())));
        assert_eq!(
            props.get("otel.severity_number"),
            Some(&JsonValue::String("5".to_string()))
        );
        assert_eq!(
            props.get("otel.trace_id"),
            Some(&JsonValue::String("0802030405060708000000000a000000".to_string()))
        );
        assert_eq!(
            props.get("dd.trace_id"),
            Some(&JsonValue::String("167772160".to_string()))
        );
        assert_eq!(
            props.get("otel.span_id"),
            Some(&JsonValue::String("000000000a000000".to_string()))
        );
        assert_eq!(
            props.get("dd.span_id"),
            Some(&JsonValue::String("167772160".to_string()))
        );
    }

    #[test]
    fn test_transform_resource_service() {
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![key_value_string_to_otlp_common_key_value("app", "test")],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };

        let resource = otlp_resource_v1::Resource {
            attributes: vec![key_value_string_to_otlp_common_key_value(SERVICE_NAME, "test_service")],
            dropped_attributes_count: 0,
            entity_refs: vec![],
        };

        let log = translate_log(lr, resource, None);
        assert_eq!(log.service(), "test_service");
        let props = log.additional_properties();
        assert_eq!(
            props.get("service.name"),
            Some(&JsonValue::String("test_service".to_string()))
        );
        assert_eq!(props.get("app"), Some(&JsonValue::String("test".into())));
    }

    #[test]
    fn test_transform_ddtags() {
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![
                key_value_string_to_otlp_common_key_value("app", "test"),
                key_value_string_to_otlp_common_key_value(DDTAGS_ATTR, "foo:bar"),
            ],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };

        let resource = otlp_resource_v1::Resource {
            attributes: vec![key_value_string_to_otlp_common_key_value(
                SERVICE_NAME,
                "test_service_name",
            )],
            dropped_attributes_count: 0,
            entity_refs: vec![],
        };
        let log = translate_log(lr, resource, None);
        let tags = log.tags();
        assert!(
            tags.has_tag("foo:bar")
                && tags.has_tag("service:test_service_name")
                && tags.has_tag("otel_source:datadog_agent")
        );
        assert_eq!(log.service(), "test_service_name");
    }

    #[test]
    fn test_transform_service_from_log_attribute() {
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![
                key_value_string_to_otlp_common_key_value("app", "test"),
                key_value_string_to_otlp_common_key_value(SERVICE_NAME, "test_service_name"),
            ],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let log = translate_log(lr, empty_resource(), None);
        assert_eq!(log.service(), "test_service_name");
        let props = log.additional_properties();
        assert_eq!(
            props.get("service.name"),
            Some(&JsonValue::String("test_service_name".to_string()))
        );
    }

    #[test]
    fn test_transform_trace_from_attributes() {
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![
                key_value_string_to_otlp_common_key_value("spanid", "2e26da881214cd7c"),
                key_value_string_to_otlp_common_key_value("traceid", "437ab4d83468c540bb0f3398a39faa59"),
            ],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let log = translate_log(lr, empty_resource(), None);
        let props = log.additional_properties();
        assert_eq!(
            props.get("otel.trace_id"),
            Some(&JsonValue::String("437ab4d83468c540bb0f3398a39faa59".to_string()))
        );
        assert_eq!(
            props.get("otel.span_id"),
            Some(&JsonValue::String("2e26da881214cd7c".to_string()))
        );
        assert_eq!(props.get("dd.span_id"), Some(&JsonValue::from("3325585652813450620")));
        assert_eq!(props.get("dd.trace_id"), Some(&JsonValue::from("13479048940416379481")));
    }

    #[test]
    fn test_transform_invalid_trace() {
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![
                key_value_string_to_otlp_common_key_value("app", "test"),
                key_value_string_to_otlp_common_key_value("spanid", "2e26da881214cd7c"),
                key_value_string_to_otlp_common_key_value("traceid", "invalidtraceid"),
                key_value_string_to_otlp_common_key_value(SERVICE_NAME, "otlp_col"),
            ],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let log = translate_log(lr, empty_resource(), None);
        let props = log.additional_properties();
        assert!(props.get("otel.trace_id").is_none());
        assert!(props.get("dd.trace_id").is_none());
        assert_eq!(
            props.get("otel.span_id"),
            Some(&JsonValue::String("2e26da881214cd7c".to_string()))
        );
        assert_eq!(props.get("dd.span_id"), Some(&JsonValue::from("3325585652813450620")));
    }

    #[test]
    fn test_transform_trace_from_attributes_size_error() {
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![
                key_value_string_to_otlp_common_key_value("app", "test"),
                key_value_string_to_otlp_common_key_value("spanid", "2023675201651514964"),
                key_value_string_to_otlp_common_key_value(
                    "traceid",
                    "eb068afe5e53704f3b0dc3d3e1e397cb760549a7b58547db4f1dee845d9101f8db1ccf8fdd0976a9112f",
                ),
                key_value_string_to_otlp_common_key_value(SERVICE_NAME, "otlp_col"),
            ],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let log = translate_log(lr, empty_resource(), None);
        let props = log.additional_properties();
        assert!(props.get("otel.trace_id").is_none());
        assert!(props.get("dd.trace_id").is_none());
        assert!(props.get("otel.span_id").is_none());
        assert!(props.get("dd.span_id").is_none());
    }

    #[test]
    fn test_derive_status_precedence() {
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: "alert".to_string(),
            body: None,
            attributes: vec![key_value_string_to_otlp_common_key_value("app", "test")],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: vec![0; 16],
            span_id: vec![0; 8],
            event_name: String::new(),
        };
        let log = translate_log(lr, empty_resource(), None);
        assert_eq!(log.status(), Some(LogStatus::Alert));
        let props = log.additional_properties();
        assert_eq!(
            props.get("otel.severity_text"),
            Some(&JsonValue::String("alert".to_string()))
        );
        assert_eq!(
            props.get("otel.severity_number"),
            Some(&JsonValue::String("5".to_string()))
        );
    }

    #[test]
    fn test_transform_body_message_comes_from_body() {
        let body = string_to_any_value("This is log");
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 13,
            severity_text: String::new(),
            body: Some(body),
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let log = translate_log(lr, empty_resource(), None);
        assert_eq!(log.message(), "This is log");
        assert_eq!(log.status(), Some(LogStatus::Warning));
        let props = log.additional_properties();
        assert_eq!(
            props.get("otel.severity_number"),
            Some(&JsonValue::String("13".to_string()))
        );
    }

    #[test]
    fn test_log_level_attribute_sets_status() {
        let body = string_to_any_value("This is log");
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 0,
            severity_text: String::new(),
            body: Some(body),
            attributes: vec![
                key_value_string_to_otlp_common_key_value("app", "test"),
                key_value_string_to_otlp_common_key_value("level", "error"),
            ],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let log = translate_log(lr, empty_resource(), None);
        assert_eq!(log.message(), "This is log");
        assert_eq!(log.status(), Some(LogStatus::Error));
    }

    #[test]
    fn test_resource_attributes_in_additional_properties() {
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![key_value_string_to_otlp_common_key_value("app", "test")],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let resource = otlp_resource_v1::Resource {
            attributes: vec![
                key_value_string_to_otlp_common_key_value(SERVICE_NAME, "test_service_name"),
                key_value_string_to_otlp_common_key_value("key", "val"),
            ],
            dropped_attributes_count: 0,
            entity_refs: vec![],
        };
        let log = translate_log(lr, resource, None);
        let props = log.additional_properties();
        assert_eq!(props.get("key"), Some(&JsonValue::String("val".to_string())));
        assert_eq!(
            props.get("service.name"),
            Some(&JsonValue::String("test_service_name".to_string()))
        );
        assert_eq!(props.get("app"), Some(&JsonValue::String("test".into())))
    }

    #[test]
    fn test_dd_hostname_and_service_preserved() {
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let resource = otlp_resource_v1::Resource {
            attributes: vec![
                key_value_string_to_otlp_common_key_value("hostname", "example_host"),
                key_value_string_to_otlp_common_key_value("service", "test_service"),
            ],
            dropped_attributes_count: 0,
            entity_refs: vec![],
        };
        let log = translate_log(lr, resource, None);
        let props = log.additional_properties();
        assert_eq!(
            props.get("otel.service"),
            Some(&JsonValue::String("test_service".to_string()))
        );
        assert_eq!(
            props.get("otel.hostname"),
            Some(&JsonValue::String("example_host".to_string()))
        );
    }

    #[test]
    fn test_nestings() {
        let nested = any_value_vec_to_map(vec![
            (
                "nest1",
                any_value_vec_to_map(vec![("nest2", string_to_any_value("val"))]),
            ),
            (
                "nest3",
                any_value_vec_to_map(vec![(
                    "nest4",
                    any_value_vec_to_map(vec![("nest5", string_to_any_value("val2"))]),
                )]),
            ),
            ("nest6", string_to_any_value("val3")),
        ]);

        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 0,
            severity_text: String::new(),
            body: None,
            attributes: vec![otlp_common::KeyValue {
                key: "root".to_string(),
                value: Some(nested),
            }],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let log = translate_log(lr, empty_resource(), None);
        let props = log.additional_properties();
        assert_eq!(
            props.get("root.nest1.nest2"),
            Some(&JsonValue::String("val".to_string()))
        );
        assert_eq!(
            props.get("root.nest3.nest4.nest5"),
            Some(&JsonValue::String("val2".to_string()))
        );
        assert_eq!(props.get("root.nest6"), Some(&JsonValue::String("val3".to_string())));
        assert_eq!(log.status(), Some(LogStatus::Trace));
    }

    #[test]
    fn test_too_many_nestings() {
        // Nested map deeper than MAX_DEPTH(10)
        let deep = any_value_vec_to_map(vec![(
            "nest2",
            any_value_vec_to_map(vec![(
                "nest3",
                any_value_vec_to_map(vec![(
                    "nest4",
                    any_value_vec_to_map(vec![(
                        "nest5",
                        any_value_vec_to_map(vec![
                            (
                                "nest6",
                                any_value_vec_to_map(vec![(
                                    "nest7",
                                    any_value_vec_to_map(vec![(
                                        "nest8",
                                        any_value_vec_to_map(vec![(
                                            "nest9",
                                            any_value_vec_to_map(vec![(
                                                "nest10",
                                                any_value_vec_to_map(vec![(
                                                    "nest11",
                                                    any_value_vec_to_map(vec![("nest12", string_to_any_value("ok"))]),
                                                )]),
                                            )]),
                                        )]),
                                    )]),
                                )]),
                            ),
                            (
                                "nest14",
                                any_value_vec_to_map(vec![("nest15", string_to_any_value("ok2"))]),
                            ),
                        ]),
                    )]),
                )]),
            )]),
        )]);
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 0,
            severity_text: String::new(),
            body: None,
            attributes: vec![otlp_common::KeyValue {
                key: "nest1".to_string(),
                value: Some(deep),
            }],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let log = translate_log(lr, empty_resource(), None);
        let props = log.additional_properties();
        assert_eq!(
            props.get("nest1.nest2.nest3.nest4.nest5.nest6.nest7.nest8.nest9.nest10"),
            Some(&JsonValue::String("{\"nest11\":{\"nest12\":\"ok\"}}".to_string()))
        );
        assert_eq!(
            props.get("nest1.nest2.nest3.nest4.nest5.nest14.nest15"),
            Some(&JsonValue::String("ok2".to_string()))
        );
    }

    #[test]
    fn test_timestamps_formatted_properly() {
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 1_700_499_303_397_000_000u64,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let log = translate_log(lr, empty_resource(), None);
        let props = log.additional_properties();
        assert_eq!(
            props.get("otel.timestamp"),
            Some(&JsonValue::String("1700499303397000000".to_string()))
        );
        assert_eq!(
            props.get("@timestamp"),
            Some(&JsonValue::String("2023-11-20T16:55:03.397Z".to_string()))
        );
    }

    #[test]
    fn test_scope_attributes_included() {
        let lr = otlp_logs_v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: Some(string_to_any_value("hello world")),
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let mut scope = otlp_common::InstrumentationScope::default();
        scope.attributes.push(key_value_string_to_otlp_common_key_value(
            "otelcol.component.id",
            "otlp",
        ));
        scope.attributes.push(key_value_string_to_otlp_common_key_value(
            "otelcol.component.kind",
            "Receiver",
        ));
        let log = translate_log(lr, empty_resource(), Some(scope));
        assert_eq!(log.message(), "hello world");
        let props = log.additional_properties();
        assert_eq!(
            props.get("otelcol.component.id"),
            Some(&JsonValue::String("otlp".to_string()))
        );
        assert_eq!(
            props.get("otelcol.component.kind"),
            Some(&JsonValue::String("Receiver".to_string()))
        );
    }
}
