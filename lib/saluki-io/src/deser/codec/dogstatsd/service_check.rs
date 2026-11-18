use nom::{
    bytes::complete::{tag, take_while1},
    character::complete::u8 as parse_u8,
    combinator::all_consuming,
    error::{Error, ErrorKind},
    sequence::{preceded, separated_pair},
    IResult, Parser as _,
};
use saluki_core::data_model::{event::service_check::*, origin::OriginTagCardinality, tags::RawTags};
use stringtheory::MetaString;
use tracing::debug;

use super::{helpers::*, DogStatsDCodecConfiguration, NomParserError};

/// Parses a service check name in permissive mode: any UTF-8, up to the `|` field delimiter.
fn permissive_service_check_name(input: &[u8]) -> IResult<&[u8], &str> {
    let (remaining, raw_name) = take_while1(|c: u8| c != b'|').parse(input)?;
    match simdutf8::basic::from_utf8(raw_name) {
        Ok(name) => Ok((remaining, name)),
        Err(_) => Err(nom::Err::Error(Error::new(raw_name, ErrorKind::Verify))),
    }
}

/// A DogStatsD service check packet.
pub struct ServiceCheckPacket<'a> {
    pub name: MetaString,
    pub status: CheckStatus,
    pub timestamp: Option<u64>,
    pub hostname: Option<&'a str>,
    pub message: Option<&'a str>,
    pub tags: RawTags<'a>,
    pub local_data: Option<&'a str>,
    pub external_data: Option<&'a str>,
    pub cardinality: Option<OriginTagCardinality>,
}

pub fn parse_dogstatsd_service_check<'a>(
    input: &'a [u8], config: &DogStatsDCodecConfiguration,
) -> IResult<&'a [u8], ServiceCheckPacket<'a>> {
    // In permissive mode, the name and hostname accept any UTF-8 (other than the `|` field delimiter), matching the
    // reference DogStatsD server. Otherwise, they're restricted to ASCII alphanumerics and separators.
    let (name_field, string_field): (StrParser, StrParser) = if config.permissive {
        (permissive_service_check_name, non_empty_utf8)
    } else {
        (ascii_alphanum_and_seps, ascii_alphanum_and_seps)
    };

    let (remaining, (name, raw_check_status)) = preceded(
        tag(SERVICE_CHECK_PREFIX),
        separated_pair(name_field, tag("|"), parse_u8),
    )
    .parse(input)?;

    let check_status =
        CheckStatus::try_from(raw_check_status).map_err(|_| nom::Err::Error(Error::new(input, ErrorKind::Verify)))?;

    let mut maybe_timestamp = None;
    let mut maybe_hostname = None;
    let mut maybe_tags = None;
    let mut maybe_message = None;
    let mut maybe_local_data = None;
    let mut maybe_external_data = None;
    let mut maybe_cardinality = None;

    let mut parse_field = |chunk: &'a [u8]| -> Result<(), NomParserError<'a>> {
        // Match on the two-byte field prefix. A one-byte field (for example, a bare `#`) can't match any of the
        // two-byte prefixes, so it either matches a single-byte prefix below or is unknown.
        match chunk.get(..2).unwrap_or(chunk) {
            // Timestamp: client-provided timestamp for the event, relative to the Unix epoch, in seconds.
            TIMESTAMP_PREFIX => {
                let (_, timestamp) = all_consuming(preceded(tag(TIMESTAMP_PREFIX), unix_timestamp)).parse(chunk)?;
                maybe_timestamp = Some(timestamp);
            }
            // Hostname: client-provided hostname for the host that this service check originated from.
            HOSTNAME_PREFIX => {
                let (_, hostname) = all_consuming(preceded(tag(HOSTNAME_PREFIX), string_field)).parse(chunk)?;
                maybe_hostname = Some(hostname);
            }
            // Local Data: client-provided data used for resolving the entity ID that this service check originated from.
            LOCAL_DATA_PREFIX if config.client_origin_detection => {
                let (_, local_data) = all_consuming(preceded(tag(LOCAL_DATA_PREFIX), local_data)).parse(chunk)?;
                maybe_local_data = Some(local_data);
            }
            // External Data: client-provided data used for resolving the entity ID that this service check originated from.
            EXTERNAL_DATA_PREFIX if config.client_origin_detection => {
                let (_, external_data) =
                    all_consuming(preceded(tag(EXTERNAL_DATA_PREFIX), external_data)).parse(chunk)?;
                maybe_external_data = Some(external_data);
            }
            // Tags: additional tags to be added to the service check. A bare `#` is a valid, empty set of tags.
            _ if chunk.starts_with(TAGS_PREFIX) => {
                let (_, tags) = all_consuming(preceded(tag(TAGS_PREFIX), tags(config))).parse(chunk)?;
                maybe_tags = Some(tags);
            }
            // Message: A message describing the current state of the service check.
            SERVICE_CHECK_MESSAGE_PREFIX => {
                let (_, message) = all_consuming(preceded(tag(SERVICE_CHECK_MESSAGE_PREFIX), utf8)).parse(chunk)?;
                maybe_message = Some(message);
            }
            // Cardinality: client-provided cardinality for the service check.
            _ if chunk.starts_with(CARDINALITY_PREFIX) && config.client_origin_detection => {
                let (_, cardinality) = cardinality(chunk)?;
                maybe_cardinality = cardinality;
            }
            // Origin detection fields are ignored when client origin detection is disabled.
            LOCAL_DATA_PREFIX | EXTERNAL_DATA_PREFIX => {}
            _ if chunk.starts_with(CARDINALITY_PREFIX) => {}
            // An unknown or malformed field (for example, a stray `T`). Like any other invalid field, the caller
            // skips it in permissive mode, matching the reference DogStatsD server, and rejects the service check
            // otherwise.
            _ => return Err(nom::Err::Error(Error::new(chunk, ErrorKind::Verify))),
        }
        Ok(())
    };

    let remaining = if !remaining.is_empty() {
        let (mut remaining, _) = tag("|")(remaining)?;
        while let Some((chunk, tail)) = split_at_delimiter(remaining, b'|') {
            // An empty field (for example, `0||#tags`) is a structural defect. In permissive mode, we skip it and keep
            // parsing the fields after it, matching the reference DogStatsD server. Otherwise, we reject the service
            // check.
            if chunk.is_empty() {
                if !config.permissive {
                    return Err(nom::Err::Error(Error::new(remaining, ErrorKind::Verify)));
                }
                remaining = tail;
                continue;
            }

            // In permissive mode, an invalid optional field is ignored rather than rejecting the whole service check,
            // matching the reference DogStatsD server.
            if let Err(e) = parse_field(chunk) {
                if !config.permissive {
                    return Err(e);
                }
                debug!(
                    field = %String::from_utf8_lossy(chunk),
                    "Ignoring invalid optional field in DogStatsD service check."
                );
            }
            remaining = tail;
        }
        remaining
    } else {
        remaining
    };

    let tags = maybe_tags.unwrap_or_else(RawTags::empty);

    let service_check_packet = ServiceCheckPacket {
        name: name.into(),
        status: check_status,
        tags,
        timestamp: maybe_timestamp,
        hostname: maybe_hostname,
        message: maybe_message,
        local_data: maybe_local_data,
        external_data: maybe_external_data,
        cardinality: maybe_cardinality,
    };
    Ok((remaining, service_check_packet))
}

#[cfg(test)]
mod tests {
    use nom::IResult;
    use saluki_core::data_model::{
        event::service_check::{CheckStatus, ServiceCheck},
        origin::OriginTagCardinality,
        tags::{SharedTagSet, TagSet},
    };
    use stringtheory::MetaString;

    use super::{parse_dogstatsd_service_check, DogStatsDCodecConfiguration};

    type NomResult<'input, T> = Result<T, nom::Err<nom::error::Error<&'input [u8]>>>;

    fn parse_dsd_service_check(input: &[u8]) -> NomResult<'_, ServiceCheck> {
        let default_config = DogStatsDCodecConfiguration::default();
        parse_dsd_service_check_with_conf(input, &default_config)
    }

    fn parse_dsd_service_check_with_conf<'input>(
        input: &'input [u8], config: &DogStatsDCodecConfiguration,
    ) -> NomResult<'input, ServiceCheck> {
        let (remaining, service_check) = parse_dsd_service_check_direct(input, config)?;
        assert!(remaining.is_empty());

        Ok(service_check)
    }

    fn parse_dsd_service_check_direct<'input>(
        input: &'input [u8], config: &DogStatsDCodecConfiguration,
    ) -> IResult<&'input [u8], ServiceCheck> {
        let (remaining, packet) = parse_dogstatsd_service_check(input, config)?;
        assert!(remaining.is_empty());

        let mut service_check_tags = TagSet::default();
        for tag in packet.tags.iter() {
            service_check_tags.insert_tag(tag);
        }

        let service_check = ServiceCheck::new(packet.name, packet.status)
            .with_timestamp(packet.timestamp)
            .with_hostname(packet.hostname.map(|s| s.into()))
            .with_tags(service_check_tags)
            .with_message(packet.message.map(|s| s.into()));

        Ok((remaining, service_check))
    }

    #[track_caller]
    fn check_basic_service_check_eq(expected: ServiceCheck, actual: ServiceCheck) {
        assert_eq!(expected.name(), actual.name());
        assert_eq!(expected.status(), actual.status());
        assert_eq!(expected.timestamp(), actual.timestamp());
        assert_eq!(expected.hostname(), actual.hostname());
        assert_eq!(expected.tags(), actual.tags());
        assert_eq!(expected.message(), actual.message());
        assert_eq!(expected.origin_tags(), actual.origin_tags());
    }

    #[test]
    fn basic_service_checks() {
        let name = "testsvc";
        let sc_status = CheckStatus::Warning;
        let raw = format!("_sc|{}|{}", name, sc_status.as_u8());
        let actual = parse_dsd_service_check(raw.as_bytes()).unwrap();
        let expected = ServiceCheck::new(name, sc_status);
        check_basic_service_check_eq(expected, actual);
    }

    #[test]
    fn service_check_timestamp() {
        let name = "testsvc";
        let sc_status = CheckStatus::Warning;
        let sc_timestamp = 1234567890;
        let raw = format!("_sc|{}|{}|d:{}", name, sc_status.as_u8(), sc_timestamp);
        let actual = parse_dsd_service_check(raw.as_bytes()).unwrap();
        let expected = ServiceCheck::new(name, sc_status).with_timestamp(sc_timestamp);
        check_basic_service_check_eq(expected, actual);
    }

    #[test]
    fn service_check_fields_after_empty_or_one_byte_field_are_parsed() {
        let permissive = DogStatsDCodecConfiguration::default().with_permissive_mode(true);
        let strict = DogStatsDCodecConfiguration::default().with_permissive_mode(false);
        let name = "testsvc";
        let sc_status = CheckStatus::Warning;
        let tags = ["tag1", "tag2"];
        let shared_tag_set = SharedTagSet::from(TagSet::from_iter(tags.iter().map(|&s| s.into())));
        let expected = || ServiceCheck::new(name, sc_status).with_tags(shared_tag_set.clone());

        // An empty field is skipped in permissive mode, but rejects the service check in strict mode.
        let raw = format!("_sc|{}|{}||#{}", name, sc_status.as_u8(), tags.join(","));
        let actual = parse_dsd_service_check_with_conf(raw.as_bytes(), &permissive).unwrap();
        check_basic_service_check_eq(expected(), actual);
        assert!(parse_dogstatsd_service_check(raw.as_bytes(), &strict).is_err());

        // A one-byte field, such as a bare `#`, carries no value, and the fields after it are parsed in both modes.
        let raw = format!("_sc|{}|{}|#|#{}", name, sc_status.as_u8(), tags.join(","));
        for config in [&permissive, &strict] {
            let actual = parse_dsd_service_check_with_conf(raw.as_bytes(), config).unwrap();
            check_basic_service_check_eq(expected(), actual);
        }
    }

    #[test]
    fn unknown_or_malformed_fields_are_rejected_only_in_strict_mode() {
        let permissive = DogStatsDCodecConfiguration::default().with_permissive_mode(true);
        let strict = DogStatsDCodecConfiguration::default().with_permissive_mode(false);

        // A stray one-byte field, and an unknown field.
        for raw in [
            b"_sc|my.check|0|T|#env:prod".as_slice(),
            b"_sc|my.check|0|x:unknown|#env:prod",
        ] {
            let (_, service_check) = parse_dogstatsd_service_check(raw, &permissive)
                .unwrap_or_else(|e| panic!("permissive mode should skip the field in {raw:?}: {e:?}"));
            assert_eq!(service_check.tags.iter().collect::<Vec<_>>(), ["env:prod"]);

            assert!(
                parse_dogstatsd_service_check(raw, &strict).is_err(),
                "strict mode should reject {raw:?}"
            );
        }
    }

    #[test]
    fn service_check_tags() {
        let name = "testsvc";
        let sc_status = CheckStatus::Warning;
        let tags = ["tag1", "tag2"];
        let raw = format!("_sc|{}|{}|#{}", name, sc_status.as_u8(), tags.join(","));
        let actual = parse_dsd_service_check(raw.as_bytes()).unwrap();
        let shared_tag_set = SharedTagSet::from(TagSet::from_iter(tags.iter().map(|&s| s.into())));
        let expected = ServiceCheck::new(name, sc_status).with_tags(shared_tag_set);
        check_basic_service_check_eq(expected, actual);
    }

    #[test]
    fn service_check_tags_with_invalid_utf8_are_normalized() {
        let mut input = b"_sc|testsvc|0|#env:prod,tag:".to_vec();
        input.extend_from_slice(&[0xff, 0xfe]);

        let tags = ["env:prod", "tag:\u{FFFD}"];
        let expected = ServiceCheck::new("testsvc", CheckStatus::Ok).with_tags(SharedTagSet::from(TagSet::from_iter(
            tags.iter().map(|&tag| tag.into()),
        )));
        let actual = parse_dsd_service_check(&input).expect("service check with invalid tag bytes should parse");

        check_basic_service_check_eq(expected, actual);
    }

    #[test]
    fn service_check_message() {
        let name = "testsvc";
        let sc_status = CheckStatus::Ok;
        let sc_message = MetaString::from("service running properly");
        let raw = format!("_sc|{}|{}|m:{}", name, sc_status.as_u8(), sc_message);
        let actual = parse_dsd_service_check(raw.as_bytes()).unwrap();
        let expected = ServiceCheck::new(name, sc_status).with_message(sc_message);
        check_basic_service_check_eq(expected, actual);
    }

    #[test]
    fn service_check_fields_after_message() {
        let name = "testsvc";
        let sc_status = CheckStatus::Ok;
        let sc_message = MetaString::from("service running properly");
        let sc_local_data = "ci-1234567890";
        let sc_external_data = "it-false,cn-redis,pu-810fe89d-da47-410b-8979-9154a40f8183";
        let raw = format!(
            "_sc|{}|{}|#tag1,tag2|m:{}|c:{}|e:{}",
            name,
            sc_status.as_u8(),
            sc_message,
            sc_local_data,
            sc_external_data,
        );
        let result = parse_dsd_service_check(raw.as_bytes()).unwrap();
        let tags = ["tag1", "tag2"];
        let shared_tag_set = SharedTagSet::from(TagSet::from_iter(tags.iter().map(|&s| s.into())));
        let expected = ServiceCheck::new(name, sc_status)
            .with_message(sc_message)
            .with_tags(shared_tag_set);
        check_basic_service_check_eq(expected, result);

        // We need client_origin_detection on in order to parse local_data, external_data, and cardinality fields
        let config = DogStatsDCodecConfiguration::default().with_client_origin_detection(true);
        let (_, packet) = parse_dogstatsd_service_check(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.local_data, Some(sc_local_data));
        assert_eq!(packet.external_data, Some(sc_external_data));
    }

    #[test]
    fn service_check_multiple_extensions() {
        let name = "testsvc";
        let sc_status = CheckStatus::Unknown;
        let sc_timestamp = 1234567890;
        let sc_hostname = MetaString::from("myhost");
        let sc_local_data = "abcdef123456";
        let sc_external_data = "it-false,cn-redis,pu-810fe89d-da47-410b-8979-9154a40f8183";
        let tags = ["tag1", "tag2"];
        let sc_message = MetaString::from("service status unknown");
        let sc_cardinality = "none";
        let raw = format!(
            "_sc|{}|{}|d:{}|h:{}|c:{}|e:{}|card:{}|#{}|m:{}",
            name,
            sc_status.as_u8(),
            sc_timestamp,
            sc_hostname,
            sc_local_data,
            sc_external_data,
            sc_cardinality,
            tags.join(","),
            sc_message
        );
        let shared_tag_set = SharedTagSet::from(TagSet::from_iter(tags.iter().map(|&s| s.into())));
        let actual = parse_dsd_service_check(raw.as_bytes()).unwrap();
        let expected = ServiceCheck::new(name, sc_status)
            .with_timestamp(sc_timestamp)
            .with_hostname(sc_hostname)
            .with_tags(shared_tag_set)
            .with_message(sc_message);
        check_basic_service_check_eq(expected, actual);

        // We need client_origin_detection on in order to parse local_data, external_data, and cardinality fields
        let config = DogStatsDCodecConfiguration::default().with_client_origin_detection(true);
        let (_, packet) = parse_dogstatsd_service_check(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.local_data, Some(sc_local_data));
        assert_eq!(packet.external_data, Some(sc_external_data));
        assert_eq!(packet.cardinality, Some(OriginTagCardinality::None));
    }

    #[test]
    fn permissive_mode_accepts_utf8_names_and_hostnames() {
        let permissive = DogStatsDCodecConfiguration::default().with_permissive_mode(true);
        let strict = DogStatsDCodecConfiguration::default().with_permissive_mode(false);

        let raw = "_sc|my/chéck|0|h:host:1|#env:prod";
        let (_, service_check) = parse_dogstatsd_service_check(raw.as_bytes(), &permissive)
            .expect("permissive mode should accept any UTF-8 in the name and hostname");
        assert_eq!(&*service_check.name, "my/chéck");
        assert_eq!(service_check.hostname, Some("host:1"));
        assert_eq!(service_check.tags.iter().collect::<Vec<_>>(), ["env:prod"]);

        assert!(parse_dogstatsd_service_check(raw.as_bytes(), &strict).is_err());
    }

    #[test]
    fn permissive_mode_ignores_invalid_optional_fields() {
        let permissive = DogStatsDCodecConfiguration::default().with_permissive_mode(true);
        let strict = DogStatsDCodecConfiguration::default().with_permissive_mode(false);

        let raw = b"_sc|my.check|0|d:notanumber|#env:prod";
        let (_, service_check) =
            parse_dogstatsd_service_check(raw, &permissive).expect("permissive mode should ignore the invalid field");
        assert_eq!(service_check.timestamp, None);
        assert_eq!(service_check.tags.iter().collect::<Vec<_>>(), ["env:prod"]);

        assert!(parse_dogstatsd_service_check(raw, &strict).is_err());
    }

    #[test]
    fn client_origin_fields_ignored_when_disabled() {
        let local_data = "abcdef123456";
        let external_data = "it-false,cn-redis,pu-810fe89d-da47-410b-8979-9154a40f8183";
        let raw = format!("_sc|testsvc|0|c:{}|e:{}|card:low", local_data, external_data);
        let config = DogStatsDCodecConfiguration::default().with_client_origin_detection(false);
        let (_, packet) = parse_dogstatsd_service_check(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.local_data, None);
        assert_eq!(packet.external_data, None);
        assert_eq!(packet.cardinality, None);
    }

    #[test]
    fn service_check_semi_real_payload_kafka() {
        let raw_payload = "_sc|kafka.can_connect|2|#env:staging,service:datadog-agent,dd.internal.entity_id:none,dd.internal.card:none,instance:kafka-127.0.0.1-9999,jmx_server:127.0.0.1|m:Unable to instantiate or initialize instance 127.0.0.1:9999. Is the target JMX Server or JVM running? Failed to retrieve RMIServer stub: javax.naming.ServiceUnavailableException [Root exception is java.rmi.ConnectException: Connection refused to host: 127.0.0.1; nested exception is: \\n\tjava.net.ConnectException: Connection refused (Connection refused)]";
        let _ = parse_dsd_service_check(raw_payload.as_bytes()).unwrap();
    }
}
