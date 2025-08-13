#![allow(dead_code)]

use saluki_context::{
    origin::OriginTagCardinality,
    tags::{BorrowedTag, RawTags, RawTagsFilter, RawTagsFilterPredicate},
};
use saluki_core::constants::datadog::*;

/// Filter predicate for well-known tags.
#[derive(Clone)]
pub struct WellKnownTagsFilterPredicate;

impl RawTagsFilterPredicate for WellKnownTagsFilterPredicate {
    fn matches(&self, tag: &str) -> bool {
        tag.starts_with(HOST_TAG_KEY_SUFFIXED)
            || tag.starts_with(ENTITY_ID_TAG_KEY_SUFFIXED)
            || tag.starts_with(JMX_CHECK_NAME_TAG_KEY_SUFFIXED)
            || tag.starts_with(CARDINALITY_TAG_KEY_SUFFIXED)
    }
}

/// Well-known tags shared across DogStatsD payloads.
///
/// Well-known tags are tags which are generally set by the DogStatsD client to convey structured information
/// about the source of the metric, event, or service check.
#[derive(Default)]
pub struct WellKnownTags<'a> {
    pub hostname: Option<&'a str>,
    pub pod_uid: Option<&'a str>,
    pub jmx_check_name: Option<&'a str>,
    pub cardinality: Option<OriginTagCardinality>,
}

impl<'a> WellKnownTags<'a> {
    /// Extracts well-known tags from the raw tags of a DogStatsD payload.
    ///
    /// All fields default to `None`.
    fn from_raw_tags(tags: RawTags<'a>) -> Self {
        let mut well_known_tags = Self::default();

        let filtered_tags = RawTagsFilter::include(tags, WellKnownTagsFilterPredicate);
        for tag in filtered_tags {
            let tag = BorrowedTag::from(tag);
            match tag.name_and_value() {
                (HOST_TAG_KEY, Some(hostname)) => {
                    well_known_tags.hostname = Some(hostname);
                }
                (ENTITY_ID_TAG_KEY, Some(entity_id)) if entity_id != ENTITY_ID_IGNORE_VALUE => {
                    well_known_tags.pod_uid = Some(entity_id);
                }
                (JMX_CHECK_NAME_TAG_KEY, Some(name)) => {
                    well_known_tags.jmx_check_name = Some(name);
                }
                (CARDINALITY_TAG_KEY, Some(value)) => {
                    if let Ok(card) = OriginTagCardinality::try_from(value) {
                        well_known_tags.cardinality = Some(card);
                    }
                }
                _ => {}
            }
        }
        well_known_tags
    }
}

#[cfg(test)]
mod tests {
    use saluki_context::{origin::OriginTagCardinality, tags::RawTags};
    use saluki_core::constants::datadog::*;

    use super::WellKnownTags;

    #[test]
    fn well_known_tags_empty() {
        let tags = RawTags::new("", usize::MAX, usize::MAX);
        let wkt = WellKnownTags::from_raw_tags(tags);

        assert_eq!(wkt.cardinality, None);
        assert_eq!(wkt.hostname, None);
        assert_eq!(wkt.jmx_check_name, None);
        assert_eq!(wkt.pod_uid, None);
    }

    #[test]
    fn well_known_tags_cardinality() {
        let cases = [
            // Happy path.
            ("none", Some(OriginTagCardinality::None)),
            ("low", Some(OriginTagCardinality::Low)),
            ("orch", Some(OriginTagCardinality::Orchestrator)),
            ("orchestrator", Some(OriginTagCardinality::Orchestrator)),
            ("high", Some(OriginTagCardinality::High)),
            // Invalid values: either the wrong case, or just not even real cardinality levels.
            ("Low", None),
            ("MEDIUM", None),
            ("HiGH", None),
            ("fakelevel", None),
        ];

        for (tag_value, expected) in cases {
            let card_tag = format!("{}:{}", CARDINALITY_TAG_KEY, tag_value);
            let tags = RawTags::new(&card_tag, usize::MAX, usize::MAX);
            let wkt = WellKnownTags::from_raw_tags(tags);

            assert_eq!(wkt.cardinality, expected);
        }
    }

    #[test]
    fn well_known_tags_hostname() {
        let cases = [("", Some("")), ("localhost", Some("localhost"))];

        for (tag_value, expected) in cases {
            let host_tag = format!("{}:{}", HOST_TAG_KEY, tag_value);
            let tags = RawTags::new(&host_tag, usize::MAX, usize::MAX);
            let wkt = WellKnownTags::from_raw_tags(tags);

            assert_eq!(wkt.hostname, expected);
        }
    }

    #[test]
    fn well_known_tags_jmx_check_name() {
        let cases = [("", Some("")), ("check_name", Some("check_name"))];

        for (tag_value, expected) in cases {
            let jmx_tag = format!("{}:{}", JMX_CHECK_NAME_TAG_KEY, tag_value);
            let tags = RawTags::new(&jmx_tag, usize::MAX, usize::MAX);
            let wkt = WellKnownTags::from_raw_tags(tags);

            assert_eq!(wkt.jmx_check_name, expected);
        }
    }

    #[test]
    fn well_known_tags_pod_uid() {
        let cases = [("asdasda", Some("asdasda")), (ENTITY_ID_IGNORE_VALUE, None)];

        for (tag_value, expected) in cases {
            let pod_tag = format!("{}:{}", ENTITY_ID_TAG_KEY, tag_value);
            let tags = RawTags::new(&pod_tag, usize::MAX, usize::MAX);
            let wkt = WellKnownTags::from_raw_tags(tags);

            assert_eq!(wkt.pod_uid, expected);
        }
    }
}
