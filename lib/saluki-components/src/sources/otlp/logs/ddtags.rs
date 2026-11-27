//! The `ddtags` of OTLP log records, from the custom infra tags that [`CustomTags::DdTags`] diverts.
//!
//! [`CustomTags::DdTags`]: crate::common::otlp::infra_attributes::CustomTags::DdTags
//!
//! This matches the logs part of the Datadog Agent's infra attributes processor (`processLogs` and
//! `writeDdtagsToLogRecords`,
//! <https://github.com/DataDog/datadog-agent/blob/d22eedcd06ef9fcedad3d43bcab45795068b8ecc/comp/otelcol/otlp/components/processor/infraattributesprocessor/logs.go>).

use std::borrow::Cow;

use saluki_core::data_model::tags::Tag;

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

/// The `ddtags` attributes of a log record, as the transform finds them.
#[derive(Default)]
pub(super) struct RecordDdtags<'a> {
    /// The index of the first attribute named exactly `ddtags`.
    pub(super) first_exact: Option<usize>,

    /// The index and value of the last attribute named `ddtags` in any case.
    pub(super) last: Option<(usize, Cow<'a, str>)>,
}

/// Returns the `ddtags` values that a log record's tags come from, as if `infra` had merged into the record first.
///
/// The Agent's `writeDdtagsToLogRecords` merges the infra tags into the first attribute named exactly `ddtags`, or
/// adds one at the end. The logs translator then reads the last `ddtags` attribute in any case. So:
///
/// - Without an exact `ddtags`, the added attribute is the last one, and only `infra` counts.
/// - When the first exact `ddtags` is also the last one, its value counts, and then `infra`.
/// - When a later attribute matches in another case, only that attribute counts, and `infra` is lost.
///
/// An empty existing value adds no tags, as the merge replaces it with `infra`.
pub(super) fn record_ddtags<'a>(
    record: RecordDdtags<'a>, infra: Option<&'a str>,
) -> (Option<Cow<'a, str>>, Option<&'a str>) {
    let Some(infra) = infra else {
        return (record.last.map(|(_, value)| value), None);
    };
    match (record.first_exact, record.last) {
        (Some(first), Some((last, value))) if first == last => (Some(value), Some(infra)),
        (Some(_), Some((_, value))) => (Some(value), None),
        _ => (None, Some(infra)),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use otlp_protos::opentelemetry::proto::common::v1::{any_value::Value, AnyValue, KeyValue};
    use otlp_protos::opentelemetry::proto::logs::v1::LogRecord;
    use otlp_protos::opentelemetry::proto::resource::v1::Resource;
    use saluki_core::data_model::{origin::OriginTagCardinality, tags::TagSet};
    use saluki_env::workload::{providers::TestWorkloadProvider, EntityId};

    use super::*;
    use crate::common::otlp::infra_attributes::{process_tags, ContainerTagPromotion, CustomTags};
    use crate::common::otlp::origin::OtlpOriginTagResolver;
    use crate::sources::otlp::logs::transform::transform_log_record;

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

    /// Returns the sorted tags of a log record with `record_attributes`, when its resource diverts `infra`.
    fn record_tags(record_attributes: Vec<KeyValue>, infra: Option<&str>) -> Vec<String> {
        let record = LogRecord {
            attributes: record_attributes,
            ..Default::default()
        };
        let log = transform_log_record(record, &Resource::default(), None, None, None, TagSet::default(), infra)
            .expect("the record should transform");
        let mut tags: Vec<String> = log.tags().into_iter().map(|tag| tag.as_str().to_owned()).collect();
        tags.sort();
        tags
    }

    fn sorted(tags: &[&str]) -> Vec<String> {
        let mut tags: Vec<String> = tags.iter().map(|tag| (*tag).to_owned()).collect();
        tags.sort();
        tags
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
            let tags = record_tags(vec![attribute("ddtags", existing)], Some("new:tag"));
            assert_eq!(tags, sorted(&expected.split(',').collect::<Vec<_>>()), "case: {name}");
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
            let record_attributes = case
                .existing_ddtags
                .map(|ddtags| strings(&[("ddtags", ddtags)]))
                .unwrap_or_default();

            let custom_tags = if case.tags_as_ddtags {
                CustomTags::DdTags
            } else {
                CustomTags::Attributes(ContainerTagPromotion::Off)
            };
            let infra = ddtags_value(process_tags(
                &mut attributes,
                &resolver,
                OriginTagCardinality::Low,
                custom_tags,
            ));
            let tags = record_tags(record_attributes, infra.as_deref());

            assert_eq!(
                as_map(&attributes),
                string_map(case.resource_attributes),
                "case: {}",
                case.name
            );
            let expected: Vec<&str> = case
                .ddtags
                .map(|ddtags| ddtags.split(',').collect())
                .unwrap_or_default();
            assert_eq!(tags, sorted(&expected), "case: {}", case.name);
        }
    }

    /// The tags come from the `ddtags` attributes as if the infra tags merged into the first attribute named exactly
    /// `ddtags`, and the transform then read the last one in any case.
    #[test]
    fn record_ddtags_follow_the_agent_merge_and_read() {
        let valueless = KeyValue {
            key: "ddtags".into(),
            value: None,
        };
        type Case<'a> = (&'a str, Vec<KeyValue>, Option<&'a str>, &'a [&'a str]);
        let cases: [Case; 8] = [
            (
                "empty string",
                vec![string("ddtags", "")],
                Some("new:tag"),
                &["new:tag"],
            ),
            ("valueless", vec![valueless], Some("new:tag"), &["new:tag"]),
            ("no record ddtags", vec![], Some("new:tag"), &["new:tag"]),
            (
                "two exact keys",
                vec![string("ddtags", "first:tag"), string("ddtags", "second:tag")],
                Some("new:tag"),
                &["second:tag"],
            ),
            (
                "only another case",
                vec![string("DDTAGS", "upper:tag")],
                Some("new:tag"),
                &["new:tag"],
            ),
            (
                "exact key, then another case",
                vec![string("ddtags", "exact:tag"), string("DDTags", "later:tag")],
                Some("new:tag"),
                &["later:tag"],
            ),
            (
                "another case, then the exact key",
                vec![string("DDTAGS", "upper:tag"), string("ddtags", "exact:tag")],
                Some("new:tag"),
                &["exact:tag", "new:tag"],
            ),
            (
                "no infra tags",
                vec![string("ddtags", "exact:tag"), string("DDTAGS", "upper:tag")],
                None,
                &["upper:tag"],
            ),
        ];
        for (name, record_attributes, infra, expected) in cases {
            assert_eq!(record_tags(record_attributes, infra), sorted(expected), "case: {name}");
        }
    }

    /// A comma in an infra tag value splits it, as when the intake reads `ddtags`.
    #[test]
    fn infra_tags_split_on_commas() {
        assert_eq!(
            record_tags(vec![string("ddtags", "user:tag")], Some("team:a,b")),
            sorted(&["user:tag", "team:a", "b"])
        );
    }
}
