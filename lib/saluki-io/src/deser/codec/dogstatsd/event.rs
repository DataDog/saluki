use nom::{
    bytes::complete::{tag, take},
    character::complete::u32 as parse_u32,
    combinator::all_consuming,
    error::{Error, ErrorKind},
    sequence::{delimited, preceded, separated_pair},
    IResult, Parser as _,
};
use saluki_context::{origin::OriginTagCardinality, tags::RawTags};
use saluki_core::data_model::event::eventd::{AlertType, Priority};
use stringtheory::MetaString;

use super::{helpers::*, message::*, DogstatsdCodecConfiguration};
use crate::deser::codec::dogstatsd::WellKnownTags;

/// A DogStatsD event packet.
pub struct EventPacket<'a> {
    pub title: MetaString,
    pub text: MetaString,
    pub timestamp: Option<u64>,
    pub hostname: Option<&'a str>,
    pub aggregation_key: Option<&'a str>,
    pub priority: Option<Priority>,
    pub alert_type: Option<AlertType>,
    pub source_type_name: Option<&'a str>,
    pub tags: RawTags<'a>,
    pub container_id: Option<&'a str>,
    pub external_data: Option<&'a str>,
    pub well_known_tags: WellKnownTags<'a>,
    pub cardinality: Option<OriginTagCardinality>,
}

pub fn parse_dogstatsd_event<'a>(
    input: &'a [u8], config: &DogstatsdCodecConfiguration,
) -> IResult<&'a [u8], EventPacket<'a>> {
    // We parse the title length and text length from `_e{<TITLE_UTF8_LENGTH>,<TEXT_UTF8_LENGTH>}:`
    let (remaining, (title_len, text_len)) = delimited(
        tag(EVENT_PREFIX),
        separated_pair(parse_u32, tag(","), parse_u32),
        tag("}:"),
    )
    .parse(input)?;

    // Title and Text are the required fields of an event.
    if title_len == 0 || text_len == 0 {
        return Err(nom::Err::Error(Error::new(input, ErrorKind::Verify)));
    }

    let (remaining, (raw_title, raw_text)) =
        separated_pair(take(title_len), tag("|"), take(text_len)).parse(remaining)?;

    let title = match simdutf8::basic::from_utf8(raw_title) {
        Ok(title) => clean_data(title),
        Err(_) => return Err(nom::Err::Error(Error::new(raw_title, ErrorKind::Verify))),
    };

    let text = match simdutf8::basic::from_utf8(raw_text) {
        Ok(text) => clean_data(text),
        Err(_) => return Err(nom::Err::Error(Error::new(raw_text, ErrorKind::Verify))),
    };

    // At this point, we may have some of this additional data, and if so, we also then would have a pipe separator at
    // the very front, which we'd want to consume before going further.
    //
    // After that, we simply split the remaining bytes by the pipe separator, and then try and parse each chunk to see
    // if it's any of the protocol extensions we know of.
    //
    // Priority and Alert Type have default values
    let mut maybe_priority = Some(Priority::Normal);
    let mut maybe_alert_type = Some(AlertType::Info);
    let mut maybe_timestamp = None;
    let mut maybe_hostname = None;
    let mut maybe_aggregation_key = None;
    let mut maybe_source_type = None;
    let mut maybe_tags = None;
    let mut maybe_container_id = None;
    let mut maybe_external_data = None;
    let mut maybe_cardinality = None;

    let remaining = if !remaining.is_empty() {
        let (mut remaining, _) = tag("|")(remaining)?;
        while let Some((chunk, tail)) = split_at_delimiter(remaining, b'|') {
            if chunk.len() < 2 {
                break;
            }
            match &chunk[..2] {
                // Timestamp: client-provided timestamp for the event, relative to the Unix epoch, in seconds.
                TIMESTAMP_PREFIX => {
                    let (_, timestamp) = all_consuming(preceded(tag(TIMESTAMP_PREFIX), unix_timestamp)).parse(chunk)?;
                    maybe_timestamp = Some(timestamp);
                }
                // Hostname: client-provided hostname for the host that this event originated from.
                HOSTNAME_PREFIX => {
                    let (_, hostname) =
                        all_consuming(preceded(tag(HOSTNAME_PREFIX), ascii_alphanum_and_seps)).parse(chunk)?;
                    maybe_hostname = Some(hostname);
                }
                // Aggregation key: key to be used to group this event with others that have the same key.
                AGGREGATION_KEY_PREFIX => {
                    let (_, aggregation_key) =
                        all_consuming(preceded(tag(AGGREGATION_KEY_PREFIX), ascii_alphanum_and_seps)).parse(chunk)?;
                    maybe_aggregation_key = Some(aggregation_key);
                }
                // Priority: client-provided priority of the event.
                PRIORITY_PREFIX => {
                    let (_, priority) =
                        all_consuming(preceded(tag(PRIORITY_PREFIX), ascii_alphanum_and_seps)).parse(chunk)?;
                    maybe_priority = Priority::try_from_string(priority);
                }
                // Source type name: client-provided source type name of the event.
                SOURCE_TYPE_PREFIX => {
                    let (_, source_type) =
                        all_consuming(preceded(tag(SOURCE_TYPE_PREFIX), ascii_alphanum_and_seps)).parse(chunk)?;
                    maybe_source_type = Some(source_type);
                }
                // Alert type: client-provided alert type of the event.
                ALERT_TYPE_PREFIX => {
                    let (_, alert_type) =
                        all_consuming(preceded(tag(ALERT_TYPE_PREFIX), ascii_alphanum_and_seps)).parse(chunk)?;
                    maybe_alert_type = AlertType::try_from_string(alert_type);
                }
                // Container ID: client-provided container ID for the container that this event originated from.
                CONTAINER_ID_PREFIX => {
                    let (_, container_id) =
                        all_consuming(preceded(tag(CONTAINER_ID_PREFIX), container_id)).parse(chunk)?;
                    maybe_container_id = Some(container_id);
                }
                // External Data: client-provided data used for resolving the entity ID that this event originated from.
                EXTERNAL_DATA_PREFIX => {
                    let (_, external_data) =
                        all_consuming(preceded(tag(EXTERNAL_DATA_PREFIX), external_data)).parse(chunk)?;
                    maybe_external_data = Some(external_data);
                }
                // Cardinality: client-provided cardinality for the event.
                _ if chunk.starts_with(CARDINALITY_PREFIX) => {
                    let (_, cardinality) = cardinality(chunk)?;
                    maybe_cardinality = Some(cardinality);
                }
                // Tags: additional tags to be added to the event.
                _ if chunk.starts_with(TAGS_PREFIX) => {
                    let (_, tags) = all_consuming(preceded(tag("#"), metric_tags(config))).parse(chunk)?;
                    maybe_tags = Some(tags);
                }
                _ => {
                    // We don't know what this is, so we just skip it.
                    //
                    // TODO: Should we throw an error, warn, or be silently permissive?
                }
            }
            remaining = tail;
        }
        remaining
    } else {
        remaining
    };

    let tags = maybe_tags.unwrap_or_else(RawTags::empty);
    let well_known_tags = WellKnownTags::from_raw_tags(tags.clone());
    let cardinality = maybe_cardinality.or(well_known_tags.cardinality);

    let eventd = EventPacket {
        title: title.into(),
        text: text.into(),
        tags,
        timestamp: maybe_timestamp,
        hostname: maybe_hostname,
        aggregation_key: maybe_aggregation_key,
        priority: maybe_priority,
        alert_type: maybe_alert_type,
        source_type_name: maybe_source_type,
        container_id: maybe_container_id,
        external_data: maybe_external_data,
        well_known_tags,
        cardinality,
    };
    Ok((remaining, eventd))
}

#[cfg(test)]
mod tests {
    use nom::IResult;
    use saluki_context::{
        origin::OriginTagCardinality,
        tags::{SharedTagSet, Tag, TagSet},
    };
    use saluki_core::data_model::event::eventd::*;
    use stringtheory::MetaString;

    use super::{parse_dogstatsd_event, DogstatsdCodecConfiguration};

    type NomResult<'input, T> = Result<T, nom::Err<nom::error::Error<&'input [u8]>>>;

    fn parse_dsd_eventd(input: &[u8]) -> NomResult<'_, EventD> {
        let default_config = DogstatsdCodecConfiguration::default();
        parse_dsd_eventd_with_conf(input, &default_config)
    }

    fn parse_dsd_eventd_with_conf<'input>(
        input: &'input [u8], config: &DogstatsdCodecConfiguration,
    ) -> NomResult<'input, EventD> {
        let (remaining, eventd) = parse_dsd_eventd_direct(input, config)?;
        assert!(remaining.is_empty());
        Ok(eventd)
    }

    fn parse_dsd_eventd_direct<'input>(
        input: &'input [u8], config: &DogstatsdCodecConfiguration,
    ) -> IResult<&'input [u8], EventD> {
        let (remaining, packet) = parse_dogstatsd_event(input, config)?;
        assert!(remaining.is_empty());

        let mut event_tags = TagSet::default();
        for tag in packet.tags.into_iter() {
            event_tags.insert_tag(tag);
        }

        let eventd = EventD::new(packet.title, packet.text)
            .with_timestamp(packet.timestamp)
            .with_hostname(packet.hostname.map(|s| s.into()))
            .with_aggregation_key(packet.aggregation_key.map(|s| s.into()))
            .with_alert_type(packet.alert_type)
            .with_priority(packet.priority)
            .with_source_type_name(packet.source_type_name.map(|s| s.into()))
            .with_alert_type(packet.alert_type)
            .with_tags(event_tags);

        Ok((remaining, eventd))
    }

    #[track_caller]
    fn check_basic_eventd_eq(expected: EventD, actual: EventD) {
        assert_eq!(expected.title(), actual.title());
        assert_eq!(expected.text(), actual.text());
        assert_eq!(expected.timestamp(), actual.timestamp());
        assert_eq!(expected.hostname(), actual.hostname());
        assert_eq!(expected.aggregation_key(), actual.aggregation_key());
        assert_eq!(expected.priority(), actual.priority());
        assert_eq!(expected.source_type_name(), actual.source_type_name());
        assert_eq!(expected.alert_type(), actual.alert_type());
        assert_eq!(expected.tags(), actual.tags());
        assert_eq!(expected.origin_tags(), actual.origin_tags());
    }

    #[test]
    fn basic_eventd() {
        let event_title = "my event";
        let event_text = "text";
        let raw = format!(
            "_e{{{},{}}}:{}|{}",
            event_title.len(),
            event_text.len(),
            event_title,
            event_text
        );

        let actual = parse_dsd_eventd(raw.as_bytes()).unwrap();
        let expected = EventD::new(event_title, event_text);
        check_basic_eventd_eq(expected, actual);
    }

    #[test]
    fn eventd_tags() {
        let event_title = "my event";
        let event_text = "text";
        let tags = ["tag1", "tag2"];
        let shared_tag_set: SharedTagSet = tags.iter().map(|&s| Tag::from(s)).collect::<TagSet>().into_shared();
        let raw = format!(
            "_e{{{},{}}}:{}|{}|#{}",
            event_title.len(),
            event_text.len(),
            event_title,
            event_text,
            tags.join(","),
        );

        let expected = EventD::new(event_title, event_text).with_tags(shared_tag_set);
        let actual = parse_dsd_eventd(raw.as_bytes()).unwrap();
        check_basic_eventd_eq(expected, actual);
    }

    #[test]
    fn eventd_priority() {
        let event_title = "my event";
        let event_text = "text";
        let event_priority = Priority::Low;
        let raw = format!(
            "_e{{{},{}}}:{}|{}|p:{}",
            event_title.len(),
            event_text.len(),
            event_title,
            event_text,
            event_priority
        );

        let expected = EventD::new(event_title, event_text).with_priority(event_priority);
        let actual = parse_dsd_eventd(raw.as_bytes()).unwrap();
        check_basic_eventd_eq(expected, actual);
    }

    #[test]
    fn eventd_alert_type() {
        let event_title = "my event";
        let event_text = "text";
        let event_alert_type = AlertType::Warning;
        let raw = format!(
            "_e{{{},{}}}:{}|{}|t:{}",
            event_title.len(),
            event_text.len(),
            event_title,
            event_text,
            event_alert_type
        );

        let expected = EventD::new(event_title, event_text).with_alert_type(event_alert_type);
        let actual = parse_dsd_eventd(raw.as_bytes()).unwrap();
        check_basic_eventd_eq(expected, actual);
    }

    #[test]
    fn eventd_multiple_extensions() {
        let event_title = "my event";
        let event_text = "text";
        let event_hostname = MetaString::from("testhost");
        let event_aggregation_key = MetaString::from("testkey");
        let event_priority = Priority::Low;
        let event_source_type = MetaString::from("testsource");
        let event_alert_type = AlertType::Success;
        let event_timestamp = 1234567890;
        let event_container_id = "abcdef123456";
        let event_external_data = "it-false,cn-redis,pu-810fe89d-da47-410b-8979-9154a40f8183";
        let event_cardinality = "low";
        let tags = ["tags1", "tags2"];
        let shared_tag_set = SharedTagSet::from(TagSet::from_iter(tags.iter().map(|&s| s.into())));
        let raw = format!(
            "_e{{{},{}}}:{}|{}|h:{}|k:{}|p:{}|s:{}|t:{}|d:{}|c:{}|e:{}|card:{}|#{}",
            event_title.len(),
            event_text.len(),
            event_title,
            event_text,
            event_hostname,
            event_aggregation_key,
            event_priority,
            event_source_type,
            event_alert_type,
            event_timestamp,
            event_container_id,
            event_external_data,
            event_cardinality,
            tags.join(","),
        );
        let actual = parse_dsd_eventd(raw.as_bytes()).unwrap();
        let expected = EventD::new(event_title, event_text)
            .with_hostname(event_hostname)
            .with_aggregation_key(event_aggregation_key)
            .with_priority(event_priority)
            .with_source_type_name(event_source_type)
            .with_alert_type(event_alert_type)
            .with_timestamp(event_timestamp)
            .with_tags(shared_tag_set);
        check_basic_eventd_eq(expected, actual);

        let config = DogstatsdCodecConfiguration::default();
        let (_, packet) = parse_dogstatsd_event(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.container_id, Some(event_container_id));
        assert_eq!(packet.external_data, Some(event_external_data));
        assert_eq!(packet.cardinality, Some(OriginTagCardinality::Low));
    }

    #[test]
    fn eventd_well_known_tags() {
        // Just ensures that we're populating well-known tags.
        let raw = "_e{5,5}:hello|world|#dd.internal.card:low";

        let config = DogstatsdCodecConfiguration::default();
        let (_, packet) = parse_dogstatsd_event(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.well_known_tags.hostname, None);
        assert_eq!(packet.well_known_tags.cardinality, Some(OriginTagCardinality::Low));
    }
}
