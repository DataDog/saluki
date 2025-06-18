use nom::{
    bytes::complete::tag,
    character::complete::u8 as parse_u8,
    combinator::all_consuming,
    error::{Error, ErrorKind},
    sequence::{preceded, separated_pair},
    IResult, Parser as _,
};
use saluki_context::{origin::OriginTagCardinality, tags::RawTags};
use saluki_core::data_model::event::service_check::*;
use stringtheory::MetaString;

use super::{helpers::*, DogstatsdCodecConfiguration};

/// A DogStatsD service check packet.
pub struct ServiceCheckPacket<'a> {
    pub name: MetaString,
    pub status: CheckStatus,
    pub timestamp: Option<u64>,
    pub hostname: Option<&'a str>,
    pub message: Option<&'a str>,
    pub tags: RawTags<'a>,
    pub container_id: Option<&'a str>,
    pub external_data: Option<&'a str>,
    pub cardinality: Option<OriginTagCardinality>,
}

pub fn parse_dogstatsd_service_check<'a>(
    input: &'a [u8], config: &DogstatsdCodecConfiguration,
) -> IResult<&'a [u8], ServiceCheckPacket<'a>> {
    let (remaining, (name, raw_check_status)) = preceded(
        tag(SERVICE_CHECK_PREFIX),
        separated_pair(ascii_alphanum_and_seps, tag("|"), parse_u8),
    )
    .parse(input)?;

    let check_status =
        CheckStatus::try_from(raw_check_status).map_err(|_| nom::Err::Error(Error::new(input, ErrorKind::Verify)))?;

    let mut maybe_timestamp = None;
    let mut maybe_hostname = None;
    let mut maybe_tags = None;
    let mut maybe_message = None;
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
                // Hostname: client-provided hostname for the host that this service check originated from.
                HOSTNAME_PREFIX => {
                    let (_, hostname) =
                        all_consuming(preceded(tag(HOSTNAME_PREFIX), ascii_alphanum_and_seps)).parse(chunk)?;
                    maybe_hostname = Some(hostname);
                }
                // Container ID: client-provided container ID for the container that this service check originated from.
                CONTAINER_ID_PREFIX => {
                    let (_, container_id) =
                        all_consuming(preceded(tag(CONTAINER_ID_PREFIX), container_id)).parse(chunk)?;
                    maybe_container_id = Some(container_id);
                }
                // External Data: client-provided data used for resolving the entity ID that this service check originated from.
                EXTERNAL_DATA_PREFIX => {
                    let (_, external_data) =
                        all_consuming(preceded(tag(EXTERNAL_DATA_PREFIX), external_data)).parse(chunk)?;
                    maybe_external_data = Some(external_data);
                }
                // Tags: additional tags to be added to the service check.
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
                _ if chunk.starts_with(CARDINALITY_PREFIX) => {
                    let (_, cardinality) = cardinality(chunk)?;
                    maybe_cardinality = cardinality;
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

    let service_check_packet = ServiceCheckPacket {
        name: name.into(),
        status: check_status,
        tags,
        timestamp: maybe_timestamp,
        hostname: maybe_hostname,
        message: maybe_message,
        container_id: maybe_container_id,
        external_data: maybe_external_data,
        cardinality: maybe_cardinality,
    };
    Ok((remaining, service_check_packet))
}

#[cfg(test)]
mod tests {
    use nom::IResult;
    use saluki_context::{
        origin::OriginTagCardinality,
        tags::{SharedTagSet, TagSet},
    };
    use saluki_core::data_model::event::service_check::{CheckStatus, ServiceCheck};
    use stringtheory::MetaString;

    use super::{parse_dogstatsd_service_check, DogstatsdCodecConfiguration};

    type NomResult<'input, T> = Result<T, nom::Err<nom::error::Error<&'input [u8]>>>;

    fn parse_dsd_service_check(input: &[u8]) -> NomResult<'_, ServiceCheck> {
        let default_config = DogstatsdCodecConfiguration::default();
        parse_dsd_service_check_with_conf(input, &default_config)
    }

    fn parse_dsd_service_check_with_conf<'input>(
        input: &'input [u8], config: &DogstatsdCodecConfiguration,
    ) -> NomResult<'input, ServiceCheck> {
        let (remaining, service_check) = parse_dsd_service_check_direct(input, config)?;
        assert!(remaining.is_empty());

        Ok(service_check)
    }

    fn parse_dsd_service_check_direct<'input>(
        input: &'input [u8], config: &DogstatsdCodecConfiguration,
    ) -> IResult<&'input [u8], ServiceCheck> {
        let (remaining, packet) = parse_dogstatsd_service_check(input, config)?;
        assert!(remaining.is_empty());

        let mut service_check_tags = TagSet::default();
        for tag in packet.tags.into_iter() {
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
        let sc_container_id = "ci-1234567890";
        let sc_external_data = "it-false,cn-redis,pu-810fe89d-da47-410b-8979-9154a40f8183";
        let raw = format!(
            "_sc|{}|{}|#tag1,tag2|m:{}|c:{}|e:{}",
            name,
            sc_status.as_u8(),
            sc_message,
            sc_container_id,
            sc_external_data,
        );
        let result = parse_dsd_service_check(raw.as_bytes()).unwrap();
        let tags = ["tag1", "tag2"];
        let shared_tag_set = SharedTagSet::from(TagSet::from_iter(tags.iter().map(|&s| s.into())));
        let expected = ServiceCheck::new(name, sc_status)
            .with_message(sc_message)
            .with_tags(shared_tag_set);
        check_basic_service_check_eq(expected, result);

        let config = DogstatsdCodecConfiguration::default();
        let (_, packet) = parse_dogstatsd_service_check(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.container_id, Some(sc_container_id));
        assert_eq!(packet.external_data, Some(sc_external_data));
    }

    #[test]
    fn service_check_multiple_extensions() {
        let name = "testsvc";
        let sc_status = CheckStatus::Unknown;
        let sc_timestamp = 1234567890;
        let sc_hostname = MetaString::from("myhost");
        let sc_container_id = "abcdef123456";
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
            sc_container_id,
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

        let config = DogstatsdCodecConfiguration::default();
        let (_, packet) = parse_dogstatsd_service_check(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.container_id, Some(sc_container_id));
        assert_eq!(packet.external_data, Some(sc_external_data));
        assert_eq!(packet.cardinality, Some(OriginTagCardinality::None));
    }

    #[test]
    fn service_check_semi_real_payload_kafka() {
        let raw_payload = "_sc|kafka.can_connect|2|#env:staging,service:datadog-agent,dd.internal.entity_id:none,dd.internal.card:none,instance:kafka-127.0.0.1-9999,jmx_server:127.0.0.1|m:Unable to instantiate or initialize instance 127.0.0.1:9999. Is the target JMX Server or JVM running? Failed to retrieve RMIServer stub: javax.naming.ServiceUnavailableException [Root exception is java.rmi.ConnectException: Connection refused to host: 127.0.0.1; nested exception is: \\n\tjava.net.ConnectException: Connection refused (Connection refused)]";
        let _ = parse_dsd_service_check(raw_payload.as_bytes()).unwrap();
    }
}
