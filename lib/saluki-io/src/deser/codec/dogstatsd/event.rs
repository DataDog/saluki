use nom::{
    bytes::complete::{tag, take},
    character::complete::u32 as parse_u32,
    error::{Error, ErrorKind},
    sequence::{delimited, separated_pair},
    IResult, Parser as _,
};
use saluki_core::data_model::{event::eventd::*, tags::RawTags};
use stringtheory::MetaString;
use tracing::debug;

use super::{helpers::*, DogStatsDCodecConfiguration, NomParserError};

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
    pub origin: ClientOrigin<'a>,
}

#[inline]
pub fn parse_dogstatsd_event<'a>(
    input: &'a [u8], config: &DogStatsDCodecConfiguration,
) -> IResult<&'a [u8], EventPacket<'a>> {
    // We parse the title length and text length from `_e{<TITLE_UTF8_LENGTH>,<TEXT_UTF8_LENGTH>}:`
    let (remaining, (title_len, text_len)) = delimited(
        tag(EVENT_PREFIX),
        separated_pair(parse_u32, tag(","), parse_u32),
        tag("}:"),
    )
    .parse(input)?;

    // Title and Text are the required fields of an event. In permissive mode, we only require a title, matching the
    // reference DogStatsD server.
    if title_len == 0 || (text_len == 0 && !config.permissive) {
        return Err(nom::Err::Error(Error::new(input, ErrorKind::Verify)));
    }

    let (remaining, (raw_title, raw_text)) =
        separated_pair(take(title_len), tag("|"), take(text_len)).parse(remaining)?;

    let title = match simdutf8::basic::from_utf8(raw_title) {
        Ok(title) => title.replace("\\n", "\n"),
        Err(_) => return Err(nom::Err::Error(Error::new(raw_title, ErrorKind::Verify))),
    };

    let text = match simdutf8::basic::from_utf8(raw_text) {
        Ok(text) => text.replace("\\n", "\n"),
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
    let mut origin = ClientOrigin::default();

    // In permissive mode, string fields (hostname, aggregation key, source type) accept any UTF-8, matching the Datadog
    // Agent. Otherwise, they're restricted to ASCII alphanumerics and separators.
    let string_field: StrParser = if config.permissive {
        non_empty_utf8
    } else {
        ascii_alphanum_and_seps
    };

    let mut parse_field = |chunk: &'a [u8]| -> Result<(), NomParserError<'a>> {
        // Match on the two-byte field prefix. A one-byte field (for example, a bare `#`) can't match any of the
        // two-byte prefixes, so it either matches a single-byte prefix below or is unknown.
        match chunk.get(..2).unwrap_or(chunk) {
            // Timestamp: client-provided timestamp for the event, relative to the Unix epoch, in seconds.
            TIMESTAMP_PREFIX => {
                let timestamp = field(chunk, TIMESTAMP_PREFIX, unix_timestamp)?;
                maybe_timestamp = Some(timestamp);
            }
            // Hostname: client-provided hostname for the host that this event originated from.
            HOSTNAME_PREFIX if chunk != HOSTNAME_PREFIX => {
                let hostname = field(chunk, HOSTNAME_PREFIX, string_field)?;
                maybe_hostname = Some(hostname);
            }
            // Aggregation key: key to be used to group this event with others that have the same key.
            AGGREGATION_KEY_PREFIX if chunk != AGGREGATION_KEY_PREFIX => {
                let aggregation_key = field(chunk, AGGREGATION_KEY_PREFIX, string_field)?;
                maybe_aggregation_key = Some(aggregation_key);
            }
            // Priority: client-provided priority of the event.
            PRIORITY_PREFIX => {
                let priority = field(chunk, PRIORITY_PREFIX, ascii_alphanum_and_seps)?;
                maybe_priority = Priority::try_from_string(priority);
            }
            // Source type name: client-provided source type name of the event.
            SOURCE_TYPE_PREFIX if chunk != SOURCE_TYPE_PREFIX => {
                let source_type = field(chunk, SOURCE_TYPE_PREFIX, string_field)?;
                maybe_source_type = Some(source_type);
            }
            // Alert type: client-provided alert type of the event.
            ALERT_TYPE_PREFIX => {
                let alert_type = field(chunk, ALERT_TYPE_PREFIX, ascii_alphanum_and_seps)?;
                maybe_alert_type = AlertType::try_from_string(alert_type);
            }
            // Local Data: client-provided data used for resolving the entity ID that this event originated from.
            LOCAL_DATA_PREFIX if config.client_origin_detection && chunk != LOCAL_DATA_PREFIX => {
                let local_data = field(chunk, LOCAL_DATA_PREFIX, local_data)?;
                origin.local_data = Some(local_data);
            }
            // External Data: client-provided data used for resolving the entity ID that this event originated from.
            EXTERNAL_DATA_PREFIX if config.client_origin_detection && chunk != EXTERNAL_DATA_PREFIX => {
                let external_data = field(chunk, EXTERNAL_DATA_PREFIX, external_data)?;
                origin.external_data = Some(external_data);
            }
            // Cardinality: client-provided cardinality for the event.
            _ if chunk.starts_with(CARDINALITY_PREFIX)
                && config.client_origin_detection
                && chunk != CARDINALITY_PREFIX =>
            {
                let (_, cardinality) = cardinality(chunk)?;
                origin.cardinality = cardinality;
            }
            // Tags: additional tags to be added to the event. A bare `#` is a valid, empty set of tags.
            _ if chunk.starts_with(TAGS_PREFIX) => {
                let tags = field(chunk, b"#", tags(config))?;
                maybe_tags = Some(tags);
            }
            // A known field with an empty value (for example, `h:`) is treated as missing.
            HOSTNAME_PREFIX
            | AGGREGATION_KEY_PREFIX
            | SOURCE_TYPE_PREFIX
            | LOCAL_DATA_PREFIX
            | EXTERNAL_DATA_PREFIX
                if chunk.len() == 2 => {}
            _ if chunk == CARDINALITY_PREFIX => {}
            // Origin detection fields are ignored when client origin detection is disabled.
            LOCAL_DATA_PREFIX | EXTERNAL_DATA_PREFIX if !config.client_origin_detection => {}
            _ if chunk.starts_with(CARDINALITY_PREFIX) && !config.client_origin_detection => {}
            // An unknown or malformed field (for example, a stray `T`). Like any other invalid field, the caller
            // skips it in permissive mode, matching the reference DogStatsD server, and rejects the event otherwise.
            _ => return Err(nom::Err::Error(Error::new(chunk, ErrorKind::Verify))),
        }
        Ok(())
    };

    let remaining = if !remaining.is_empty() {
        let (mut remaining, _) = tag("|")(remaining)?;
        while let Some((chunk, tail)) = split_at_delimiter(remaining, b'|') {
            // An empty field (for example, `text||#tags`) is a structural defect. In permissive mode, we skip it and
            // keep parsing the fields after it, matching the reference DogStatsD server. Otherwise, we reject the
            // event.
            if chunk.is_empty() {
                if !config.permissive {
                    return Err(nom::Err::Error(Error::new(remaining, ErrorKind::Verify)));
                }
                remaining = tail;
                continue;
            }

            // In permissive mode, an invalid optional field is ignored rather than rejecting the whole event, matching
            // the reference DogStatsD server.
            if let Err(e) = parse_field(chunk) {
                if !config.permissive {
                    return Err(e);
                }
                debug!(
                    field = %String::from_utf8_lossy(chunk),
                    "Ignoring invalid optional field in DogStatsD event."
                );
            }
            remaining = tail;
        }
        remaining
    } else {
        remaining
    };

    let tags = maybe_tags.unwrap_or_else(RawTags::empty);

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
        origin,
    };
    Ok((remaining, eventd))
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::{
        event::eventd::{AlertType, EventD, Priority},
        origin::OriginTagCardinality,
        tags::{SharedTagSet, Tag, TagSet},
    };
    use stringtheory::MetaString;

    use super::{parse_dogstatsd_event, DogStatsDCodecConfiguration};

    /// Builds an `_e{title_len,text_len}:title|text` frame, followed by `extensions` verbatim.
    fn event_frame(title: &str, text: &str, extensions: &str) -> String {
        format!("_e{{{},{}}}:{}|{}{}", title.len(), text.len(), title, text, extensions)
    }

    #[track_caller]
    fn parse_dsd_eventd(input: &[u8]) -> EventD {
        parse_dsd_eventd_with_conf(input, &DogStatsDCodecConfiguration::default())
    }

    #[track_caller]
    fn parse_dsd_eventd_with_conf(input: &[u8], config: &DogStatsDCodecConfiguration) -> EventD {
        let (remaining, packet) = parse_dogstatsd_event(input, config)
            .unwrap_or_else(|e| panic!("failed to parse {:?}: {e:?}", String::from_utf8_lossy(input)));
        assert!(remaining.is_empty());

        let mut event_tags = TagSet::default();
        for tag in packet.tags.iter() {
            event_tags.insert_tag(tag);
        }

        EventD::new(packet.title, packet.text)
            .with_timestamp(packet.timestamp)
            .with_hostname(packet.hostname.map(|s| s.into()))
            .with_aggregation_key(packet.aggregation_key.map(|s| s.into()))
            .with_alert_type(packet.alert_type)
            .with_priority(packet.priority)
            .with_source_type_name(packet.source_type_name.map(|s| s.into()))
            .with_tags(event_tags)
    }

    #[test]
    fn eventd_single_extensions() {
        let title = "my event";
        let text = "text";
        let tags = ["tag1", "tag2"];
        let shared_tag_set: SharedTagSet = tags.iter().map(|&s| Tag::from(s)).collect::<TagSet>().into_shared();

        let cases = [
            ("basic", String::new(), EventD::new(title, text)),
            (
                "tags",
                format!("|#{}", tags.join(",")),
                EventD::new(title, text).with_tags(shared_tag_set),
            ),
            (
                "priority",
                format!("|p:{}", Priority::Low),
                EventD::new(title, text).with_priority(Priority::Low),
            ),
            (
                "alert type",
                format!("|t:{}", AlertType::Warning),
                EventD::new(title, text).with_alert_type(AlertType::Warning),
            ),
        ];

        for (name, extensions, expected) in cases {
            let actual = parse_dsd_eventd(event_frame(title, text, &extensions).as_bytes());
            assert_eq!(actual, expected, "{name}");
        }
    }

    #[test]
    fn eventd_fields_after_empty_or_one_byte_field_are_parsed() {
        let permissive = DogStatsDCodecConfiguration::default().with_permissive_mode(true);
        let strict = DogStatsDCodecConfiguration::default().with_permissive_mode(false);
        let expected = EventD::new("title", "text")
            .with_tags(SharedTagSet::from(TagSet::from_iter(["tag1".into(), "tag2".into()])));

        // An empty field is skipped in permissive mode, but rejects the event in strict mode.
        let raw = event_frame("title", "text", "||#tag1,tag2");
        assert_eq!(parse_dsd_eventd_with_conf(raw.as_bytes(), &permissive), expected);
        assert!(parse_dogstatsd_event(raw.as_bytes(), &strict).is_err());

        // A one-byte field, such as a bare `#`, carries no value, and the fields after it are parsed in both modes.
        let raw = event_frame("title", "text", "|#|#tag1,tag2");
        for config in [&permissive, &strict] {
            assert_eq!(parse_dsd_eventd_with_conf(raw.as_bytes(), config), expected);
        }
    }

    #[test]
    fn unknown_or_malformed_fields_are_rejected_only_in_strict_mode() {
        let permissive = DogStatsDCodecConfiguration::default().with_permissive_mode(true);
        let strict = DogStatsDCodecConfiguration::default().with_permissive_mode(false);

        // A stray one-byte field, and an unknown field.
        for raw in [
            b"_e{5,4}:title|text|T|#env:prod".as_slice(),
            b"_e{5,4}:title|text|x:unknown|#env:prod",
        ] {
            let (_, event) = parse_dogstatsd_event(raw, &permissive)
                .unwrap_or_else(|e| panic!("permissive mode should skip the field in {raw:?}: {e:?}"));
            assert_eq!(event.tags.iter().collect::<Vec<_>>(), ["env:prod"]);

            assert!(
                parse_dogstatsd_event(raw, &strict).is_err(),
                "strict mode should reject {raw:?}"
            );
        }
    }

    #[test]
    fn event_tags_with_invalid_utf8_are_normalized() {
        let mut input = b"_e{5,4}:title|text|#env:prod,tag:".to_vec();
        input.extend_from_slice(&[0xff, 0xfe]);

        let tags = ["env:prod", "tag:\u{FFFD}"];
        let expected = EventD::new("title", "text").with_tags(SharedTagSet::from(TagSet::from_iter(
            tags.iter().map(|&tag| tag.into()),
        )));

        assert_eq!(parse_dsd_eventd(&input), expected);
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
        let event_local_data = "abcdef123456";
        let event_external_data = "it-false,cn-redis,pu-810fe89d-da47-410b-8979-9154a40f8183";
        let event_cardinality = "low";
        let tags = ["tags1", "tags2"];
        let shared_tag_set = SharedTagSet::from(TagSet::from_iter(tags.iter().map(|&s| s.into())));
        let raw = event_frame(
            event_title,
            event_text,
            &format!(
                "|h:{}|k:{}|p:{}|s:{}|t:{}|d:{}|c:{}|e:{}|card:{}|#{}",
                event_hostname,
                event_aggregation_key,
                event_priority,
                event_source_type,
                event_alert_type,
                event_timestamp,
                event_local_data,
                event_external_data,
                event_cardinality,
                tags.join(","),
            ),
        );
        let actual = parse_dsd_eventd(raw.as_bytes());
        let expected = EventD::new(event_title, event_text)
            .with_hostname(event_hostname)
            .with_aggregation_key(event_aggregation_key)
            .with_priority(event_priority)
            .with_source_type_name(event_source_type)
            .with_alert_type(event_alert_type)
            .with_timestamp(event_timestamp)
            .with_tags(shared_tag_set);
        assert_eq!(actual, expected);

        // We need client_origin_detection on in order to parse local_data, external_data, and cardinality fields
        let config = DogStatsDCodecConfiguration::default().with_client_origin_detection(true);
        let (_, packet) = parse_dogstatsd_event(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.origin.local_data, Some(event_local_data));
        assert_eq!(packet.origin.external_data, Some(event_external_data));
        assert_eq!(packet.origin.cardinality, Some(OriginTagCardinality::Low));
    }

    #[test]
    fn event_rejects_empty_title_or_text() {
        use nom::error::ErrorKind;

        // In strict mode (the codec default), title and text are both required: a declared length of zero for either
        // one is a structural error, so the parser rejects the frame with a `Verify` error rather than emitting an
        // event with an empty title/text. Permissive mode only requires a title; see
        // `permissive_mode_only_requires_a_title`.
        let config = DogStatsDCodecConfiguration::default();

        match parse_dogstatsd_event(b"_e{0,4}:|text", &config) {
            Err(nom::Err::Error(e)) => assert_eq!(e.code, ErrorKind::Verify),
            Err(other) => panic!("expected Verify error for empty title, got {other:?}"),
            Ok(_) => panic!("empty title must be rejected"),
        }

        match parse_dogstatsd_event(b"_e{5,0}:title|", &config) {
            Err(nom::Err::Error(e)) => assert_eq!(e.code, ErrorKind::Verify),
            Err(other) => panic!("expected Verify error for empty text, got {other:?}"),
            Ok(_) => panic!("empty text must be rejected"),
        }
    }

    #[test]
    fn permissive_mode_only_requires_a_title() {
        let config = DogStatsDCodecConfiguration::default().with_permissive_mode(true);

        // Like the reference DogStatsD server, permissive mode accepts an event with empty text...
        let (_, event) = parse_dogstatsd_event(b"_e{5,0}:title|", &config)
            .expect("empty text should be accepted in permissive mode");
        assert_eq!(&*event.title, "title");
        assert_eq!(&*event.text, "");

        // ...but still rejects an event with an empty title.
        assert!(parse_dogstatsd_event(b"_e{0,4}:|text", &config).is_err());
    }

    #[test]
    fn permissive_mode_accepts_utf8_string_fields() {
        let permissive = DogStatsDCodecConfiguration::default().with_permissive_mode(true);
        let strict = DogStatsDCodecConfiguration::default().with_permissive_mode(false);

        let raw = "_e{5,4}:title|text|h:host:1|k:key/1|s:my/söurce|#env:prod";
        let (_, event) = parse_dogstatsd_event(raw.as_bytes(), &permissive)
            .expect("permissive mode should accept any UTF-8 in string fields");
        assert_eq!(event.hostname, Some("host:1"));
        assert_eq!(event.aggregation_key, Some("key/1"));
        assert_eq!(event.source_type_name, Some("my/söurce"));
        assert_eq!(event.tags.iter().collect::<Vec<_>>(), ["env:prod"]);

        assert!(parse_dogstatsd_event(raw.as_bytes(), &strict).is_err());
    }

    #[test]
    fn permissive_mode_ignores_invalid_optional_fields() {
        let permissive = DogStatsDCodecConfiguration::default().with_permissive_mode(true);
        let strict = DogStatsDCodecConfiguration::default().with_permissive_mode(false);

        for raw in [
            "_e{5,4}:title|text|d:notanumber|#env:prod",
            "_e{5,4}:title|text|p:not!valid|#env:prod",
            "_e{5,4}:title|text|t:not!valid|#env:prod",
        ] {
            let (_, event) = parse_dogstatsd_event(raw.as_bytes(), &permissive)
                .unwrap_or_else(|e| panic!("permissive mode should accept {raw:?}: {e:?}"));
            assert_eq!(event.timestamp, None);
            assert_eq!(event.priority, Some(Priority::Normal));
            assert_eq!(event.alert_type, Some(AlertType::Info));
            assert_eq!(event.tags.iter().collect::<Vec<_>>(), ["env:prod"]);

            assert!(
                parse_dogstatsd_event(raw.as_bytes(), &strict).is_err(),
                "strict mode should reject {raw:?}"
            );
        }
    }

    #[test]
    fn unknown_priority_and_alert_type_keep_defaults() {
        // Regression test: an unrecognized priority or alert type resolves to the default (normal, info), as in the
        // reference DogStatsD server.
        let config = DogStatsDCodecConfiguration::default();
        let (_, event) =
            parse_dogstatsd_event(b"_e{5,4}:title|text|p:urgent|t:fatal", &config).expect("event should parse");
        assert_eq!(event.priority, Some(Priority::Normal));
        assert_eq!(event.alert_type, Some(AlertType::Info));
    }

    #[test]
    fn client_origin_fields_ignored_when_disabled() {
        let local_data = "abcdef123456";
        let external_data = "it-false,cn-redis,pu-810fe89d-da47-410b-8979-9154a40f8183";
        let raw = format!("_e{{5,4}}:title|text|c:{}|e:{}|card:low", local_data, external_data);
        let config = DogStatsDCodecConfiguration::default().with_client_origin_detection(false);
        let (_, packet) = parse_dogstatsd_event(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.origin.local_data, None);
        assert_eq!(packet.origin.external_data, None);
        assert_eq!(packet.origin.cardinality, None);
    }

    #[test]
    fn empty_structured_fields_treated_as_missing() {
        // All optional stringy fields are empty — should parse successfully and treat them as missing.
        let raw = "_e{5,4}:title|text|h:|k:|s:|c:|e:|card:|#";
        let config = DogStatsDCodecConfiguration::default();
        let (_, packet) = parse_dogstatsd_event(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.hostname, None);
        assert_eq!(packet.aggregation_key, None);
        assert_eq!(packet.source_type_name, None);
        assert_eq!(packet.origin.local_data, None);
        assert_eq!(packet.origin.external_data, None);
        assert_eq!(packet.origin.cardinality, None);
        assert!(packet.tags.iter().next().is_none());
    }
}
