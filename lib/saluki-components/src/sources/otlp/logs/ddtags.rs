//! The `ddtags` of OTLP log records, from the custom infra tags that `logs_tags_as_ddtags` diverts.
//!
//! This matches the logs part of the Datadog Agent's infra attributes processor (`processLogs` and
//! `writeDdtagsToLogRecords`,
//! <https://github.com/DataDog/datadog-agent/blob/d22eedcd06ef9fcedad3d43bcab45795068b8ecc/comp/otelcol/otlp/components/processor/infraattributesprocessor/logs.go>).

use otlp_protos::opentelemetry::proto::common::v1::KeyValue;
use saluki_core::data_model::tags::Tag;

use crate::common::otlp::util::{first_attribute, put_str_attribute};
use crate::common::otlp::value::otlp_value_as_string;

const DDTAGS: &str = "ddtags";

/// Returns the diverted tags of a resource as one `ddtags` value: sorted and comma-separated.
///
/// Returns `None` if there are no tags.
pub(super) fn ddtags_value(mut tags: Vec<Tag>) -> Option<String> {
    if tags.is_empty() {
        return None;
    }
    tags.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    let len = tags.iter().map(|tag| tag.as_str().len() + 1).sum::<usize>() - 1;
    let mut value = String::with_capacity(len);
    for tag in &tags {
        if !value.is_empty() {
            value.push(',');
        }
        value.push_str(tag.as_str());
    }
    Some(value)
}

/// Adds `tags` to the `ddtags` attribute of a log record, as `writeDdtagsToLogRecords` does.
///
/// An existing `ddtags` value that isn't empty keeps its tags, and `tags` follow them after a comma. Otherwise, `tags`
/// replace the value.
///
/// The existing value converts with `otlp_value_as_string`, as `AsString` does. An array or map value differs from
/// `pdata` until #3397 lands.
pub(super) fn merge_ddtags(record_attributes: &mut Vec<KeyValue>, tags: &str) {
    let merged = match first_attribute(record_attributes, DDTAGS).map(otlp_value_as_string) {
        Some(existing) if !existing.is_empty() => format!("{existing},{tags}"),
        _ => tags.to_owned(),
    };
    put_str_attribute(record_attributes, DDTAGS, merged);
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use otlp_protos::opentelemetry::proto::common::v1::{any_value::Value, AnyValue};
    use saluki_core::data_model::origin::OriginTagCardinality;
    use saluki_env::workload::{providers::TestWorkloadProvider, EntityId};

    use super::*;
    use crate::common::otlp::infra_attributes::{process_tags, ContainerTagPromotion, CustomTags};
    use crate::common::otlp::origin::OtlpOriginTagResolver;

    fn attribute(key: &str, value: Value) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue { value: Some(value) }),
        }
    }

    fn string(key: &str, value: &str) -> KeyValue {
        attribute(key, Value::StringValue(value.to_owned()))
    }

    fn strings(attributes: &[(&str, &str)]) -> Vec<KeyValue> {
        attributes.iter().map(|(key, value)| string(key, value)).collect()
    }

    /// Returns the attributes as a map, for comparisons that don't depend on attribute order.
    fn as_map(attributes: &[KeyValue]) -> BTreeMap<String, Option<Value>> {
        attributes
            .iter()
            .map(|attribute| {
                let value = attribute.value.as_ref().and_then(|value| value.value.clone());
                (attribute.key.clone(), value)
            })
            .collect()
    }

    fn string_map(attributes: &[(&str, &str)]) -> BTreeMap<String, Option<Value>> {
        as_map(&strings(attributes))
    }

    #[test]
    fn ddtags_value_sorts_and_joins_the_tags() {
        assert_eq!(ddtags_value(vec![]), None);
        let tags = vec![Tag::from("zeta:1"), Tag::from("alpha:2"), Tag::from("mid:3")];
        assert_eq!(ddtags_value(tags).as_deref(), Some("alpha:2,mid:3,zeta:1"));
    }

    /// `writeDdtagsToLogRecords` converts an existing `ddtags` with `AsString` before it appends the new tags.
    #[test]
    fn merge_ddtags_converts_an_existing_value_as_pdata_does() {
        let cases = [
            ("double", Value::DoubleValue(1.5), "1.5,new:tag"),
            ("integer", Value::IntValue(7), "7,new:tag"),
            ("bytes", Value::BytesValue(b"abc".to_vec()), "YWJj,new:tag"),
        ];
        for (name, existing, expected) in cases {
            let mut record_attributes = vec![attribute("ddtags", existing)];
            merge_ddtags(&mut record_attributes, "new:tag");
            assert_eq!(
                first_attribute(&record_attributes, "ddtags")
                    .map(otlp_value_as_string)
                    .as_deref(),
                Some(expected),
                "case: {name}"
            );
        }
    }

    /// Ports `logsDDTagsTests` from `TestLogsTagsAsDDTags`
    /// (<https://github.com/DataDog/datadog-agent/blob/d22eedcd06ef9fcedad3d43bcab45795068b8ecc/comp/otelcol/otlp/components/processor/infraattributesprocessor/logs_ddtags_test.go>).
    #[test]
    fn logs_tags_as_ddtags() {
        struct Case {
            name: &'static str,
            tags_as_ddtags: bool,
            tagger_tags: &'static [&'static str],
            existing_ddtags: Option<&'static str>,
            resource_attributes: &'static [(&'static str, &'static str)],
            ddtags: Option<&'static str>,
        }
        let cases = [
            Case {
                name: "disabled (default), custom tag stays a resource attribute",
                tags_as_ddtags: false,
                tagger_tags: &["team:platform"],
                existing_ddtags: None,
                resource_attributes: &[("container.id", "test"), ("team", "platform")],
                ddtags: None,
            },
            Case {
                name: "enabled, custom tag becomes ddtags instead of a resource attribute",
                tags_as_ddtags: true,
                tagger_tags: &["team:platform"],
                existing_ddtags: None,
                resource_attributes: &[("container.id", "test")],
                ddtags: Some("team:platform"),
            },
            Case {
                name: "enabled, DD-convention key (pod_name) stays a resource attribute",
                tags_as_ddtags: true,
                tagger_tags: &["pod_name:foo"],
                existing_ddtags: None,
                resource_attributes: &[("container.id", "test"), ("pod_name", "foo")],
                ddtags: None,
            },
            Case {
                name: "enabled, KubernetesDDTags-only key (kube_service) stays a resource attribute",
                tags_as_ddtags: true,
                tagger_tags: &["kube_service:mysvc"],
                existing_ddtags: None,
                resource_attributes: &[("container.id", "test"), ("kube_service", "mysvc")],
                ddtags: None,
            },
            Case {
                name: "enabled, USM tag (service) flows through USM path, not ddtags",
                tags_as_ddtags: true,
                tagger_tags: &["service:svc"],
                existing_ddtags: None,
                resource_attributes: &[("container.id", "test"), ("service.name", "svc")],
                ddtags: None,
            },
            Case {
                name: "enabled, custom tag is merged with pre-existing ddtags log record attribute",
                tags_as_ddtags: true,
                tagger_tags: &["team:platform"],
                existing_ddtags: Some("user_set:tag"),
                resource_attributes: &[("container.id", "test")],
                ddtags: Some("user_set:tag,team:platform"),
            },
            Case {
                name: "enabled, multiple custom tags produce deterministically sorted ddtags",
                tags_as_ddtags: true,
                tagger_tags: &["zeta:1", "alpha:2", "mid:3"],
                existing_ddtags: None,
                resource_attributes: &[("container.id", "test")],
                ddtags: Some("alpha:2,mid:3,zeta:1"),
            },
            Case {
                name: "enabled, no custom tags leaves pre-existing ddtags untouched",
                tags_as_ddtags: true,
                tagger_tags: &["pod_name:foo"],
                existing_ddtags: Some("user_set:tag"),
                resource_attributes: &[("container.id", "test"), ("pod_name", "foo")],
                ddtags: Some("user_set:tag"),
            },
        ];

        for case in cases {
            let provider = TestWorkloadProvider::with_entity(EntityId::Container("test".into()), case.tagger_tags);
            let resolver = OtlpOriginTagResolver::new(Arc::new(provider));
            let mut attributes = strings(&[("container.id", "test")]);
            let mut record_attributes = case
                .existing_ddtags
                .map(|ddtags| strings(&[("ddtags", ddtags)]))
                .unwrap_or_default();

            let custom_tags = if case.tags_as_ddtags {
                CustomTags::DdTags
            } else {
                CustomTags::Attributes(ContainerTagPromotion::Off)
            };
            let tags = process_tags(&mut attributes, &resolver, OriginTagCardinality::Low, custom_tags);
            if let Some(ddtags) = ddtags_value(tags) {
                merge_ddtags(&mut record_attributes, &ddtags);
            }

            assert_eq!(
                as_map(&attributes),
                string_map(case.resource_attributes),
                "case: {}",
                case.name
            );
            assert_eq!(
                first_attribute(&record_attributes, "ddtags")
                    .map(otlp_value_as_string)
                    .as_deref(),
                case.ddtags,
                "case: {}",
                case.name
            );
        }
    }

    #[test]
    fn merge_ddtags_replaces_an_empty_value_and_merges_any_other_type() {
        let cases = [
            ("empty string", vec![string("ddtags", "")], "new:tag"),
            (
                "valueless",
                vec![KeyValue {
                    key: "ddtags".into(),
                    value: None,
                }],
                "new:tag",
            ),
            ("integer", vec![attribute("ddtags", Value::IntValue(5))], "5,new:tag"),
            (
                "first of two",
                vec![string("ddtags", "first:tag"), string("ddtags", "second:tag")],
                "first:tag,new:tag",
            ),
        ];
        for (name, mut record_attributes, expected) in cases {
            merge_ddtags(&mut record_attributes, "new:tag");
            assert_eq!(
                first_attribute(&record_attributes, "ddtags")
                    .map(otlp_value_as_string)
                    .as_deref(),
                Some(expected),
                "case: {name}"
            );
        }
    }
}
