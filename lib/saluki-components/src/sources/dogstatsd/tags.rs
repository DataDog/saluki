use saluki_core::{
    constants::datadog::*,
    data_model::{origin::OriginTagCardinality, tags::RawTags},
};
use smallvec::SmallVec;

/// Regular (non-well-known) tags from a DogStatsD payload.
///
/// Keeps up to 64 tag references inline without allocating. The inline capacity is not a limit on tag count.
/// Larger lists move to heap storage.
pub type RegularTags<'a> = SmallVec<[&'a str; 64]>;

/// Well-known tags shared across DogStatsD payloads.
///
/// The DogStatsD client uses well-known keys to carry a hostname, entity ID, JMX check name, or cardinality hint.
/// These values describe the payload or its origin instead of appearing as regular tags.
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
    /// A tag with a well-known key stays out of the regular list even if the parser ignores its value.
    /// Each well-known field starts as `None`.
    ///
    /// The split scans the raw tags once. The caller can reuse the regular tags for hashing and context creation
    /// without parsing the payload again.
    pub fn split_raw_tags(tags: &'a RawTags<'_>) -> (Self, RegularTags<'a>) {
        let mut well_known_tags = Self::default();
        let mut regular_tags = RegularTags::new();

        for tag in tags.iter() {
            // The first byte selects which well-known prefixes can match. A full prefix match includes the colon.
            match tag.as_bytes().first() {
                Some(b'h') => {
                    if let Some(hostname) = tag.strip_prefix(HOST_TAG_KEY_SUFFIXED) {
                        well_known_tags.hostname = Some(hostname);
                        continue;
                    }
                }
                Some(b'd') => {
                    if let Some(entity_id) = tag.strip_prefix(ENTITY_ID_TAG_KEY_SUFFIXED) {
                        if entity_id != ENTITY_ID_IGNORE_VALUE {
                            well_known_tags.pod_uid = Some(entity_id);
                        }
                        continue;
                    }
                    if let Some(name) = tag.strip_prefix(JMX_CHECK_NAME_TAG_KEY_SUFFIXED) {
                        well_known_tags.jmx_check_name = Some(name);
                        continue;
                    }
                    if let Some(value) = tag.strip_prefix(CARDINALITY_TAG_KEY_SUFFIXED) {
                        if let Ok(card) = OriginTagCardinality::try_from(value) {
                            well_known_tags.cardinality = Some(card);
                        }
                        continue;
                    }
                }
                _ => {}
            }
            regular_tags.push(tag);
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

    #[test]
    fn well_known_tag_values_keep_suffixes_and_ignore_invalid_overrides() {
        let tags = format!(
            "host:old,host:new:port,dd.internal.entity_id:pod,dd.internal.entity_id:{ENTITY_ID_IGNORE_VALUE},\
             dd.internal.card:low,dd.internal.card:bad,dd.internal.jmx_check_name:,dd.internal.jmx_check_name:x:y,\
             ,dd.internal.card.extra:keep,jmx_check_name:keep"
        );
        let tags = RawTags::new(&tags, usize::MAX, usize::MAX);
        let (wkt, regular_tags) = WellKnownTags::split_raw_tags(&tags);

        assert_eq!(wkt.hostname, Some("new:port"));
        assert_eq!(wkt.pod_uid, Some("pod"));
        assert_eq!(wkt.cardinality, Some(OriginTagCardinality::Low));
        assert_eq!(wkt.jmx_check_name, Some("x:y"));
        assert_eq!(
            regular_tags.as_slice(),
            ["", "dd.internal.card.extra:keep", "jmx_check_name:keep"]
        );
    }
}
