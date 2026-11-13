use saluki_core::{
    constants::datadog::*,
    data_model::{
        origin::OriginTagCardinality,
        tags::{BorrowedTag, RawTags},
    },
};
use smallvec::SmallVec;

/// Regular (non-well-known) tags from a DogStatsD payload.
///
/// As an optimization, up to 32 tags are stored inline without allocating. This isn't a limit on the number of tags:
/// any beyond that are stored on the heap.
pub type RegularTags<'a> = SmallVec<[&'a str; 32]>;

/// Returns `true` if the given tag is a well-known tag.
fn is_well_known_tag(tag: &str) -> bool {
    tag.starts_with(HOST_TAG_KEY_SUFFIXED)
        || tag.starts_with(ENTITY_ID_TAG_KEY_SUFFIXED)
        || tag.starts_with(JMX_CHECK_NAME_TAG_KEY_SUFFIXED)
        || tag.starts_with(CARDINALITY_TAG_KEY_SUFFIXED)
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
    /// Splits the raw tags of a DogStatsD payload into well-known tags and regular tags.
    ///
    /// Any tag with a well-known tag key is excluded from the regular tags, even if its value is ignored. All fields
    /// of the well-known tags default to `None`.
    ///
    /// The raw tags are walked exactly once, so callers can iterate over the regular tags as many times as needed
    /// (such as for both hashing and building a context) without re-parsing the raw tags.
    pub fn split_raw_tags(tags: &'a RawTags<'_>) -> (Self, RegularTags<'a>) {
        let mut well_known_tags = Self::default();
        let mut regular_tags = RegularTags::new();

        for tag in tags.iter() {
            if !is_well_known_tag(tag) {
                regular_tags.push(tag);
                continue;
            }

            match BorrowedTag::from(tag).name_and_value() {
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

        (well_known_tags, regular_tags)
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::{
        constants::datadog::*,
        data_model::{origin::OriginTagCardinality, tags::RawTags},
    };

    use super::WellKnownTags;

    #[test]
    fn well_known_tags_are_parsed_from_their_tag() {
        let tag = |key: &str, value: &str| format!("{key}:{value}");
        let card = |value| (None, None, None, value);
        // Expected hostname, pod UID, JMX check name, and cardinality.
        let cases = [
            (String::new(), (None, None, None, None)),
            // Cardinality values are case-insensitive, to match the core Datadog Agent.
            (tag(CARDINALITY_TAG_KEY, "none"), card(Some(OriginTagCardinality::None))),
            (tag(CARDINALITY_TAG_KEY, "low"), card(Some(OriginTagCardinality::Low))),
            (
                tag(CARDINALITY_TAG_KEY, "orch"),
                card(Some(OriginTagCardinality::Orchestrator)),
            ),
            (
                tag(CARDINALITY_TAG_KEY, "orchestrator"),
                card(Some(OriginTagCardinality::Orchestrator)),
            ),
            (tag(CARDINALITY_TAG_KEY, "high"), card(Some(OriginTagCardinality::High))),
            (tag(CARDINALITY_TAG_KEY, "Low"), card(Some(OriginTagCardinality::Low))),
            (tag(CARDINALITY_TAG_KEY, "HiGH"), card(Some(OriginTagCardinality::High))),
            (tag(CARDINALITY_TAG_KEY, "MEDIUM"), card(None)),
            (tag(CARDINALITY_TAG_KEY, "fakelevel"), card(None)),
            (tag(HOST_TAG_KEY, ""), (Some(""), None, None, None)),
            (tag(HOST_TAG_KEY, "localhost"), (Some("localhost"), None, None, None)),
            (tag(JMX_CHECK_NAME_TAG_KEY, ""), (None, None, Some(""), None)),
            (
                tag(JMX_CHECK_NAME_TAG_KEY, "check_name"),
                (None, None, Some("check_name"), None),
            ),
            (tag(ENTITY_ID_TAG_KEY, "asdasda"), (None, Some("asdasda"), None, None)),
            (tag(ENTITY_ID_TAG_KEY, ENTITY_ID_IGNORE_VALUE), (None, None, None, None)),
        ];

        for (raw_tags, expected) in cases {
            let tags = RawTags::new(&raw_tags, usize::MAX, usize::MAX);
            let (wkt, _) = WellKnownTags::split_raw_tags(&tags);
            let actual = (wkt.hostname, wkt.pod_uid, wkt.jmx_check_name, wkt.cardinality);
            assert_eq!(actual, expected, "raw tags {raw_tags:?}");
        }
    }

    #[test]
    fn regular_tags_exclude_well_known_tags() {
        let raw_tags_input = format!(
            "env:prod,{}:my-host,host,hostname:other,{}:{},{}:bogus,{}:check,env:prod",
            HOST_TAG_KEY, ENTITY_ID_TAG_KEY, ENTITY_ID_IGNORE_VALUE, CARDINALITY_TAG_KEY, JMX_CHECK_NAME_TAG_KEY
        );
        let tags = RawTags::new(&raw_tags_input, usize::MAX, usize::MAX);
        let (wkt, regular_tags) = WellKnownTags::split_raw_tags(&tags);

        // Well-known tags are excluded even when their value is ignored, while regular tags retain their order and
        // duplicates.
        assert_eq!(
            regular_tags.as_slice(),
            ["env:prod", "host", "hostname:other", "env:prod"]
        );
        assert_eq!(wkt.hostname, Some("my-host"));
        assert_eq!(wkt.pod_uid, None);
        assert_eq!(wkt.cardinality, None);
        assert_eq!(wkt.jmx_check_name, Some("check"));
    }
}
